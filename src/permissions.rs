use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::events::{emit_event, event_type};

/// Why a permission check was denied. Every variant maps to a specific,
/// stable string stored in `permission_checks.reason` and returned over
/// the HTTP API — don't change a variant's `as_str()` output without
/// treating it as a breaking change for anything consuming that string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    /// No active (non-revoked) grant exists for this agent+capability at all.
    NoGrant,
    /// At least one active grant exists for this capability, but none of
    /// its scopes match the requested scope.
    ScopeMismatch,
    /// A grant exists whose scope would match, but it has expired.
    /// Reported in preference to `ScopeMismatch` when both are true
    /// across different grants — "it used to work and lapsed" is a more
    /// useful signal to an operator than "scope never matched".
    Expired,
    /// The agent exists but its status is not 'active'.
    AgentSuspended,
    /// No agent exists with the given id at all — distinct from
    /// `AgentSuspended` because it usually indicates a bug or a forged
    /// id, not an administrative suspension of a real agent.
    AgentNotFound,
    /// No capability exists with the given name.
    CapabilityNotFound,
}

impl DenialReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DenialReason::NoGrant => "no_grant",
            DenialReason::ScopeMismatch => "scope_mismatch",
            DenialReason::Expired => "expired",
            DenialReason::AgentSuspended => "agent_suspended",
            DenialReason::AgentNotFound => "agent_not_found",
            DenialReason::CapabilityNotFound => "capability_not_found",
        }
    }
}

/// The outcome of a permission check. `Allowed` carries the specific
/// grant that authorized it — later phases (the action engine) store
/// this `grant_id` on the action they create, which is what makes "this
/// action was authorized by this specific grant" a join instead of a
/// guess.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResult {
    Allowed { grant_id: Uuid },
    Denied { reason: DenialReason },
}

impl PermissionResult {
    pub fn is_allowed(&self) -> bool {
        matches!(self, PermissionResult::Allowed { .. })
    }

    pub fn is_denied(&self) -> bool {
        !self.is_allowed()
    }
}

/// Scope-matching rules for v0.1 (see phase2-permission-engine.md §3):
///
/// - Exact string match.
/// - A granted scope ending in `/**` matches any requested scope sharing
///   that prefix (`/workspace/**` matches `/workspace/output/file.txt`).
/// - A granted scope ending in a bare `*` matches any requested scope
///   sharing that prefix, with no slash required (`api.*` matches
///   `api.example.com`; `*` alone matches everything).
///
/// No hierarchical inheritance, no mid-string wildcards — a `*` is only
/// recognized when it's the very last character of the granted scope.
/// These are deliberate v0.1 limitations, not oversights; see this
/// module's tests for the exact boundary behavior they imply, including
/// the easy-to-miss case that `/workspace/**` does **not** match the
/// bare string `/workspace` itself.
pub fn scope_matches(granted_scope: &str, requested_scope: &str) -> bool {
    if granted_scope == requested_scope {
        return true;
    }
    if let Some(prefix) = granted_scope.strip_suffix("/**") {
        return requested_scope.starts_with(prefix)
            && (requested_scope.len() > prefix.len()
                && requested_scope.as_bytes()[prefix.len()] == b'/');
    }
    if let Some(prefix) = granted_scope.strip_suffix('*') {
        return requested_scope.starts_with(prefix);
    }
    false
}

/// The single enforcement choke point, for callers that just need a
/// standalone answer (the `/permissions/check` debug endpoint, and every
/// existing Phase 2 test). Opens its own short transaction, delegates to
/// `check_permission_tx`, and commits.
///
/// For a caller that needs the permission check to participate in a
/// larger atomic operation — Phase 3's `propose_action`, for instance,
/// which must have the permission check, the `actions` row insert, and
/// the resulting event all succeed or fail together — use
/// `check_permission_tx` directly with the caller's own open
/// transaction instead of this wrapper. No Postgres savepoint/nested
/// transaction is needed for that: `check_permission_tx` just runs more
/// queries within whatever transaction it's handed.
///
/// Returns `Ok(PermissionResult)` for any outcome the caller should act
/// on — allowed and denied are both legitimate, fully-handled results.
/// Returns `Err` only for an actual database failure: a transport-level
/// problem distinct from "the check ran and said no".
pub async fn check_permission(
    pool: &PgPool,
    agent_id: Uuid,
    capability_name: &str,
    requested_scope: &str,
) -> Result<PermissionResult, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let result = check_permission_tx(&mut tx, agent_id, capability_name, requested_scope).await?;
    tx.commit().await?;
    Ok(result)
}

/// Transaction-scoped core of `check_permission` — see that function's
/// doc comment for when to use this instead. Every branch, success or
/// failure, writes a row to `permission_checks` *and* emits a
/// `PermissionAllowed`/`PermissionDenied` event before returning, within
/// the same transaction the caller passed in, so "why was this denied"
/// is always reconstructable afterward and never out of sync with the
/// event log.
pub async fn check_permission_tx(
    tx: &mut Transaction<'_, Postgres>,
    agent_id: Uuid,
    capability_name: &str,
    requested_scope: &str,
) -> Result<PermissionResult, sqlx::Error> {
    let agent_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await?;

    let agent_status = match agent_status {
        Some(s) => s,
        None => {
            return finalize_tx(
                tx,
                agent_id,
                capability_name,
                requested_scope,
                PermissionResult::Denied {
                    reason: DenialReason::AgentNotFound,
                },
            )
            .await;
        }
    };

    if agent_status != "active" {
        return finalize_tx(
            tx,
            agent_id,
            capability_name,
            requested_scope,
            PermissionResult::Denied {
                reason: DenialReason::AgentSuspended,
            },
        )
        .await;
    }

    let capability_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM capabilities WHERE name = $1")
            .bind(capability_name)
            .fetch_optional(&mut **tx)
            .await?;

    let capability_id = match capability_id {
        Some(id) => id,
        None => {
            return finalize_tx(
                tx,
                agent_id,
                capability_name,
                requested_scope,
                PermissionResult::Denied {
                    reason: DenialReason::CapabilityNotFound,
                },
            )
            .await;
        }
    };

    // Fetch every non-revoked grant for this (agent, capability) pair —
    // including already-expired ones — so the denial reason below can
    // distinguish "never had a matching grant" from "had one, it lapsed".
    let rows: Vec<(Uuid, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, scope, expires_at FROM grants \
         WHERE agent_id = $1 AND capability_id = $2 AND revoked_at IS NULL",
    )
    .bind(agent_id)
    .bind(capability_id)
    .fetch_all(&mut **tx)
    .await?;

    if rows.is_empty() {
        return finalize_tx(
            tx,
            agent_id,
            capability_name,
            requested_scope,
            PermissionResult::Denied {
                reason: DenialReason::NoGrant,
            },
        )
        .await;
    }

    let now = Utc::now();
    let mut saw_expired_match = false;

    for (grant_id, scope, expires_at) in &rows {
        if !scope_matches(scope, requested_scope) {
            continue;
        }
        let is_expired = expires_at.map(|exp| exp <= now).unwrap_or(false);
        if is_expired {
            saw_expired_match = true;
            continue; // a different grant in the list might still cover it and be active
        }
        return finalize_tx(
            tx,
            agent_id,
            capability_name,
            requested_scope,
            PermissionResult::Allowed {
                grant_id: *grant_id,
            },
        )
        .await;
    }

    let reason = if saw_expired_match {
        DenialReason::Expired
    } else {
        DenialReason::ScopeMismatch
    };

    finalize_tx(
        tx,
        agent_id,
        capability_name,
        requested_scope,
        PermissionResult::Denied { reason },
    )
    .await
}

async fn finalize_tx(
    tx: &mut Transaction<'_, Postgres>,
    agent_id: Uuid,
    capability_name: &str,
    requested_scope: &str,
    result: PermissionResult,
) -> Result<PermissionResult, sqlx::Error> {
    let (decision, matched_grant_id, reason): (&str, Option<Uuid>, Option<&str>) = match &result {
        PermissionResult::Allowed { grant_id } => ("allowed", Some(*grant_id), None),
        PermissionResult::Denied { reason } => ("denied", None, Some(reason.as_str())),
    };

    // Deliberately no foreign key on agent_id in the schema (see
    // migrations/0006_permission_checks.sql) — AgentNotFound denials must
    // still be loggable even though, by definition, no row in `agents`
    // exists for this insert to reference.
    sqlx::query(
        "INSERT INTO permission_checks \
         (agent_id, capability_name, requested_scope, decision, matched_grant_id, reason) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(agent_id)
    .bind(capability_name)
    .bind(requested_scope)
    .bind(decision)
    .bind(matched_grant_id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;

    let event_type = match decision {
        "allowed" => event_type::PERMISSION_ALLOWED,
        _ => event_type::PERMISSION_DENIED,
    };
    let payload = serde_json::json!({
        "capability_name": capability_name,
        "requested_scope": requested_scope,
        "reason": reason,
    });

    emit_event(tx, agent_id, None, matched_grant_id, event_type, payload).await?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(scope_matches(
            "/workspace/output.txt",
            "/workspace/output.txt"
        ));
    }

    #[test]
    fn exact_match_is_case_sensitive() {
        assert!(!scope_matches("/Workspace", "/workspace"));
    }

    #[test]
    fn double_star_matches_nested_paths() {
        assert!(scope_matches("/workspace/**", "/workspace/output/file.txt"));
        assert!(scope_matches("/workspace/**", "/workspace/a"));
    }

    #[test]
    fn double_star_requires_something_after_the_slash() {
        // Known, deliberate v0.1 edge case: `/workspace/**` does NOT
        // match the bare directory `/workspace` itself (no trailing
        // slash + content to match against the prefix). To also cover
        // the bare directory, grant it explicitly, or use a single
        // trailing `*` instead (see the next test).
        assert!(!scope_matches("/workspace/**", "/workspace"));
    }

    #[test]
    fn single_star_matches_broader_prefix_including_the_bare_prefix_itself() {
        assert!(scope_matches("/workspace*", "/workspace"));
        assert!(scope_matches("/workspace*", "/workspace/output/file.txt"));
    }

    #[test]
    fn bare_star_matches_everything() {
        assert!(scope_matches("*", ""));
        assert!(scope_matches("*", "anything/at/all"));
    }

    #[test]
    fn mid_string_wildcard_is_not_special() {
        // '*' is only recognized as the final character of the granted
        // scope; a '*' elsewhere is just a literal character, so this
        // does NOT behave as a middle wildcard.
        assert!(!scope_matches("api.*.example.com", "api.foo.example.com"));
    }

    #[test]
    fn no_match_without_wildcard_or_exact_equality() {
        assert!(!scope_matches("/workspace", "/workspace/output.txt"));
        assert!(!scope_matches("/workspace/", "/workspace/output.txt"));
    }

    #[test]
    fn empty_granted_scope_only_matches_empty_requested_scope() {
        assert!(scope_matches("", ""));
        assert!(!scope_matches("", "anything"));
    }

    #[test]
    fn denial_reason_strings_are_stable() {
        // These strings are a de facto public API (stored in the DB,
        // returned over HTTP) — this test exists so a future refactor
        // that accidentally changes one is caught immediately rather
        // than silently breaking anything matching on the old string.
        assert_eq!(DenialReason::NoGrant.as_str(), "no_grant");
        assert_eq!(DenialReason::ScopeMismatch.as_str(), "scope_mismatch");
        assert_eq!(DenialReason::Expired.as_str(), "expired");
        assert_eq!(DenialReason::AgentSuspended.as_str(), "agent_suspended");
        assert_eq!(DenialReason::AgentNotFound.as_str(), "agent_not_found");
        assert_eq!(
            DenialReason::CapabilityNotFound.as_str(),
            "capability_not_found"
        );
    }
}
