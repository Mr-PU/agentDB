use crate::models::{OsqueryProcessRow, OsqueryServiceRow, OsquerySocketRow};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const PROCESSES_QUERY: &str = "SELECT pid, name, CAST(uid AS TEXT) AS username, state, user_time, system_time, resident_size, start_time FROM processes;";
const SOCKETS_QUERY: &str = "SELECT pid, local_port, remote_address, remote_port, protocol, state FROM process_open_sockets WHERE remote_address != '';";
const SERVICES_QUERY: &str = "SELECT id AS name, COALESCE(active_state, sub_state, load_state) AS status FROM systemd_units WHERE id LIKE '%.service' OR fragment_path LIKE '%.service';";

use std::sync::OnceLock;

/// Linux's USER_HZ / clock ticks-per-second, used to convert osquery's raw
/// `user_time`/`system_time` (in clock ticks) into seconds of CPU time.
///
/// Read via `sysconf(_SC_CLK_TCK)` once at first use and cached — this is
/// the correct way to get this value (it can, in principle, differ from
/// the near-universal default of 100 depending on kernel configuration),
/// rather than assuming the default and being wrong on the rare system
/// where it isn't. Falls back to 100.0 with a warning if `sysconf` can't
/// determine it or on a non-Unix build.
static CLK_TCK_CACHE: OnceLock<f64> = OnceLock::new();

fn clk_tck() -> f64 {
    *CLK_TCK_CACHE.get_or_init(|| {
        #[cfg(unix)]
        {
            // SAFETY: sysconf with a valid `_SC_CLK_TCK` argument reads a
            // system parameter and has no memory-safety implications;
            // it does not accept or dereference any pointers here.
            let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if ticks > 0 {
                return ticks as f64;
            }
        }
        warn!(
            "could not determine CLK_TCK via sysconf; falling back to \
             100.0, the near-universal default on Linux. CPU percentages \
             will be wrong on a system where this doesn't hold."
        );
        100.0
    })
}

/// Runs `osqueryi --json "<query>"` and deserializes the result.
/// Isolated here so the rest of the ingest logic doesn't care how the
/// data arrived (pull today, push/differential later per Phase 1 §3.2).
async fn run_osquery<T: serde::de::DeserializeOwned>(query: &str) -> anyhow::Result<Vec<T>> {
    let output = Command::new("osqueryi")
        .arg("--json")
        .arg(query)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "osqueryi exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let rows: Vec<T> = serde_json::from_slice(&output.stdout)?;
    Ok(rows)
}

fn parse_i64(s: &Option<String>) -> Option<i64> {
    s.as_ref().and_then(|v| v.parse::<i64>().ok())
}

fn parse_i32(s: &Option<String>) -> Option<i32> {
    s.as_ref().and_then(|v| v.parse::<i32>().ok())
}

/// What the ingest needs to know about a process's previous sample in
/// order to compute a CPU-rate delta on this poll.
#[derive(Clone)]
struct PrevSample {
    cpu_ticks_total: Option<i64>,
    started_at: Option<DateTime<Utc>>,
    last_seen_at: DateTime<Utc>,
}

/// What to write for a process row on this poll, as decided by
/// `compute_process_sample` below.
#[derive(Debug, PartialEq)]
struct ProcessSampleUpdate {
    cpu_percent: Option<f64>,
    prev_cpu_ticks_total: Option<i64>,
    prev_sampled_at: Option<DateTime<Utc>>,
}

/// Pure decision logic for one process's sample on one poll: whether this
/// is the same process identity as the previous sample (vs. a PID reused
/// by a new process), and if so, what CPU-rate percentage that implies.
///
/// Deliberately has no I/O — everything it needs is passed in — so it can
/// be unit tested directly against the DB-free cases that matter most:
/// first observation, a normal continuing sample, PID reuse, a backwards
/// counter, and a zero-elapsed-time sample. `poll_processes` is the only
/// caller; it supplies the DB-read `prev` and writes the result back.
fn compute_process_sample(
    new_ticks_total: i64,
    new_started_at: Option<DateTime<Utc>>,
    prev: Option<&PrevSample>,
    now: DateTime<Utc>,
    clk_tck: f64,
) -> ProcessSampleUpdate {
    // A PID is treated as a fresh process identity (not the same process
    // continuing) if we have no prior sample, or if the observed start
    // time doesn't match what we had stored — the latter is the
    // PID-reuse case.
    let is_same_identity = match prev {
        Some(p) => match (p.started_at, new_started_at) {
            (Some(old), Some(new)) => old == new,
            // If either side is missing a start time, don't assume
            // continuity — better to under-report a CPU rate than
            // silently diff across a PID reuse.
            _ => false,
        },
        None => false,
    };

    if !is_same_identity {
        return ProcessSampleUpdate {
            cpu_percent: None, // first observation of this process identity
            prev_cpu_ticks_total: None,
            prev_sampled_at: None,
        };
    }

    let p = prev.expect("is_same_identity is only true when prev is Some");

    let cpu_percent = match p.cpu_ticks_total {
        Some(prev_ticks) if new_ticks_total >= prev_ticks => {
            let elapsed_secs = (now - p.last_seen_at).num_milliseconds() as f64 / 1000.0;
            if elapsed_secs > 0.0 {
                let delta_ticks = (new_ticks_total - prev_ticks) as f64;
                Some(((delta_ticks / clk_tck) / elapsed_secs) * 100.0)
            } else {
                None // two samples in the same instant — nothing to divide by
            }
        }
        // Counter went backwards (shouldn't happen for a continuing
        // identity, but guard rather than produce a negative percentage)
        // or no prior ticks recorded — treat as first sample.
        _ => None,
    };

    ProcessSampleUpdate {
        cpu_percent,
        prev_cpu_ticks_total: p.cpu_ticks_total,
        prev_sampled_at: Some(p.last_seen_at),
    }
}

/// One full poll cycle for processes: query osquery, normalize, and
/// upsert inside a single transaction (Phase 1 §4.3 — a partial poll
/// failure must not leave half-updated state visible to readers).
///
/// This also fixes two bugs present in the initial scaffold:
///
/// - `cpu_percent` is now a real rate (ticks delta / elapsed time),
///   computed by reading each pid's previous sample before upserting,
///   not a raw ever-growing cumulative counter.
/// - PID reuse is detected by comparing the newly observed `started_at`
///   against the stored one: if they differ for the same pid, this is
///   treated as a *new* process identity — `started_at` is overwritten
///   (previously it silently never was) and the CPU baseline is reset
///   rather than diffed against the old process's ticks.
pub async fn poll_processes(pool: &PgPool) -> anyhow::Result<usize> {
    let poll_started_at = Utc::now();
    let poll_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "INSERT INTO ingest_polls (source, started_at) VALUES ('processes', $1) RETURNING id",
    )
    .bind(poll_started_at)
    .fetch_one(pool)
    .await?;

    let result = run_osquery::<OsqueryProcessRow>(PROCESSES_QUERY).await;

    match result {
        Ok(rows) => {
            let count = rows.len();

            // Read existing samples up front so the CPU-rate calculation
            // below has something to diff against, without a per-row
            // round trip.
            type ExistingProcessSample = (i32, Option<i64>, Option<DateTime<Utc>>, DateTime<Utc>);

            let existing: Vec<ExistingProcessSample> = sqlx::query_as(
                "SELECT pid, cpu_ticks_total, started_at, last_seen_at FROM processes",
            )
            .fetch_all(pool)
            .await?;

            let mut prev_by_pid: HashMap<i32, PrevSample> = HashMap::with_capacity(existing.len());
            for (pid, ticks, started_at, last_seen_at) in existing {
                prev_by_pid.insert(
                    pid,
                    PrevSample {
                        cpu_ticks_total: ticks,
                        started_at,
                        last_seen_at,
                    },
                );
            }

            let mut tx = pool.begin().await?;
            let now = Utc::now();

            for row in &rows {
                let pid: i32 = row.pid.parse().unwrap_or(-1);
                if pid < 0 {
                    continue;
                }

                let new_ticks_total = parse_i64(&row.user_time).unwrap_or(0)
                    + parse_i64(&row.system_time).unwrap_or(0);
                let memory_bytes = parse_i64(&row.resident_size);
                let start_epoch = parse_i64(&row.start_time);
                let new_started_at = start_epoch.and_then(|secs| DateTime::from_timestamp(secs, 0));

                let prev = prev_by_pid.get(&pid);
                let update =
                    compute_process_sample(new_ticks_total, new_started_at, prev, now, clk_tck());

                let cpu_percent = update.cpu_percent;
                let prev_ticks_to_store = update.prev_cpu_ticks_total;
                let prev_sampled_at_to_store = update.prev_sampled_at;

                sqlx::query(
                    r#"
                    INSERT INTO processes (
                        pid, name, owner, status, cpu_percent, cpu_ticks_total,
                        prev_cpu_ticks_total, prev_sampled_at, memory_bytes,
                        started_at, last_seen_at
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
                    ON CONFLICT (pid) DO UPDATE SET
                        name = EXCLUDED.name,
                        owner = EXCLUDED.owner,
                        status = EXCLUDED.status,
                        cpu_percent = EXCLUDED.cpu_percent,
                        cpu_ticks_total = EXCLUDED.cpu_ticks_total,
                        prev_cpu_ticks_total = EXCLUDED.prev_cpu_ticks_total,
                        prev_sampled_at = EXCLUDED.prev_sampled_at,
                        memory_bytes = EXCLUDED.memory_bytes,
                        started_at = EXCLUDED.started_at,
                        last_seen_at = now()
                    "#,
                )
                .bind(pid)
                .bind(&row.name)
                .bind(&row.username)
                .bind(&row.state)
                .bind(cpu_percent)
                .bind(new_ticks_total)
                .bind(prev_ticks_to_store)
                .bind(prev_sampled_at_to_store)
                .bind(memory_bytes)
                .bind(new_started_at)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;

            sqlx::query(
                "UPDATE ingest_polls SET finished_at = now(), row_count = $1 WHERE id = $2",
            )
            .bind(count as i32)
            .bind(poll_id)
            .execute(pool)
            .await?;

            info!(count, "processes poll complete");
            Ok(count)
        }
        Err(e) => {
            error!(error = %e, "processes poll failed");
            sqlx::query("UPDATE ingest_polls SET finished_at = now(), error = $1 WHERE id = $2")
                .bind(e.to_string())
                .bind(poll_id)
                .execute(pool)
                .await
                .ok();
            Err(e)
        }
    }
}

/// Network connection poll: fixed to upsert against a real identity key
/// (see migrations/0004_network_connection_identity.sql) instead of
/// blindly inserting a new row every cycle, which previously turned this
/// table into a growing log of observations rather than current state.
pub async fn poll_network(pool: &PgPool) -> anyhow::Result<usize> {
    let started_at = Utc::now();
    let poll_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "INSERT INTO ingest_polls (source, started_at) VALUES ('network', $1) RETURNING id",
    )
    .bind(started_at)
    .fetch_one(pool)
    .await?;

    match run_osquery::<OsquerySocketRow>(SOCKETS_QUERY).await {
        Ok(rows) => {
            let mut tx = pool.begin().await?;
            let count = rows.len();

            for row in &rows {
                let pid = row.pid.parse::<i32>().ok();
                sqlx::query(
                    r#"
                    INSERT INTO network_connections (process_pid, local_port, remote_addr, remote_port, protocol, state, last_seen_at)
                    VALUES ($1, $2, $3, $4, $5, $6, now())
                    ON CONFLICT (
                        COALESCE(process_pid, -1),
                        COALESCE(local_port, -1),
                        COALESCE(remote_addr, ''),
                        COALESCE(remote_port, -1),
                        COALESCE(protocol, '')
                    )
                    DO UPDATE SET
                        state = EXCLUDED.state,
                        last_seen_at = now()
                    "#,
                )
                .bind(pid)
                .bind(parse_i32(&row.local_port))
                .bind(&row.remote_address)
                .bind(parse_i32(&row.remote_port))
                .bind(&row.protocol)
                .bind(&row.state)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;

            sqlx::query(
                "UPDATE ingest_polls SET finished_at = now(), row_count = $1 WHERE id = $2",
            )
            .bind(count as i32)
            .bind(poll_id)
            .execute(pool)
            .await?;

            info!(count, "network poll complete");
            Ok(count)
        }
        Err(e) => {
            error!(error = %e, "network poll failed");
            sqlx::query("UPDATE ingest_polls SET finished_at = now(), error = $1 WHERE id = $2")
                .bind(e.to_string())
                .bind(poll_id)
                .execute(pool)
                .await
                .ok();
            Err(e)
        }
    }
}

pub async fn poll_services(pool: &PgPool) -> anyhow::Result<usize> {
    let started_at = Utc::now();
    let poll_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "INSERT INTO ingest_polls (source, started_at) VALUES ('services', $1) RETURNING id",
    )
    .bind(started_at)
    .fetch_one(pool)
    .await?;

    match run_osquery::<OsqueryServiceRow>(SERVICES_QUERY).await {
        Ok(rows) => {
            let mut tx = pool.begin().await?;
            let count = rows.len();

            for row in &rows {
                sqlx::query(
                    r#"
                    INSERT INTO services (name, status, last_seen_at)
                    VALUES ($1, $2, now())
                    ON CONFLICT (name) DO UPDATE SET
                        status = EXCLUDED.status,
                        last_seen_at = now()
                    "#,
                )
                .bind(&row.name)
                .bind(&row.status)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;

            sqlx::query(
                "UPDATE ingest_polls SET finished_at = now(), row_count = $1 WHERE id = $2",
            )
            .bind(count as i32)
            .bind(poll_id)
            .execute(pool)
            .await?;

            info!(count, "services poll complete");
            Ok(count)
        }
        Err(e) => {
            error!(error = %e, "services poll failed");
            sqlx::query("UPDATE ingest_polls SET finished_at = now(), error = $1 WHERE id = $2")
                .bind(e.to_string())
                .bind(poll_id)
                .execute(pool)
                .await
                .ok();
            Err(e)
        }
    }
}

/// Stale-row sweep for processes: removes rows not seen in the last N
/// cycles rather than deleting immediately on a single missed poll.
pub async fn sweep_stale_processes(pool: &PgPool, stale_after: Duration) -> anyhow::Result<u64> {
    let cutoff = Utc::now() - chrono::Duration::from_std(stale_after)?;
    let result = sqlx::query("DELETE FROM processes WHERE last_seen_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        warn!(removed = result.rows_affected(), "swept stale process rows");
    }
    Ok(result.rows_affected())
}

/// Same idea for network connections — a connection that stops appearing
/// in osquery's results (closed, process exited) should eventually be
/// removed, but not the instant a single poll misses it.
pub async fn sweep_stale_network(pool: &PgPool, stale_after: Duration) -> anyhow::Result<u64> {
    let cutoff = Utc::now() - chrono::Duration::from_std(stale_after)?;
    let result = sqlx::query("DELETE FROM network_connections WHERE last_seen_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    if result.rows_affected() > 0 {
        warn!(removed = result.rows_affected(), "swept stale network rows");
    }
    Ok(result.rows_affected())
}

/// Starts the ingest loops (processes/network every 5s, services every
/// 30s, stale sweep every 60s — Phase 1 §3.1 defaults) as independently
/// cancellable background tasks, and returns their `JoinHandle`s so the
/// caller can wait for clean shutdown.
///
/// Each loop uses `tokio::select!` against the shared `CancellationToken`
/// rather than being killed with `.abort()` from outside: `.abort()` can
/// tear a task down mid-poll at an arbitrary await point, whereas this
/// approach only exits between ticks, after any in-flight poll has
/// already committed or rolled back on its own. In practice a poll
/// wrapped in a transaction is safe to abort either way (`sqlx`'s
/// `Transaction` rolls back on drop), but checking the token explicitly
/// keeps shutdown behavior predictable rather than relying on that as an
/// implementation detail.
pub fn spawn_ingest_loops(pool: PgPool, shutdown: CancellationToken) -> Vec<JoinHandle<()>> {
    let pool_fast = pool.clone();
    let shutdown_fast = shutdown.clone();
    let fast = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = poll_processes(&pool_fast).await {
                        error!(error = %e, "process poll cycle failed");
                    }
                    if let Err(e) = poll_network(&pool_fast).await {
                        error!(error = %e, "network poll cycle failed");
                    }
                }
                _ = shutdown_fast.cancelled() => {
                    info!("process/network ingest loop shutting down");
                    break;
                }
            }
        }
    });

    let pool_slow = pool.clone();
    let shutdown_slow = shutdown.clone();
    let slow = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = poll_services(&pool_slow).await {
                        error!(error = %e, "services poll cycle failed");
                    }
                }
                _ = shutdown_slow.cancelled() => {
                    info!("services ingest loop shutting down");
                    break;
                }
            }
        }
    });

    let pool_stale = pool.clone();
    let shutdown_stale = shutdown.clone();
    let stale = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = sweep_stale_processes(&pool_stale, Duration::from_secs(120)).await {
                        error!(error = %e, "stale process sweep failed");
                    }
                    if let Err(e) = sweep_stale_network(&pool_stale, Duration::from_secs(180)).await {
                        error!(error = %e, "stale network sweep failed");
                    }
                }
                _ = shutdown_stale.cancelled() => {
                    info!("stale-sweep loop shutting down");
                    break;
                }
            }
        }
    });

    vec![fast, slow, stale]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs_offset: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs_offset, 0).unwrap()
    }

    #[test]
    fn first_observation_has_no_cpu_percent() {
        let update = compute_process_sample(500, Some(ts(0)), None, ts(5), 100.0);
        assert_eq!(
            update,
            ProcessSampleUpdate {
                cpu_percent: None,
                prev_cpu_ticks_total: None,
                prev_sampled_at: None,
            }
        );
    }

    #[test]
    fn continuing_process_computes_a_real_rate() {
        // 100 ticks over 5 seconds at CLK_TCK=100 => 1 second of CPU time
        // over 5 seconds of wall time => 20% average.
        let prev = PrevSample {
            cpu_ticks_total: Some(1000),
            started_at: Some(ts(0)),
            last_seen_at: ts(0),
        };
        let update = compute_process_sample(1100, Some(ts(0)), Some(&prev), ts(5), 100.0);

        assert_eq!(update.prev_cpu_ticks_total, Some(1000));
        assert_eq!(update.prev_sampled_at, Some(ts(0)));
        let cpu = update.cpu_percent.expect("expected a computed cpu_percent");
        assert!((cpu - 20.0).abs() < 0.01, "expected ~20%, got {cpu}");
    }

    #[test]
    fn pid_reuse_resets_identity_and_cpu_baseline() {
        // Same pid, but the newly observed start time doesn't match the
        // stored one — this must be treated as a different process, not
        // a continuation, even though cpu_ticks_total looks plausible.
        let prev = PrevSample {
            cpu_ticks_total: Some(50_000), // old process had been running a while
            started_at: Some(ts(-10_000)), // old process's start time
            last_seen_at: ts(0),
        };
        // New process just started, ticks reset near zero.
        let update = compute_process_sample(50, Some(ts(1)), Some(&prev), ts(5), 100.0);

        assert_eq!(
            update,
            ProcessSampleUpdate {
                cpu_percent: None, // must not diff 50 - 50_000 as if continuous
                prev_cpu_ticks_total: None,
                prev_sampled_at: None,
            }
        );
    }

    #[test]
    fn missing_start_time_on_either_side_does_not_assume_continuity() {
        let prev = PrevSample {
            cpu_ticks_total: Some(1000),
            started_at: None, // unknown — osquery didn't report one
            last_seen_at: ts(0),
        };
        let update = compute_process_sample(1100, Some(ts(5)), Some(&prev), ts(10), 100.0);
        assert_eq!(update.cpu_percent, None);

        let prev2 = PrevSample {
            cpu_ticks_total: Some(1000),
            started_at: Some(ts(0)),
            last_seen_at: ts(0),
        };
        let update2 = compute_process_sample(1100, None, Some(&prev2), ts(10), 100.0);
        assert_eq!(update2.cpu_percent, None);
    }

    #[test]
    fn backwards_counter_does_not_produce_negative_percentage() {
        // Should not happen for a genuinely continuing process, but if
        // osquery ever reports a lower cumulative value, don't emit a
        // nonsensical negative rate.
        let prev = PrevSample {
            cpu_ticks_total: Some(5000),
            started_at: Some(ts(0)),
            last_seen_at: ts(0),
        };
        let update = compute_process_sample(100, Some(ts(0)), Some(&prev), ts(5), 100.0);
        assert_eq!(update.cpu_percent, None);
    }

    #[test]
    fn zero_elapsed_time_does_not_divide_by_zero() {
        let prev = PrevSample {
            cpu_ticks_total: Some(1000),
            started_at: Some(ts(0)),
            last_seen_at: ts(5),
        };
        // `now` equals `last_seen_at` — two samples at the same instant.
        let update = compute_process_sample(1100, Some(ts(0)), Some(&prev), ts(5), 100.0);
        assert_eq!(update.cpu_percent, None);
    }

    #[test]
    fn respects_the_provided_clk_tck_rather_than_assuming_100() {
        // 100 ticks over 2 seconds at CLK_TCK=50 => 2 seconds of CPU time
        // over 2 seconds of wall time => 100%.
        let prev = PrevSample {
            cpu_ticks_total: Some(0),
            started_at: Some(ts(0)),
            last_seen_at: ts(0),
        };
        let update = compute_process_sample(100, Some(ts(0)), Some(&prev), ts(2), 50.0);
        let cpu = update.cpu_percent.expect("expected a computed cpu_percent");
        assert!((cpu - 100.0).abs() < 0.01, "expected ~100%, got {cpu}");
    }
}
