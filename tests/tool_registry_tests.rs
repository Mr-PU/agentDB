mod common;

use agentdb_runtime::actions::HandlerRegistry;
use agentdb_runtime::tool_registry::{
    call_tool, list_tools_for_agent, CallToolError, CallToolResult,
};
use chrono::Utc;
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
        .expect("seeded capability not found — did migrations 0005-0012 run?")
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

// --- List-time filtering --------------------------------------------------

#[tokio::test]
async fn agent_with_no_grants_sees_no_tools() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-no-grants-tools").await;

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    assert!(tools.is_empty());
}

#[tokio::test]
async fn one_grant_makes_every_tool_sharing_that_capability_visible() {
    // filesystem.read gates all three read-only tools (query.processes,
    // query.network, query.services) — one grant should surface all
    // three, proving list-time filtering is capability-level, not
    // tool-level.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-shared-capability").await;
    grant(&pool, agent_id, "filesystem.read", "anything").await;

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    assert!(names.contains(&"query.processes"));
    assert!(names.contains(&"query.network"));
    assert!(names.contains(&"query.services"));
}

#[tokio::test]
async fn a_narrowly_scoped_grant_still_makes_the_tool_visible() {
    // The whole point of list-time filtering being coarse: a grant
    // scoped to one specific pid still surfaces process.stop in the
    // list, even though a call for a *different* pid would be denied.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-narrow-scope-visible").await;
    grant(&pool, agent_id, "process.stop", "pid:1234").await;

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    assert!(tools.iter().any(|t| t.name == "process.stop"));
}

#[tokio::test]
async fn grant_for_a_different_capability_does_not_surface_the_tool() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-wrong-capability-tools").await;
    grant(&pool, agent_id, "process.stop", "pid:1234").await;

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    assert!(!tools.iter().any(|t| t.name == "service.restart"));
}

#[tokio::test]
async fn suspended_agent_sees_no_tools_regardless_of_grants() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-suspended-tools").await;
    grant(&pool, agent_id, "process.stop", "pid:1234").await;
    sqlx::query("UPDATE agents SET status = 'suspended' WHERE id = $1")
        .bind(agent_id)
        .execute(&pool)
        .await
        .unwrap();

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    assert!(tools.is_empty());
}

#[tokio::test]
async fn expired_grant_does_not_surface_the_tool() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-expired-tools").await;
    let cap_id = capability_id(&pool, "process.stop").await;
    let past = Utc::now() - chrono::Duration::seconds(1);
    sqlx::query(
        "INSERT INTO grants (agent_id, capability_id, scope, granted_by, granted_at, expires_at) \
         VALUES ($1, $2, 'pid:1234', 'test-suite', $3, $3)",
    )
    .bind(agent_id)
    .bind(cap_id)
    .bind(past)
    .execute(&pool)
    .await
    .unwrap();

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    assert!(!tools.iter().any(|t| t.name == "process.stop"));
}

// --- Call-time enforcement: the part that actually matters --------------

#[tokio::test]
async fn visible_write_tool_still_denies_a_call_with_a_different_scope() {
    // The core property this phase exists to prove: list-time visibility
    // never implies call-time success.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-visible-but-denied").await;
    grant(&pool, agent_id, "process.stop", "pid:1234").await;
    let registry = HandlerRegistry::default_handlers();

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    assert!(
        tools.iter().any(|t| t.name == "process.stop"),
        "precondition: process.stop must be visible for this test to prove anything"
    );

    let result = call_tool(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 9999}), // different pid — not covered by the grant
    )
    .await
    .unwrap();

    match result {
        CallToolResult::Action { action } => {
            assert_eq!(action.status, "failed");
            assert!(action.error.as_deref().unwrap().contains("scope_mismatch"));
        }
        other => panic!("expected a denied action record, got {other:?}"),
    }
}

#[tokio::test]
async fn revocation_between_list_and_call_denies_the_call() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-revoked-between").await;
    let grant_id = grant(&pool, agent_id, "process.stop", "pid:1234").await;
    let registry = HandlerRegistry::default_handlers();

    let tools = list_tools_for_agent(&pool, agent_id).await.unwrap();
    assert!(tools.iter().any(|t| t.name == "process.stop"));

    sqlx::query("UPDATE grants SET revoked_at = now(), revoked_by = 'test-suite' WHERE id = $1")
        .bind(grant_id)
        .execute(&pool)
        .await
        .unwrap();

    let result = call_tool(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1234}),
    )
    .await
    .unwrap();

    let action_id = match result {
        CallToolResult::Action { action } => {
            assert_eq!(action.status, "failed");
            action.id
        }
        other => panic!("expected a denied action record, got {other:?}"),
    };

    // The denial must be logged as an event too — reusing Phase 4's
    // event trail for free, no new plumbing needed for this.
    let events = agentdb_runtime::events::action_causal_chain(&pool, action_id)
        .await
        .unwrap();
    assert!(!events.is_empty());
}

#[tokio::test]
async fn unknown_tool_name_is_rejected() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-unknown-tool").await;
    let registry = HandlerRegistry::default_handlers();

    let result = call_tool(
        &pool,
        &registry,
        agent_id,
        "totally.made.up",
        serde_json::json!({}),
    )
    .await;
    assert!(matches!(result, Err(CallToolError::UnknownTool(_))));
}

// --- Write tools produce exactly what calling /actions directly would ---

#[tokio::test]
async fn calling_a_write_tool_produces_exactly_one_action_and_its_events() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-write-tool-artifacts").await;
    grant(&pool, agent_id, "service.restart", "nginx").await;
    let registry = HandlerRegistry::default_handlers();

    let result = call_tool(
        &pool,
        &registry,
        agent_id,
        "service.restart",
        serde_json::json!({"service_name": "nginx"}),
    )
    .await
    .unwrap();

    let action = match result {
        CallToolResult::Action { action } => action,
        other => panic!("expected an action record, got {other:?}"),
    };
    assert_eq!(action.status, "proposed");

    let action_count: i64 = sqlx::query_scalar("SELECT count(*) FROM actions WHERE agent_id = $1")
        .bind(agent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        action_count, 1,
        "exactly one actions row — the registry must not create a second record"
    );

    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE action_id = $1")
        .bind(action.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        event_count >= 1,
        "the action must have at least its ActionProposed event"
    );
}

// --- Read tools ------------------------------------------------------------

#[tokio::test]
async fn read_tool_denies_without_a_grant() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-read-tool-denied").await;
    let registry = HandlerRegistry::default_handlers();

    let result = call_tool(
        &pool,
        &registry,
        agent_id,
        "query.processes",
        serde_json::json!({}),
    )
    .await;
    assert!(matches!(result, Err(CallToolError::PermissionDenied(_))));
}

#[tokio::test]
async fn read_tool_returns_real_mirror_data_once_granted() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-read-tool-allowed").await;
    grant(&pool, agent_id, "filesystem.read", "anything").await;
    let registry = HandlerRegistry::default_handlers();

    sqlx::query(
        "INSERT INTO processes (pid, name, status, memory_bytes, last_seen_at) \
         VALUES (99999, 'test-process-xyz', 'running', 1024, now())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result = call_tool(
        &pool,
        &registry,
        agent_id,
        "query.processes",
        serde_json::json!({"name_filter": "test-process-xyz"}),
    )
    .await
    .unwrap();

    match result {
        CallToolResult::Data { data } => {
            let rows = data.as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["pid"], 99999);
        }
        other => panic!("expected data, got {other:?}"),
    }
}

// --- HTTP surface -----------------------------------------------------------

mod http {
    use super::*;
    use agentdb_runtime::api;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn mcp_tools_endpoint_lists_visible_tools() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-http-mcp-list").await;
        grant(&pool, agent_id, "process.stop", "pid:1").await;

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/mcp/tools?agent_id={agent_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let names: Vec<&str> = json
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"process.stop"));
    }

    #[tokio::test]
    async fn mcp_call_endpoint_proposes_a_write_action() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-http-mcp-call").await;
        grant(&pool, agent_id, "service.restart", "nginx").await;

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "agent_id": agent_id,
                            "tool_name": "service.restart",
                            "parameters": {"service_name": "nginx"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response).await;
        assert_eq!(json["status"], "proposed");
    }

    #[tokio::test]
    async fn mcp_call_endpoint_returns_403_for_a_denied_read_tool() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-http-mcp-denied").await;

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "agent_id": agent_id,
                            "tool_name": "query.processes",
                            "parameters": {}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_call_endpoint_returns_404_for_unknown_tool() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-http-mcp-unknown").await;

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "agent_id": agent_id,
                            "tool_name": "not.a.real.tool",
                            "parameters": {}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn admin_tools_list_returns_the_seeded_tools() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let names: Vec<&str> = json
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"process.stop"));
        assert!(names.contains(&"query.processes"));
    }

    #[tokio::test]
    async fn admin_create_tool_rejects_inconsistent_read_only_and_action_type() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tools")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "name": "bad.tool.definition",
                            "description": "inconsistent",
                            "required_capability_name": "process.stop",
                            "backing_action_type": "process.stop",
                            "is_read_only": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn admin_create_tool_conflict_returns_409() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        // "process.stop" already exists via the seed migration.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tools")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "name": "process.stop",
                            "description": "duplicate",
                            "required_capability_name": "process.stop",
                            "backing_action_type": "process.stop",
                            "is_read_only": false
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
