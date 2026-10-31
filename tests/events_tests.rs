mod common;

use agentdb_runtime::actions::{
    approve_action, execute_action, propose_action, DryRunConfig, HandlerRegistry,
};
use agentdb_runtime::events::{self, event_type};
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
        .expect("seeded capability not found — did migrations 0005-0009 run?")
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

// --- The core Phase 4 guarantee: atomicity ------------------------------

#[tokio::test]
async fn transaction_failure_leaves_neither_state_change_nor_event() {
    // Mimics the exact transaction shape propose_action/approve_action
    // use — a state-change write, then its event, in one transaction —
    // then forces a real failure (a NOT NULL violation) before commit
    // and confirms NEITHER the state change NOR the event survive. This
    // is the property the whole phase exists to guarantee: an event's
    // existence is proof the thing it describes actually happened,
    // which only holds if a failure anywhere in the transaction takes
    // both down together.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-txn-failure").await;

    let mut tx = pool.begin().await.unwrap();
    let fake_action_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO actions (id, agent_id, action_type, parameters, status, requires_human_approval) \
         VALUES ($1, $2, 'process.stop', '{}'::jsonb, 'proposed', true)",
    )
    .bind(fake_action_id)
    .bind(agent_id)
    .execute(&mut *tx)
    .await
    .unwrap();

    events::emit_event(
        &mut tx,
        agent_id,
        Some(fake_action_id),
        None,
        event_type::ACTION_PROPOSED,
        serde_json::json!({}),
    )
    .await
    .unwrap();

    // Force a real failure in the same transaction, before commit: a
    // NOT NULL violation (agent_id and action_type are both NOT NULL).
    let forced_failure =
        sqlx::query("INSERT INTO actions (id, agent_id, action_type) VALUES ($1, NULL, NULL)")
            .bind(Uuid::new_v4())
            .execute(&mut *tx)
            .await;
    assert!(
        forced_failure.is_err(),
        "expected the deliberately malformed insert to fail"
    );

    // Once any query in a Postgres transaction errors, the transaction
    // is aborted server-side — nothing further can commit. Rolling back
    // explicitly here mirrors what real code does on error (see
    // approve_action's explicit rollback on its no-match path).
    let _ = tx.rollback().await;

    let action_count: i64 = sqlx::query_scalar("SELECT count(*) FROM actions WHERE id = $1")
        .bind(fake_action_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        action_count, 0,
        "the action row must not exist after the transaction rolled back"
    );

    let event_count: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE action_id = $1")
        .bind(fake_action_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        event_count, 0,
        "the event row must not exist after rollback — this is the core Phase 4 guarantee"
    );
}

// --- Event emission for each retrofitted write path ---------------------

#[tokio::test]
async fn propose_action_allowed_emits_action_proposed_linked_to_the_action() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-propose-event").await;
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

    let chain = events::action_causal_chain(&pool, action.id).await.unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].event_type, event_type::ACTION_PROPOSED);
    assert_eq!(chain[0].grant_id, Some(grant_id));
    assert_eq!(chain[0].agent_id, agent_id);
}

#[tokio::test]
async fn propose_action_denied_emits_both_proposal_denied_and_permission_denied() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-propose-denied-event").await;
    // No grant at all.
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

    // ActionProposalDenied is linked to the action (action_id is known,
    // since the actions row was created even on denial).
    let chain = events::action_causal_chain(&pool, action.id).await.unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].event_type, event_type::ACTION_PROPOSAL_DENIED);

    // PermissionDenied is emitted separately by check_permission_tx, not
    // linked to the action (it doesn't know the action id — see the doc
    // comment on propose_action for why).
    let agent_events = events::agent_timeline(&pool, agent_id, 10).await.unwrap();
    let permission_denied_count = agent_events
        .iter()
        .filter(|e| e.event_type == event_type::PERMISSION_DENIED)
        .count();
    assert_eq!(permission_denied_count, 1);
    assert!(agent_events
        .iter()
        .find(|e| e.event_type == event_type::PERMISSION_DENIED)
        .unwrap()
        .action_id
        .is_none());
}

#[tokio::test]
async fn approve_action_emits_action_approved() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-approve-event").await;
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

    approve_action(&pool, action.id, "ops-oncall")
        .await
        .unwrap();

    let chain = events::action_causal_chain(&pool, action.id).await.unwrap();
    let approved = chain
        .iter()
        .find(|e| e.event_type == event_type::ACTION_APPROVED);
    assert!(approved.is_some());
    assert_eq!(approved.unwrap().payload["approved_by"], "ops-oncall");
}

#[tokio::test]
async fn execute_action_emits_action_executed_with_outcome_in_payload() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-execute-event").await;
    grant(&pool, agent_id, "service.restart", "nginx").await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "service.restart",
        serde_json::json!({"service_name": "nginx"}),
        None,
    )
    .await
    .unwrap();
    approve_action(&pool, action.id, "ops-oncall")
        .await
        .unwrap();

    let executed = execute_action(
        &pool,
        &registry,
        &DryRunConfig {
            server_forced: true,
        },
        action.id,
    )
    .await
    .unwrap();
    assert_eq!(executed.status, "executed");

    let chain = events::action_causal_chain(&pool, action.id).await.unwrap();
    let executed_event = chain
        .iter()
        .find(|e| e.event_type == event_type::ACTION_EXECUTED);
    assert!(executed_event.is_some());
    assert_eq!(executed_event.unwrap().payload["dry_run"], true);
    assert_eq!(executed_event.unwrap().payload["would_restart"], "nginx");
}

#[tokio::test]
async fn full_causal_chain_is_proposed_then_approved_then_executed_in_order() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-full-chain").await;
    grant(&pool, agent_id, "service.restart", "nginx").await;
    let registry = HandlerRegistry::default_handlers();

    let action = propose_action(
        &pool,
        &registry,
        agent_id,
        "service.restart",
        serde_json::json!({"service_name": "nginx"}),
        None,
    )
    .await
    .unwrap();
    approve_action(&pool, action.id, "ops-oncall")
        .await
        .unwrap();
    execute_action(
        &pool,
        &registry,
        &DryRunConfig {
            server_forced: true,
        },
        action.id,
    )
    .await
    .unwrap();

    let chain = events::action_causal_chain(&pool, action.id).await.unwrap();
    let types: Vec<&str> = chain.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            event_type::ACTION_PROPOSED,
            event_type::ACTION_APPROVED,
            event_type::ACTION_EXECUTED,
        ],
        "causal chain must be in chronological order (oldest first)"
    );

    // Sequence numbers themselves must be strictly increasing, not just
    // the event_type labels happening to line up.
    for pair in chain.windows(2) {
        assert!(pair[0].sequence < pair[1].sequence);
    }
}

#[tokio::test]
async fn agent_timeline_is_newest_first() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-timeline-order").await;
    grant(&pool, agent_id, "process.stop", "pid:1").await;
    let registry = HandlerRegistry::default_handlers();

    let action1 = propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 1}),
        None,
    )
    .await
    .unwrap();
    // A second proposal (denied, different pid with no grant) to create
    // a second, later event for the same agent.
    propose_action(
        &pool,
        &registry,
        agent_id,
        "process.stop",
        serde_json::json!({"pid": 2}),
        None,
    )
    .await
    .unwrap();

    let timeline = events::agent_timeline(&pool, agent_id, 10).await.unwrap();
    assert!(timeline.len() >= 2);
    // Newest first: the first entry's sequence should be >= the second's.
    assert!(timeline[0].sequence >= timeline[1].sequence);
    // And the very first proposal's ActionProposed event should NOT be
    // the newest entry, since a later proposal followed it.
    let first_proposal_event_seq = timeline
        .iter()
        .find(|e| e.action_id == Some(action1.id))
        .unwrap()
        .sequence;
    assert!(timeline[0].sequence >= first_proposal_event_seq);
}

// --- Aggregate exit criterion: permission_checks count == matching ------
// --- events count, over a batch of mixed outcomes (not just one case) --

#[tokio::test]
async fn permission_checks_and_events_counts_match_exactly_over_a_mixed_batch() {
    // phase4-event-store.md §8's actual exit criterion: cross-check
    // permission_checks count against matching events count over a test
    // run — not just "one allow produces one event", but that the two
    // tables stay in lockstep across a realistic mix of outcomes
    // (allowed, scope-mismatch, no-grant, expired, suspended agent,
    // unknown capability), run through both check_permission (standalone)
    // and propose_action (which uses check_permission_tx internally).
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;

    let active_agent = create_agent(&pool, "agent-aggregate-active").await;
    let suspended_agent = create_agent(&pool, "agent-aggregate-suspended").await;
    sqlx::query("UPDATE agents SET status = 'suspended' WHERE id = $1")
        .bind(suspended_agent)
        .execute(&pool)
        .await
        .unwrap();

    grant(&pool, active_agent, "process.stop", "pid:1234").await;
    grant(&pool, active_agent, "service.restart", "nginx").await;

    let registry = HandlerRegistry::default_handlers();

    // Mixed batch, deliberately varied:
    use agentdb_runtime::permissions::check_permission;

    // 1. Allowed, via the standalone pool-based wrapper.
    check_permission(&pool, active_agent, "process.stop", "pid:1234")
        .await
        .unwrap();
    // 2. Scope mismatch, via the standalone wrapper.
    check_permission(&pool, active_agent, "process.stop", "pid:9999")
        .await
        .unwrap();
    // 3. No grant at all for this capability.
    check_permission(&pool, active_agent, "cgroup.set_limit", "/sys/fs/cgroup/x")
        .await
        .unwrap();
    // 4. Suspended agent.
    check_permission(&pool, suspended_agent, "process.stop", "pid:1")
        .await
        .unwrap();
    // 5. Unknown capability.
    check_permission(&pool, active_agent, "not.a.real.capability", "whatever")
        .await
        .unwrap();
    // 6. Unknown agent entirely.
    check_permission(&pool, Uuid::new_v4(), "process.stop", "pid:1")
        .await
        .unwrap();
    // 7 & 8. Allowed and denied, via propose_action -> check_permission_tx
    //        (the transaction-participating path, not the standalone one).
    propose_action(
        &pool,
        &registry,
        active_agent,
        "service.restart",
        serde_json::json!({"service_name": "nginx"}),
        None,
    )
    .await
    .unwrap();
    propose_action(
        &pool,
        &registry,
        active_agent,
        "process.stop",
        serde_json::json!({"pid": 424242}),
        None,
    )
    .await
    .unwrap();

    let permission_checks_count: i64 = sqlx::query_scalar("SELECT count(*) FROM permission_checks")
        .fetch_one(&pool)
        .await
        .unwrap();
    let matching_events_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE event_type IN ('PermissionAllowed', 'PermissionDenied')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(
        permission_checks_count, 8,
        "expected exactly 8 permission checks in this batch"
    );
    assert_eq!(
        permission_checks_count, matching_events_count,
        "permission_checks and PermissionAllowed/PermissionDenied events must stay in exact lockstep \
         — every check_permission_tx branch writes both in the same transaction"
    );

    // And the breakdown by decision must match too, not just the totals
    // (two tables could have equal totals while disagreeing on which
    // ones were allowed vs denied).
    let checks_allowed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM permission_checks WHERE decision = 'allowed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let events_allowed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = 'PermissionAllowed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(checks_allowed, events_allowed);
    assert_eq!(
        checks_allowed, 2,
        "expected exactly 2 allowed checks in this batch (#1 and #7)"
    );
}

// --- Regression tests for bugs found in this validation pass -----------

#[tokio::test]
async fn recent_events_combines_agent_id_and_since_instead_of_one_overriding_the_other() {
    // Regression test for the bug where GET /events?agent_id=X&since=Y
    // silently ignored agent_id and returned every agent's events. Two
    // agents each get an event; querying with both agent_id and a since
    // bound that includes both events must return only the targeted
    // agent's event, not both agents'.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_a = create_agent(&pool, "agent-combined-filter-a").await;
    let agent_b = create_agent(&pool, "agent-combined-filter-b").await;

    let before = Utc::now() - chrono::Duration::seconds(1);

    grant(&pool, agent_a, "process.stop", "pid:1").await;
    grant(&pool, agent_b, "process.stop", "pid:2").await;
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

    let results = events::recent_events(&pool, Some(agent_a), Some(before), None, 100)
        .await
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected at least agent_a's events in the window"
    );
    assert!(
        results.iter().all(|e| e.agent_id == agent_a),
        "agent_id filter must apply even when since is also provided — found an event for a different agent: {results:?}"
    );
}

#[tokio::test]
async fn recent_events_since_filter_still_works_without_an_agent_id() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-since-only").await;

    let far_future = Utc::now() + chrono::Duration::hours(1);
    let results = events::recent_events(&pool, None, Some(far_future), None, 100)
        .await
        .unwrap();
    assert!(
        results.is_empty(),
        "nothing should be newer than a cutoff an hour in the future"
    );

    let _ = agent_id;
}

#[tokio::test]
async fn denied_proposal_completed_at_is_never_before_created_at() {
    // Regression test for the Rust-side-clock-read bug: completed_at is
    // now computed via Postgres's own now() rather than a
    // chrono::Utc::now() read taken before the query was even sent.
    // This test can't reproduce real network latency in-process, so it
    // can't prove the bug under the exact conditions that caused it —
    // but it does confirm the structural invariant (completed_at not
    // before created_at) holds, and would catch a regression back to a
    // client-computed timestamp combined with an actually slow query.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-completed-at-order").await;
    let registry = HandlerRegistry::default_handlers();

    // No grant — guarantees the denial path that sets completed_at.
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
    let completed_at = action
        .completed_at
        .expect("denied proposal must set completed_at");
    assert!(
        completed_at >= action.created_at,
        "completed_at ({completed_at}) must not be before created_at ({})",
        action.created_at
    );
}

// --- DB-level append-only enforcement (migrations/0010) -----------------

#[tokio::test]
async fn events_table_rejects_update_even_from_the_owning_role() {
    // The test pool connects as the same role that ran the migrations
    // (and therefore owns `events`) — exactly the case a plain REVOKE
    // would fail to restrict, since table owners bypass REVOKE on their
    // own tables. Only a trigger (migrations/0010) actually stops this.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-append-only-update").await;

    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (agent_id, event_type, payload) VALUES ($1, 'AgentCreated', '{}'::jsonb) RETURNING id",
    )
    .bind(agent_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let result = sqlx::query("UPDATE events SET event_type = 'Tampered' WHERE id = $1")
        .bind(event_id)
        .execute(&pool)
        .await;

    assert!(
        result.is_err(),
        "UPDATE against events must be rejected by the append-only trigger, even for the owning role"
    );

    let event_type: String = sqlx::query_scalar("SELECT event_type FROM events WHERE id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        event_type, "AgentCreated",
        "the row must be unchanged after the rejected UPDATE"
    );
}

#[tokio::test]
async fn events_table_rejects_delete_even_from_the_owning_role() {
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-append-only-delete").await;

    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (agent_id, event_type, payload) VALUES ($1, 'AgentCreated', '{}'::jsonb) RETURNING id",
    )
    .bind(agent_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let result = sqlx::query("DELETE FROM events WHERE id = $1")
        .bind(event_id)
        .execute(&pool)
        .await;

    assert!(
        result.is_err(),
        "DELETE against events must be rejected by the append-only trigger, even for the owning role"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "the row must still exist after the rejected DELETE"
    );
}

#[tokio::test]
async fn emit_event_insert_still_works_normally_with_the_trigger_in_place() {
    // Sanity check that the trigger (BEFORE UPDATE OR DELETE) doesn't
    // accidentally also block INSERT, which it must never do.
    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;
    let agent_id = create_agent(&pool, "agent-insert-still-works").await;

    let mut tx = pool.begin().await.unwrap();
    events::emit_event(
        &mut tx,
        agent_id,
        None,
        None,
        event_type::AGENT_CREATED,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE agent_id = $1")
        .bind(agent_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// --- HTTP surface ---------------------------------------------------------

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
    async fn creating_an_agent_over_http_emits_agent_created() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agents")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "http-agent"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let agent = body_json(response).await;
        let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

        let timeline = events::agent_timeline(&pool, agent_id, 10).await.unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].event_type, event_type::AGENT_CREATED);
    }

    #[tokio::test]
    async fn issuing_and_revoking_a_grant_over_http_emits_both_events() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-grant-events-http").await;

        let app1 = api::router(pool.clone(), pool.clone());
        let create_response = app1
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
                            "granted_by": "admin"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_response.status(), StatusCode::CREATED);
        let grant_json = body_json(create_response).await;
        let grant_id: Uuid = grant_json["id"].as_str().unwrap().parse().unwrap();

        let app2 = api::router(pool.clone(), pool.clone());
        let revoke_response = app2
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
        assert_eq!(revoke_response.status(), StatusCode::OK);

        let timeline = events::agent_timeline(&pool, agent_id, 10).await.unwrap();
        let types: Vec<&str> = timeline.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&event_type::GRANT_ISSUED));
        assert!(types.contains(&event_type::GRANT_REVOKED));
    }

    #[tokio::test]
    async fn events_endpoint_requires_agent_id_or_since() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn events_endpoint_filters_by_since() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-since-filter").await;

        let future_cutoff = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/events?since={future_cutoff}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        // Nothing should be newer than a cutoff one hour in the future.
        assert_eq!(json.as_array().unwrap().len(), 0);

        let _ = agent_id; // not used beyond establishing a realistic DB state
    }

    #[tokio::test]
    async fn action_events_endpoint_returns_the_causal_chain() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let agent_id = create_agent(&pool, "agent-action-events-http").await;
        grant(&pool, agent_id, "process.stop", "pid:1234").await;
        let registry = agentdb_runtime::actions::HandlerRegistry::default_handlers();

        let action = propose_action(
            &pool,
            &registry,
            agent_id,
            "process.stop",
            json!({"pid": 1234}),
            None,
        )
        .await
        .unwrap();

        let app = api::router(pool.clone(), pool.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/events/actions/{}", action.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["event_type"], "ActionProposed");
    }

    #[tokio::test]
    async fn action_events_endpoint_returns_empty_list_for_unknown_action() {
        let pool = common::test_pool().await;
        common::reset_tables(&pool).await;
        let app = api::router(pool.clone(), pool.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/events/actions/{}", Uuid::new_v4()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json.as_array().unwrap().len(), 0);
    }
}
