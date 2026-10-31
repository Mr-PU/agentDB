use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::fmt;
use uuid::Uuid;

use crate::events::{emit_event, event_type};
use crate::permissions::{check_permission_tx, PermissionResult};

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

#[derive(Debug)]
pub enum ActionError {
    InvalidParameters(String),
    SystemCallFailed(String),
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::InvalidParameters(msg) => write!(f, "invalid parameters: {msg}"),
            ActionError::SystemCallFailed(msg) => write!(f, "system call failed: {msg}"),
        }
    }
}

impl std::error::Error for ActionError {}

#[derive(Debug)]
pub enum ProposeError {
    UnknownActionType(String),
    Action(ActionError),
    Database(sqlx::Error),
}

impl fmt::Display for ProposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProposeError::UnknownActionType(t) => write!(f, "unknown action type: {t}"),
            ProposeError::Action(e) => write!(f, "{e}"),
            ProposeError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

#[derive(Debug)]
pub enum ApproveError {
    NotFound,
    /// The action exists but isn't in `proposed` status — either it was
    /// already approved/executed, or it never reached a proposable state
    /// (e.g. permission was denied at proposal time, leaving it `failed`).
    WrongStatus,
    Database(sqlx::Error),
}

#[derive(Debug)]
pub enum ExecuteError {
    NotFound,
    NotApproved(String),
    UnknownActionType(String),
    Database(sqlx::Error),
}

// ---------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------

/// What a handler's `execute()` produced — stored verbatim in
/// `actions.outcome` for the audit trail. `dry_run: true` in `details`
/// is set by each handler itself on the simulated path, so the outcome
/// JSON is self-describing even without cross-referencing
/// `effective_dry_run` on the action row.
#[derive(Debug, Clone)]
pub struct ActionOutcome {
    pub details: Value,
}

/// Every action handler implements this uniform contract so the engine
/// can treat them identically (see phase3-action-engine.md §4).
///
/// `execute` and `compensate` both take `dry_run: bool` — the one place
/// in this file where the "two implementations behind a toggle" design
/// actually lives. Every handler is responsible for checking this flag
/// itself and returning a structured simulation instead of making the
/// real system call when it's `true`. This keeps the toggle colocated
/// with the logic it's toggling rather than living as a wrapper the
/// engine applies from outside, which would make it too easy for a
/// handler to accidentally run its real path under the dry-run engine
/// path (or vice versa) if the two were ever separated.
#[async_trait]
pub trait ActionHandler: Send + Sync {
    fn action_type(&self) -> &'static str;
    fn required_capability(&self) -> &'static str;
    fn scope_of(&self, parameters: &Value) -> Result<String, ActionError>;
    fn is_reversible(&self) -> bool;
    async fn execute(
        &self,
        parameters: &Value,
        dry_run: bool,
    ) -> Result<ActionOutcome, ActionError>;
    async fn compensate(
        &self,
        parameters: &Value,
        outcome: &ActionOutcome,
        dry_run: bool,
    ) -> Result<(), ActionError>;
}

fn get_i64(parameters: &Value, field: &str) -> Result<i64, ActionError> {
    parameters
        .get(field)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ActionError::InvalidParameters(format!("missing or non-integer '{field}'")))
}

fn get_str<'a>(parameters: &'a Value, field: &str) -> Result<&'a str, ActionError> {
    parameters
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ActionError::InvalidParameters(format!("missing or non-string '{field}'")))
}

// ---------------------------------------------------------------------
// process.stop — irreversible; SIGTERM, escalate to SIGKILL after a grace period
// ---------------------------------------------------------------------

pub struct ProcessStopHandler;

#[async_trait]
impl ActionHandler for ProcessStopHandler {
    fn action_type(&self) -> &'static str {
        "process.stop"
    }

    fn required_capability(&self) -> &'static str {
        "process.stop"
    }

    fn scope_of(&self, parameters: &Value) -> Result<String, ActionError> {
        let pid = get_i64(parameters, "pid")?;
        Ok(format!("pid:{pid}"))
    }

    fn is_reversible(&self) -> bool {
        false // a stopped process cannot be resumed as the same process
    }

    async fn execute(
        &self,
        parameters: &Value,
        dry_run: bool,
    ) -> Result<ActionOutcome, ActionError> {
        let pid = get_i64(parameters, "pid")?;

        if dry_run {
            return Ok(ActionOutcome {
                details: serde_json::json!({
                    "dry_run": true,
                    "would_signal": "SIGTERM, escalating to SIGKILL after 3s if still alive",
                    "pid": pid,
                }),
            });
        }

        real::stop_process(pid).await?;

        Ok(ActionOutcome {
            details: serde_json::json!({ "dry_run": false, "pid": pid, "signaled": true }),
        })
    }

    async fn compensate(
        &self,
        _parameters: &Value,
        _outcome: &ActionOutcome,
        _dry_run: bool,
    ) -> Result<(), ActionError> {
        // Irreversible by design (see is_reversible). A restart is a
        // separate, explicit process.start action proposed on its own —
        // never an automatic consequence of this one failing or being
        // rolled back (phase3-action-engine.md §4.1).
        Ok(())
    }
}

// ---------------------------------------------------------------------
// process.start — reversible via process.stop on the newly created pid
// ---------------------------------------------------------------------

pub struct ProcessStartHandler;

#[async_trait]
impl ActionHandler for ProcessStartHandler {
    fn action_type(&self) -> &'static str {
        "process.start"
    }

    fn required_capability(&self) -> &'static str {
        "process.start"
    }

    fn scope_of(&self, parameters: &Value) -> Result<String, ActionError> {
        Ok(get_str(parameters, "command")?.to_string())
    }

    fn is_reversible(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        parameters: &Value,
        dry_run: bool,
    ) -> Result<ActionOutcome, ActionError> {
        let command = get_str(parameters, "command")?;

        if dry_run {
            return Ok(ActionOutcome {
                details: serde_json::json!({ "dry_run": true, "would_start": command }),
            });
        }

        let pid = real::start_process(command).await?;

        Ok(ActionOutcome {
            details: serde_json::json!({ "dry_run": false, "pid": pid, "command": command }),
        })
    }

    async fn compensate(
        &self,
        _parameters: &Value,
        outcome: &ActionOutcome,
        dry_run: bool,
    ) -> Result<(), ActionError> {
        if dry_run {
            return Ok(());
        }
        let pid = outcome
            .details
            .get("pid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                ActionError::InvalidParameters("no pid recorded in outcome to compensate".into())
            })?;
        real::stop_process(pid).await
    }
}

// ---------------------------------------------------------------------
// cgroup.set_limit — reversible; reads the previous value before writing
// ---------------------------------------------------------------------

pub struct CgroupSetLimitHandler;

#[async_trait]
impl ActionHandler for CgroupSetLimitHandler {
    fn action_type(&self) -> &'static str {
        "cgroup.set_limit"
    }

    fn required_capability(&self) -> &'static str {
        "cgroup.set_limit"
    }

    fn scope_of(&self, parameters: &Value) -> Result<String, ActionError> {
        Ok(get_str(parameters, "cgroup_path")?.to_string())
    }

    fn is_reversible(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        parameters: &Value,
        dry_run: bool,
    ) -> Result<ActionOutcome, ActionError> {
        let cgroup_path = get_str(parameters, "cgroup_path")?;
        let limit_bytes = get_i64(parameters, "memory_limit_bytes")?;

        let previous_value = real::read_cgroup_memory_max(cgroup_path).await?;

        if dry_run {
            return Ok(ActionOutcome {
                details: serde_json::json!({
                    "dry_run": true,
                    "cgroup_path": cgroup_path,
                    "would_set_memory_max": limit_bytes,
                    "previous_value": previous_value,
                }),
            });
        }

        real::write_cgroup_memory_max(cgroup_path, limit_bytes).await?;

        Ok(ActionOutcome {
            details: serde_json::json!({
                "dry_run": false,
                "cgroup_path": cgroup_path,
                "new_memory_max": limit_bytes,
                "previous_value": previous_value,
            }),
        })
    }

    async fn compensate(
        &self,
        parameters: &Value,
        outcome: &ActionOutcome,
        dry_run: bool,
    ) -> Result<(), ActionError> {
        if dry_run {
            return Ok(());
        }
        let cgroup_path = get_str(parameters, "cgroup_path")?;
        let previous_value = outcome
            .details
            .get("previous_value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ActionError::InvalidParameters("no previous_value recorded to restore".into())
            })?;
        real::write_cgroup_memory_max_raw(cgroup_path, previous_value).await
    }
}

// ---------------------------------------------------------------------
// service.restart — not meaningfully reversible
// ---------------------------------------------------------------------

pub struct ServiceRestartHandler;

#[async_trait]
impl ActionHandler for ServiceRestartHandler {
    fn action_type(&self) -> &'static str {
        "service.restart"
    }

    fn required_capability(&self) -> &'static str {
        "service.restart"
    }

    fn scope_of(&self, parameters: &Value) -> Result<String, ActionError> {
        Ok(get_str(parameters, "service_name")?.to_string())
    }

    fn is_reversible(&self) -> bool {
        false // "restart" has no meaningful undo — phase3-action-engine.md §4.4
    }

    async fn execute(
        &self,
        parameters: &Value,
        dry_run: bool,
    ) -> Result<ActionOutcome, ActionError> {
        let service_name = get_str(parameters, "service_name")?;

        if dry_run {
            return Ok(ActionOutcome {
                details: serde_json::json!({ "dry_run": true, "would_restart": service_name }),
            });
        }

        real::restart_service(service_name).await?;

        Ok(ActionOutcome {
            details: serde_json::json!({ "dry_run": false, "service_name": service_name, "restarted": true }),
        })
    }

    async fn compensate(
        &self,
        _parameters: &Value,
        _outcome: &ActionOutcome,
        _dry_run: bool,
    ) -> Result<(), ActionError> {
        Ok(()) // no meaningful compensation — see is_reversible
    }
}

// ---------------------------------------------------------------------
// Real system-call implementations, isolated in one place
// ---------------------------------------------------------------------

/// Every actual host-touching system call lives in this one module,
/// deliberately separated from the handler structs above. Every function
/// here is only ever called from a handler's `execute`/`compensate` on
/// the `dry_run == false` path — nothing here runs unless the dry-run
/// toggle has explicitly been turned off, and grepping this module name
/// is enough to find every place this codebase can touch the real host.
mod real {
    use super::ActionError;

    #[cfg(unix)]
    pub async fn stop_process(pid: i64) -> Result<(), ActionError> {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        let nix_pid = Pid::from_raw(pid as i32);

        kill(nix_pid, Signal::SIGTERM).map_err(|e| {
            ActionError::SystemCallFailed(format!("SIGTERM to pid {pid} failed: {e}"))
        })?;

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // A signal of `None` probes for existence/permission without
        // actually sending anything — the standard nix idiom for "is
        // this pid still alive".
        if kill(nix_pid, None).is_ok() {
            kill(nix_pid, Signal::SIGKILL).map_err(|e| {
                ActionError::SystemCallFailed(format!("SIGKILL to pid {pid} failed: {e}"))
            })?;
        }

        Ok(())
    }

    #[cfg(not(unix))]
    pub async fn stop_process(_pid: i64) -> Result<(), ActionError> {
        Err(ActionError::SystemCallFailed(
            "process.stop's real path is only implemented on unix".into(),
        ))
    }

    #[cfg(unix)]
    pub async fn start_process(command: &str) -> Result<i64, ActionError> {
        let parts: Vec<&str> = command.split_whitespace().collect();
        let (program, args) = parts
            .split_first()
            .ok_or_else(|| ActionError::InvalidParameters("command is empty".into()))?;

        // Not awaited: a started process/service is expected to keep
        // running independently of this action's lifetime. Dropping the
        // `Child` handle here does NOT kill the process — tokio only
        // does that if `.kill_on_drop(true)` was set on the Command,
        // which it deliberately isn't.
        let child = tokio::process::Command::new(program)
            .args(args)
            .spawn()
            .map_err(|e| {
                ActionError::SystemCallFailed(format!("failed to spawn '{command}': {e}"))
            })?;

        let pid = child.id().ok_or_else(|| {
            ActionError::SystemCallFailed("spawned process has no pid (exited immediately?)".into())
        })?;

        Ok(pid as i64)
    }

    #[cfg(not(unix))]
    pub async fn start_process(_command: &str) -> Result<i64, ActionError> {
        Err(ActionError::SystemCallFailed(
            "process.start's real path is only implemented on unix".into(),
        ))
    }

    pub async fn read_cgroup_memory_max(cgroup_path: &str) -> Result<String, ActionError> {
        let file_path = format!("{cgroup_path}/memory.max");
        tokio::fs::read_to_string(&file_path)
            .await
            .map(|s| s.trim().to_string())
            .map_err(|e| ActionError::SystemCallFailed(format!("failed to read {file_path}: {e}")))
    }

    pub async fn write_cgroup_memory_max(
        cgroup_path: &str,
        limit_bytes: i64,
    ) -> Result<(), ActionError> {
        write_cgroup_memory_max_raw(cgroup_path, &limit_bytes.to_string()).await
    }

    pub async fn write_cgroup_memory_max_raw(
        cgroup_path: &str,
        raw_value: &str,
    ) -> Result<(), ActionError> {
        let file_path = format!("{cgroup_path}/memory.max");
        tokio::fs::write(&file_path, raw_value)
            .await
            .map_err(|e| ActionError::SystemCallFailed(format!("failed to write {file_path}: {e}")))
    }

    pub async fn restart_service(service_name: &str) -> Result<(), ActionError> {
        let output = tokio::process::Command::new("systemctl")
            .arg("restart")
            .arg(service_name)
            .output()
            .await
            .map_err(|e| {
                ActionError::SystemCallFailed(format!("failed to invoke systemctl: {e}"))
            })?;

        if !output.status.success() {
            return Err(ActionError::SystemCallFailed(format!(
                "systemctl restart {service_name} exited with {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------

pub struct HandlerRegistry {
    handlers: HashMap<&'static str, Box<dyn ActionHandler>>,
}

impl HandlerRegistry {
    pub fn default_handlers() -> Self {
        let mut handlers: HashMap<&'static str, Box<dyn ActionHandler>> = HashMap::new();
        for handler in [
            Box::new(ProcessStopHandler) as Box<dyn ActionHandler>,
            Box::new(ProcessStartHandler) as Box<dyn ActionHandler>,
            Box::new(CgroupSetLimitHandler) as Box<dyn ActionHandler>,
            Box::new(ServiceRestartHandler) as Box<dyn ActionHandler>,
        ] {
            handlers.insert(handler.action_type(), handler);
        }
        Self { handlers }
    }

    pub fn get(&self, action_type: &str) -> Option<&dyn ActionHandler> {
        self.handlers.get(action_type).map(|b| b.as_ref())
    }

    pub fn action_types(&self) -> Vec<&'static str> {
        let mut types: Vec<&'static str> = self.handlers.keys().copied().collect();
        types.sort_unstable();
        types
    }
}

// ---------------------------------------------------------------------
// Dry-run toggle
// ---------------------------------------------------------------------

/// The server-level safety ceiling. `server_forced == true` means every
/// action runs in simulation mode no matter what an individual request
/// asks for — this is the actual toggle: set `ACTIONS_DRY_RUN=false` to
/// allow real execution at all. A request can still ask for *more*
/// caution (`requested_dry_run: true`) even when the server allows real
/// execution, but it can never ask for *less* — there is no way for a
/// client to force real execution past a server configured for dry-run.
#[derive(Debug, Clone, Copy)]
pub struct DryRunConfig {
    pub server_forced: bool,
}

impl DryRunConfig {
    pub fn from_env() -> Self {
        let server_forced = std::env::var("ACTIONS_DRY_RUN")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true); // default: dry run — real execution is opt-in, not opt-out

        if server_forced {
            tracing::warn!(
                "ACTIONS_DRY_RUN is enabled (or unset) — every action will run in \
                 simulation mode and will NOT touch real processes, cgroups, or \
                 services. Set ACTIONS_DRY_RUN=false to allow real execution."
            );
        } else {
            tracing::warn!(
                "ACTIONS_DRY_RUN=false — approved actions CAN make real changes to \
                 this host (killing processes, restarting services, writing cgroup \
                 limits). Individual requests can still opt into dry_run for a \
                 specific action, but none can force real execution if this were \
                 set back to true."
            );
        }

        Self { server_forced }
    }

    /// Combines the server ceiling with a per-request preference. The
    /// server can force dry-run on; a request can only ask for *more*
    /// caution than the server already requires, never less.
    pub fn effective(&self, requested_dry_run: Option<bool>) -> bool {
        self.server_forced || requested_dry_run.unwrap_or(false)
    }
}

// ---------------------------------------------------------------------
// Action record (matches the `actions` table)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ActionRecord {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub action_type: String,
    pub parameters: Value,
    pub grant_id: Option<Uuid>,
    pub status: String,
    pub requested_dry_run: Option<bool>,
    pub effective_dry_run: Option<bool>,
    pub outcome: Option<Value>,
    pub error: Option<String>,
    pub requires_human_approval: bool,
    pub approved_by: Option<String>,
    pub approved_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ---------------------------------------------------------------------
// Engine: propose / approve / execute
// ---------------------------------------------------------------------

/// Proposes an action: resolves the handler, derives its scope from the
/// parameters, and runs it through `permissions::check_permission_tx` —
/// the same single enforcement choke point Phase 2 built, called
/// directly in-process, never over HTTP (see CONTRIBUTING.md), and now
/// participating in this function's own transaction rather than opening
/// (and committing) one of its own. The permission check, the
/// `permission_checks`/`PermissionAllowed`/`PermissionDenied` logging it
/// does internally, the `actions` row insert, and this function's own
/// `ActionProposed`/`ActionProposalDenied` event all commit together —
/// or none of them do.
///
/// If permission is denied, the action is still recorded (status
/// `failed`, with the denial reason) rather than silently rejected — so
/// a denied proposal is just as auditable as an approved one.
///
/// The action's id is generated up front (rather than left to the
/// table's `DEFAULT gen_random_uuid()`) so the `ActionProposed`/
/// `ActionProposalDenied` event emitted at the end of this function can
/// reference it as `action_id` — the row and its founding event share an
/// id decided before either exists, rather than the event needing the
/// row to exist first to know what to point at.
pub async fn propose_action(
    pool: &PgPool,
    registry: &HandlerRegistry,
    agent_id: Uuid,
    action_type: &str,
    parameters: Value,
    requested_dry_run: Option<bool>,
) -> Result<ActionRecord, ProposeError> {
    let handler = registry
        .get(action_type)
        .ok_or_else(|| ProposeError::UnknownActionType(action_type.to_string()))?;

    let scope = handler
        .scope_of(&parameters)
        .map_err(ProposeError::Action)?;

    let mut tx = pool.begin().await.map_err(ProposeError::Database)?;

    let permission = check_permission_tx(&mut tx, agent_id, handler.required_capability(), &scope)
        .await
        .map_err(ProposeError::Database)?;

    let (status, grant_id, error): (&str, Option<Uuid>, Option<String>) = match &permission {
        PermissionResult::Allowed { grant_id } => ("proposed", Some(*grant_id), None),
        PermissionResult::Denied { reason } => (
            "failed",
            None,
            Some(format!("permission denied: {}", reason.as_str())),
        ),
    };

    // v0.1: every action type requires approval, no exceptions yet —
    // full autonomy is an explicit future opt-in per action type, not
    // the default (phase3-action-engine.md §2).
    let requires_approval = true;

    let action_id = Uuid::new_v4();

    // completed_at is computed by Postgres's own `now()`, not a
    // Rust-side `chrono::Utc::now()` read — the same clock source
    // `created_at`'s `DEFAULT now()` uses. Binding a client-computed
    // timestamp here would read the clock before this query is even
    // sent, and under enough app/DB latency or clock skew (a real
    // possibility once the app and database run on separate hosts),
    // that could land `completed_at` before `created_at` on a row
    // that's allegedly already complete. execute_action's two UPDATEs
    // already use SQL-side `now()` for the same reason — this makes
    // propose_action's denial path consistent with that.
    let row = sqlx::query_as::<_, ActionRecord>(
        "INSERT INTO actions \
         (id, agent_id, action_type, parameters, grant_id, status, error, \
          requires_human_approval, requested_dry_run, completed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                 CASE WHEN $6 = 'failed' THEN now() ELSE NULL END) \
         RETURNING *",
    )
    .bind(action_id)
    .bind(agent_id)
    .bind(action_type)
    .bind(&parameters)
    .bind(grant_id)
    .bind(status)
    .bind(&error)
    .bind(requires_approval)
    .bind(requested_dry_run)
    .fetch_one(&mut *tx)
    .await
    .map_err(ProposeError::Database)?;

    let proposal_event_type = match status {
        "proposed" => event_type::ACTION_PROPOSED,
        _ => event_type::ACTION_PROPOSAL_DENIED,
    };
    let event_payload = serde_json::json!({
        "action_type": action_type,
        "parameters": &parameters,
        "status": status,
        "error": &error,
    });

    emit_event(
        &mut tx,
        agent_id,
        Some(action_id),
        grant_id,
        proposal_event_type,
        event_payload,
    )
    .await
    .map_err(ProposeError::Database)?;

    tx.commit().await.map_err(ProposeError::Database)?;

    Ok(row)
}

/// Approves a proposed action. The `WHERE status = 'proposed'` condition
/// on the UPDATE is load-bearing, not decorative — it's what prevents
/// two concurrent approval requests from both succeeding (the same class
/// of race fixed in Phase 2's `revoke_grant`; see CONTRIBUTING.md). The
/// status update and its `ActionApproved` event commit together.
pub async fn approve_action(
    pool: &PgPool,
    action_id: Uuid,
    approved_by: &str,
) -> Result<ActionRecord, ApproveError> {
    let mut tx = pool.begin().await.map_err(ApproveError::Database)?;

    let row = sqlx::query_as::<_, ActionRecord>(
        "UPDATE actions SET status = 'approved', approved_by = $2, approved_at = now() \
         WHERE id = $1 AND status = 'proposed' \
         RETURNING *",
    )
    .bind(action_id)
    .bind(approved_by)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApproveError::Database)?;

    let row = match row {
        Some(r) => r,
        None => {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM actions WHERE id = $1)")
                    .bind(action_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(ApproveError::Database)?;

            // Nothing to commit on this path — explicit rollback rather
            // than relying on drop behavior, matching this codebase's
            // preference for stating intent over relying on an implicit
            // default (see similar explicit-over-implicit choices in
            // Phase 2's revoke_grant).
            tx.rollback().await.map_err(ApproveError::Database)?;

            return if exists {
                Err(ApproveError::WrongStatus)
            } else {
                Err(ApproveError::NotFound)
            };
        }
    };

    emit_event(
        &mut tx,
        row.agent_id,
        Some(action_id),
        row.grant_id,
        event_type::ACTION_APPROVED,
        serde_json::json!({ "approved_by": approved_by }),
    )
    .await
    .map_err(ApproveError::Database)?;

    tx.commit().await.map_err(ApproveError::Database)?;

    Ok(row)
}

/// Executes an approved action. The dry-run/real split lives entirely
/// inside the handler's own `execute()` (see the trait doc comment) —
/// this function's only job regarding the toggle is to compute the
/// effective mode once (server ceiling + request preference) and pass it
/// through, then record which mode actually ran.
///
/// Honesty note carried over from phase3-action-engine.md §5: wrapping
/// this in a transaction only guarantees the *database* row update is
/// atomic. It cannot undo a real-world side effect a handler already
/// caused (a signal already sent, a file already written) if something
/// fails partway through — true atomicity for those depends on each
/// handler's own verify-before-commit discipline, not on this function.
/// v0.1 keeps the failure path simple and honest: a handler error marks
/// the action `failed` with that error recorded; it does not attempt
/// automatic compensation, since compensation is only meaningful for a
/// handler that reports a *partial* success before failing, and no
/// handler here does that yet. The `compensate` trait method and the
/// `rolled_back`/`compensated` statuses exist for that future case
/// rather than being exercised by this version.
///
/// Transaction shape here is deliberately *different* from
/// `propose_action`/`approve_action`, and this is worth being explicit
/// about: those two are pure, fast DB operations end to end, so wrapping
/// each entirely in one transaction is free. `handler.execute()` is not
/// — `process.stop`'s real path alone can sleep for a 3-second signal
/// grace period, and `service.restart`'s can block on `systemctl` for an
/// unbounded amount of time. Holding a Postgres transaction (and the
/// connection-pool slot and any row locks that implies) open across that
/// would be a real operational problem, not just a style preference. So
/// this function uses two short, separate transactions bracketing the
/// untransacted real work in between: one to mark `executing`, the real
/// call itself with no open transaction at all, then one to record the
/// terminal status *and* emit its event together. The thing that must
/// never be inconsistent — a terminal status without its event, or vice
/// versa — is still guaranteed atomic; the "mark executing" step and the
/// real call itself were never going to have a meaningful atomicity
/// guarantee regardless of how this were written.
pub async fn execute_action(
    pool: &PgPool,
    registry: &HandlerRegistry,
    dry_run_config: &DryRunConfig,
    action_id: Uuid,
) -> Result<ActionRecord, ExecuteError> {
    let action: ActionRecord = sqlx::query_as("SELECT * FROM actions WHERE id = $1")
        .bind(action_id)
        .fetch_optional(pool)
        .await
        .map_err(ExecuteError::Database)?
        .ok_or(ExecuteError::NotFound)?;

    if action.status != "approved" {
        return Err(ExecuteError::NotApproved(action.status.clone()));
    }

    let handler = registry
        .get(&action.action_type)
        .ok_or_else(|| ExecuteError::UnknownActionType(action.action_type.clone()))?;

    let effective_dry_run = dry_run_config.effective(action.requested_dry_run);

    // Short transaction #1: mark executing. No event for this transient
    // state — the event catalog (events.rs) only covers terminal
    // outcomes (ActionExecuted/ActionFailed), not the in-flight state.
    //
    // KNOWN GAP, flagged rather than silently left: if this process
    // crashes (or is killed) between this transaction committing and
    // the terminal-state transaction below, an action can be left
    // permanently stuck in 'executing' with no terminal event ever
    // recorded — nothing currently detects or recovers from that. This
    // is the single largest window of real risk in the whole engine,
    // specifically because `handler.execute()` can run for several
    // seconds (process.stop's grace period, an unbounded systemctl
    // call). A query like `SELECT * FROM actions WHERE status =
    // 'executing' AND created_at < now() - interval '5 minutes'` would
    // find a stuck action today, but there's no automated sweep for it
    // yet, and no defined recovery semantics (retry? mark failed? needs
    // a human decision first, since the real-world effect may or may
    // not have actually happened). Worth solving properly rather than
    // patching quickly — not done in this version.
    sqlx::query("UPDATE actions SET status = 'executing', effective_dry_run = $2 WHERE id = $1")
        .bind(action_id)
        .bind(effective_dry_run)
        .execute(pool)
        .await
        .map_err(ExecuteError::Database)?;

    // The real (or simulated) work. Deliberately no open transaction
    // here — see the function doc comment above.
    let exec_result = handler.execute(&action.parameters, effective_dry_run).await;

    // Short transaction #2: record the terminal status and emit its
    // event together, atomically.
    let mut tx = pool.begin().await.map_err(ExecuteError::Database)?;

    let (updated, event_type_for_outcome, event_payload) = match exec_result {
        Ok(outcome) => {
            let row = sqlx::query_as::<_, ActionRecord>(
                "UPDATE actions SET status = 'executed', outcome = $2, completed_at = now() \
                 WHERE id = $1 RETURNING *",
            )
            .bind(action_id)
            .bind(&outcome.details)
            .fetch_one(&mut *tx)
            .await
            .map_err(ExecuteError::Database)?;
            (row, event_type::ACTION_EXECUTED, outcome.details)
        }
        Err(action_err) => {
            let error_message = action_err.to_string();
            let row = sqlx::query_as::<_, ActionRecord>(
                "UPDATE actions SET status = 'failed', error = $2, completed_at = now() \
                 WHERE id = $1 RETURNING *",
            )
            .bind(action_id)
            .bind(&error_message)
            .fetch_one(&mut *tx)
            .await
            .map_err(ExecuteError::Database)?;
            (
                row,
                event_type::ACTION_FAILED,
                serde_json::json!({ "error": error_message }),
            )
        }
    };

    emit_event(
        &mut tx,
        updated.agent_id,
        Some(action_id),
        updated.grant_id,
        event_type_for_outcome,
        event_payload,
    )
    .await
    .map_err(ExecuteError::Database)?;

    tx.commit().await.map_err(ExecuteError::Database)?;

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_server_ceiling_wins_regardless_of_request() {
        let server_forced = DryRunConfig {
            server_forced: true,
        };
        assert!(server_forced.effective(Some(false)));
        assert!(server_forced.effective(None));
        assert!(server_forced.effective(Some(true)));
    }

    #[test]
    fn dry_run_request_can_ask_for_more_caution_when_server_allows_real() {
        let server_allows_real = DryRunConfig {
            server_forced: false,
        };
        assert!(server_allows_real.effective(Some(true)));
    }

    #[test]
    fn dry_run_defaults_to_real_when_server_allows_and_no_preference_stated() {
        let server_allows_real = DryRunConfig {
            server_forced: false,
        };
        assert!(!server_allows_real.effective(None));
        assert!(!server_allows_real.effective(Some(false)));
    }

    #[test]
    fn process_stop_scope_is_pid_prefixed() {
        let handler = ProcessStopHandler;
        let scope = handler
            .scope_of(&serde_json::json!({ "pid": 1234 }))
            .unwrap();
        assert_eq!(scope, "pid:1234");
    }

    #[test]
    fn process_stop_scope_rejects_missing_pid() {
        let handler = ProcessStopHandler;
        assert!(handler.scope_of(&serde_json::json!({})).is_err());
    }

    #[test]
    fn process_stop_scope_rejects_non_integer_pid() {
        let handler = ProcessStopHandler;
        assert!(handler
            .scope_of(&serde_json::json!({ "pid": "not-a-number" }))
            .is_err());
    }

    #[test]
    fn process_start_scope_is_the_command_itself() {
        let handler = ProcessStartHandler;
        let scope = handler
            .scope_of(&serde_json::json!({ "command": "/usr/bin/worker --flag" }))
            .unwrap();
        assert_eq!(scope, "/usr/bin/worker --flag");
    }

    #[test]
    fn cgroup_set_limit_scope_is_the_cgroup_path() {
        let handler = CgroupSetLimitHandler;
        let scope = handler
            .scope_of(&serde_json::json!({ "cgroup_path": "/sys/fs/cgroup/demo" }))
            .unwrap();
        assert_eq!(scope, "/sys/fs/cgroup/demo");
    }

    #[test]
    fn service_restart_scope_is_the_service_name() {
        let handler = ServiceRestartHandler;
        let scope = handler
            .scope_of(&serde_json::json!({ "service_name": "nginx" }))
            .unwrap();
        assert_eq!(scope, "nginx");
    }

    #[test]
    fn reversibility_flags_match_the_doc() {
        assert!(!ProcessStopHandler.is_reversible());
        assert!(ProcessStartHandler.is_reversible());
        assert!(CgroupSetLimitHandler.is_reversible());
        assert!(!ServiceRestartHandler.is_reversible());
    }

    #[test]
    fn registry_exposes_exactly_the_four_seeded_capabilities() {
        let registry = HandlerRegistry::default_handlers();
        let mut types = registry.action_types();
        types.sort_unstable();
        assert_eq!(
            types,
            vec![
                "cgroup.set_limit",
                "process.start",
                "process.stop",
                "service.restart"
            ]
        );
    }

    #[tokio::test]
    async fn dry_run_execute_never_touches_the_real_path_for_process_stop() {
        // A pid that definitely does not exist — if this accidentally
        // took the real path, nix::sys::signal::kill would return an
        // error (ESRCH) and this test would fail. The dry-run path
        // short-circuits before any real syscall, so this succeeding is
        // itself evidence the toggle actually gates the real call.
        let handler = ProcessStopHandler;
        let result = handler
            .execute(&serde_json::json!({ "pid": 999_999_999 }), true)
            .await
            .unwrap();
        assert_eq!(result.details["dry_run"], true);
    }
}
