use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::{api_error, internal_error, ApiState};
use crate::events::{action_causal_chain, recent_events};
use crate::extract::ValidatedQuery;

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/events", get(list_events))
        .route("/events/actions/:action_id", get(action_events))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// Filters to one agent's events — phase4-event-store.md §5's
    /// primary reconstruction query. Combines with `since` and
    /// `event_type` when more than one is present (all three apply
    /// together, not one overriding another). At least one of
    /// `agent_id` or `since` is required.
    agent_id: Option<Uuid>,
    /// "What changed since this time" — the literal query behind
    /// phase6-demo-loop.md's flagship demo. ISO 8601 / RFC 3339.
    /// Combines with `agent_id` when both are present, e.g.
    /// `?agent_id=X&since=Y` means "X's events since Y", not "every
    /// agent's events since Y with agent_id ignored."
    since: Option<DateTime<Utc>>,
    event_type: Option<String>,
    limit: Option<i64>,
}

async fn list_events(
    State(state): State<ApiState>,
    ValidatedQuery(params): ValidatedQuery<EventsQuery>,
) -> Response {
    let limit = params.limit.unwrap_or(100).clamp(1, 500);

    if params.agent_id.is_none() && params.since.is_none() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "at least one of 'agent_id' or 'since' is required",
        );
    }

    // A single combined-filter call — agent_id and since (and
    // event_type) all apply together when present, rather than one
    // silently overriding the other. See the doc comment on
    // events::recent_events for the bug this replaced.
    match recent_events(
        &state.pool,
        params.agent_id,
        params.since,
        params.event_type.as_deref(),
        limit,
    )
    .await
    {
        Ok(events) => Json(events).into_response(),
        Err(e) => internal_error("listing events", e),
    }
}

/// Deliberately returns an empty list rather than 404 for an unknown
/// action_id — distinguishing "no events for a real action" from
/// "action doesn't exist" would need an extra query against `actions`,
/// and this endpoint's job is reading events, not validating action ids.
/// Both cases legitimately have nothing to show.
async fn action_events(State(state): State<ApiState>, Path(action_id): Path<Uuid>) -> Response {
    match action_causal_chain(&state.pool, action_id).await {
        Ok(events) => Json(events).into_response(),
        Err(e) => internal_error("listing action causal chain", e),
    }
}
