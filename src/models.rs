use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct Process {
    pub id: Uuid,
    pub pid: i32,
    pub name: Option<String>,
    pub owner: Option<String>,
    pub status: Option<String>,
    /// CPU usage as a percentage of one core, computed from the delta in
    /// cpu_ticks_total between polls. NULL on first observation of a
    /// process identity (no prior sample to diff against yet) — this is
    /// meaningfully different from 0.0 (idle) and callers should treat it
    /// as "not yet known", not "no CPU usage".
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<i64>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct NetworkConnection {
    pub id: Uuid,
    pub process_pid: Option<i32>,
    pub local_port: Option<i32>,
    pub remote_addr: Option<String>,
    pub remote_port: Option<i32>,
    pub protocol: Option<String>,
    pub state: Option<String>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct Service {
    pub id: Uuid,
    pub name: String,
    pub status: Option<String>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ProcessQuery {
    pub name_filter: Option<String>,
    pub min_memory_bytes: Option<i64>,
    pub limit: Option<i64>,
    /// "memory" (default) or "cpu". Whitelisted explicitly in api.rs
    /// rather than interpolated directly into SQL — this field comes
    /// straight from user input.
    pub sort_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MirrorHealth {
    pub source: String,
    pub last_poll_started: Option<DateTime<Utc>>,
    pub last_poll_finished: Option<DateTime<Utc>>,
    pub last_row_count: Option<i32>,
    pub last_error: Option<String>,
}

/// Raw shape returned by `osqueryi --json "SELECT ... FROM processes"`.
/// osquery returns nearly everything as strings; this struct is the
/// deserialization target before normalization into `Process`.
#[derive(Debug, Deserialize)]
pub struct OsqueryProcessRow {
    pub pid: String,
    pub name: String,
    pub username: Option<String>,
    pub state: Option<String>,
    pub user_time: Option<String>,
    pub system_time: Option<String>,
    pub resident_size: Option<String>,
    pub start_time: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OsquerySocketRow {
    pub pid: String,
    pub local_port: Option<String>,
    pub remote_address: Option<String>,
    pub remote_port: Option<String>,
    pub protocol: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OsqueryServiceRow {
    pub name: String,
    pub status: Option<String>,
}
