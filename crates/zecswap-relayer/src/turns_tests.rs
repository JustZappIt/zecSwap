use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::*;

async fn post(
    app: &axum::Router,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn a_swaps_requests_take_turns() {
    let relayer = Arc::new(crate::funding_tests::relayer(true));
    let app = api::router(Arc::clone(&relayer));
    let funding = |swap: B256| {
        serde_json::json!({
            "swapId": swap, "chainId": 11155111,
            "to": Network::Testnet.relay_adapt(11_155_111), "data": "0x01020304", "value": "0",
        })
    };
    let share = |nonce| {
        zecswap_core::derive_maker_share(&[1; 32], nonce)
            .unwrap()
            .public()
    };
    let (busy, other) = (B256::repeat_byte(6), B256::repeat_byte(7));
    let lock = serde_json::to_value(LockClaim {
        swap_id: busy,
        terms: zecswap_api::Terms {
            maker: Address::repeat_byte(4),
            token: Address::repeat_byte(3),
            amount: 1,
            maker_key: share(0),
            user_key: share(1),
            user: Address::repeat_byte(5),
            t0: 1,
            t1: 2,
            payout_note: B256::repeat_byte(8),
        },
        deadline: 1,
        signature: alloy_primitives::FixedBytes::<65>::repeat_byte(1),
    })
    .unwrap();

    let running = relayer.in_flight.start(busy).unwrap();
    for (path, body) in [
        ("/v1/reverse/fund", funding(busy)),
        ("/v1/lock-claim", lock),
    ] {
        let (status, body) = post(&app, path, body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(body["code"], "unavailable");
    }
    // Refused for its calldata: it ran.
    let (status, body) = post(&app, "/v1/reverse/fund", funding(other)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid V2 Relay Adapt calldata");
    drop(running);
    let (status, _) = post(&app, "/v1/reverse/fund", funding(busy)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let operations = &serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"];
    assert_eq!(
        (
            &operations["fund_reverse"]["refused"],
            &operations["lock_claim"]["refused"]
        ),
        (&3.into(), &1.into())
    );
    assert_eq!(operations["fund_reverse"]["failed"], 0);
}
