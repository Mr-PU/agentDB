use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connects to the database named by `DATABASE_URL` and runs migrations
/// against it. Integration tests are expected to run against a
/// disposable database — CI spins up a fresh Postgres service container
/// per run (see `.github/workflows/ci.yml`); for local runs, point
/// `DATABASE_URL` at something you don't mind being truncated
/// repeatedly, never at a database with data you care about.
pub async fn test_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set to run integration tests — see CONTRIBUTING.md. \
         Point it at a disposable Postgres instance, e.g. via `make up`.",
    );

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("failed to connect to the integration test database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations against the integration test database");

    pool
}

/// Truncates every mirror/bookkeeping/permission table so each test
/// starts from a known-empty state, regardless of what a previous test
/// (or a prior manual run) left behind.
///
/// Deliberately does NOT truncate `capabilities` or `tools` — both are
/// seeded once by migration (0007 and 0012 respectively) and treated as
/// fixed reference data, not per-test state. Truncating either would
/// wipe seed rows that tests look up by name (e.g. `"process.stop"` as
/// a capability, `"query.processes"` as a tool), breaking every test
/// that grants a capability or calls a seeded tool. A row created by an
/// individual test via `POST /capabilities` or `POST /tools` can leak
/// into later tests within the same run; tests that care should use a
/// unique, test-specific name rather than relying on this function to
/// clean it up.
pub async fn reset_tables(pool: &PgPool) {
    let mut tx = pool
        .begin()
        .await
        .expect("failed to start reset transaction");

    sqlx::query("SELECT pg_advisory_xact_lock(20000001)")
        .execute(&mut *tx)
        .await
        .expect("failed to acquire reset lock");

    sqlx::query(
        "TRUNCATE processes, network_connections, services, resources, ingest_polls, \
         permission_checks, events, actions, grants, agents RESTART IDENTITY CASCADE",
    )
    .execute(&mut *tx)
    .await
    .expect("failed to truncate tables between tests");

    tx.commit()
        .await
        .expect("failed to commit reset transaction");
}

/// Whether `osqueryi` is available on `PATH`. Tests that need to run
/// against real osquery output check this first and skip (printing why)
/// rather than failing, so the suite still passes in environments where
/// osquery isn't installed — while still running for real wherever it
/// is, including in CI (see the "Install osquery" step in ci.yml).
#[allow(dead_code)]
pub fn osquery_available() -> bool {
    std::process::Command::new("osqueryi")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
