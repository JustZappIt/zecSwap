use std::sync::Arc;

use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::{StatusCode, header, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::service::{ErrorCode, ErrorResponse};

/// A monitoring route's bearer token. Every request is refused while none is set.
#[derive(Default)]
pub struct MonitorToken(Option<blake2b_simd::Hash>);

impl MonitorToken {
    pub fn new(token: Option<&str>) -> Self {
        Self(token.map(|value| blake2b_simd::blake2b(value.as_bytes())))
    }

    /// The token in `variable`, at least 32 characters; none if it is unset.
    pub fn from_env(variable: &str) -> anyhow::Result<Self> {
        let token = match std::env::var(variable) {
            Ok(value) => Some(zeroize::Zeroizing::new(value)),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(token) = &token {
            anyhow::ensure!(
                token.len() >= 32,
                "{variable} must be at least 32 characters"
            );
        }
        Ok(Self::new(token.as_ref().map(|value| value.as_str())))
    }

    pub fn authorized(&self, header: Option<&str>) -> bool {
        let Some(expected) = self.0 else {
            return false;
        };
        let Some(value) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
            return false;
        };
        // Hash equality is constant time; both hashes have the same fixed length.
        blake2b_simd::blake2b(value.as_bytes()) == expected
    }
}

/// Middleware for monitoring routes: a request without the bearer token gets `401`.
pub async fn require_monitor(
    State(token): State<Arc<MonitorToken>>,
    request: Request,
    next: Next,
) -> Response {
    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !token.authorized(header) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(request).await
}

pub fn error(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(ErrorResponse {
            code,
            error: message.into(),
        }),
    )
        .into_response()
}

pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(request, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|e| error(e.status(), ErrorCode::InvalidRequest, e.body_text()))
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

pub struct Path<T>(pub T);

impl<T, S> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Self(value))
            .map_err(|e| error(e.status(), ErrorCode::InvalidRequest, e.body_text()))
    }
}

pub async fn not_found() -> Response {
    error(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "route not found",
    )
}

pub async fn method_not_allowed() -> Response {
    error(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorCode::MethodNotAllowed,
        "method not allowed",
    )
}

pub async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitoring_is_closed_by_default_and_requires_the_exact_bearer_token() {
        assert!(!MonitorToken::new(None).authorized(Some("Bearer anything")));
        let token = MonitorToken::new(Some("test-token"));
        assert!(token.authorized(Some("Bearer test-token")));
        for header in [
            None,
            Some("test-token"),
            Some("Bearer test-tokeN"),
            Some("Bearer "),
        ] {
            assert!(!token.authorized(header));
        }
    }
}
