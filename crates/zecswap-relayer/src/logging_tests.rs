use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};
use zecswap_api::{relayer::Note, reverse::Refund};
use zecswap_chain::evm::judge::READY_DEPTH;
use zecswap_core::derive_maker_share;

use crate::deposits_tests::{Called, Protect};
use crate::held_tests::{held_in, until_none_held};
use crate::retries_tests::post;

/// What a test's subscriber writes.
#[derive(Clone, Default)]
pub(crate) struct Capture(pub(crate) Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What this thread logs from now until the guards drop. Another dispatcher stays registered
/// beside the one capturing: with one alone, tracing caches whether each callsite is on as the
/// thread first reaching it sees it, and a test on another thread reaching it first turns it off.
pub(crate) fn capture() -> (Capture, [tracing::subscriber::DefaultGuard; 2]) {
    let captured = Capture::default();
    let writer = captured.clone();
    let beside = tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let capturing = tracing::subscriber::set_default(subscriber);
    (captured, [capturing, beside])
}

#[tokio::test]
async fn rejected_request_keeps_swap_context_without_logging_secret_payload() {
    let (captured, _guards) = capture();
    let contract = Address::repeat_byte(1);
    let relayer = Relayer {
        config: Config {
            network: zecswap_api::Network::Testnet,
            evm_rpc: "http://127.0.0.1:1".into(),
            contract,
            token: Address::repeat_byte(4),
            maker: Address::repeat_byte(5),
            listen: "127.0.0.1:0".parse().unwrap(),
            fee: 1,
            fee_gas: 0,
            providers: vec![],
            fee_margin_bps: 0,
            claim_margin: 30,
            reveals: None,
            top_up_transactions: 8,
            reverse_funding: None,
            railgun_sends: None,
            gas_alerts: None,
            deposits: None,
            held_claims: None,
        },
        account: Address::repeat_byte(2),
        domain: Domain {
            chain_id: 1,
            contract: contract.into(),
        },
        settlement: Settlement::read_only("http://127.0.0.1:1", contract).unwrap(),
        lanes: crate::lanes::Lanes::connect(
            "http://127.0.0.1:1",
            contract,
            &PrivateKeySigner::random(),
        )
        .unwrap(),
        windows: zecswap_chain::evm::Windows {
            min: 600,
            late: 1_200,
        },
        turns: Default::default(),
        steps: Default::default(),
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: None,
        funding: None,

        deposits: None,
        pricing: None,
        held: None,
        history: None,
    };
    let swap_id = B256::repeat_byte(0xab);
    let secret = B256::repeat_byte(0xcd);
    let signature = alloy_primitives::FixedBytes::<65>::repeat_byte(0xef);
    let note = B256::repeat_byte(0x98);
    let share = |nonce| {
        zecswap_core::derive_maker_share(&[1; 32], nonce)
            .unwrap()
            .public()
    };
    let terms = zecswap_api::Terms {
        maker: Address::repeat_byte(3),
        token: Address::repeat_byte(4),
        amount: 1,
        maker_key: share(0),
        user_key: share(1),
        user: Address::repeat_byte(5),
        t0: 1,
        t1: 2,
        payout_note: B256::ZERO,
    };
    let result = relayer
        .refund_reverse(Refund {
            swap_id,
            terms: terms.clone(),
            secret,
            payout: Payout {
                swap_id: B256::repeat_byte(0x12),
                terms,
                fee: 1,
                signature,
                note: Note {
                    npk: note,
                    encrypted_bundle: [note; 3],
                    shield_key: note,
                },
            },
        })
        .await;
    assert!(matches!(result, Err(RelayerError::Rejected(_))));
    let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains(&format!("swap_id={swap_id}")));
    assert!(output.contains("refund_reverse") && output.contains("different swaps"));
    for private in [secret.to_string(), signature.to_string(), note.to_string()] {
        assert!(
            !output.contains(&private),
            "request data must not be logged"
        );
    }
}

/// Through every step a held claim takes, refused, held, held again, sent, paid out and deleted,
/// and a refunded swap's, held and dropped, nothing logs its share, nor does the monitor or an
/// answer show it; and through `[reveals]` no node sees it: Protect alone, in the claim.
#[tokio::test]
async fn a_held_claim_never_logs_or_shows_its_share_and_only_its_claim_carries_it() {
    let (captured, _guards) = capture();
    let Some(chain) = crate::retries_tests::deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let node = Protect::start(&chain.chain, "/").await;
    let protect = Protect::start(&chain.chain, "/?hint=hash&blockRange=2").await;
    let config = Config {
        evm_rpc: node.url.clone(),
        reveals: Some(RevealsConfig {
            private_rpc: protect.url.clone(),
        }),
        held_claims: held_in(dir.path()),
        ..chain.config()
    };
    let relayer = chain.relayer_with(config).await;
    relayer.top_up().await.unwrap();
    let app = api::router(Arc::clone(&relayer));
    let (swap, refunded) = (chain.opened(0).await, chain.opened(1).await);
    let mut split = chain.claim(&swap);
    split.payout.swap_id = refunded.id;
    let mut answers = Vec::new();
    for claim in [split, chain.claim(&swap), chain.claim(&swap)] {
        answers.push(post(app.clone(), "/v1/claim/hold", claim).await);
    }
    answers.push(post(app.clone(), "/v1/claim/hold", chain.claim(&refunded)).await);
    relayer.held_pass().await.unwrap();
    let cancel = derive_maker_share(&[9; 32], 1).unwrap();
    let maker = &chain.maker;
    maker
        .refund(refunded.id, &refunded.terms, &cancel)
        .await
        .unwrap();
    maker.ready(swap.id, &swap.terms).await.unwrap();
    chain.mine_blocks(READY_DEPTH).await;
    until_none_held(&relayer).await;
    for claim in [chain.claim(&swap), chain.claim(&refunded)] {
        answers.push(post(app.clone(), "/v1/claim/hold", claim).await);
    }
    let statuses: Vec<u16> = answers.iter().map(|(status, _)| status.as_u16()).collect();
    assert_eq!(statuses, [400, 200, 200, 200, 200, 400]);
    let landed = maker.swap(swap.id, &swap.terms).await.unwrap().unwrap();
    assert!(landed.stage == Stage::Claimed && landed.paid_out);

    let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    for (id, step) in [
        (swap.id, "held"),
        (swap.id, "sent"),
        (swap.id, "deleted"),
        (refunded.id, "held"),
        (refunded.id, "dropped"),
    ] {
        let logged = output.lines().any(|line| {
            line.contains(&format!("swap_id={id}")) && line.contains(&format!("step=\"{step}\""))
        });
        assert!(logged, "{step} is not logged for {id}:\n{output}");
    }
    let output = output.to_lowercase();
    let monitor = serde_json::to_string(&relayer.monitor_snapshot()).unwrap();
    let answered = format!("{answers:?}").to_lowercase();
    let reads = node.calls(None);
    for share in [&swap, &refunded].map(|swap| swap.keys.share.to_be_bytes()) {
        let hex = alloy_primitives::hex::encode(share);
        let decimal = alloy_primitives::U256::from_be_bytes(share).to_string();
        for said in [hex, decimal] {
            assert!(!output.contains(&said), "a share is logged");
            assert!(
                !monitor.to_lowercase().contains(&said),
                "the monitor shows a share"
            );
            assert!(!answered.contains(&said), "an answer shows a share");
            let sent = |call: &Called| call.params.to_string().to_lowercase().contains(&said);
            assert!(!reads.iter().any(sent), "the node saw a share");
        }
    }
    // Protect was asked its chain at the start, and given the one claim.
    let methods: Vec<String> = protect
        .calls(None)
        .into_iter()
        .map(|call| call.method)
        .collect();
    assert_eq!(methods, ["eth_chainId", "eth_sendRawTransaction"]);
}
