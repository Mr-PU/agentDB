use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await?;
    Ok(pool)
}

pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// Re-grants SELECT on every current table in `public` to `agentdb_reader`.
/// Run this after every `run_migrations()` call, on every startup — not
/// just once at install time. It's cheap and fully idempotent, and it's
/// what keeps the reader role's access correct as new tables get added by
/// Phase 2+ migrations, without depending on which role authored them
/// (see the note in migrations/0002_readonly_role.sql for why
/// `ALTER DEFAULT PRIVILEGES` alone isn't sufficient here).
///
/// Silently no-ops if the `agentdb_reader` role doesn't exist yet (e.g. a
/// fresh install where migration 0002 hasn't run) rather than failing
/// startup — logs a warning instead, matching the fallback pattern used
/// by `connect_readonly`.
pub async fn ensure_reader_grants(pool: &PgPool) -> anyhow::Result<()> {
    let role_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'agentdb_reader')",
    )
    .fetch_one(pool)
    .await?;

    if !role_exists {
        tracing::warn!(
            "agentdb_reader role does not exist yet — skipping grant refresh. \
             This is expected before migration 0002 has run."
        );
        return Ok(());
    }

    sqlx::query("GRANT USAGE ON SCHEMA public TO agentdb_reader")
        .execute(pool)
        .await?;
    sqlx::query("GRANT SELECT ON ALL TABLES IN SCHEMA public TO agentdb_reader")
        .execute(pool)
        .await?;

    tracing::info!("refreshed agentdb_reader SELECT grants on all current tables");
    Ok(())
}

/// Separate pool for the /query endpoint, intended to run as the
/// `agentdb_reader` role created in migrations/0002_readonly_role.sql
/// (SELECT-only grants). Kept as its own pool — not reused from the main
/// migration-owning connection — so a bug that accidentally shares the
/// main pool with /query can't silently regain write access.
///
/// Falls back to the main DATABASE_URL with a loud warning if
/// DATABASE_URL_READONLY isn't set, so local development still works, but
/// production deployments are expected to set it.
pub async fn connect_readonly(fallback_url: &str) -> anyhow::Result<PgPool> {
    let readonly_url = match std::env::var("DATABASE_URL_READONLY") {
        Ok(url) => url,
        Err(_) => {
            tracing::warn!(
                "DATABASE_URL_READONLY not set; /query is using the main \
                 (potentially write-capable) database connection. Set \
                 DATABASE_URL_READONLY to a connection string using the \
                 agentdb_reader role before exposing this endpoint beyond \
                 local development."
            );
            fallback_url.to_string()
        }
    };

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&readonly_url)
        .await?;
    Ok(pool)
}
