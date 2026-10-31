mod common;

use agentdb_runtime::actions::{approve_action, propose_action, HandlerRegistry};
use agentdb_runtime::diagnosis::{
    diagnose, gather_context, DiagnosisConfig, DiagnosisError, DiagnosisMode,
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

async fn insert_process(pool: &PgPool, pid: i32, name: &str, cpu_percent: Option<f64>) {
    sqlx::query(
        "INSERT INTO processes (pid, name, status, cpu_percent, memory_bytes, last_seen_at) \
         VALUES ($1, $2, 'running', $3, 1024, now()) \
         ON CONFLICT (pid) DO UPDATE SET name = EXCLUDED.name, cpu_percent = EXCLUDED.cpu_percent, last_seen_at = now()",
    )
    .bind(pid)
    .bind(name)
    .bind(cpu_percent)
    .execute(pool)
    .await
    .unwrap();
}

fn rule_based_only_config() -> DiagnosisConfig {
    DiagnosisConfig {
        default_mode: DiagnosisMode::RuleBased,
        llm_client: None,
    }
}

// --- Context gathering against real data ---------------------------------

#[tokio::test]
async fn gather_context_returns_real_seeded_processes_sorted_by_cpu() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    insert_process(&pool, 100, "low-cpu", Some(5.0)).await;
    insert_process(&pool, 200, "high-cpu", Some(95.0)).await;

    let ctx = gather_context(&pool, None, 10).await.unwrap();

    assert!(!ctx.top_processes_by_cpu.is_empty());
    assert_eq!(
        ctx.top_processes_by_cpu[0].pid, 200,
        "highest-CPU process must sort first"
    );
}

#[tokio::test]
async fn gather_context_scopes_recent_events_to_the_given_agent() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_a = create_agent(&pool, "agent-ctx-scope-a").await;
    let agent_b = create_agent(&pool, "agent-ctx-scope-b").await;

    sqlx::query(
        "INSERT INTO grants (agent_id, capability_id, scope, granted_by) \
         SELECT $1, id, 'pid:1', 'test' FROM capabilities WHERE name = 'process.stop'",
    )
    .bind(agent_a)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO grants (agent_id, capability_id, scope, granted_by) \
         SELECT $1, id, 'pid:2', 'test' FROM capabilities WHERE name = 'process.stop'",
    )
    .bind(agent_b)
    .execute(&pool)
    .await
    .unwrap();

    let registry = HandlerRegistry::default_handlers();
    propose_action(
        &pool,
        &registry,
        agent_a,
        "process.stop",
        serde_json::json!({"pid": 1}),
        None,
    )
    .await
    .unwrap();
    propose_action(
        &pool,
        &registry,
        agent_b,
        "process.stop",
        serde_json::json!({"pid": 2}),
        None,
    )
    .await
    .unwrap();

    let ctx_a = gather_context(&pool, Some(agent_a), 10).await.unwrap();
    assert!(
        ctx_a.recent_events.iter().all(|e| e.agent_id == agent_a),
        "agent-scoped context must not include another agent's events"
    );
    assert!(
        !ctx_a.recent_events.is_empty(),
        "agent_a should have at least its own ActionProposed event"
    );
}

// --- End-to-end diagnosis via the top-level entrypoint -------------------

#[tokio::test]
async fn diagnose_end_to_end_rule_based_identifies_a_high_cpu_process() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    insert_process(&pool, 4242, "runaway-worker", Some(94.0)).await;

    let config = rule_based_only_config();
    let result = diagnose(&pool, &config, None, None, 10).await.unwrap();

    assert_eq!(result.mode, DiagnosisMode::RuleBased);
    assert!(result.summary.contains("runaway-worker"));
    assert!(result.suggested_action.is_some());
}

#[tokio::test]
async fn diagnose_end_to_end_reports_no_clear_cause_on_a_quiet_system() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    insert_process(&pool, 1, "idle-process", Some(1.0)).await;

    let config = rule_based_only_config();
    let result = diagnose(&pool, &config, None, None, 10).await.unwrap();

    assert!(result.suggested_action.is_none());
}

#[tokio::test]
async fn diagnose_full_loop_proposal_through_a_suggested_action_is_approvable() {
    // Proves the "fix it" composition claim directly: take a diagnosis's
    // suggested_action and actually propose + approve it through the
    // real Phase 3 engine, with no special-cased path for
    // diagnosis-originated actions.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-full-loop").await;
    sqlx::query(
        "INSERT INTO grants (agent_id, capability_id, scope, granted_by) \
         SELECT $1, id, 'pid:4242', 'test' FROM capabilities WHERE name = 'process.stop'",
    )
    .bind(agent_id)
    .execute(&pool)
    .await
    .unwrap();
    insert_process(&pool, 4242, "runaway-worker", Some(94.0)).await;

    let config = rule_based_only_config();
    let diagnosis = diagnose(&pool, &config, Some(agent_id), None, 10)
        .await
        .unwrap();
    let suggested = diagnosis
        .suggested_action
        .expect("expected a suggested action");
    assert_eq!(suggested.action_type, "process.stop");
    assert_eq!(suggested.parameters["pid"], 4242);

    let registry = HandlerRegistry::default_handlers();
    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        &suggested.action_type,
        suggested.parameters,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        action.status, "proposed",
        "the suggested fix must be proposable through the real engine, unmodified"
    );

    let approved = approve_action(&pool, action.id, "test-suite")
        .await
        .unwrap();
    assert_eq!(approved.status, "approved");
}

#[tokio::test]
async fn diagnose_rejects_llm_mode_when_not_configured() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let config = rule_based_only_config(); // llm_client: None

    let result = diagnose(&pool, &config, None, Some(DiagnosisMode::Llm), 10).await;
    assert!(matches!(result, Err(DiagnosisError::LlmNotConfigured)));
}

// --- HTTP surface -----------------------------------------------------------

mod http {
    use super::*;
    use agentdb_runtime::{actions_api, api, auth, diagnosis, rate_limit};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::{middleware, Extension};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn app_rule_based_only(pool: PgPool) -> axum::Router {
        api::router(pool.clone(), pool)
            .layer(middleware::from_fn(auth::require_api_key))
            .layer(Extension(auth::ApiKeyConfig::from_env()))
            .layer(middleware::from_fn(rate_limit::rate_limit))
            .layer(Extension(rate_limit::RateLimiter::from_env()))
            .layer(Extension(actions_api::ActionsConfig::from_env()))
            .layer(Extension(diagnosis::DiagnosisConfig {
                default_mode: diagnosis::DiagnosisMode::RuleBased,
                llm_client: None,
            }))
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn diagnose_endpoint_defaults_to_rule_based_and_returns_a_diagnosis() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        insert_process(&pool, 5555, "cpu-hog", Some(90.0)).await;
        let app = app_rule_based_only(pool);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/diagnose")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["mode"], "rule_based");
        assert!(json["summary"].as_str().unwrap().contains("cpu-hog"));
    }

    #[tokio::test]
    async fn diagnose_endpoint_rejects_an_unconfigured_llm_mode_request() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = app_rule_based_only(pool);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/diagnose")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"mode": "llm"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn diagnose_endpoint_rejects_an_invalid_mode_string() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = app_rule_based_only(pool);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/diagnose")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"mode": "gpt4"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn diagnose_endpoint_clamps_lookback_minutes_to_a_sane_range() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = app_rule_based_only(pool);

        // Wildly out-of-range values must not error — they clamp, same
        // pattern as every other limit/paging parameter in this codebase.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/diagnose")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"lookback_minutes": 999999}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}

// --- LLM-gated test, matching the osquery-gated pattern in ingest_tests.rs

#[tokio::test]
async fn llm_diagnosis_runs_against_the_real_api_when_configured() {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let model = std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty());

    let (api_key, model) = match (api_key, model) {
        (Some(k), Some(m)) => (k, m),
        _ => {
            eprintln!("skipping: ANTHROPIC_API_KEY and/or ANTHROPIC_MODEL not set");
            return;
        }
    };

    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    insert_process(&pool, 7777, "test-llm-process", Some(90.0)).await;

    let config = DiagnosisConfig {
        default_mode: DiagnosisMode::Llm,
        llm_client: Some(std::sync::Arc::new(
            agentdb_runtime::diagnosis::AnthropicClient::new(api_key, model),
        )),
    };

    let result = diagnose(&pool, &config, None, Some(DiagnosisMode::Llm), 10)
        .await
        .expect("real LLM diagnosis call failed — this is exactly the untested path flagged in diagnosis.rs");

    assert_eq!(result.mode, DiagnosisMode::Llm);
    assert!(!result.summary.trim().is_empty());
}

#[tokio::test]
async fn otari_diagnosis_runs_against_a_real_gateway_when_configured() {
    // Mirrors llm_diagnosis_runs_against_the_real_api_when_configured
    // exactly, for the Otari provider. The otari SDK itself reads
    // OTARI_API_KEY/OTARI_API_BASE — this test only needs to also check
    // for OTARI_MODEL (this project's own env var) before deciding
    // whether to run for real.
    let api_key = std::env::var("OTARI_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let api_base = std::env::var("OTARI_API_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let model = std::env::var("OTARI_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty());

    let model = match (api_key, api_base, model) {
        (Some(_), Some(_), Some(m)) => m,
        _ => {
            eprintln!("skipping: OTARI_API_KEY, OTARI_API_BASE, and/or OTARI_MODEL not set");
            return;
        }
    };

    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    insert_process(&pool, 8888, "test-otari-process", Some(88.0)).await;

    let config = DiagnosisConfig {
        default_mode: DiagnosisMode::Llm,
        llm_client: Some(std::sync::Arc::new(
            agentdb_runtime::diagnosis::OtariClient::new(model),
        )),
    };

    let result = diagnose(&pool, &config, None, Some(DiagnosisMode::Llm), 10)
        .await
        .expect("real Otari gateway call failed — this is exactly the untested path flagged in diagnosis.rs");

    assert_eq!(result.mode, DiagnosisMode::Llm);
    assert!(!result.summary.trim().is_empty());
}
