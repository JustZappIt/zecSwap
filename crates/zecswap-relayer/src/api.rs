use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tracing::error;
use zecswap_api::relayer::{Claim, LockClaim, Payout, Sent, Terms};

use crate::{Relayer, RelayerError};

pub fn router(relayer: Arc<Relayer>) -> Router {
    Router::new()
        .route("/v1/terms", get(terms))
        .route("/v1/lock-claim", post(lock_claim))
        .route("/v1/claim", post(claim))
        .route("/v1/payout", post(payout))
        .route("/v1/rescue", post(rescue))
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
    Json(request): Json<Payout>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.rescue(request).await?))
}

impl IntoResponse for RelayerError {
    fn into_response(self) -> Response {
        match &self {
            RelayerError::Rejected(reason) => {
                (StatusCode::BAD_REQUEST, Json(json!({ "error": reason }))).into_response()
            }
            RelayerError::Internal(e) => {
                error!("{e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal error" })),
                )
                    .into_response()
            }
        }
    }
}
