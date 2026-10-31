use agentdb_runtime::{actions_api, api, auth, db, diagnosis, ingest, rate_limit};
use axum::{middleware, Extension};
use clap::{Parser, Subcommand};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "agentdb")]
#[command(
    about = "AgentDB Runtime — Phases 1-6: state mirror, permissions, actions, events, tool registry, diagnosis"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the HTTP API server and the background ingest loop together.
    Serve {
        #[arg(long, default_value = "8080")]
        port: u16,
    },
    /// Run a single ingest poll cycle and exit (useful for testing/cron).
    PollOnce,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://agentdb:agentdb@localhost:5432/agentdb".to_string());

    let pool = db::connect(&database_url).await?;
    db::run_migrations(&pool).await?;
    db::ensure_reader_grants(&pool).await?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { port } => {
            let shutdown = CancellationToken::new();
            let ingest_handles = ingest::spawn_ingest_loops(pool.clone(), shutdown.clone());

            let readonly_pool = db::connect_readonly(&database_url).await?;
            let api_key_config = auth::ApiKeyConfig::from_env();
            let limiter = rate_limit::RateLimiter::from_env();
            let actions_config = actions_api::ActionsConfig::from_env();
            let diagnosis_config = diagnosis::DiagnosisConfig::from_env();

            // Layer order matters for auth/rate_limit specifically: axum
            // applies the *last*-added `.layer()` outermost, so it runs
            // first on an incoming request. We want: rate-limiter
            // Extension inserted -> rate_limit middleware runs (so even
            // invalid-key brute-force attempts are throttled) -> api-key
            // Extension inserted -> auth middleware runs -> handler. That
            // means, in code, the layers closest to the handler are added
            // first, and the outermost (rate limiter) added last.
            //
            // ActionsConfig has no corresponding middleware — it's pure
            // data injection, read directly by handlers via
            // Extension<ActionsConfig> — so unlike the two above, its
            // position in this chain doesn't affect request handling.
            let app = api::router(pool, readonly_pool)
                .layer(middleware::from_fn(auth::require_api_key))
                .layer(Extension(api_key_config))
                .layer(middleware::from_fn(rate_limit::rate_limit))
                .layer(Extension(limiter))
                .layer(Extension(actions_config))
                .layer(Extension(diagnosis_config));

            let addr = format!("0.0.0.0:{port}");
            tracing::info!(%addr, "agentdb serving");
            let listener = tokio::net::TcpListener::bind(&addr).await?;

            // into_make_service_with_connect_info is what makes
            // ConnectInfo<SocketAddr> available to rate_limit's IP-based
            // fallback key.
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await?;

            tracing::info!("HTTP server stopped, shutting down ingest loops");
            shutdown.cancel();

            // Give in-flight polls a bounded window to finish and commit
            // cleanly rather than waiting forever or killing them outright.
            let join_all = futures_join_all(ingest_handles);
            if tokio::time::timeout(Duration::from_secs(10), join_all)
                .await
                .is_err()
            {
                tracing::warn!("ingest loops did not shut down within 10s; exiting anyway");
            }
        }
        Commands::PollOnce => {
            let procs = ingest::poll_processes(&pool).await?;
            let net = ingest::poll_network(&pool).await?;
            let svcs = ingest::poll_services(&pool).await?;
            println!("processes={procs} network={net} services={svcs}");
        }
    }

    Ok(())
}

/// Resolves once either Ctrl+C or SIGTERM is received, so `axum::serve`'s
/// graceful shutdown and the ingest-loop cancellation below both trigger
/// on a real shutdown signal rather than only on Ctrl+C — SIGTERM is what
/// most container orchestrators (Docker, Kubernetes) actually send.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C, starting graceful shutdown"),
        _ = terminate => tracing::info!("received SIGTERM, starting graceful shutdown"),
    }
}

/// Small local stand-in for `futures::future::join_all` so this file
/// doesn't need to pull in the whole `futures` crate for one call site.
async fn futures_join_all(handles: Vec<tokio::task::JoinHandle<()>>) {
    for handle in handles {
        if let Err(e) = handle.await {
            tracing::warn!(error = %e, "ingest task panicked during shutdown");
        }
    }
}
