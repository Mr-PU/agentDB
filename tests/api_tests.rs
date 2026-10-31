mod common;

use agentdb_runtime::api;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // for `.oneshot(...)`

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("failed to read response body");
    serde_json::from_slice(&bytes).expect("response body was not valid JSON")
}

#[tokio::test]
async fn healthz_returns_ok_without_requiring_data() {
    let pool = common::test_pool().await;
    let app = api::router(pool.clone(), pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn processes_returns_empty_array_on_a_fresh_database() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let app = api::router(pool.clone(), pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/processes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await, serde_json::json!([]));
}

#[tokio::test]
async fn processes_cpu_sort_is_accepted_and_does_not_error() {
    // Regression check for the sort_by=cpu whitelist added alongside the
    // cpu_percent fix — a bad implementation here would either 500 or
    // silently ignore the parameter.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let app = api::router(pool.clone(), pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/processes?sort_by=cpu&limit=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn query_endpoint_rejects_a_stacked_statement() {
    // Regression check for the exact vulnerability described earlier:
    // a naive prefix check would let this through to Postgres.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let app = api::router(pool.clone(), pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/query?sql=SELECT%201%3B%20DROP%20TABLE%20processes%3B")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // And confirm the table really is still there and usable afterward —
    // the strongest possible version of this regression check.
    let app2 = api::router(pool.clone(), pool.clone());
    let followup = app2
        .oneshot(
            Request::builder()
                .uri("/processes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(followup.status(), StatusCode::OK);
}

#[tokio::test]
async fn query_endpoint_allows_a_plain_select() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let app = api::router(pool.clone(), pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/query?sql=SELECT%201%20AS%20one")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json[0]["one"], 1);
}

#[tokio::test]
async fn query_endpoint_allows_a_semicolon_inside_a_string_literal() {
    // The false-positive case that a naive "reject any semicolon" check
    // would get wrong.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let app = api::router(pool.clone(), pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/query?sql=SELECT%20%27foo%3Bbar%27%20AS%20val")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn mirror_health_reflects_a_recorded_poll() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;

    sqlx::query(
        "INSERT INTO ingest_polls (source, started_at, finished_at, row_count) \
         VALUES ('processes', now(), now(), 3)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = api::router(pool.clone(), pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health/mirror")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json[0]["source"], "processes");
    assert_eq!(json[0]["last_row_count"], 3);
}
