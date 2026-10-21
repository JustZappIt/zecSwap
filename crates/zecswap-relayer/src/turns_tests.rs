use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tokio::sync::oneshot;
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
    // Another maker's: refused before anything is read from the chain.
    let terms = zecswap_api::Terms {
        maker: Address::repeat_byte(9),
        token: Address::repeat_byte(3),
        amount: 1,
        maker_key: share(0),
        user_key: share(1),
        user: Address::repeat_byte(5),
        t0: 1,
        t1: 2,
        payout_note: B256::repeat_byte(8),
    };
    let claim = serde_json::to_value(Claim {
        swap_id: busy,
        terms: terms.clone(),
        secret: B256::repeat_byte(2),
        payout: Payout {
            swap_id: busy,
            terms,
            note: zecswap_api::relayer::Note {
                npk: B256::repeat_byte(3),
                encrypted_bundle: [B256::repeat_byte(4); 3],
                shield_key: B256::repeat_byte(5),
            },
            fee: 1,
            signature: alloy_primitives::FixedBytes::<65>::repeat_byte(1),
        },
    })
    .unwrap();

    let running = relayer.turns.take(busy).await;
    // Refused for its calldata: another swap's request runs meanwhile.
    let (status, body) = post(&app, "/v1/reverse/fund", funding(other)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid V2 Relay Adapt calldata");
    let waiting =
        [("/v1/reverse/fund", funding(busy)), ("/v1/claim", claim)].map(|(path, body)| {
            let app = app.clone();
            tokio::spawn(async move { post(&app, path, body).await })
        });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        waiting.iter().all(|request| !request.is_finished()),
        "a request ran while another for its swap did"
    );
    drop(running);
    for request in waiting {
        let (status, _) = request.await.unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let operations = &serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"];
    assert_eq!(
        (
            &operations["fund_reverse"]["refused"],
            &operations["claim"]["refused"]
        ),
        (&2.into(), &1.into())
    );
    assert_eq!(operations["fund_reverse"]["failed"], 0);
    assert_eq!(relayer.turns.swaps(), 0, "a turn lingers");
}

/// A caller that stops waiting, as a client does at its timeout, leaves its request running to
/// its end, and the swap's next request waits for it.
#[tokio::test]
async fn a_request_runs_to_its_end_after_its_caller_leaves() {
    let relayer = Arc::new(crate::funding_tests::relayer(true));
    let swap = B256::repeat_byte(6);
    let (started, has_started) = oneshot::channel();
    let (release, released) = oneshot::channel::<()>();
    let (ended, has_ended) = oneshot::channel();
    let caller = tokio::spawn({
        let relayer = Arc::clone(&relayer);
        async move {
            relayer
                .in_turn("claim", swap, |_| async move {
                    started.send(()).unwrap();
                    released.await.unwrap();
                    ended.send(()).unwrap();
                    Ok(Sent {
                        transactions: vec![B256::repeat_byte(1)],
                    })
                })
                .await
        }
    });
    has_started.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());

    let next = tokio::spawn({
        let relayer = Arc::clone(&relayer);
        async move {
            let nothing = Sent {
                transactions: vec![],
            };
            relayer
                .in_turn("claim", swap, |_| async { Ok(nothing) })
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!next.is_finished(), "it ran before the one left running");
    release.send(()).unwrap();
    has_ended.await.unwrap();
    next.await.unwrap().unwrap();
    // Counted, though nobody read its answer.
    let operations = &serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"];
    assert_eq!(operations["claim"]["sent"], 1);
}
