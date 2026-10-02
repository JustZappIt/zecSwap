use alloy::node_bindings::Anvil;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing::{Instrument, instrument::WithSubscriber};
use zecswap_chain::evm::{Address, B256, PrivateKeySigner, Settlement, U256};

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
async fn submitted_and_mined_events_keep_the_same_swap_and_transaction_hash() {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let captured = Capture::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let private_key = format!("{:x}", key.to_bytes());
    let chain = Settlement::connect(&anvil.endpoint(), Address::ZERO, key).unwrap();
    let swap_id = B256::repeat_byte(0xab);
    let transaction_hash = async {
        chain
            .send_eth(Address::repeat_byte(1), U256::from(1))
            .instrument(tracing::info_span!("swap", %swap_id, operation = "test_send"))
            .await
            .unwrap()
    }
    .with_subscriber(subscriber)
    .await;
    let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    for outcome in ["submitted", "mined"] {
        let line = output
            .lines()
            .find(|line| line.contains(&format!("outcome=\"{outcome}\"")))
            .expect("transaction lifecycle event");
        assert!(line.contains(&format!("swap_id={swap_id}")));
        assert!(line.contains(&format!("transaction_hash={transaction_hash}")));
    }
    assert!(!output.contains(&private_key));
}
