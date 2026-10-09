use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use tracing::error;
use zecswap_api::relayer::{AlreadySpent, Claim, LockClaim, Payout, RailgunTransact, Sent, Terms};
use zecswap_api::server::{self, Json};
use zecswap_api::service::ErrorCode;

use crate::monitor::MonitorSnapshot;
use crate::{Relayer, RelayerError, Sending};

pub fn router(relayer: Arc<Relayer>) -> Router {
    let monitor = Router::new()
        .route("/v1/monitor", get(monitor))
        .route("/v1/monitor/sends", get(sends))
        .route_layer(middleware::from_fn_with_state(
            relayer.monitor.token.clone(),
            server::require_monitor,
        ));
    Router::new()
        .route("/v1/terms", get(terms))
        .merge(monitor)
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
        .route(
            "/v1/railgun/transact",
            post(railgun_transact).layer(DefaultBodyLimit::max(132 * 1024)),
        )
        .fallback(server::not_found)
        .method_not_allowed_fallback(server::method_not_allowed)
        .layer(middleware::from_fn(server::no_store))
        .with_state(relayer)
}

async fn terms(State(relayer): State<Arc<Relayer>>) -> Json<Terms> {
    Json(relayer.terms().await)
}

async fn monitor(State(relayer): State<Arc<Relayer>>) -> Json<MonitorSnapshot> {
    Json(relayer.monitor_snapshot())
}

#[derive(serde::Deserialize)]
struct SendsQuery {
    since: Option<u64>,
}

async fn sends(State(relayer): State<Arc<Relayer>>, Query(query): Query<SendsQuery>) -> Response {
    match relayer.sends_snapshot(query.since) {
        Ok(Some(snapshot)) => axum::Json(snapshot).into_response(),
        Ok(None) => server::error(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "this relayer sends no Railgun transactions",
        ),
        Err(e) => {
            error!("sends snapshot: {e:#}");
            server::error(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                "monitoring temporarily unavailable",
            )
        }
    }
}

async fn lock_claim(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<LockClaim>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.lock_claim(request).await;
    Ok(Json(relayer.observe("lock_claim", sent)?))
}

async fn claim(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Claim>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.claim(request).await;
    Ok(Json(relayer.observe("claim", sent)?))
}

async fn payout(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Payout>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.payout(request).await;
    Ok(Json(relayer.observe("payout", sent)?))
}

async fn rescue(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::relayer::Rescue>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.rescue(request).await;
    Ok(Json(relayer.observe("rescue", sent)?))
}

async fn ready_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Authorization>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.ready_reverse(request).await;
    Ok(Json(relayer.observe("ready_reverse", sent)?))
}

async fn fund_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Funding>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.fund_reverse(request).await;
    Ok(Json(relayer.observe("fund_reverse", sent)?))
}

async fn lock_reverse_refund(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Authorization>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.lock_reverse_refund(request).await;
    Ok(Json(relayer.observe("lock_reverse_refund", sent)?))
}

async fn refund_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Refund>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.refund_reverse(request).await;
    Ok(Json(relayer.observe("refund_reverse", sent)?))
}

async fn reverse_refund_payout(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Payout>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.reverse_refund_payout(request).await;
    Ok(Json(relayer.observe("reverse_refund_payout", sent)?))
}

async fn rescue_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::relayer::Rescue>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer.rescue_reverse(request).await;
    Ok(Json(relayer.observe("rescue_reverse", sent)?))
}

/// A repeated post of the same bytes answers with the first send's hash, and is not counted
/// again.
async fn railgun_transact(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<RailgunTransact>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = match relayer.railgun_transact(request).await {
        Ok(Sending::Again(sent)) => Ok(sent),
        Ok(Sending::New(sent)) => relayer.observe("railgun_transact", Ok(sent)),
        Err(error) => relayer.observe("railgun_transact", Err(error)),
    };
    Ok(Json(sent?))
}

impl IntoResponse for RelayerError {
    fn into_response(self) -> Response {
        match &self {
            RelayerError::Rejected(reason) => {
                server::error(StatusCode::BAD_REQUEST, ErrorCode::Rejected, reason)
            }
            RelayerError::Spent(transactions) => (
                StatusCode::CONFLICT,
                axum::Json(AlreadySpent {
                    code: ErrorCode::AlreadySpent,
                    error: self.to_string(),
                    transactions: transactions.clone(),
                }),
            )
                .into_response(),
            RelayerError::Unsettled(reason) => server::error(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                *reason,
            ),
            RelayerError::Unpriced => server::error(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                self.to_string(),
            ),
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
