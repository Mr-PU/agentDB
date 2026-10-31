use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::actions::{
    approve_action, execute_action, propose_action, ActionRecord, ApproveError, DryRunConfig,
    ExecuteError, HandlerRegistry, ProposeError,
};
use crate::api::{api_error, internal_error, ApiState};
use crate::extract::{ValidatedJson, ValidatedQuery};

/// Shared, process-lifetime configuration for the action engine: the
/// handler registry (stateless, built once) and the dry-run ceiling
/// (read from `ACTIONS_DRY_RUN` once at startup). Injected via
/// `Extension`, the same pattern already used for `auth::ApiKeyConfig`
/// and `rate_limit::RateLimiter` — see `main.rs`.
#[derive(Clone)]
pub struct ActionsConfig(pub Arc<ActionsConfigInner>);

pub struct ActionsConfigInner {
    pub registry: HandlerRegistry,
    pub dry_run: DryRunConfig,
}

impl ActionsConfig {
    pub fn from_env() -> Self {
        Self(Arc::new(ActionsConfigInner {
            registry: HandlerRegistry::default_handlers(),
            dry_run: DryRunConfig::from_env(),
        }))
    }
}

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/actions", post(create_action).get(list_actions))
        .route("/actions/:id", get(get_action))
        .route("/actions/:id/approve", post(approve_and_execute))
        .route("/action-types", get(list_action_types))
}

// ---------------------------------------------------------------------
// POST /actions
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateActionRequest {
    agent_id: Uuid,
    action_type: String,
    #[serde(default)]
    parameters: Value,
    /// Optional per-action preference for more caution than the server
    /// otherwise requires. See `DryRunConfig::effective` — this can
    /// never force real execution past the server's own ceiling.
    dry_run: Option<bool>,
}

async fn create_action(
    State(state): State<ApiState>,
    Extension(config): Extension<ActionsConfig>,
    ValidatedJson(req): ValidatedJson<CreateActionRequest>,
) -> Response {
    if req.action_type.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "action_type must not be empty");
    }

    let result = propose_action(
        &state.pool,
        &config.0.registry,
        req.agent_id,
        req.action_type.trim(),
        req.parameters,
        req.dry_run,
    )
    .await;

    match result {
        Ok(action) => (StatusCode::CREATED, Json(action)).into_response(),
        Err(ProposeError::UnknownActionType(t)) => {
            api_error(StatusCode::BAD_REQUEST, format!("unknown action_type: {t}"))
        }
        Err(ProposeError::Action(e)) => api_error(StatusCode::BAD_REQUEST, e.to_string()),
        Err(ProposeError::Database(e)) => internal_error("proposing action", e),
    }
}

// ---------------------------------------------------------------------
// GET /actions/:id
// ---------------------------------------------------------------------

async fn get_action(State(state): State<ApiState>, Path(action_id): Path<Uuid>) -> Response {
    let row = sqlx::query_as::<_, ActionRecord>("SELECT * FROM actions WHERE id = $1")
        .bind(action_id)
        .fetch_optional(&state.pool)
        .await;

    match row {
        Ok(Some(action)) => Json(action).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "action not found"),
        Err(e) => internal_error("looking up action", e),
    }
}

// ---------------------------------------------------------------------
// GET /actions?agent_id=&status=&limit=
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ActionsQuery {
    agent_id: Uuid,
    status: Option<String>,
    limit: Option<i64>,
}

const VALID_STATUSES: &[&str] = &[
    "proposed",
    "approved",
    "executing",
    "executed",
    "failed",
    "rolled_back",
    "compensated",
];

async fn list_actions(
    State(state): State<ApiState>,
    ValidatedQuery(params): ValidatedQuery<ActionsQuery>,
) -> Response {
    let limit = params.limit.unwrap_or(100).clamp(1, 500);

    let status_filter = match params.status.as_deref() {
        Some(s) if VALID_STATUSES.contains(&s) => Some(s),
        Some(other) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid status '{other}', expected one of: {}",
                    VALID_STATUSES.join(", ")
                ),
            )
        }
        None => None,
    };

    let rows = sqlx::query_as::<_, ActionRecord>(
        "SELECT * FROM actions \
         WHERE agent_id = $1 AND ($2::text IS NULL OR status = $2) \
         ORDER BY created_at DESC \
         LIMIT $3",
    )
    .bind(params.agent_id)
    .bind(status_filter)
    .bind(limit)
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("listing actions", e),
    }
}

// ---------------------------------------------------------------------
// POST /actions/:id/approve
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApproveActionRequest {
    approved_by: String,
}

/// Approves the action and, on success, immediately executes it — this
/// matches the CLI flow described in phase6-demo-loop.md §5 ("Approve
/// action... ? [y/N]: y" leads straight to execution). There's no
/// separate `/actions/:id/execute` endpoint in v0.1: approval *is* the
/// trigger. If that ever needs to be a separate, explicit step (e.g. to
/// let an approval be revoked before execution), splitting this into two
/// calls is straightforward — `approve_action` and `execute_action` are
/// already two distinct functions in `actions.rs`, this handler just
/// chains them.
async fn approve_and_execute(
    State(state): State<ApiState>,
    Extension(config): Extension<ActionsConfig>,
    Path(action_id): Path<Uuid>,
    ValidatedJson(req): ValidatedJson<ApproveActionRequest>,
) -> Response {
    if req.approved_by.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "approved_by must not be empty");
    }

    // The returned row isn't needed beyond confirming approval
    // succeeded — execution below proceeds on `action_id` regardless,
    // and re-fetches the row's current state itself.
    match approve_action(&state.pool, action_id, req.approved_by.trim()).await {
        Ok(_) => {}
        Err(ApproveError::NotFound) => return api_error(StatusCode::NOT_FOUND, "action not found"),
        Err(ApproveError::WrongStatus) => {
            return api_error(
                StatusCode::CONFLICT,
                "action is not in 'proposed' status (already approved, executed, or failed at proposal time)",
            )
        }
        Err(ApproveError::Database(e)) => return internal_error("approving action", e),
    }

    let executed = execute_action(
        &state.pool,
        &config.0.registry,
        &config.0.dry_run,
        action_id,
    )
    .await;

    match executed {
        Ok(action) => Json(action).into_response(),
        Err(ExecuteError::NotFound) => {
            // Should be unreachable immediately after a successful
            // approve_action on the same id, but handled explicitly
            // rather than assumed away.
            api_error(StatusCode::NOT_FOUND, "action not found during execution")
        }
        Err(ExecuteError::NotApproved(status)) => api_error(
            StatusCode::CONFLICT,
            format!("action status changed to '{status}' before execution could proceed"),
        ),
        Err(ExecuteError::UnknownActionType(t)) => internal_error(
            "executing action",
            format!("unknown action_type at execution time: {t}"),
        ),
        Err(ExecuteError::Database(e)) => internal_error("executing action", e),
    }
}

// ---------------------------------------------------------------------
// GET /action-types
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ActionTypeInfo {
    action_type: &'static str,
    required_capability: &'static str,
    is_reversible: bool,
}

async fn list_action_types(Extension(config): Extension<ActionsConfig>) -> Response {
    let info: Vec<ActionTypeInfo> = config
        .0
        .registry
        .action_types()
        .into_iter()
        .filter_map(|action_type| {
            config.0.registry.get(action_type).map(|h| ActionTypeInfo {
                action_type: h.action_type(),
                required_capability: h.required_capability(),
                is_reversible: h.is_reversible(),
            })
        })
        .collect();

    Json(info).into_response()
}
