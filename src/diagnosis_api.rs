use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Extension, Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::{api_error, internal_error, ApiState};
use crate::diagnosis::{diagnose, DiagnosisConfig, DiagnosisError, DiagnosisMode};
use crate::extract::ValidatedJson;

pub fn routes() -> Router<ApiState> {
    Router::new().route("/diagnose", post(diagnose_handler))
}

#[derive(Debug, Deserialize)]
struct DiagnoseRequest {
    /// Optional — when present, scopes the `recent_events` portion of
    /// the gathered context to this agent (see the doc comment on
    /// `diagnosis::DiagnosticContext` for what is and isn't
    /// agent-scoped). Does NOT gate the diagnosis behind this agent's
    /// grants; see that same doc comment for why.
    agent_id: Option<Uuid>,
    /// "rule_based" or "llm". Omit to use the server's configured
    /// default (`DIAGNOSIS_MODE`). Requesting "llm" when it isn't
    /// configured is a clear 400, never a silent fallback.
    mode: Option<String>,
    #[serde(default)]
    lookback_minutes: Option<i64>,
}

/// `agentdb ask "why is my machine slow?"` lands here. `agentdb ask "can
/// you fix it?"` is NOT a second endpoint — it's the caller taking this
/// response's `suggested_action` and passing it to `POST /actions` or
/// `POST /mcp/tools/call` themselves, going through the real
/// permission-check and approval-gate flow unchanged. See the Phase 6
/// walkthrough in the README for the full two-step example.
async fn diagnose_handler(
    State(state): State<ApiState>,
    Extension(config): Extension<DiagnosisConfig>,
    ValidatedJson(req): ValidatedJson<DiagnoseRequest>,
) -> Response {
    let mode = match req.mode.as_deref() {
        Some(m) => match DiagnosisMode::parse_str(m) {
            Some(parsed) => Some(parsed),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    format!("invalid mode '{m}', expected 'rule_based' or 'llm'"),
                )
            }
        },
        None => None,
    };

    let lookback_minutes = req.lookback_minutes.unwrap_or(10).clamp(1, 1440);

    match diagnose(&state.pool, &config, req.agent_id, mode, lookback_minutes).await {
        Ok(result) => Json(result).into_response(),
        Err(DiagnosisError::Database(e)) => internal_error("running diagnosis", e),
        Err(DiagnosisError::LlmNotConfigured) => api_error(
            StatusCode::BAD_REQUEST,
            "LLM diagnosis mode was requested but is not configured — set ANTHROPIC_API_KEY and ANTHROPIC_MODEL",
        ),
        // 502, not 500: the failure is in an upstream dependency (the
        // Anthropic API), not this service's own database or logic —
        // worth distinguishing in the status code for anyone debugging.
        Err(DiagnosisError::LlmRequestFailed(msg)) => {
            api_error(StatusCode::BAD_GATEWAY, format!("LLM request failed: {msg}"))
        }
        Err(DiagnosisError::LlmResponseMalformed(msg)) => {
            api_error(StatusCode::BAD_GATEWAY, format!("LLM response could not be used: {msg}"))
        }
    }
}
