use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// The event types this codebase actually emits, as of Phase 4. This is
/// deliberately a plain set of string constants rather than a Rust enum
/// stored as-is: the strings are what live in the database and get
/// returned over HTTP, so they're the real contract — an enum here would
/// just be a second representation of the same thing, not a stronger
/// guarantee, and `events.event_type` has no CHECK constraint pinning it
/// to a fixed list (unlike `actions.status`) because the catalog is
/// expected to grow as later phases add new event-producing code paths,
/// without needing a migration each time.
pub mod event_type {
    pub const AGENT_CREATED: &str = "AgentCreated";
    pub const GRANT_ISSUED: &str = "GrantIssued";
    pub const GRANT_REVOKED: &str = "GrantRevoked";
    pub const PERMISSION_ALLOWED: &str = "PermissionAllowed";
    pub const PERMISSION_DENIED: &str = "PermissionDenied";
    pub const ACTION_PROPOSED: &str = "ActionProposed";
    /// Not in phase4-event-store.md's original catalog — added because
    /// Phase 3's propose_action still creates an `actions` row (status
    /// `failed`) even when permission is denied, and that denial
    /// deserves its own event distinct from a successful proposal, not
    /// just the separate, not-action-linked PermissionDenied event
    /// emitted by check_permission itself.
    pub const ACTION_PROPOSAL_DENIED: &str = "ActionProposalDenied";
    pub const ACTION_APPROVED: &str = "ActionApproved";
    pub const ACTION_EXECUTED: &str = "ActionExecuted";
    pub const ACTION_FAILED: &str = "ActionFailed";
}

/// Writes one event row using the given transaction — the caller is
/// responsible for committing (or letting it roll back on error/drop).
/// This is the only way an event should ever be written: every call site
/// in `permissions.rs`, `actions.rs`, and `permissions_api.rs` emits
/// inside the same transaction as the state change the event describes,
/// which is what makes an event's existence proof that the thing it
/// describes actually happened — see the module-level discussion in
/// phase4-event-store.md §4.
pub async fn emit_event(
    tx: &mut Transaction<'_, Postgres>,
    agent_id: Uuid,
    action_id: Option<Uuid>,
    grant_id: Option<Uuid>,
    event_type: &str,
    payload: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO events (agent_id, action_id, grant_id, event_type, payload) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(agent_id)
    .bind(action_id)
    .bind(grant_id)
    .bind(event_type)
    .bind(payload)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

#[derive(Debug, Serialize, Clone, sqlx::FromRow)]
pub struct EventRecord {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub action_id: Option<Uuid>,
    pub grant_id: Option<Uuid>,
    pub event_type: String,
    pub payload: Value,
    pub occurred_at: DateTime<Utc>,
    pub sequence: i64,
}

/// Full timeline for one agent, newest first — matching the convention
/// every other list endpoint in this codebase uses (`list_grants`,
/// `list_actions`, `list_permission_checks`). `action_causal_chain`
/// below is ordered the other way (oldest first) because a causal chain
/// reads naturally as a story ("proposed, then approved, then
/// executed"); a timeline reads naturally as a feed (most recent first).
/// This asymmetry is intentional, not an inconsistency — see each
/// function's `ORDER BY` direction. Both order by `sequence`, not
/// `occurred_at` — see the comment on that column in
/// migrations/0009_events.sql.
///
/// A thin wrapper over `recent_events` with no time bound — kept as its
/// own named function since "give me everything for this agent" is
/// common enough to deserve a direct call without the caller needing to
/// know `recent_events`'s full filter set.
pub async fn agent_timeline(
    pool: &PgPool,
    agent_id: Uuid,
    limit: i64,
) -> Result<Vec<EventRecord>, sqlx::Error> {
    recent_events(pool, Some(agent_id), None, None, limit).await
}

/// The causal chain for one action: every event tied to it, in order —
/// e.g. ActionProposed (carrying the grant_id that authorized it) ->
/// ActionApproved -> ActionExecuted.
pub async fn action_causal_chain(
    pool: &PgPool,
    action_id: Uuid,
) -> Result<Vec<EventRecord>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM events WHERE action_id = $1 ORDER BY sequence ASC")
        .bind(action_id)
        .fetch_all(pool)
        .await
}

/// General-purpose filtered event feed, newest first. Backs both
/// `agent_timeline` (agent_id only) and the "what changed in the last N
/// minutes" literal query behind phase6-demo-loop.md's flagship demo
/// (`since` only, or `since` combined with `agent_id` to bound one
/// agent's recent activity specifically).
///
/// Each filter is independently optional and all three combine with
/// AND — this single function, rather than separate
/// per-filter-combination functions, is what makes "agent_id AND since
/// together" actually work; an earlier version of this code had
/// `agent_timeline` and `recent_events` as two separate queries with no
/// way to combine their filters, which silently dropped `agent_id`
/// whenever `since` was also provided.
pub async fn recent_events(
    pool: &PgPool,
    agent_id: Option<Uuid>,
    since: Option<DateTime<Utc>>,
    event_type_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<EventRecord>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM events \
         WHERE ($1::uuid IS NULL OR agent_id = $1) \
           AND ($2::timestamptz IS NULL OR occurred_at > $2) \
           AND ($3::text IS NULL OR event_type = $3) \
         ORDER BY sequence DESC \
         LIMIT $4",
    )
    .bind(agent_id)
    .bind(since)
    .bind(event_type_filter)
    .bind(limit)
    .fetch_all(pool)
    .await
}
