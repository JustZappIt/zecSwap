use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

pub(crate) fn relayer(enabled: bool) -> Relayer {
    let contract = Address::repeat_byte(1);
    let account = Address::repeat_byte(5);
    let config = Config {
        network: Network::Testnet,
        evm_rpc: "http://127.0.0.1:1".into(),
        contract,
        token: Address::repeat_byte(3),
        maker: Address::repeat_byte(4),
        listen: "127.0.0.1:0".parse().unwrap(),
        fee: 1,
        fee_gas: 0,
        providers: vec![],
        fee_margin_bps: 0,
        claim_margin: 30,
        top_up_transactions: 8,
        reverse_funding: enabled.then_some(ReverseFundingConfig {
            relay_adapt: None,
            max_gas_limit: 4_000_000,
            max_gas_price_wei: 20_000_000_000,
            fee: 250_000,
            fee_gas: 0,
        }),
        railgun_sends: None,
        gas_alerts: None,
    };
    Relayer {
        funding: config.funding_policy(account, 11_155_111).unwrap(),
        config,
        account,
        domain: Domain {
            chain_id: 11_155_111,
            contract: contract.into(),
        },
        settlement: Settlement::read_only("http://127.0.0.1:1", contract).unwrap(),
        lanes: crate::lanes::Lanes::connect(
            "http://127.0.0.1:1",
            contract,
            &PrivateKeySigner::random(),
        )
        .unwrap(),
        lock_duration: 600,
        in_flight: Default::default(),
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: None,
        pricing: None,
        history: None,
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
        "to": Network::Testnet.relay_adapt(11_155_111), "data": "0x01020304", "value": "0",
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

#[tokio::test]
async fn funding_capability_is_opt_in_and_old_config_still_loads() {
    let config: Config = toml::from_str(include_str!("../relayer.example.toml")).unwrap();
    assert!(config.reverse_funding.is_none());
    let old = serde_json::to_value(relayer(false).terms().await).unwrap();
    assert!(old.get("reverseFunding").is_none());
    let enabled = serde_json::to_value(relayer(true).terms().await).unwrap();
    assert_eq!(enabled["reverseFunding"]["maxGasPriceWei"], "20000000000");
    assert_eq!(enabled["reverseFunding"]["maxCalldataBytes"], 65536);
    assert_eq!(enabled["reverseFunding"]["fee"], "250000");
}

#[test]
fn fundings_go_through_railguns_own_relay_adapt_on_the_networks_chain() {
    let account = Address::repeat_byte(5);
    let mut config = relayer(true).config;
    let relay_adapt = |config: &Config, chain_id| {
        config
            .funding_policy(account, chain_id)
            .map(|policy| policy.unwrap().relay_adapt)
    };
    let railguns = Network::Testnet.relay_adapt(11_155_111).unwrap();
    assert_eq!(relay_adapt(&config, 11_155_111).unwrap(), railguns);
    let named = |config: &mut Config, address| {
        config.reverse_funding.as_mut().unwrap().relay_adapt = address;
    };
    named(&mut config, Some(railguns));
    assert_eq!(relay_adapt(&config, 11_155_111).unwrap(), railguns);
    let other = Address::repeat_byte(2);
    named(&mut config, Some(other));
    assert!(relay_adapt(&config, 11_155_111).is_err());
    // A local chain has no Railgun of its own: its Relay Adapt must be named.
    assert_eq!(relay_adapt(&config, 31_337).unwrap(), other);
    named(&mut config, None);
    assert!(relay_adapt(&config, 31_337).is_err());
    config.reverse_funding = None;
    assert!(
        config
            .funding_policy(account, 11_155_111)
            .unwrap()
            .is_none()
    );
}
