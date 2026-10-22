use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use tracing::error;
use zecswap_api::relayer::{
    Claim, Conflict, Held, Payout, RailgunTransact, RescueDeposit, Sent, ShieldDeposit, Terms,
};
use zecswap_api::server::{self, Json};
use zecswap_api::service::ErrorCode;

use crate::monitor::MonitorSnapshot;
use crate::{Holding, Relayer, RelayerError};

/// What a `503` with nothing better to say asks to wait, in seconds: a send's outcome is known
/// within a block or two, prices within a minute.
const UNSETTLED_WAIT: u64 = 15;
const UNPRICED_WAIT: u64 = 60;
const MONITOR_WAIT: u64 = 30;

pub fn router(relayer: Arc<Relayer>) -> Router {
    let monitor = Router::new()
        .route("/v1/monitor", get(monitor))
        .route("/v1/monitor/sends", get(sends))
        .route("/v1/monitor/deposits", get(deposits))
        .route_layer(middleware::from_fn_with_state(
            relayer.monitor.token.clone(),
            server::require_monitor,
        ));
    Router::new()
        .route("/v1/terms", get(terms))
        .merge(monitor)
        .route("/v1/claim", post(claim))
        .route("/v1/claim/hold", post(hold_claim))
        .route("/v1/payout", post(payout))
        .route("/v1/rescue", post(rescue))
        .route(
            "/v1/reverse/fund",
            post(fund_reverse).layer(DefaultBodyLimit::max(132 * 1024)),
        )
        .route("/v1/reverse/ready", post(ready_reverse))
        .route("/v1/reverse/refund", post(refund_reverse))
        .route("/v1/reverse/refund-payout", post(reverse_refund_payout))
        .route("/v1/reverse/rescue", post(rescue_reverse))
        .route(
            "/v1/railgun/transact",
            post(railgun_transact).layer(DefaultBodyLimit::max(132 * 1024)),
        )
        .route("/v1/deposits/shield", post(shield_deposit))
        .route("/v1/deposits/rescue", post(rescue_deposit))
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
struct LedgerQuery {
    since: Option<u64>,
}

async fn sends(State(relayer): State<Arc<Relayer>>, Query(query): Query<LedgerQuery>) -> Response {
    ledger(
        relayer.sends_snapshot(query.since),
        "this relayer sends no Railgun transactions",
    )
}

async fn deposits(
    State(relayer): State<Arc<Relayer>>,
    Query(query): Query<LedgerQuery>,
) -> Response {
    ledger(
        relayer.deposits_snapshot(query.since),
        "this relayer takes no deposits",
    )
}

fn ledger(snapshot: anyhow::Result<Option<impl serde::Serialize>>, none: &str) -> Response {
    match snapshot {
        Ok(Some(snapshot)) => axum::Json(snapshot).into_response(),
        Ok(None) => server::error(StatusCode::NOT_FOUND, ErrorCode::NotFound, none),
        Err(e) => {
            error!("ledger snapshot: {e:#}");
            unavailable("monitoring temporarily unavailable", MONITOR_WAIT)
        }
    }
}

/// A `503`, with how long to wait before asking again.
fn unavailable(message: impl Into<String>, seconds: u64) -> Response {
    let mut response = server::error(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Unavailable,
        message,
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    response
}

async fn claim(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Claim>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn("claim", request.swap_id, |relayer| async move {
            relayer.claim(request).await
        })
        .await;
    Ok(Json(sent?))
}

/// `{"held": true}` once held; a claimed swap is answered as `/v1/claim` answers it.
async fn hold_claim(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Claim>,
) -> Result<Response, RelayerError> {
    let id = request.swap_id;
    let holding = {
        let relayer = Arc::clone(&relayer);
        async move { relayer.hold_claim(request).await }
    };
    let holding = relayer
        .in_line("hold_claim", id, holding, Relayer::observe_holding)
        .await?;
    Ok(match holding {
        Holding::Held => Json(Held { held: true }).into_response(),
        Holding::Claimed(sent) => Json(sent).into_response(),
    })
}

async fn payout(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Payout>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn("payout", request.swap_id, |relayer| async move {
            relayer.payout(request).await
        })
        .await;
    Ok(Json(sent?))
}

async fn rescue(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::relayer::Rescue>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn("rescue", request.swap_id, |relayer| async move {
            relayer.rescue(request).await
        })
        .await;
    Ok(Json(sent?))
}

async fn ready_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Authorization>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn("ready_reverse", request.swap_id, |relayer| async move {
            relayer.ready_reverse(request).await
        })
        .await;
    Ok(Json(sent?))
}

async fn fund_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Funding>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn("fund_reverse", request.swap_id, |relayer| async move {
            relayer.fund_reverse(request).await
        })
        .await;
    Ok(Json(sent?))
}

async fn refund_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::reverse::Refund>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn("refund_reverse", request.swap_id, |relayer| async move {
            relayer.refund_reverse(request).await
        })
        .await;
    Ok(Json(sent?))
}

async fn reverse_refund_payout(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<Payout>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn(
            "reverse_refund_payout",
            request.swap_id,
            |relayer| async move { relayer.reverse_refund_payout(request).await },
        )
        .await;
    Ok(Json(sent?))
}

async fn rescue_reverse(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<zecswap_api::relayer::Rescue>,
) -> Result<Json<Sent>, RelayerError> {
    let sent = relayer
        .in_turn("rescue_reverse", request.swap_id, |relayer| async move {
            relayer.rescue_reverse(request).await
        })
        .await;
    Ok(Json(sent?))
}

/// A repeated post of the same bytes answers with the first send's hash, and is not counted
/// again.
async fn railgun_transact(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<RailgunTransact>,
) -> Result<Json<Sent>, RelayerError> {
    let sending = relayer.railgun_transact(request).await;
    Ok(Json(relayer.observe_sending("railgun_transact", sending)?))
}

/// As `railgun_transact`, a repeat of the same authorization answers with the first send's hash.
async fn shield_deposit(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<ShieldDeposit>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.shield_deposit(request).await?))
}

async fn rescue_deposit(
    State(relayer): State<Arc<Relayer>>,
    Json(request): Json<RescueDeposit>,
) -> Result<Json<Sent>, RelayerError> {
    Ok(Json(relayer.rescue_deposit(request).await?))
}

impl IntoResponse for RelayerError {
    fn into_response(self) -> Response {
        match &self {
            RelayerError::Rejected(reason) => {
                server::error(StatusCode::BAD_REQUEST, ErrorCode::Rejected, reason)
            }
            RelayerError::Spent(transactions) => {
                conflict(ErrorCode::AlreadySpent, self.to_string(), transactions)
            }
            RelayerError::Conflict(transactions, _) => {
                conflict(ErrorCode::Conflict, self.to_string(), transactions)
            }
            RelayerError::Unsettled(reason) => unavailable(*reason, UNSETTLED_WAIT),
            RelayerError::Unpriced => unavailable(self.to_string(), UNPRICED_WAIT),
            RelayerError::Reverted(seconds) | RelayerError::Busy(_, seconds) => {
                unavailable(self.to_string(), *seconds)
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

fn conflict(code: ErrorCode, error: String, transactions: &[alloy_primitives::B256]) -> Response {
    let body = Conflict {
        code,
        error,
        transactions: transactions.to_vec(),
    };
    (StatusCode::CONFLICT, axum::Json(body)).into_response()
}
