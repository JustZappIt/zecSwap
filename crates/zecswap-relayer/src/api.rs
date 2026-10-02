use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use tracing::error;
use zecswap_api::relayer::{Claim, LockClaim, Payout, Sent, Terms};
use zecswap_api::server::{self, Json};
use zecswap_api::service::ErrorCode;

use crate::{Relayer, RelayerError};

pub fn router(relayer: Arc<Relayer>) -> Router {
    Router::new()
        .route("/v1/terms", get(terms))
        .route("/v1/lock-claim", post(lock_claim))
        .route("/v1/claim", post(claim))
        .route("/v1/payout", post(payout))
        .route("/v1/rescue", post(rescue))
        .route(
            "/v1/reverse/fund",
            post(fund_reverse).layer(DefaultBodyLimit::max(132 * 1024)),
        )
        .route("/v1/reverse/ready", post(ready_reverse))
        .route("/v1/reverse/lock-refund", post(lock_reverse_refund))
        .route("/v1/reverse/refund", post(refund_reverse))
        .route("/v1/reverse/refund-payout", post(reverse_refund_payout))
        .route("/v1/reverse/rescue", post(rescue_reverse))
        .fallback(server::not_found)
        .method_not_allowed_fallback(server::method_not_allowed)
        .layer(middleware::from_fn(server::no_store))
        .with_state(relayer)
}

async fn terms(State(relayer): State<Arc<Relayer>>) -> Json<Terms> {
    Json(relayer.terms())
}

async fn lock_claim(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<LockClaim>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.lock_claim(request).await?))
}

async fn claim(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Claim>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.claim(request).await?))
}

async fn payout(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Payout>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.payout(request).await?))
}

async fn rescue(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::relayer::Rescue>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.rescue(request).await?))
}

async fn ready_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Authorization>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.ready_reverse(request).await?))
}

async fn fund_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Funding>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.fund_reverse(request).await?))
}

async fn lock_reverse_refund(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Authorization>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.lock_reverse_refund(request).await?))
}

async fn refund_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Refund>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.refund_reverse(request).await?))
}

async fn reverse_refund_payout(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Payout>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.reverse_refund_payout(request).await?))
}

async fn rescue_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::relayer::Rescue>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.rescue_reverse(request).await?))
}

impl IntoResponse for RelayerError {
    fn into_response(self) -> Response {
        match &self {
            RelayerError::Rejected(reason) => {
                server::error(StatusCode::BAD_REQUEST, ErrorCode::Rejected, reason)
            }
            RelayerError::Internal(e) => {
                error!("{e:#}");
                server::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    "internal error",
                )
            }
        }
    }
}
