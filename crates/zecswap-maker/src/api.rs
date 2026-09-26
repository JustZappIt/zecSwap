//! The quote API. Everything it returns is advisory: clients verify the swap on-chain.

use std::sync::Arc;

use alloy_primitives::B256;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tracing::error;
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};

use crate::maker::{Maker, MakerError};

pub fn router(maker: Arc<Maker>) -> Router {
    Router::new()
        .route("/v1/quote", post(quote))
        .route("/v1/quote/{quote_id}/accept", post(accept))
        .with_state(maker)
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
        let status = match &self {
            MakerError::Rejected(_) => StatusCode::BAD_REQUEST,
            MakerError::UnknownQuote => StatusCode::NOT_FOUND,
            MakerError::Unavailable | MakerError::WatchtowerUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            MakerError::Internal(e) => {
                error!("{e:#}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal error" })),
                )
                    .into_response();
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
