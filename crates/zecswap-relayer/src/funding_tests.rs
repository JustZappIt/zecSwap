use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

pub(crate) fn relayer(enabled: bool) -> Relayer {
    let contract = Address::repeat_byte(1);
    Relayer {
        config: Config {
            evm_rpc: "http://127.0.0.1:1".into(),
            contract,
            token: Address::repeat_byte(3),
            maker: Address::repeat_byte(4),
            listen: "127.0.0.1:0".parse().unwrap(),
            fee: 1,
            claim_margin: 30,
            reverse_funding: enabled.then_some(ReverseFundingConfig {
                relay_adapt: Address::repeat_byte(2),
                max_gas_limit: 4_000_000,
                max_gas_price_wei: 20_000_000_000,
                fee: 250_000,
            }),
            railgun_sends: None,
        },
        account: Address::repeat_byte(5),
        domain: Domain {
            chain_id: 11_155_111,
            contract: contract.into(),
        },
        settlement: Settlement::read_only("http://127.0.0.1:1", contract).unwrap(),
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: None,
    }
}

async fn post(enabled: bool, body: String) -> (StatusCode, serde_json::Value) {
    let response = api::router(Arc::new(relayer(enabled)))
        .oneshot(
            Request::post("/v1/reverse/fund")
                .header("Content-Type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["Cache-Control"], "no-store");
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn request() -> serde_json::Value {
    serde_json::json!({
        "swapId": B256::repeat_byte(6), "chainId": 11155111,
        "to": Address::repeat_byte(2), "data": "0x01020304", "value": "0",
    })
}

#[tokio::test]
async fn funding_route_rejects_disabled_wrong_chain_and_malformed_requests_without_rpc() {
    let (status, body) = post(false, request().to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "rejected");
    assert!(body["error"].as_str().unwrap().contains("disabled"));
    let mut wrong_chain = request();
    wrong_chain["chainId"] = 1.into();
    let (_, body) = post(true, wrong_chain.to_string()).await;
    assert_eq!(body["error"], "wrong funding chain");
    let (_, body) = post(true, request().to_string()).await;
    assert_eq!(body["error"], "invalid V2 Relay Adapt calldata");
    let mut extra = request();
    extra["privateKey"] = "must not be accepted".into();
    let (_, body) = post(true, extra.to_string()).await;
    assert_eq!(body["code"], "invalidRequest");
    let mut numeric = request();
    numeric["value"] = 0.into();
    let (_, body) = post(true, numeric.to_string()).await;
    assert_eq!(body["code"], "invalidRequest");
    let mut too_large = request();
    too_large["data"] = format!("0x{}", "01".repeat(70 * 1024)).into();
    let (status, body) = post(true, too_large.to_string()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["code"], "invalidRequest");
}

#[test]
fn funding_capability_is_opt_in_and_old_config_still_loads() {
    let config: Config = toml::from_str(include_str!("../relayer.example.toml")).unwrap();
    assert!(config.reverse_funding.is_none());
    let old = serde_json::to_value(relayer(false).terms()).unwrap();
    assert!(old.get("reverseFunding").is_none());
    let enabled = serde_json::to_value(relayer(true).terms()).unwrap();
    assert_eq!(enabled["reverseFunding"]["maxGasPriceWei"], "20000000000");
    assert_eq!(enabled["reverseFunding"]["maxCalldataBytes"], 65536);
    assert_eq!(enabled["reverseFunding"]["fee"], "250000");
}
