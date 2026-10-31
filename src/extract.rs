//! Drop-in replacements for `axum::extract::Json` and `axum::extract::Query`
//! that convert extraction failures into this project's `{"error": "..."}`
//! envelope (`api::api_error`) instead of Axum's default plain-text
//! rejection body.
//!
//! This exists because Phase 1 had no JSON-body endpoints at all — every
//! Phase 1 query parameter was `Option<T>`, so a missing/malformed value
//! just meant "use the default" rather than ever reaching Axum's
//! rejection path. Phase 2 adds the project's first POST/JSON endpoints
//! and the first *required* query parameters (`agent_id` on `/grants`
//! and `/permission-checks`), both of which can genuinely fail to
//! extract — and without this wrapper, that failure would be the one
//! place in the API that doesn't look like the rest of it.

use async_trait::async_trait;
use axum::{
    extract::{FromRequest, FromRequestParts, Query, Request},
    http::request::Parts,
    response::{IntoResponse, Response},
    Json,
};
use serde::de::DeserializeOwned;

use crate::api::api_error;

/// Use exactly like `axum::extract::Json<T>` — same `Deserialize`
/// requirement, same behavior on success. Differs only in what the
/// response looks like when the body is missing, malformed, or has the
/// wrong content-type.
pub struct ValidatedJson<T>(pub T);

#[async_trait]
impl<T, S> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ValidatedJson(value)),
            Err(rejection) => {
                // Capture the message via Display before `into_response`
                // consumes the rejection; derive the status from its own
                // IntoResponse impl (guaranteed to exist for any
                // rejection type) rather than assuming a `.status()`
                // convenience method is present on this specific type
                // across every axum 0.7.x patch version.
                let message = rejection.to_string();
                let status = rejection.into_response().status();
                Err(api_error(status, message))
            }
        }
    }
}

/// Use exactly like `axum::extract::Query<T>`. Differs only in what the
/// response looks like when a required field is missing or a value
/// can't be parsed into its target type (e.g. `agent_id=not-a-uuid`).
pub struct ValidatedQuery<T>(pub T);

#[async_trait]
impl<T, S> FromRequestParts<S> for ValidatedQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(ValidatedQuery(value)),
            Err(rejection) => {
                let message = rejection.to_string();
                let status = rejection.into_response().status();
                Err(api_error(status, message))
            }
        }
    }
}
