use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::actions::{propose_action, ActionRecord, HandlerRegistry, ProposeError};
use crate::api::{query_network, query_processes, query_services};
use crate::permissions::{check_permission, PermissionResult};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ToolDefinition {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub required_capability_id: Uuid,
    pub backing_action_type: Option<String>,
    pub is_read_only: bool,
}

#[derive(Debug)]
pub enum CallToolError {
    UnknownTool(String),
    PermissionDenied(crate::permissions::DenialReason),
    InvalidParameters(String),
    Propose(ProposeError),
    Database(sqlx::Error),
}

/// What a tool call produces: either data (read-only tools) or an
/// `ActionRecord` (write tools, routed through Phase 3's action engine
/// unmodified — the registry never executes anything itself). A write
/// tool's result still has `status: "proposed"` (or `"failed"` on
/// denial) exactly as if `/actions` had been called directly — there is
/// no separate "pending_approval" wrapper invented here; the action's
/// own `status` field is the signal, same as everywhere else in this
/// codebase.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallToolResult {
    Data { data: Value },
    Action { action: Box<ActionRecord> },
}

/// List-time filtering: a coarse, capability-level visibility check —
/// "does this agent hold *any* active grant for this capability,
/// regardless of scope." This is intentionally imprecise
/// (phase5-tool-registry.md §4): an agent with a grant scoped to a
/// single PID still sees `process.stop` in its tool list, because the
/// question here is only "is it worth this agent knowing this tool
/// exists," not "can it use it right now with these exact parameters."
/// The precise, scope-specific decision happens only at call time, in
/// `call_tool` below — never here.
///
/// An agent that isn't `active` sees an empty list regardless of any
/// grants it holds, mirroring `check_permission`'s own agent-status
/// gate — a suspended agent shouldn't be shown tools it would be denied
/// the moment it tried to use any of them.
pub async fn list_tools_for_agent(
    pool: &PgPool,
    agent_id: Uuid,
) -> Result<Vec<ToolDefinition>, sqlx::Error> {
    let agent_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(pool)
            .await?;

    if agent_status.as_deref() != Some("active") {
        return Ok(Vec::new());
    }

    sqlx::query_as(
        "SELECT t.* FROM tools t \
         WHERE EXISTS ( \
             SELECT 1 FROM grants g \
             WHERE g.agent_id = $1 \
               AND g.capability_id = t.required_capability_id \
               AND g.revoked_at IS NULL \
               AND (g.expires_at IS NULL OR g.expires_at > now()) \
         ) \
         ORDER BY t.name",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
}

/// Call-time enforcement. This is where the real, scope-specific
/// decision happens — every call re-checks permission regardless of
/// whether the tool appeared in a prior `list_tools_for_agent` call,
/// because a grant can expire or be revoked in the gap between listing
/// and calling.
///
/// For a write tool, this function does NOT perform its own permission
/// check at all — it delegates entirely to `propose_action`, which
/// internally calls `check_permission_tx` using the handler's own
/// `scope_of()` derivation (e.g. the exact `pid:1234` from the call's
/// parameters). Doing a second, coarser check here first would be a
/// second authorization path layered on top of the real one — exactly
/// what CONTRIBUTING.md's "single enforcement choke point" rule exists
/// to prevent. For a read-only tool, there is no Phase 3 engine to
/// delegate to, so this function performs the one and only check itself,
/// using the tool's own name as the scope (Phase 1's mirror queries have
/// no finer-grained natural scope than "this query" — see the seed
/// migration's comment on why `filesystem.read` is used for all three).
pub async fn call_tool(
    pool: &PgPool,
    registry: &HandlerRegistry,
    agent_id: Uuid,
    tool_name: &str,
    parameters: Value,
) -> Result<CallToolResult, CallToolError> {
    let tool: Option<ToolDefinition> = sqlx::query_as("SELECT * FROM tools WHERE name = $1")
        .bind(tool_name)
        .fetch_optional(pool)
        .await
        .map_err(CallToolError::Database)?;

    let tool = tool.ok_or_else(|| CallToolError::UnknownTool(tool_name.to_string()))?;

    if tool.is_read_only {
        let capability_name: String =
            sqlx::query_scalar("SELECT name FROM capabilities WHERE id = $1")
                .bind(tool.required_capability_id)
                .fetch_one(pool)
                .await
                .map_err(CallToolError::Database)?;

        let permission = check_permission(pool, agent_id, &capability_name, tool_name)
            .await
            .map_err(CallToolError::Database)?;

        if let PermissionResult::Denied { reason } = permission {
            return Err(CallToolError::PermissionDenied(reason));
        }

        let data = execute_read_tool(pool, tool_name, &parameters).await?;
        return Ok(CallToolResult::Data { data });
    }

    let action_type = tool.backing_action_type.ok_or_else(|| {
        // Should be unreachable given the schema's CHECK constraint
        // (is_read_only = (backing_action_type IS NULL)), but handled
        // explicitly rather than unwrapped, in case the registry is ever
        // populated through a path that bypasses that constraint.
        CallToolError::InvalidParameters(format!("tool '{tool_name}' has no backing action type"))
    })?;

    let action = propose_action(pool, registry, agent_id, &action_type, parameters, None)
        .await
        .map_err(CallToolError::Propose)?;

    Ok(CallToolResult::Action {
        action: Box::new(action),
    })
}

/// The read-tool dispatch table. Each arm calls the exact same
/// pool-level function the corresponding `/processes`, `/network`,
/// `/services` HTTP handler in `api.rs` uses — see the doc comments on
/// `api::query_processes`/`query_network`/`query_services` for why those
/// are shared functions rather than SQL duplicated in two places.
async fn execute_read_tool(
    pool: &PgPool,
    tool_name: &str,
    parameters: &Value,
) -> Result<Value, CallToolError> {
    match tool_name {
        "query.processes" => {
            let name_filter = parameters.get("name_filter").and_then(|v| v.as_str()).unwrap_or("");
            let sort_by = parameters.get("sort_by").and_then(|v| v.as_str());
            let limit = parameters
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(50)
                .clamp(1, 500);

            let rows = query_processes(pool, name_filter, sort_by, 0, limit)
                .await
                .map_err(CallToolError::Database)?;
            Ok(serde_json::to_value(rows).expect("Process serialization cannot fail"))
        }
        "query.network" => {
            let limit = parameters
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(200)
                .clamp(1, 500);
            let rows = query_network(pool, limit).await.map_err(CallToolError::Database)?;
            Ok(serde_json::to_value(rows).expect("NetworkConnection serialization cannot fail"))
        }
        "query.services" => {
            let rows = query_services(pool).await.map_err(CallToolError::Database)?;
            Ok(serde_json::to_value(rows).expect("Service serialization cannot fail"))
        }
        other => Err(CallToolError::InvalidParameters(format!(
            "tool '{other}' is marked read-only but has no read handler registered in execute_read_tool — \
             this is a registry/code mismatch, not a bad request"
        ))),
    }
}
