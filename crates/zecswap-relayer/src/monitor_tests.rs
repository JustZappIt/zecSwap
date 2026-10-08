//! The monitor counts each operation's requests by how they ended, and names a failure by its
//! kind alone: nothing a request or an error said reaches it.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use zecswap_api::server::MonitorToken;

use super::*;
use crate::monitor::Monitor;

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

fn monitor(token: Option<&str>) -> Request<Body> {
    let mut request = Request::get("/v1/monitor");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn the_monitor_counts_outcomes_and_names_failures_by_kind_alone() {
    let mut relayer = crate::funding_tests::relayer(false);
    relayer.monitor = Monitor::new(MonitorToken::new(Some("monitor-test-token")));
    let app = api::router(Arc::new(relayer));
    for token in [None, Some("wrong-token")] {
        assert_eq!(send(&app, monitor(token)).await.0, StatusCode::UNAUTHORIZED);
    }

    let share = |nonce| {
        zecswap_core::derive_maker_share(&[1; 32], nonce)
            .unwrap()
            .public()
    };
    let lock = |maker: Address| LockClaim {
        swap_id: B256::repeat_byte(8),
        terms: zecswap_api::Terms {
            maker,
            token: Address::repeat_byte(3),
            amount: 1,
            maker_key: share(0),
            user_key: share(1),
            user: Address::repeat_byte(6),
            t0: 1,
            t1: 2,
            payout_note: B256::repeat_byte(7),
        },
        deadline: 1,
        signature: [9; 65].into(),
    };
    let post = |claim: LockClaim| {
        Request::post("/v1/lock-claim")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&claim).unwrap()))
            .unwrap()
    };
    // Another maker's swap is refused before anything is read; this maker's fails to reach
    // the chain.
    let refused = send(&app, post(lock(Address::repeat_byte(0x66)))).await;
    assert_eq!(refused.0, StatusCode::BAD_REQUEST);
    let failed = send(&app, post(lock(Address::repeat_byte(4)))).await;
    assert_eq!(failed.0, StatusCode::INTERNAL_SERVER_ERROR);

    let (status, shown) = send(&app, monitor(Some("monitor-test-token"))).await;
    assert_eq!(status, StatusCode::OK);
    let operations = shown["operations"].as_object().unwrap();
    assert_eq!(operations.len(), 10);
    let locked = &operations["lock_claim"];
    assert_eq!(
        (&locked["sent"], &locked["refused"], &locked["failed"]),
        (&0.into(), &1.into(), &1.into())
    );
    assert_eq!(
        (&locked["gasUsed"], &locked["gasCostWei"]),
        (&"0".into(), &"0".into())
    );
    assert!(locked["lastAt"].as_u64().is_some());
    assert!(operations["payout"]["lastAt"].is_null());
    assert_eq!(shown["lastFailure"]["operation"], "lock_claim");
    assert_eq!(shown["lastFailure"]["kind"], "internal");
    assert_eq!(
        shown["fees"],
        serde_json::json!({"payout": "1", "funding": null})
    );
    let text = shown.to_string();
    for said in ["127.0.0.1", "another token or maker", "error"] {
        assert!(!text.contains(said), "{said} in {text}");
    }
}
