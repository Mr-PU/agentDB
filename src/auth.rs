use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::json;
use std::sync::Arc;

const API_KEY_HEADER: &str = "x-api-key";

/// Holds the configured API key (if any), injected into the router via
/// `Extension` so the middleware function below can read it without
/// needing its own separate `State` type layered alongside `ApiState`.
#[derive(Clone)]
pub struct ApiKeyConfig(pub Arc<Option<String>>);

impl ApiKeyConfig {
    pub fn from_env() -> Self {
        let key = std::env::var("API_KEY").ok();
        if key.is_none() {
            tracing::warn!(
                "API_KEY not set — the HTTP API is unauthenticated. This is \
                 fine for local development against localhost, but set \
                 API_KEY before exposing this service on any shared or \
                 reachable network, since the mirror endpoints expose live \
                 process/network/service data."
            );
        }
        Self(Arc::new(key))
    }
}

/// Rejects any request missing a matching `X-API-Key` header, unless no
/// key is configured (see `ApiKeyConfig::from_env`'s warning). Applied as
/// a layer over the whole router in `main.rs`, so every route — including
/// `/healthz` — requires it once `API_KEY` is set. If an unauthenticated
/// liveness check is needed behind a load balancer, put the balancer's
/// health check on the same trusted network as the app rather than
/// carving out an unauthenticated route for it.
pub async fn require_api_key(
    Extension(config): Extension<ApiKeyConfig>,
    req: Request,
    next: Next,
) -> Response {
    match config.0.as_ref() {
        None => next.run(req).await,
        Some(expected_key) => {
            let provided = req
                .headers()
                .get(API_KEY_HEADER)
                .and_then(|v| v.to_str().ok());

            if provided == Some(expected_key.as_str()) {
                next.run(req).await
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({
                        "error": format!("missing or invalid {API_KEY_HEADER} header")
                    })),
                )
                    .into_response()
            }
        }
    }
}
