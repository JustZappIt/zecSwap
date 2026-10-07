use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};
use zecswap_api::{relayer::Note, reverse::Refund};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn rejected_request_keeps_swap_context_without_logging_secret_payload() {
    let captured = Capture::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let contract = Address::repeat_byte(1);
    let relayer = Relayer {
        config: Config {
            evm_rpc: "http://127.0.0.1:1".into(),
            contract,
            listen: "127.0.0.1:0".parse().unwrap(),
            fee: 1,
            claim_margin: 30,
            reverse_funding: None,
        },
        account: Address::repeat_byte(2),
        domain: Domain {
            chain_id: 1,
            contract: contract.into(),
        },
        settlement: Settlement::read_only("http://127.0.0.1:1", contract).unwrap(),
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
