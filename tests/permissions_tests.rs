mod common;

use agentdb_runtime::permissions::{check_permission, DenialReason, PermissionResult};
use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

async fn create_agent(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO agents (name) VALUES ($1) RETURNING id")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn suspend_agent(pool: &PgPool, agent_id: Uuid) {
    sqlx::query("UPDATE agents SET status = 'suspended' WHERE id = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn capability_id(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM capabilities WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("seeded capability not found — did migrations 0005-0007 run?")
}

async fn grant(
    pool: &PgPool,
    agent_id: Uuid,
    capability_name: &str,
    scope: &str,
    expires_at: Option<chrono::DateTime<Utc>>,
) -> Uuid {
    let cap_id = capability_id(pool, capability_name).await;
    sqlx::query_scalar(
        "INSERT INTO grants (agent_id, capability_id, scope, granted_by, expires_at) \
         VALUES ($1, $2, $3, 'test-suite', $4) RETURNING id",
    )
    .bind(agent_id)
    .bind(cap_id)
    .bind(scope)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Inserts a grant directly with a past `expires_at`, bypassing the
/// HTTP API's future-only validation on purpose — the database layer
/// permits this (a grant legitimately becomes expired after creation;
/// the schema only rejects expires_at < granted_at, not "already past").
async fn grant_already_expired(
    pool: &PgPool,
    agent_id: Uuid,
    capability_name: &str,
    scope: &str,
) -> Uuid {
    let cap_id = capability_id(pool, capability_name).await;
    let expired_at = Utc::now() - Duration::seconds(1);
    let granted_at = expired_at - Duration::seconds(10);
    sqlx::query_scalar(
        "INSERT INTO grants (agent_id, capability_id, scope, granted_by, granted_at, expires_at) \
         VALUES ($1, $2, $3, 'test-suite', $4, $5) RETURNING id",
    )
    .bind(agent_id)
    .bind(cap_id)
    .bind(scope)
    .bind(granted_at)
    .bind(expired_at)
    .fetch_one(pool)
    .await
    .unwrap()
}

// --- Exit criterion: no grant -> denied, reason no_grant ---------------

#[tokio::test]
async fn denies_when_no_grant_exists() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-no-grant").await;

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::NoGrant
        }
    );
}

// --- Exit criterion: scope mismatch -> denied, reason scope_mismatch --

#[tokio::test]
async fn denies_on_scope_mismatch() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-scope-mismatch").await;
    grant(&pool, agent_id, "process.stop", "pid:9999", None).await;

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::ScopeMismatch
        }
    );
}

#[tokio::test]
async fn allows_when_scope_matches_exactly() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-allowed").await;
    let grant_id = grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(result, PermissionResult::Allowed { grant_id });
}

#[tokio::test]
async fn allows_when_scope_matches_via_glob() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-glob").await;
    let grant_id = grant(&pool, agent_id, "filesystem.read", "/workspace/**", None).await;

    let result = check_permission(
        &pool,
        agent_id,
        "filesystem.read",
        "/workspace/output/file.txt",
    )
    .await
    .unwrap();

    assert_eq!(result, PermissionResult::Allowed { grant_id });
}

// --- Exit criterion: expired grant -> denied, reason expired, row persists

#[tokio::test]
async fn denies_with_expired_reason_even_though_the_row_still_exists() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-expired").await;
    grant_already_expired(&pool, agent_id, "process.stop", "pid:1234").await;

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::Expired
        }
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM grants WHERE agent_id = $1")
        .bind(agent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "expiry must not delete the grant row");
}

#[tokio::test]
async fn expired_reason_wins_over_scope_mismatch_across_different_grants() {
    // One grant matches scope but is expired; a second is active but
    // covers a different scope. 'expired' should win — it's a more
    // specific, more actionable signal than a generic scope mismatch.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-mixed-grants").await;

    grant_already_expired(&pool, agent_id, "process.stop", "pid:1234").await;
    grant(&pool, agent_id, "process.stop", "pid:9999", None).await;

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::Expired
        }
    );
}

#[tokio::test]
async fn an_active_grant_is_found_even_after_an_unrelated_expired_one() {
    // Order independence: an active matching grant must be found and
    // allowed even if an expired, non-matching grant is checked first
    // internally.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-order-independence").await;

    grant_already_expired(&pool, agent_id, "process.stop", "pid:9999").await;
    let active_grant_id = grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Allowed {
            grant_id: active_grant_id
        }
    );
}

// --- Exit criterion: revoking mid-session blocks the very next check --

#[tokio::test]
async fn revoking_a_grant_blocks_the_very_next_check() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-revoke").await;
    let grant_id = grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

    let before = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();
    assert_eq!(before, PermissionResult::Allowed { grant_id });

    sqlx::query("UPDATE grants SET revoked_at = now(), revoked_by = 'test-suite' WHERE id = $1")
        .bind(grant_id)
        .execute(&pool)
        .await
        .unwrap();

    let after = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();
    assert_eq!(
        after,
        PermissionResult::Denied {
            reason: DenialReason::NoGrant
        },
        "revocation must take effect on the very next check — no caching lag"
    );
}

// --- Exit criterion: suspended agent denied regardless of valid grants

#[tokio::test]
async fn suspended_agent_is_denied_regardless_of_valid_grants() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-suspended").await;
    grant(&pool, agent_id, "process.stop", "pid:1234", None).await;
    suspend_agent(&pool, agent_id).await;

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::AgentSuspended
        }
    );
}

#[tokio::test]
async fn reactivating_a_suspended_agent_restores_access_from_its_existing_grant() {
    // Suspension doesn't destroy the grant — it just blocks the check
    // while suspended. Flipping status back to active should restore
    // access without needing to re-grant anything.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-reactivated").await;
    let grant_id = grant(&pool, agent_id, "process.stop", "pid:1234", None).await;
    suspend_agent(&pool, agent_id).await;

    let while_suspended = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();
    assert_eq!(
        while_suspended,
        PermissionResult::Denied {
            reason: DenialReason::AgentSuspended
        }
    );

    sqlx::query("UPDATE agents SET status = 'active' WHERE id = $1")
        .bind(agent_id)
        .execute(&pool)
        .await
        .unwrap();

    let after_reactivation = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();
    assert_eq!(after_reactivation, PermissionResult::Allowed { grant_id });
}

#[tokio::test]
async fn retired_agent_is_denied_like_suspended() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-retired").await;
    grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

    sqlx::query("UPDATE agents SET status = 'retired' WHERE id = $1")
        .bind(agent_id)
        .execute(&pool)
        .await
        .unwrap();

    let result = check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::AgentSuspended
        },
        "any non-active status is denied with the same reason — the API doesn't \
         distinguish 'suspended' from 'retired' in the denial itself"
    );
}

// --- Edge cases beyond the doc's explicit checklist --------------------

#[tokio::test]
async fn unknown_agent_id_is_denied_and_still_logged() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let fake_agent_id = Uuid::new_v4();

    let result = check_permission(&pool, fake_agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::AgentNotFound
        }
    );

    // This is exactly why permission_checks.agent_id has no foreign key
    // — the attempt must still be loggable for later audit/security review.
    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM permission_checks WHERE agent_id = $1 AND reason = 'agent_not_found'",
    )
    .bind(fake_agent_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logged, 1);
}

#[tokio::test]
async fn unknown_capability_is_denied() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-bad-capability").await;

    let result = check_permission(&pool, agent_id, "totally.made.up", "whatever")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::CapabilityNotFound
        }
    );
}

#[tokio::test]
async fn grant_for_a_different_capability_does_not_leak_access() {
    // An agent with a process.stop grant must not be allowed to
    // service.restart just because some grant exists.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-wrong-capability").await;
    grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

    let result = check_permission(&pool, agent_id, "service.restart", "nginx")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::NoGrant
        }
    );
}

#[tokio::test]
async fn grant_for_a_different_agent_does_not_leak_access() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_a = create_agent(&pool, "agent-a").await;
    let agent_b = create_agent(&pool, "agent-b").await;
    grant(&pool, agent_a, "process.stop", "pid:1234", None).await;

    let result = check_permission(&pool, agent_b, "process.stop", "pid:1234")
        .await
        .unwrap();

    assert_eq!(
        result,
        PermissionResult::Denied {
            reason: DenialReason::NoGrant
        }
    );
}

#[tokio::test]
async fn every_check_writes_exactly_one_permission_checks_row() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-logging").await;
    grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

    check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap(); // allow
    check_permission(&pool, agent_id, "process.stop", "pid:9999")
        .await
        .unwrap(); // deny: scope mismatch

    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM permission_checks WHERE agent_id = $1")
            .bind(agent_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(total, 2);

    let allowed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM permission_checks WHERE agent_id = $1 AND decision = 'allowed'",
    )
    .bind(agent_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(allowed, 1);
}

#[tokio::test]
async fn allowed_rows_carry_a_grant_id_and_no_reason_denied_rows_the_reverse() {
    // Mirrors the CHECK constraint in migrations/0006 at the application
    // level — if this test ever fails without the migration's CHECK also
    // failing, the invariant has drifted between the two layers.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-invariant").await;
    grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

    check_permission(&pool, agent_id, "process.stop", "pid:1234")
        .await
        .unwrap();
    check_permission(&pool, agent_id, "process.stop", "pid:9999")
        .await
        .unwrap();

    let rows: Vec<(String, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT decision, matched_grant_id, reason FROM permission_checks WHERE agent_id = $1",
    )
    .bind(agent_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    for (decision, grant_id, reason) in rows {
        if decision == "allowed" {
            assert!(grant_id.is_some());
            assert!(reason.is_none());
        } else {
            assert!(grant_id.is_none());
            assert!(reason.is_some());
        }
    }
}

// --- HTTP surface --------------------------------------------------------

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
    async fn post_agents_creates_an_active_agent() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "demo-agent"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response).await;
        assert_eq!(json["name"], "demo-agent");
        assert_eq!(json["status"], "active");
    }

    #[tokio::test]
    async fn post_agents_rejects_empty_name() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "   "}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_agent_404s_for_unknown_id() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/agents/{}", Uuid::new_v4()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn grants_api_lists_only_active_grants_by_default() {
        // Exercises the real HTTP surface against a seeded mixed dataset
        // (one active, one expired, one revoked) — this is the doc's
        // exit criterion for GET /grants giving a correct, complete
        // picture.
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-api-grants").await;

        let active_id = grant(&pool, agent_id, "process.stop", "pid:1", None).await;
        grant_already_expired(&pool, agent_id, "process.stop", "pid:2").await;
        let revoked_id = grant(&pool, agent_id, "process.stop", "pid:3", None).await;
        sqlx::query(
            "UPDATE grants SET revoked_at = now(), revoked_by = 'test-suite' WHERE id = $1",
        )
        .bind(revoked_id)
        .execute(&pool)
        .await
        .unwrap();

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/grants?agent_id={agent_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let ids: Vec<String> = json
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["id"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(ids, vec![active_id.to_string()]);
    }

    #[tokio::test]
    async fn grants_api_include_inactive_returns_all_three_states() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-api-grants-all").await;

        grant(&pool, agent_id, "process.stop", "pid:1", None).await;
        grant_already_expired(&pool, agent_id, "process.stop", "pid:2").await;
        let revoked_id = grant(&pool, agent_id, "process.stop", "pid:3", None).await;
        sqlx::query(
            "UPDATE grants SET revoked_at = now(), revoked_by = 'test-suite' WHERE id = $1",
        )
        .bind(revoked_id)
        .execute(&pool)
        .await
        .unwrap();

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/grants?agent_id={agent_id}&include_inactive=true"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json.as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn create_grant_rejects_a_past_expires_at() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-past-expiry").await;
        let app = api::router(pool.clone(), pool.clone());

        let past = (Utc::now() - Duration::seconds(60)).to_rfc3339();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grants")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "agent_id": agent_id,
                            "capability_name": "process.stop",
                            "scope": "pid:1234",
                            "granted_by": "test",
                            "expires_at": past
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
    async fn create_grant_404s_for_unknown_agent() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grants")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "agent_id": Uuid::new_v4(),
                            "capability_name": "process.stop",
                            "scope": "pid:1234",
                            "granted_by": "test"
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
    async fn create_grant_400s_for_unknown_capability() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-bad-cap-http").await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grants")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "agent_id": agent_id,
                            "capability_name": "not.a.real.capability",
                            "scope": "pid:1234",
                            "granted_by": "test"
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
    async fn revoke_grant_is_rejected_the_second_time() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-double-revoke").await;
        let grant_id = grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

        let app1 = api::router(pool.clone(), pool.clone());
        let first = app1
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/grants/{grant_id}/revoke"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"revoked_by": "admin"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let app2 = api::router(pool.clone(), pool.clone());
        let second = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/grants/{grant_id}/revoke"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"revoked_by": "admin"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn revoke_grant_404s_for_unknown_grant() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/grants/{}/revoke", Uuid::new_v4()))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"revoked_by": "admin"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn permission_checks_endpoint_rejects_an_invalid_decision_filter() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-bad-filter").await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/permission-checks?agent_id={agent_id}&decision=maybe"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn debug_check_permission_endpoint_round_trips_an_allow() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-debug-endpoint").await;
        grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/permissions/check")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "agent_id": agent_id,
                            "capability_name": "process.stop",
                            "scope": "pid:1234"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["decision"], "allowed");
        assert!(json["grant_id"].is_string());
    }

    #[tokio::test]
    async fn grants_endpoint_rejects_malformed_agent_id_with_json_error() {
        // Regression check for the ValidatedQuery fix: before it, a
        // malformed UUID in a required query param fell through to
        // Axum's default plain-text rejection instead of this project's
        // {"error": "..."} envelope.
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/grants?agent_id=not-a-valid-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = body_json(response).await;
        assert!(
            json.get("error").is_some(),
            "expected the shared {{\"error\": ...}} envelope, got: {json}"
        );
    }

    #[tokio::test]
    async fn grants_endpoint_rejects_missing_required_agent_id_with_json_error() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/grants")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = body_json(response).await;
        assert!(json.get("error").is_some());
    }

    #[tokio::test]
    async fn create_grant_rejects_malformed_json_body_with_json_error() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grants")
                    .header("content-type", "application/json")
                    .body(Body::from("{ not valid json"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = body_json(response).await;
        assert!(
            json.get("error").is_some(),
            "expected the shared {{\"error\": ...}} envelope, got: {json}"
        );
    }

    #[tokio::test]
    async fn concurrent_revokes_only_let_one_succeed() {
        // Regression check for the TOCTOU race in revoke_grant: fires
        // two revoke requests at the same grant concurrently and asserts
        // exactly one succeeds (200) and the other is rejected (409) —
        // never both succeeding (which would mean the second silently
        // overwrote the first's revoked_by).
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-concurrent-revoke").await;
        let grant_id = grant(&pool, agent_id, "process.stop", "pid:1234", None).await;

        let pool_a = pool.clone();
        let pool_b = pool.clone();

        let task_a = tokio::spawn(async move {
            let app = api::router(pool_a.clone(), pool_a);
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/grants/{grant_id}/revoke"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"revoked_by": "requester-a"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        });

        let task_b = tokio::spawn(async move {
            let app = api::router(pool_b.clone(), pool_b);
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/grants/{grant_id}/revoke"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"revoked_by": "requester-b"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        });

        let (status_a, status_b) = tokio::join!(task_a, task_b);
        let statuses = [status_a.unwrap(), status_b.unwrap()];

        let ok_count = statuses.iter().filter(|s| **s == StatusCode::OK).count();
        let conflict_count = statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count();

        assert_eq!(
            (ok_count, conflict_count),
            (1, 1),
            "expected exactly one 200 and one 409, got {statuses:?}"
        );

        // And confirm the row itself reflects exactly one revocation,
        // not a silently-overwritten one.
        let revoked_by: String = sqlx::query_scalar("SELECT revoked_by FROM grants WHERE id = $1")
            .bind(grant_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            revoked_by == "requester-a" || revoked_by == "requester-b",
            "revoked_by should be whichever request won the race, got: {revoked_by}"
        );
    }

    #[tokio::test]
    async fn capability_name_conflict_returns_409() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app1 = api::router(pool.clone(), pool.clone());

        // "process.stop" already exists via the seed migration.
        let response = app1
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/capabilities")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "process.stop"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
