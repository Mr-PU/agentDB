use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::actions::ProposeError;
use crate::actions_api::ActionsConfig;
use crate::api::{api_error, internal_error, is_unique_violation, ApiState};
use crate::extract::{ValidatedJson, ValidatedQuery};
use crate::tool_registry::{
    call_tool, list_tools_for_agent, CallToolError, CallToolResult, ToolDefinition,
};

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/mcp/tools", get(list_mcp_tools))
        .route("/mcp/tools/call", post(call_mcp_tool))
        .route("/tools", get(list_all_tools).post(create_tool))
}

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
// GET /mcp/tools
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListToolsQuery {
    agent_id: Uuid,
}

/// Returns the tool list visible to the given agent — see
/// `tool_registry::list_tools_for_agent` for what "visible" means here
/// (coarse, capability-level; not a guarantee any specific call will
/// succeed).
async fn list_mcp_tools(
    State(state): State<ApiState>,
    ValidatedQuery(params): ValidatedQuery<ListToolsQuery>,
) -> Response {
    match list_tools_for_agent(&state.pool, params.agent_id).await {
        Ok(tools) => Json(tools).into_response(),
        Err(e) => internal_error("listing tools for agent", e),
    }
}

// ---------------------------------------------------------------------
// POST /mcp/tools/call
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CallToolRequest {
    agent_id: Uuid,
    tool_name: String,
    #[serde(default)]
    parameters: Value,
}

async fn call_mcp_tool(
    State(state): State<ApiState>,
    Extension(actions_config): Extension<ActionsConfig>,
    ValidatedJson(req): ValidatedJson<CallToolRequest>,
) -> Response {
    let result = call_tool(
        &state.pool,
        &actions_config.0.registry,
        req.agent_id,
        &req.tool_name,
        req.parameters,
    )
    .await;

    match result {
        Ok(CallToolResult::Data { data }) => Json(data).into_response(),
        // A write tool's result is the action record itself, status and
        // all — 201 because a new `actions` row was created, same as
        // POST /actions directly. The caller reads `status` to know
        // whether this needs approval ("proposed") or was denied
        // ("failed"), exactly as if they'd called /actions themselves —
        // no separate pending_approval wrapper invented for this path.
        Ok(CallToolResult::Action { action }) => {
            (StatusCode::CREATED, Json(action)).into_response()
        }
        Err(CallToolError::UnknownTool(name)) => {
            api_error(StatusCode::NOT_FOUND, format!("unknown tool: {name}"))
        }
        Err(CallToolError::PermissionDenied(reason)) => api_error(
            StatusCode::FORBIDDEN,
            format!("permission denied: {}", reason.as_str()),
        ),
        Err(CallToolError::InvalidParameters(msg)) => api_error(StatusCode::BAD_REQUEST, msg),
        Err(CallToolError::Propose(propose_err)) => match propose_err {
            ProposeError::UnknownActionType(t) => internal_error(
                "calling tool",
                format!("tool references an action_type with no registered handler: {t}"),
            ),
            ProposeError::Action(action_err) => {
                api_error(StatusCode::BAD_REQUEST, action_err.to_string())
            }
            ProposeError::Database(e) => internal_error("calling tool", e),
        },
        Err(CallToolError::Database(e)) => internal_error("calling tool", e),
    }
}

// ---------------------------------------------------------------------
// GET /tools, POST /tools — admin view/registration
// ---------------------------------------------------------------------

async fn list_all_tools(State(state): State<ApiState>) -> Response {
    let rows: Result<Vec<ToolDefinition>, sqlx::Error> =
        sqlx::query_as("SELECT * FROM tools ORDER BY name")
            .fetch_all(&state.pool)
            .await;

    match rows {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("listing tools", e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateToolRequest {
    name: String,
    description: String,
    #[serde(default)]
    input_schema: Value,
    required_capability_name: String,
    backing_action_type: Option<String>,
    #[serde(default)]
    is_read_only: bool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct ToolResponse {
    id: Uuid,
    name: String,
    description: String,
    input_schema: Value,
    required_capability_id: Uuid,
    backing_action_type: Option<String>,
    is_read_only: bool,
}

async fn create_tool(
    State(state): State<ApiState>,
    ValidatedJson(req): ValidatedJson<CreateToolRequest>,
) -> Response {
    let name = match require_non_empty("name", &req.name, 200) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    let description = match require_non_empty("description", &req.description, 2000) {
        Ok(d) => d,
        Err(resp) => return *resp,
    };
    let capability_name = match require_non_empty(
        "required_capability_name",
        &req.required_capability_name,
        200,
    ) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };

    // Mirrors the schema's own CHECK constraint
    // (is_read_only = (backing_action_type IS NULL)) at the API layer,
    // so a mismatch here is a clear 400 rather than an opaque database
    // constraint-violation error.
    match (req.is_read_only, &req.backing_action_type) {
        (true, Some(_)) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "a read-only tool must not set backing_action_type",
            )
        }
        (false, None) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "a write tool (is_read_only: false) must set backing_action_type",
            )
        }
        _ => {}
    }

    let capability_id: Result<Option<Uuid>, sqlx::Error> =
        sqlx::query_scalar("SELECT id FROM capabilities WHERE name = $1")
            .bind(capability_name)
            .fetch_optional(&state.pool)
            .await;

    let capability_id = match capability_id {
        Ok(Some(id)) => id,
        Ok(None) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("unknown capability: {capability_name}"),
            )
        }
        Err(e) => return internal_error("looking up capability for tool", e),
    };

    let result = sqlx::query_as::<_, ToolResponse>(
        "INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, name, description, input_schema, required_capability_id, backing_action_type, is_read_only",
    )
    .bind(name)
    .bind(description)
    .bind(&req.input_schema)
    .bind(capability_id)
    .bind(&req.backing_action_type)
    .bind(req.is_read_only)
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(tool) => (StatusCode::CREATED, Json(tool)).into_response(),
        Err(e) if is_unique_violation(&e) => api_error(
            StatusCode::CONFLICT,
            format!("tool '{name}' already exists"),
        ),
        Err(e) => internal_error("creating tool", e),
    }
}
