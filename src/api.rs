use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use tracing::error;

use crate::extract::ValidatedQuery;
use crate::models::{MirrorHealth, NetworkConnection, Process, ProcessQuery, Service};
use crate::query_guard::validate_single_select;

/// Main pool serves the typed mirror endpoints. `readonly_pool` is a
/// separate connection intended to run as the SELECT-only `agentdb_reader`
/// role (see db::connect_readonly) and backs only /query — kept distinct
/// from `pool` so the raw-SQL endpoint never inherits write access just
/// because it shares a connection with the rest of the API.
#[derive(Clone)]
pub struct ApiState {
    pub pool: PgPool,
    pub readonly_pool: PgPool,
}

pub fn router(pool: PgPool, readonly_pool: PgPool) -> Router {
    let state = ApiState {
        pool,
        readonly_pool,
    };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/processes", get(list_processes))
        .route("/network", get(list_network))
        .route("/services", get(list_services))
        .route("/health/mirror", get(mirror_health))
        .route("/query", get(raw_query))
        .merge(crate::permissions_api::routes())
        .merge(crate::actions_api::routes())
        .merge(crate::events_api::routes())
        .merge(crate::tool_registry_api::routes())
        .merge(crate::diagnosis_api::routes())
        .with_state(state)
}

/// Consistent `{"error": "..."}` envelope for every failure response, so
/// API consumers can rely on one shape instead of some endpoints
/// returning a bare string body and others JSON.
///
/// For 5xx responses the underlying error is logged server-side but not
/// echoed to the client verbatim — a raw `sqlx::Error` can include table
/// or column names, which is more implementation detail than an external
/// caller needs, and not something to leak by default even on an
/// internal tool. 4xx responses (bad input) are safe to return as-is
/// since they describe what the *caller* did wrong.
pub(crate) fn api_error(status: StatusCode, client_message: impl Into<String>) -> Response {
    let client_message = client_message.into();
    (status, Json(json!({ "error": client_message }))).into_response()
}

/// True if `err` is a Postgres unique-violation (SQLSTATE 23505) —
/// checked by code rather than a convenience method so this doesn't
/// depend on a specific sqlx 0.7.x patch version having added one.
/// Shared by every `*_api.rs` module that needs to turn a `UNIQUE`
/// constraint hit into a 409 instead of a generic 500.
pub(crate) fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505"))
}

pub(crate) fn internal_error(context: &str, err: impl std::fmt::Display) -> Response {
    error!(error = %err, context, "internal error");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("internal error while {context}"),
    )
}

/// Plain liveness check for load balancers / orchestrators — deliberately
/// does not touch the database, so it answers even if Postgres is
/// temporarily unreachable. Use `/health/mirror` to check ingest health.
async fn healthz() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn list_processes(
    State(state): State<ApiState>,
    ValidatedQuery(params): ValidatedQuery<ProcessQuery>,
) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let name_filter = params.name_filter.unwrap_or_default();
    let min_memory = params.min_memory_bytes.unwrap_or(0);

    match query_processes(
        &state.pool,
        &name_filter,
        params.sort_by.as_deref(),
        min_memory,
        limit,
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("querying processes", e),
    }
}

/// Pool-level process query, shared by the `/processes` HTTP handler
/// above and `tool_registry::execute_read_tool`'s `query.processes`
/// tool — written once here so the two never drift out of sync with
/// each other.
pub(crate) async fn query_processes(
    pool: &PgPool,
    name_filter: &str,
    sort_by: Option<&str>,
    min_memory_bytes: i64,
    limit: i64,
) -> Result<Vec<Process>, sqlx::Error> {
    // Whitelist, not string interpolation of user input into SQL — the
    // ORDER BY clause below is a fixed literal chosen from this match,
    // never built from `sort_by` directly.
    let order_by = match sort_by {
        Some("cpu") => "cpu_percent DESC NULLS LAST",
        _ => "memory_bytes DESC NULLS LAST",
    };

    let sql = format!(
        r#"
        SELECT id, pid, name, owner, status, cpu_percent, memory_bytes, started_at, last_seen_at
        FROM processes
        WHERE ($1 = '' OR name ILIKE '%' || $1 || '%')
          AND memory_bytes >= $2
          AND last_seen_at > now() - interval '30 seconds'
        ORDER BY {order_by}
        LIMIT $3
        "#
    );

    sqlx::query_as::<_, Process>(&sql)
        .bind(name_filter)
        .bind(min_memory_bytes)
        .bind(limit)
        .fetch_all(pool)
        .await
}

async fn list_network(State(state): State<ApiState>) -> impl IntoResponse {
    match query_network(&state.pool, 200).await {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("querying network connections", e),
    }
}

pub(crate) async fn query_network(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<NetworkConnection>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT id, process_pid, local_port, remote_addr, remote_port, protocol, state, last_seen_at
        FROM network_connections
        WHERE last_seen_at > now() - interval '30 seconds'
        ORDER BY last_seen_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

async fn list_services(State(state): State<ApiState>) -> impl IntoResponse {
    match query_services(&state.pool).await {
        Ok(data) => Json(data).into_response(),
        Err(e) => internal_error("querying services", e),
    }
}

pub(crate) async fn query_services(pool: &PgPool) -> Result<Vec<Service>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT id, name, status, last_seen_at
        FROM services
        ORDER BY name
        "#,
    )
    .fetch_all(pool)
    .await
}

/// Phase 1 §6 benchmarking support — last poll time, duration, row counts
/// per source. Used by scripts/benchmark.sh to check the staleness bound.
async fn mirror_health(State(state): State<ApiState>) -> impl IntoResponse {
    let pool = state.pool;
    let rows = sqlx::query_as::<
        _,
        (
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<i32>,
            Option<String>,
        ),
    >(
        r#"
        SELECT DISTINCT ON (source) source, started_at, finished_at, row_count, error
        FROM ingest_polls
        ORDER BY source, started_at DESC
        "#,
    )
    .fetch_all(&pool)
    .await;

    match rows {
        Ok(data) => {
            let health: Vec<MirrorHealth> = data
                .into_iter()
                .map(|(source, started, finished, count, error)| MirrorHealth {
                    source,
                    last_poll_started: started,
                    last_poll_finished: finished,
                    last_row_count: count,
                    last_error: error,
                })
                .collect();
            Json(health).into_response()
        }
        Err(e) => internal_error("querying mirror health", e),
    }
}

#[derive(Deserialize)]
struct RawQueryParams {
    sql: String,
}

/// Read-only raw SQL endpoint per architecture doc §5.
///
/// Two independent layers enforce read-only-ness here, deliberately not
/// relying on either one alone:
///
/// 1. `validate_single_select` (code level) parses the string — tracking
///    quotes and comments — to reject anything that isn't a single SELECT
///    (or WITH...SELECT) statement, before it's sent to Postgres at all.
/// 2. `state.readonly_pool` (database level) is expected to run as the
///    `agentdb_reader` role (SELECT-only grants — see
///    migrations/0002_readonly_role.sql and db::connect_readonly), so even
///    a validator bypass we haven't thought of still can't write, because
///    Postgres itself rejects the attempt.
///
/// Errors from Postgres for this endpoint specifically *are* returned to
/// the caller close to verbatim (as a 400, not 500) — unlike the other
/// endpoints, the caller wrote this SQL themselves, so a syntax or
/// permission error is diagnostic information about their own input, not
/// an internal detail to hide.
async fn raw_query(
    State(state): State<ApiState>,
    ValidatedQuery(params): ValidatedQuery<RawQueryParams>,
) -> impl IntoResponse {
    if let Err(msg) = validate_single_select(&params.sql) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }

    let rows = sqlx::query(&params.sql)
        .fetch_all(&state.readonly_pool)
        .await;
    match rows {
        Ok(rows) => {
            let json_rows: Vec<serde_json::Value> =
                rows.iter().map(crate::query_util::row_to_json).collect();
            Json(json_rows).into_response()
        }
        Err(e) => api_error(StatusCode::BAD_REQUEST, e.to_string()),
    }
}
