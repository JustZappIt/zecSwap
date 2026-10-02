//! The quote API. Everything it returns is advisory: clients verify the swap on-chain.

use std::sync::Arc;

use alloy_primitives::B256;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use tracing::error;
use zecswap_api::server::{self, Json, Path};
use zecswap_api::service::{ErrorCode, MakerInfo};
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};

use crate::maker::{Maker, MakerError};

pub fn router(maker: Arc<Maker>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/info", get(info))
        .route("/v1/monitor", get(monitor))
        .route("/v1/quote", post(quote))
        .route("/v1/quote/{quote_id}/accept", post(accept))
        .route("/v1/reverse/quote", post(reverse_quote))
        .route("/v1/reverse/quote/{quote_id}/accept", post(accept_reverse))
        .route("/v1/reverse/swaps/{swap_id}", get(reverse_status))
        .fallback(server::not_found)
        .method_not_allowed_fallback(server::method_not_allowed)
        .layer(middleware::from_fn(server::no_store))
        .with_state(maker)
}

async fn health(State(maker): State<Arc<Maker>>) -> Result<StatusCode, MakerError> {
    maker.check_watchtower()?;
    Ok(StatusCode::NO_CONTENT)
}

async fn info(State(maker): State<Arc<Maker>>) -> Json<MakerInfo> {
    Json(maker.info())
}

async fn monitor(State(maker): State<Arc<Maker>>, headers: HeaderMap) -> Response {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    if !maker.monitor_authorized(authorization) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    match maker.monitor_snapshot().await {
        Ok(snapshot) => axum::Json(snapshot).into_response(),
        Err(e) => {
            error!("monitor snapshot: {e:#}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "monitoring temporarily unavailable",
            )
                .into_response()
        }
    }
}

async fn reverse_quote(
    State(maker): State<Arc<Maker>>,
    Json(request): Json<zecswap_api::reverse::QuoteRequest>,
) -> Result<Json<zecswap_api::reverse::Quote>, MakerError> {
    Ok(Json(maker.reverse_quote(request).await?))
}

async fn accept_reverse(
    State(maker): State<Arc<Maker>>,
    Path(quote_id): Path<B256>,
    Json(acceptance): Json<Acceptance>,
) -> Result<Json<Accepted>, MakerError> {
    Ok(Json(maker.accept_reverse(quote_id, acceptance).await?))
}

async fn reverse_status(
    State(maker): State<Arc<Maker>>,
    Path(swap_id): Path<B256>,
) -> Result<Json<zecswap_api::reverse::Status>, MakerError> {
    maker
        .reverse_status(swap_id)
        .await?
        .map(Json)
        .ok_or(MakerError::UnknownSwap)
}

async fn quote(
    State(maker): State<Arc<Maker>>,
    Json(request): Json<QuoteRequest>,
) -> Result<Json<Quote>, MakerError> {
    Ok(Json(maker.quote(request).await?))
}

async fn accept(
    State(maker): State<Arc<Maker>>,
    Path(quote_id): Path<B256>,
    Json(acceptance): Json<Acceptance>,
) -> Result<Json<Accepted>, MakerError> {
    Ok(Json(maker.accept(quote_id, acceptance).await?))
}

impl IntoResponse for MakerError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            MakerError::Rejected(_) => (StatusCode::BAD_REQUEST, ErrorCode::Rejected),
            MakerError::UnknownQuote => (StatusCode::NOT_FOUND, ErrorCode::UnknownQuote),
            MakerError::UnknownSwap => (StatusCode::NOT_FOUND, ErrorCode::UnknownSwap),
            MakerError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable),
            MakerError::WatchtowerUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::WatchtowerUnavailable,
            ),
            MakerError::Internal(e) => {
                error!("{e:#}");
                return server::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    "internal error",
                );
            }
        };
        server::error(status, code, self.to_string())
    }
}
