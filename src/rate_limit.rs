use axum::{
    extract::{ConnectInfo, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::json;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct Window {
    count: u32,
    resets_at: Instant,
}

/// Simple in-memory, fixed-window rate limiter, keyed by API key when
/// present (so each caller has their own budget) and falling back to
/// remote IP otherwise.
///
/// This is deliberately not a distributed limiter — state lives in this
/// process's memory only. That's the right tradeoff for a single-instance
/// MVP; it is NOT sufficient once this service runs as multiple replicas
/// behind a load balancer, since each replica would enforce its own
/// separate limit. Flagged here rather than silently assumed away: if
/// this runs behind a load balancer with more than one instance, replace
/// this with a shared store (e.g. Redis-backed) before relying on the
/// limit for anything beyond basic abuse deterrence.
#[derive(Clone)]
pub struct RateLimiter {
    windows: Arc<Mutex<HashMap<String, Window>>>,
    limit: u32,
    window_duration: Duration,
}

impl RateLimiter {
    /// Reads `RATE_LIMIT_PER_MINUTE` (default 300) and spawns a background
    /// cleanup task so the key map doesn't grow unbounded from one-off
    /// callers/IPs that are never seen again. Must be called from within a
    /// running Tokio runtime (it spawns a task).
    pub fn from_env() -> Self {
        let limit = std::env::var("RATE_LIMIT_PER_MINUTE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(300);

        let limiter = Self {
            windows: Arc::new(Mutex::new(HashMap::new())),
            limit,
            window_duration: Duration::from_secs(60),
        };

        tracing::info!(
            limit,
            "rate limiting enabled ({} requests/minute per key)",
            limit
        );

        let cleanup_map = limiter.windows.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                let now = Instant::now();
                let mut map = cleanup_map.lock().unwrap_or_else(|e| e.into_inner());
                map.retain(|_, w| w.resets_at + Duration::from_secs(300) > now);
            }
        });

        limiter
    }

    /// Returns `true` if the request under `key` is within its window's
    /// budget (and records it), `false` if the caller should be rejected.
    fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());

        let window = map.entry(key.to_string()).or_insert_with(|| Window {
            count: 0,
            resets_at: now + self.window_duration,
        });

        if now >= window.resets_at {
            window.count = 0;
            window.resets_at = now + self.window_duration;
        }

        window.count += 1;
        window.count <= self.limit
    }
}

/// Applied as a layer ahead of `auth::require_api_key` (see `main.rs`) so
/// that even repeated requests with an *invalid* API key are rate
/// limited by source IP — otherwise this middleware would do nothing to
/// slow down someone brute-forcing the key.
pub async fn rate_limit(
    Extension(limiter): Extension<RateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    let key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| format!("key:{s}"))
        .or_else(|| {
            req.extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| format!("ip:{}", ci.0.ip()))
        })
        .unwrap_or_else(|| "unknown".to_string());

    if limiter.allow(&key) {
        next.run(req).await
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "rate limit exceeded, try again shortly" })),
        )
            .into_response()
    }
}
