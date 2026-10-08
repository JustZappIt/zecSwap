//! What the relayer exports for monitoring: for each operation, the transactions it sent, the
//! requests it refused and those that failed, and the gas its transactions burned. Failures are
//! named by kind alone: their messages can carry RPC URLs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::Address;
use serde::Serialize;
use zecswap_api::server::MonitorToken;

/// Every operation, by the name its requests are logged under.
const OPERATIONS: [&str; 11] = [
    "lock_claim",
    "claim",
    "payout",
    "rescue",
    "fund_reverse",
    "ready_reverse",
    "lock_reverse_refund",
    "refund_reverse",
    "reverse_refund_payout",
    "rescue_reverse",
    "railgun_transact",
];

pub(crate) struct Monitor {
    pub(crate) token: Arc<MonitorToken>,
    started_at: u64,
    state: Mutex<State>,
}

struct State {
    operations: BTreeMap<&'static str, Operation>,
    last_failure: Option<Failure>,
}

#[derive(Clone, Copy, Default)]
struct Operation {
    sent: u64,
    refused: u64,
    failed: u64,
    gas_used: u128,
    gas_cost_wei: u128,
    last_at: Option<u64>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Failure {
    operation: &'static str,
    at: u64,
    kind: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorSnapshot {
    schema_version: u32,
    generated_at: u64,
    uptime_seconds: u64,
    relayer: Address,
    chain_id: u64,
    contract: Address,
    token: Address,
    maker: Address,
    fees: Fees,
    operations: BTreeMap<&'static str, OperationSnapshot>,
    last_failure: Option<Failure>,
}

#[derive(Serialize)]
pub(crate) struct Fees {
    pub(crate) payout: String,
    pub(crate) funding: Option<String>,
    pub(crate) sends: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationSnapshot {
    sent: u64,
    refused: u64,
    failed: u64,
    gas_used: String,
    gas_cost_wei: String,
    last_at: Option<u64>,
}

/// What the snapshot says of the relayer itself.
pub(crate) struct Deployment {
    pub(crate) relayer: Address,
    pub(crate) chain_id: u64,
    pub(crate) contract: Address,
    pub(crate) token: Address,
    pub(crate) maker: Address,
    pub(crate) fees: Fees,
}

impl Monitor {
    pub(crate) fn new(token: MonitorToken) -> Self {
        Self {
            token: Arc::new(token),
            started_at: now(),
            state: Mutex::new(State {
                operations: OPERATIONS
                    .into_iter()
                    .map(|operation| (operation, Operation::default()))
                    .collect(),
                last_failure: None,
            }),
        }
    }

    /// `transactions` sent and mined for `operation`.
    pub(crate) fn sent(&self, operation: &'static str, transactions: u64) {
        self.record(operation, |counts| counts.sent += transactions);
    }

    /// The gas one of `operation`'s transactions burned, and that gas in wei, once its receipt
    /// has been read.
    pub(crate) fn burned(&self, operation: &'static str, gas: u64, wei: u128) {
        let mut state = self.state.lock().unwrap();
        let counts = state.operations.entry(operation).or_default();
        counts.gas_used += u128::from(gas);
        counts.gas_cost_wei += wei;
    }

    pub(crate) fn refused(&self, operation: &'static str) {
        self.record(operation, |counts| counts.refused += 1);
    }

    pub(crate) fn failed(&self, operation: &'static str, kind: &'static str) {
        let at = self.record(operation, |counts| counts.failed += 1);
        self.state.lock().unwrap().last_failure = Some(Failure {
            operation,
            at,
            kind,
        });
    }

    fn record(&self, operation: &'static str, count: impl FnOnce(&mut Operation)) -> u64 {
        let at = now();
        let mut state = self.state.lock().unwrap();
        let counts = state.operations.entry(operation).or_default();
        count(counts);
        counts.last_at = Some(at);
        at
    }

    pub(crate) fn snapshot(&self, deployment: Deployment) -> MonitorSnapshot {
        let state = self.state.lock().unwrap();
        let generated_at = now();
        MonitorSnapshot {
            schema_version: 1,
            generated_at,
            uptime_seconds: generated_at.saturating_sub(self.started_at),
            relayer: deployment.relayer,
            chain_id: deployment.chain_id,
            contract: deployment.contract,
            token: deployment.token,
            maker: deployment.maker,
            fees: deployment.fees,
            operations: state
                .operations
                .iter()
                .map(|(operation, counts)| {
                    (
                        *operation,
                        OperationSnapshot {
                            sent: counts.sent,
                            refused: counts.refused,
                            failed: counts.failed,
                            gas_used: counts.gas_used.to_string(),
                            gas_cost_wei: counts.gas_cost_wei.to_string(),
                            last_at: counts.last_at,
                        },
                    )
                })
                .collect(),
            last_failure: state.last_failure,
        }
    }
}

/// What kind of failure `error` is: a transaction that reverted, one sent whose outcome is
/// unknown, or anything else.
pub(crate) fn failure_kind(error: &zecswap_chain::Error) -> &'static str {
    match error {
        zecswap_chain::Error::Reverted(_) => "reverted",
        zecswap_chain::Error::Unconfirmed(_) => "unconfirmed",
        _ => "internal",
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}
