mod common;

use agentdb_runtime::actions::{
    approve_action, execute_action, propose_action, ApproveError, DryRunConfig, HandlerRegistry,
};
use sqlx::PgPool;
use uuid::Uuid;

async fn create_agent(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO agents (name) VALUES ($1) RETURNING id")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn capability_id(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM capabilities WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("seeded capability not found — did migrations 0005-0008 run?")
}

async fn grant(pool: &PgPool, agent_id: Uuid, capability_name: &str, scope: &str) -> Uuid {
    let cap_id = capability_id(pool, capability_name).await;
    sqlx::query_scalar(
        "INSERT INTO grants (agent_id, capability_id, scope, granted_by) \
         VALUES ($1, $2, $3, 'test-suite') RETURNING id",
    )
    .bind(agent_id)
    .bind(cap_id)
    .bind(scope)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn dry_run_forced() -> DryRunConfig {
    DryRunConfig {
        server_forced: true,
    }
}

fn dry_run_allows_real() -> DryRunConfig {
    DryRunConfig {
        server_forced: false,
    }
}

// --- Propose: permission gating ----------------------------------------

#[tokio::test]
async fn propose_without_a_grant_fails_and_is_never_executable() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-no-grant-action").await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1234}),
        None,
    )
    .await
    .unwrap();

    assert_eq!(action.status, "failed");
    assert!(action
        .error
        .as_deref()
        .unwrap()
        .contains("permission denied"));
    assert!(action.grant_id.is_none());

    // And it must not be approvable — approve_action's WHERE status =
    // 'proposed' should reject this.
    let approve_result = approve_action(&pool, action.id, "someone").await;
    assert!(matches!(approve_result, Err(ApproveError::WrongStatus)));
}

#[tokio::test]
async fn propose_with_a_matching_grant_succeeds_and_records_the_grant_id() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-with-grant-action").await;
    let grant_id = grant(&pool, agent_id, "process.stop", "pid:1234").await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1234}),
        None,
    )
    .await
    .unwrap();

    assert_eq!(action.status, "proposed");
    assert_eq!(action.grant_id, Some(grant_id));
}

#[tokio::test]
async fn propose_with_wrong_scope_fails_even_with_a_grant_for_the_capability() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-wrong-scope-action").await;
    grant(&pool, agent_id, "process.stop", "pid:9999").await; // different pid

    let registry = HandlerRegistry::default_handlers();
    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1234}),
        None,
    )
    .await
    .unwrap();

    assert_eq!(action.status, "failed");
    assert!(action.error.as_deref().unwrap().contains("scope_mismatch"));
}

#[tokio::test]
async fn propose_rejects_invalid_parameters_before_touching_permissions() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-bad-params").await;
    let registry = HandlerRegistry::default_handlers();

    // No "pid" field at all — scope_of should fail before any permission
    // check or DB row is written.
    let result = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({}),
        None,
    )
    .await;

    assert!(result.is_err());

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM actions WHERE agent_id = $1")
        .bind(agent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "an action with invalid parameters should never be recorded"
    );
}

// --- Approve: race safety -----------------------------------------------

#[tokio::test]
async fn approving_an_already_approved_action_is_rejected() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-double-approve").await;
    grant(&pool, agent_id, "process.stop", "pid:1234").await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1234}),
        None,
    )
    .await
    .unwrap();

    approve_action(&pool, action.id, "first-approver")
        .await
        .unwrap();
    let second = approve_action(&pool, action.id, "second-approver").await;

    assert!(matches!(second, Err(ApproveError::WrongStatus)));
}

#[tokio::test]
async fn concurrent_approvals_only_let_one_succeed() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-concurrent-approve").await;
    grant(&pool, agent_id, "process.stop", "pid:1234").await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1234}),
        None,
    )
    .await
    .unwrap();

    let pool_a = pool.clone();
    let pool_b = pool.clone();
    let action_id = action.id;

    let task_a =
        tokio::spawn(async move { approve_action(&pool_a, action_id, "requester-a").await });
    let task_b =
        tokio::spawn(async move { approve_action(&pool_b, action_id, "requester-b").await });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let results = [result_a.unwrap(), result_b.unwrap()];

    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let wrong_status_count = results
        .iter()
        .filter(|r| matches!(r, Err(ApproveError::WrongStatus)))
        .count();

    assert_eq!(
        (ok_count, wrong_status_count),
        (1, 1),
        "expected exactly one success and one WrongStatus, got {results:?}"
    );
}

// --- Execute: the dry-run ceiling is the whole point of this phase -----

#[tokio::test]
async fn execute_under_a_forced_dry_run_server_never_takes_the_real_path() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-dry-run-forced").await;
    grant(&pool, agent_id, "process.stop", "pid:999999999").await;
    let registry = HandlerRegistry::default_handlers();

    // Requests real execution (dry_run: Some(false)) — but the server
    // ceiling must win regardless.
    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 999999999}),
        Some(false),
    )
    .await
    .unwrap();

    approve_action(&pool, action.id, "approver").await.unwrap();

    let executed = execute_action(&pool, &registry, &dry_run_forced(), action.id)
        .await
        .unwrap();

    assert_eq!(executed.status, "executed");
    assert_eq!(executed.requested_dry_run, Some(false));
    assert_eq!(
        executed.effective_dry_run,
        Some(true),
        "server ceiling must force dry-run even though the request asked for real execution"
    );
    assert_eq!(executed.outcome.as_ref().unwrap()["dry_run"], true);

    // If this had taken the real path against a pid that doesn't exist,
    // the handler would have returned a SystemCallFailed error and the
    // action would be 'failed', not 'executed'. Landing in 'executed'
    // with dry_run:true in the outcome is itself evidence the real path
    // was never taken.
}

#[tokio::test]
async fn execute_with_server_allowing_real_and_no_request_preference_defaults_to_real() {
    // Uses cgroup.set_limit's real path against a path that doesn't
    // exist on this machine, so the real syscall is expected to fail —
    // which is exactly what proves it was actually attempted rather than
    // simulated. A dry-run would never touch the filesystem and so could
    // never fail this way.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-real-default").await;
    let fake_cgroup = "/this/path/definitely/does/not/exist/on/this/machine";
    grant(&pool, agent_id, "cgroup.set_limit", fake_cgroup).await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "cgroup.set_limit",
        serde_json::json!({"cgroup_path": fake_cgroup, "memory_limit_bytes": 1024}),
        None, // no preference stated
    )
    .await
    .unwrap();

    approve_action(&pool, action.id, "approver").await.unwrap();

    let executed = execute_action(&pool, &registry, &dry_run_allows_real(), action.id)
        .await
        .unwrap();

    assert_eq!(executed.effective_dry_run, Some(false));
    assert_eq!(
        executed.status, "failed",
        "expected a real (non-dry-run) attempt to fail against a nonexistent cgroup path, \
         proving the real path was actually taken rather than simulated"
    );
    assert!(executed
        .error
        .as_deref()
        .unwrap()
        .contains("failed to read"));
}

#[tokio::test]
async fn execute_respects_a_request_asking_for_more_caution_than_the_server_requires() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-extra-caution").await;
    grant(&pool, agent_id, "service.restart", "nginx").await;
    let registry = HandlerRegistry::default_handlers();

    // Server allows real execution, but this specific request opts into
    // dry_run anyway.
    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "service.restart",
        serde_json::json!({"service_name": "nginx"}),
        Some(true),
    )
    .await
    .unwrap();

    approve_action(&pool, action.id, "approver").await.unwrap();

    let executed = execute_action(&pool, &registry, &dry_run_allows_real(), action.id)
        .await
        .unwrap();

    assert_eq!(executed.status, "executed");
    assert_eq!(executed.effective_dry_run, Some(true));
    assert_eq!(executed.outcome.as_ref().unwrap()["dry_run"], true);
    // Never actually invoked systemctl — if it had, this test would be
    // trying to restart a real nginx service on the test runner.
}

#[tokio::test]
async fn execute_fails_cleanly_for_an_action_not_yet_approved() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-not-approved").await;
    grant(&pool, agent_id, "process.stop", "pid:1234").await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1234}),
        None,
    )
    .await
    .unwrap();

    // Never approved.
    let result = execute_action(&pool, &registry, &dry_run_forced(), action.id).await;
    assert!(matches!(
        result,
        Err(agentdb_runtime::actions::ExecuteError::NotApproved(status)) if status == "proposed"
    ));
}

// --- HTTP surface ---------------------------------------------------------

mod http {
    use super::*;
    use agentdb_runtime::{actions_api, api, auth, rate_limit};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::{middleware, Extension};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    const TEST_API_KEY: &str = "test-api-key";

    fn test_api_key_config() -> auth::ApiKeyConfig {
        auth::ApiKeyConfig(std::sync::Arc::new(Some(TEST_API_KEY.to_string())))
    }

    fn auth_request(method: &str, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("x-api-key", TEST_API_KEY)
            .body(body)
            .unwrap()
    }

    fn empty_auth_request(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("x-api-key", TEST_API_KEY)
            .body(Body::empty())
            .unwrap()
    }

    /// Builds the full app (router + the Extension layers actions_api's
    /// handlers depend on) with a forced-dry-run ActionsConfig, since
    /// these tests exercise real process.stop/service.restart action
    /// types and must never risk taking the real path against whatever
    /// host runs the test suite.
    fn app_with_dry_run_forced(pool: PgPool) -> axum::Router {
        api::router(pool.clone(), pool)
            .layer(middleware::from_fn(auth::require_api_key))
            .layer(Extension(test_api_key_config()))
            .layer(middleware::from_fn(rate_limit::rate_limit))
            .layer(Extension(rate_limit::RateLimiter::from_env()))
            .layer(Extension(actions_api::ActionsConfig(std::sync::Arc::new(
                actions_api::ActionsConfigInner {
                    registry: agentdb_runtime::actions::HandlerRegistry::default_handlers(),
                    dry_run: agentdb_runtime::actions::DryRunConfig {
                        server_forced: true,
                    },
                },
            ))))
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn action_types_endpoint_lists_all_four_with_correct_metadata() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = app_with_dry_run_forced(pool);

        let response = app
            .oneshot(empty_auth_request("GET", "/action-types"))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let types = json.as_array().unwrap();
        assert_eq!(types.len(), 4);

        let process_stop = types
            .iter()
            .find(|t| t["action_type"] == "process.stop")
            .unwrap();
        assert_eq!(process_stop["required_capability"], "process.stop");
        assert_eq!(process_stop["is_reversible"], false);

        let process_start = types
            .iter()
            .find(|t| t["action_type"] == "process.start")
            .unwrap();
        assert_eq!(process_start["is_reversible"], true);
    }

    #[tokio::test]
    async fn full_propose_approve_execute_flow_over_http() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-http-flow").await;
        grant(&pool, agent_id, "service.restart", "nginx").await;

        let app1 = app_with_dry_run_forced(pool.clone());
        let propose_response = app1
            .oneshot(auth_request(
                "POST",
                "/actions",
                Body::from(
                    json!({
                        "agent_id": agent_id,
                        "action_type": "service.restart",
                        "parameters": {"service_name": "nginx"}
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(propose_response.status(), StatusCode::CREATED);
        let proposed = body_json(propose_response).await;
        assert_eq!(proposed["status"], "proposed");
        let action_id = proposed["id"].as_str().unwrap();

        let app2 = app_with_dry_run_forced(pool.clone());
        let approve_response = app2
            .oneshot(auth_request(
                "POST",
                &format!("/actions/{action_id}/approve"),
                Body::from(json!({"approved_by": "ops-oncall"}).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(approve_response.status(), StatusCode::OK);
        let executed = body_json(approve_response).await;
        assert_eq!(executed["status"], "executed");
        assert_eq!(executed["effective_dry_run"], true);
        assert_eq!(executed["outcome"]["dry_run"], true);
        assert_eq!(executed["outcome"]["would_restart"], "nginx");
    }

    #[tokio::test]
    async fn propose_with_unknown_action_type_returns_400() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-bad-type-http").await;
        let app = app_with_dry_run_forced(pool);

        let response = app
            .oneshot(auth_request(
                "POST",
                "/actions",
                Body::from(
                    json!({
                        "agent_id": agent_id,
                        "action_type": "reboot.the.datacenter",
                        "parameters": {}
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn approve_on_unknown_action_returns_404() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = app_with_dry_run_forced(pool);

        let response = app
            .oneshot(auth_request(
                "POST",
                &format!("/actions/{}/approve", Uuid::new_v4()),
                Body::from(json!({"approved_by": "someone"}).to_string()),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_actions_filters_by_status_and_rejects_invalid_status() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-list-actions").await;
        grant(&pool, agent_id, "process.stop", "pid:1").await;
        let registry = agentdb_runtime::actions::HandlerRegistry::default_handlers();

        propose_action(
            &pool,
            &registry,
            agent_id,
            "process.stop",
            json!({"pid": 1}),
            None,
        )
        .await
        .unwrap();
        propose_action(
            &pool,
            &registry,
            agent_id,
            "process.stop",
            json!({"pid": 999}),
            None,
        )
        .await
        .unwrap();

        let app = app_with_dry_run_forced(pool.clone());
        let response = app
            .oneshot(empty_auth_request(
                "GET",
                &format!("/actions?agent_id={agent_id}&status=proposed"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json.as_array().unwrap().len(), 1);

        let app2 = app_with_dry_run_forced(pool);
        let bad_status_response = app2
            .oneshot(empty_auth_request(
                "GET",
                &format!("/actions?agent_id={agent_id}&status=not-a-real-status"),
            ))
            .await
            .unwrap();
        assert_eq!(bad_status_response.status(), StatusCode::BAD_REQUEST);
    }
}
