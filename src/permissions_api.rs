use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{api_error, internal_error, is_unique_violation, ApiState};
use crate::events::{emit_event, event_type};
use crate::extract::{ValidatedJson, ValidatedQuery};
use crate::permissions::{check_permission, PermissionResult};

/// Routes for this module, returned unstated so `api::router` can
/// `.merge()` them in before attaching the shared `ApiState` once for
/// the whole combined router.
pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/agents", post(create_agent))
        .route("/agents/:id", get(get_agent))
        .route(
            "/capabilities",
            get(list_capabilities).post(create_capability),
        )
        .route("/grants", post(create_grant).get(list_grants))
        .route("/grants/:id/revoke", post(revoke_grant))
        .route("/permission-checks", get(list_permission_checks))
        .route("/permissions/check", post(debug_check_permission))
}

// ---------------------------------------------------------------------
// Shared input validation
// ---------------------------------------------------------------------

/// Trims and bounds-checks a required string field, returning a ready-to-
/// use `Response` on failure so callers can `match` and `return` inline
/// without duplicating the same empty/too-long checks at every call site.
fn require_non_empty<'a>(
    field_name: &str,
    value: &'a str,
    max_len: usize,
) -> Result<&'a str, Box<Response>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            format!("{field_name} must not be empty"),
        )));
    }
    if trimmed.len() > max_len {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            format!("{field_name} must be at most {max_len} characters"),
        )));
    }
    Ok(trimmed)
}

// ---------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateAgentRequest {
    name: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct AgentResponse {
    id: Uuid,
    name: String,
    status: String,
    created_at: DateTime<Utc>,
}

async fn create_agent(
    State(state): State<ApiState>,
    ValidatedJson(req): ValidatedJson<CreateAgentRequest>,
) -> Response {
    let name = match require_non_empty("name", &req.name, 200) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal_error("starting transaction for agent creation", e),
    };

    let result = sqlx::query_as::<_, AgentResponse>(
        "INSERT INTO agents (name) VALUES ($1) RETURNING id, name, status, created_at",
    )
    .bind(name)
    .fetch_one(&mut *tx)
    .await;

    let agent = match result {
        Ok(a) => a,
        Err(e) => return internal_error("creating agent", e),
    };

    if let Err(e) = emit_event(
        &mut tx,
        agent.id,
        None,
        None,
        event_type::AGENT_CREATED,
        serde_json::json!({ "name": agent.name }),
    )
    .await
    {
        return internal_error("recording agent creation event", e);
    }

    if let Err(e) = tx.commit().await {
        return internal_error("committing agent creation", e);
    }

    (StatusCode::CREATED, Json(agent)).into_response()
}

async fn get_agent(State(state): State<ApiState>, Path(agent_id): Path<Uuid>) -> Response {
    let row = sqlx::query_as::<_, AgentResponse>(
        "SELECT id, name, status, created_at FROM agents WHERE id = $1",
    )
    .bind(agent_id)
    .fetch_optional(&state.pool)
    .await;

    match row {
        Ok(Some(agent)) => Json(agent).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "agent not found"),
        Err(e) => internal_error("looking up agent", e),
    }
}

// ---------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
struct CapabilityResponse {
    id: Uuid,
    name: String,
    description: Option<String>,
}

async fn list_capabilities(State(state): State<ApiState>) -> Response {
    let rows = sqlx::query_as::<_, CapabilityResponse>(
        "SELECT id, name, description FROM capabilities ORDER BY name",
    )
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("listing capabilities", e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateCapabilityRequest {
    name: String,
    description: Option<String>,
}

async fn create_capability(
    State(state): State<ApiState>,
    ValidatedJson(req): ValidatedJson<CreateCapabilityRequest>,
) -> Response {
    let name = match require_non_empty("name", &req.name, 200) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };

    let description = match &req.description {
        Some(d) if d.len() > 2000 => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "description must be at most 2000 characters",
            );
        }
        other => other.as_deref(),
    };

    let result = sqlx::query_as::<_, CapabilityResponse>(
        "INSERT INTO capabilities (name, description) VALUES ($1, $2) RETURNING id, name, description",
    )
    .bind(name)
    .bind(description)
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(cap) => (StatusCode::CREATED, Json(cap)).into_response(),
        Err(e) if is_unique_violation(&e) => api_error(
            StatusCode::CONFLICT,
            format!("capability '{name}' already exists"),
        ),
        Err(e) => internal_error("creating capability", e),
    }
}

// ---------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------

/// `non_expiring` is computed (`expires_at IS NULL`) rather than derived
/// client-side, so API consumers get an explicit signal to surface
/// ("review recommended" per phase2-permission-engine.md §5) without
/// having to remember that NULL means that themselves.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct GrantResponse {
    id: Uuid,
    agent_id: Uuid,
    capability_name: String,
    scope: String,
    granted_by: String,
    granted_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by: Option<String>,
    non_expiring: bool,
}

const GRANT_SELECT_COLUMNS: &str = "g.id, g.agent_id, c.name AS capability_name, g.scope, \
     g.granted_by, g.granted_at, g.expires_at, g.revoked_at, g.revoked_by, \
     (g.expires_at IS NULL) AS non_expiring";

#[derive(Debug, Deserialize)]
struct CreateGrantRequest {
    agent_id: Uuid,
    capability_name: String,
    scope: String,
    granted_by: String,
    /// Omit (or send `null`) for a non-expiring grant. If present, must
    /// be strictly in the future at request time — a grant that's
    /// already expired the moment it's created is almost certainly a
    /// client bug, not an intentional no-op grant.
    expires_at: Option<DateTime<Utc>>,
}

async fn create_grant(
    State(state): State<ApiState>,
    ValidatedJson(req): ValidatedJson<CreateGrantRequest>,
) -> Response {
    let scope = match require_non_empty("scope", &req.scope, 500) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let granted_by = match require_non_empty("granted_by", &req.granted_by, 200) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let capability_name = match require_non_empty("capability_name", &req.capability_name, 200) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };

    if let Some(exp) = req.expires_at {
        if exp <= Utc::now() {
            return api_error(StatusCode::BAD_REQUEST, "expires_at must be in the future");
        }
    }

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal_error("starting transaction for grant creation", e),
    };

    let agent_exists: Result<Option<Uuid>, sqlx::Error> =
        sqlx::query_scalar("SELECT id FROM agents WHERE id = $1")
            .bind(req.agent_id)
            .fetch_optional(&mut *tx)
            .await;

    let agent_exists = match agent_exists {
        Ok(v) => v,
        Err(e) => return internal_error("looking up agent for grant", e),
    };
    if agent_exists.is_none() {
        return api_error(StatusCode::NOT_FOUND, "agent not found");
    }

    let capability_id: Result<Option<Uuid>, sqlx::Error> =
        sqlx::query_scalar("SELECT id FROM capabilities WHERE name = $1")
            .bind(capability_name)
            .fetch_optional(&mut *tx)
            .await;

    let capability_id = match capability_id {
        Ok(Some(id)) => id,
        Ok(None) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("unknown capability: {capability_name}"),
            )
        }
        Err(e) => return internal_error("looking up capability for grant", e),
    };

    let sql = format!(
        "WITH inserted AS (
            INSERT INTO grants (agent_id, capability_id, scope, granted_by, expires_at)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING *
        )
        SELECT {GRANT_SELECT_COLUMNS}
        FROM inserted g JOIN capabilities c ON c.id = g.capability_id"
    );

    let result = sqlx::query_as::<_, GrantResponse>(&sql)
        .bind(req.agent_id)
        .bind(capability_id)
        .bind(scope)
        .bind(granted_by)
        .bind(req.expires_at)
        .fetch_one(&mut *tx)
        .await;

    let grant = match result {
        Ok(g) => g,
        Err(e) => return internal_error("creating grant", e),
    };

    if let Err(e) = emit_event(
        &mut tx,
        req.agent_id,
        None,
        Some(grant.id),
        event_type::GRANT_ISSUED,
        serde_json::json!({
            "capability_name": capability_name,
            "scope": scope,
            "granted_by": granted_by,
            "expires_at": req.expires_at,
        }),
    )
    .await
    {
        return internal_error("recording grant issuance event", e);
    }

    if let Err(e) = tx.commit().await {
        return internal_error("committing grant creation", e);
    }

    (StatusCode::CREATED, Json(grant)).into_response()
}

#[derive(Debug, Deserialize)]
struct GrantsQuery {
    agent_id: Uuid,
    /// When true, also returns expired and revoked grants — useful for
    /// an audit view. Default (false/omitted) matches
    /// phase2-permission-engine.md §6's "what can this agent do right
    /// now" query: active, non-expired grants only.
    include_inactive: Option<bool>,
    limit: Option<i64>,
}

async fn list_grants(
    State(state): State<ApiState>,
    ValidatedQuery(params): ValidatedQuery<GrantsQuery>,
) -> Response {
    let include_inactive = params.include_inactive.unwrap_or(false);
    // Default 100, capped at 500 — same bound used for /permission-checks.
    // Without this, an agent with a long grant history and
    // include_inactive=true could return an unbounded number of rows.
    let limit = params.limit.unwrap_or(100).clamp(1, 500);

    let sql = if include_inactive {
        format!(
            "SELECT {GRANT_SELECT_COLUMNS} FROM grants g JOIN capabilities c ON c.id = g.capability_id \
             WHERE g.agent_id = $1 ORDER BY g.granted_at DESC LIMIT $2"
        )
    } else {
        format!(
            "SELECT {GRANT_SELECT_COLUMNS} FROM grants g JOIN capabilities c ON c.id = g.capability_id \
             WHERE g.agent_id = $1 AND g.revoked_at IS NULL \
             AND (g.expires_at IS NULL OR g.expires_at > now()) \
             ORDER BY g.granted_at DESC LIMIT $2"
        )
    };

    let rows = sqlx::query_as::<_, GrantResponse>(&sql)
        .bind(params.agent_id)
        .bind(limit)
        .fetch_all(&state.pool)
        .await;

    match rows {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("listing grants", e),
    }
}

#[derive(Debug, Deserialize)]
struct RevokeGrantRequest {
    revoked_by: String,
}

async fn revoke_grant(
    State(state): State<ApiState>,
    Path(grant_id): Path<Uuid>,
    ValidatedJson(req): ValidatedJson<RevokeGrantRequest>,
) -> Response {
    let revoked_by = match require_non_empty("revoked_by", &req.revoked_by, 200) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return internal_error("starting transaction for grant revocation", e),
    };

    let existing: Result<Option<(Option<DateTime<Utc>>,)>, sqlx::Error> =
        sqlx::query_as("SELECT revoked_at FROM grants WHERE id = $1")
            .bind(grant_id)
            .fetch_optional(&mut *tx)
            .await;

    match existing {
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "grant not found"),
        Ok(Some((Some(_already_revoked_at),))) => {
            return api_error(StatusCode::CONFLICT, "grant is already revoked")
        }
        Ok(Some((None,))) => {} // not yet revoked — proceed
        Err(e) => return internal_error("looking up grant", e),
    }

    // The `AND revoked_at IS NULL` here is not redundant with the check
    // above — it's what actually closes the race. Without it, two
    // concurrent revoke requests could both pass the SELECT check above
    // (both see revoked_at IS NULL) and then both succeed at this
    // UPDATE, with the second one silently overwriting the first's
    // revoked_by/revoked_at. With this condition, only the first writer
    // can match; the second gets zero rows back. Wrapping the SELECT and
    // this UPDATE in the same transaction doesn't change this property —
    // Postgres row-level locking under the default READ COMMITTED
    // isolation still serializes two concurrent UPDATEs against the same
    // row regardless of whether the preceding SELECT shares a
    // transaction with it.
    let sql = format!(
        "WITH updated AS (
            UPDATE grants SET revoked_at = now(), revoked_by = $2
            WHERE id = $1 AND revoked_at IS NULL
            RETURNING *
        )
        SELECT {GRANT_SELECT_COLUMNS}
        FROM updated g JOIN capabilities c ON c.id = g.capability_id"
    );

    let result = sqlx::query_as::<_, GrantResponse>(&sql)
        .bind(grant_id)
        .bind(revoked_by)
        .fetch_one(&mut *tx)
        .await;

    let grant = match result {
        Ok(g) => g,
        // fetch_one on zero rows is RowNotFound. Given the SELECT above
        // already confirmed the grant exists and wasn't revoked moments
        // earlier, the only way this UPDATE can match nothing is that a
        // concurrent request revoked it in between — the race this
        // WHERE clause exists to catch. Report it the same way as the
        // non-race "already revoked" case (409), not as a 500: this is
        // an expected, handled outcome of concurrent requests, not a
        // database failure.
        Err(sqlx::Error::RowNotFound) => {
            return api_error(StatusCode::CONFLICT, "grant is already revoked")
        }
        Err(e) => return internal_error("revoking grant", e),
    };

    if let Err(e) = emit_event(
        &mut tx,
        grant.agent_id,
        None,
        Some(grant.id),
        event_type::GRANT_REVOKED,
        serde_json::json!({ "revoked_by": revoked_by }),
    )
    .await
    {
        return internal_error("recording grant revocation event", e);
    }

    if let Err(e) = tx.commit().await {
        return internal_error("committing grant revocation", e);
    }

    Json(grant).into_response()
}

// ---------------------------------------------------------------------
// Permission checks (read-only audit log)
// ---------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
struct PermissionCheckResponse {
    id: Uuid,
    agent_id: Uuid,
    capability_name: String,
    requested_scope: String,
    decision: String,
    matched_grant_id: Option<Uuid>,
    reason: Option<String>,
    checked_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct PermissionChecksQuery {
    agent_id: Uuid,
    /// "allowed" or "denied" — anything else is a 400, not silently
    /// ignored, since a typo'd filter silently returning unfiltered
    /// results would be a worse failure mode than an explicit error.
    decision: Option<String>,
    limit: Option<i64>,
}

async fn list_permission_checks(
    State(state): State<ApiState>,
    ValidatedQuery(params): ValidatedQuery<PermissionChecksQuery>,
) -> Response {
    let limit = params.limit.unwrap_or(100).clamp(1, 500);

    let decision_filter = match params.decision.as_deref() {
        Some("allowed") => Some("allowed"),
        Some("denied") => Some("denied"),
        Some(other) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("invalid decision filter '{other}', expected 'allowed' or 'denied'"),
            );
        }
        None => None,
    };

    let rows = sqlx::query_as::<_, PermissionCheckResponse>(
        "SELECT id, agent_id, capability_name, requested_scope, decision, \
                matched_grant_id, reason, checked_at \
         FROM permission_checks \
         WHERE agent_id = $1 AND ($2::text IS NULL OR decision = $2) \
         ORDER BY checked_at DESC \
         LIMIT $3",
    )
    .bind(params.agent_id)
    .bind(decision_filter)
    .bind(limit)
    .fetch_all(&state.pool)
    .await;

    match rows {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("listing permission checks", e),
    }
}

// ---------------------------------------------------------------------
// Debug/inspection endpoint
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CheckPermissionRequest {
    agent_id: Uuid,
    capability_name: String,
    scope: String,
}

#[derive(Debug, Serialize)]
struct CheckPermissionResponse {
    decision: &'static str,
    grant_id: Option<Uuid>,
    reason: Option<&'static str>,
}

/// Exposes `permissions::check_permission` over HTTP for manual testing
/// and inspection. This is NOT the real enforcement call site for later
/// phases — Phase 3's action engine runs in the same process and calls
/// `check_permission` directly in-process, not over HTTP. This route
/// exists so Phase 2 can be exercised and demoed end-to-end before
/// Phase 3's action engine exists to call it for real.
async fn debug_check_permission(
    State(state): State<ApiState>,
    ValidatedJson(req): ValidatedJson<CheckPermissionRequest>,
) -> Response {
    let result =
        check_permission(&state.pool, req.agent_id, &req.capability_name, &req.scope).await;

    match result {
        Ok(PermissionResult::Allowed { grant_id }) => Json(CheckPermissionResponse {
            decision: "allowed",
            grant_id: Some(grant_id),
            reason: None,
        })
        .into_response(),
        Ok(PermissionResult::Denied { reason }) => Json(CheckPermissionResponse {
            decision: "denied",
            grant_id: None,
            reason: Some(reason.as_str()),
        })
        .into_response(),
        Err(e) => internal_error("checking permission", e),
    }
}
