//! An account's transactions mine in order, so each kind of send has its own account, derived
//! from the relayer's key and kept in ETH by the relayer's own account.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, U256, keccak256};
use anyhow::Context as _;
use tracing::{info, warn};
use zecswap_chain::evm::railgun::Signed;
use zecswap_chain::evm::{PrivateKeySigner, Settlement};
use zeroize::Zeroizing;

use crate::{Config, Relayer};

/// A bound on a lock's or claim's gas, Sepolia's repriced state included.
const SETTLEMENT_GAS: u64 = 1_000_000;
const TOP_UP_INTERVAL: Duration = Duration::from_secs(30);

/// Claims and refunds; claim and refund locks, and readies; Railgun sends; fundings.
pub(crate) struct Lanes {
    pub(crate) claims: Settlement,
    pub(crate) locks: Settlement,
    pub(crate) sends: Settlement,
    pub(crate) funding: Settlement,
}

impl Lanes {
    pub(crate) fn connect(
        rpc: &str,
        contract: Address,
        key: &PrivateKeySigner,
    ) -> anyhow::Result<Self> {
        let lane = |name| -> anyhow::Result<Settlement> {
            Ok(Settlement::connect(rpc, contract, derive(key, name)?)?)
        };
        Ok(Self {
            claims: lane("claims")?,
            locks: lane("locks")?,
            sends: lane("sends")?,
            funding: lane("funding")?,
        })
    }

    /// The lanes `config` sends on, with the gas of each one's largest transaction.
    fn used<'a>(&'a self, config: &Config) -> Vec<(&'static str, &'a Settlement, u64)> {
        let mut used = vec![
            ("claims", &self.claims, SETTLEMENT_GAS),
            ("locks", &self.locks, SETTLEMENT_GAS),
        ];
        if let Some(sends) = &config.railgun_sends {
            used.push(("sends", &self.sends, sends.max_gas_limit));
        }
        if let Some(funding) = &config.reverse_funding {
            used.push(("funding", &self.funding, funding.max_gas_limit));
        }
        used
    }
}

pub(crate) fn derive(key: &PrivateKeySigner, lane: &str) -> anyhow::Result<PrivateKeySigner> {
    let mut material = Zeroizing::new(b"ZecSwap relayer lane".to_vec());
    material.extend_from_slice(key.to_bytes().as_slice());
    material.extend_from_slice(lane.as_bytes());
    PrivateKeySigner::from_bytes(&keccak256(material.as_slice()))
        .with_context(|| format!("deriving the {lane} account's key"))
}

impl Relayer {
    pub async fn run_top_ups(self: Arc<Self>) {
        loop {
            if let Err(error) = self.top_up().await {
                // RPC errors can carry the node's URL: log the kind alone.
                let failure_kind = error
                    .downcast_ref()
                    .map_or("internal", crate::monitor::failure_kind);
                warn!(
                    operation = "top_up",
                    failure_kind, "topping up the sending accounts failed; retrying"
                );
            }
            tokio::time::sleep(TOP_UP_INTERVAL).await;
        }
    }

    /// Tops up each lane under half of what `top_up_transactions` cost at the gas price now.
    pub(crate) async fn top_up(&self) -> anyhow::Result<Vec<(&'static str, U256)>> {
        let gas_price = U256::from(self.settlement.gas_price().await?);
        let mut sent = Vec::new();
        for (lane, settlement, gas) in self.lanes.used(&self.config) {
            let account = settlement.account().context("a lane sends")?;
            let keep = gas_price * U256::from(gas) * U256::from(self.config.top_up_transactions);
            let balance = self.settlement.eth_balance(account).await?;
            if balance >= keep / U256::from(2) {
                continue;
            }
            let amount = keep - balance;
            if self.settlement.eth_balance(self.account).await? < amount {
                warn!(
                    lane,
                    "the relayer's own account has too little ETH to top this lane up"
                );
                continue;
            }
            self.settlement.send_eth(account, amount).await?;
            info!(lane, %account, %amount, "topped up a lane");
            sent.push((lane, amount));
        }
        Ok(sent)
    }

    /// Sends recorded before the lanes were signed by the relayer's own account.
    pub(crate) fn sender_of(&self, signed: &Signed) -> &Settlement {
        if signed.signer() == self.lanes.sends.account() {
            &self.lanes.sends
        } else {
            &self.settlement
        }
    }

    pub(crate) fn lane_accounts(&self) -> BTreeMap<&'static str, Address> {
        self.lanes
            .used(&self.config)
            .into_iter()
            .filter_map(|(lane, settlement, _)| Some((lane, settlement.account()?)))
            .collect()
    }
}
