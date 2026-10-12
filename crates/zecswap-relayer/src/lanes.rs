//! An account's transactions mine in order, so each kind of send has its own account, derived
//! from the relayer's key and kept in ETH by the relayer's own account.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, U256, keccak256};
use anyhow::Context as _;
use tracing::{info, warn};
use zecswap_chain::evm::railgun::Signed;
use zecswap_chain::evm::{PrivateKeySigner, RpcMeter, Settlement};
use zeroize::Zeroizing;

use crate::{Config, Relayer};

/// A bound on a lock's or claim's gas, Sepolia's repriced state included.
pub(crate) const SETTLEMENT_GAS: u64 = 1_000_000;
/// Lanes are topped up after each of their sends, which alone spend their ETH, and this often
/// besides, as the fee cap their float is kept at follows the base fee.
const TOP_UP_INTERVAL: Duration = Duration::from_secs(120);

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
        Self::connect_metered(rpc, contract, key, &RpcMeter::default())
    }

    /// Every lane counts its requests on `meter`.
    pub(crate) fn connect_metered(
        rpc: &str,
        contract: Address,
        key: &PrivateKeySigner,
        meter: &RpcMeter,
    ) -> anyhow::Result<Self> {
        let lane = |name| -> anyhow::Result<Settlement> {
            Ok(Settlement::connect_metered(
                rpc,
                contract,
                derive(key, name)?,
                meter,
            )?)
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

/// Sends each lane's ETH, all but its gas, to `to`. Run with the relayer stopped: its next top-up
/// would fill them again.
pub async fn drain_lanes(
    config: &Config,
    key: &PrivateKeySigner,
    to: Address,
) -> anyhow::Result<()> {
    let lanes = Lanes::connect(&config.evm_rpc, config.contract, key)?;
    for (lane, settlement) in [
        ("claims", &lanes.claims),
        ("locks", &lanes.locks),
        ("sends", &lanes.sends),
        ("funding", &lanes.funding),
    ] {
        let account = settlement.account().context("a lane sends")?;
        let balance = settlement.eth_balance(account).await?;
        // Twice a transfer's gas at today's cap, so a base fee that rises meanwhile still fits.
        let gas = U256::from(2 * 21_000) * U256::from(settlement.max_fee_per_gas().await?);
        if balance <= gas {
            info!(lane, %account, %balance, "nothing to drain");
            continue;
        }
        let amount = balance - gas;
        let hash = settlement.send_eth(to, amount).await?;
        info!(lane, %account, %amount, %hash, "drained a lane");
    }
    Ok(())
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
            tokio::select! {
                () = tokio::time::sleep(TOP_UP_INTERVAL) => {}
                () = self.lanes.claims.sent() => {}
                () = self.lanes.locks.sent() => {}
                () = self.lanes.sends.sent() => {}
                () = self.lanes.funding.sent() => {}
            }
        }
    }

    /// Tops up each lane under half of what `top_up_transactions` of its largest transactions
    /// must hold at the fee cap sends are filled with now. Every balance is read in one call.
    pub(crate) async fn top_up(&self) -> anyhow::Result<Vec<(&'static str, U256)>> {
        let fee = U256::from(self.settlement.max_fee_per_gas().await?);
        let lanes = self.lanes.used(&self.config);
        let mut owners = lanes
            .iter()
            .map(|(_, settlement, _)| settlement.account().context("a lane sends"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        owners.push(self.account);
        let mut balances = self.settlement.eth_balances(&owners).await?;
        let mut own = balances
            .pop()
            .context("no balance for the relayer's own account")?;
        let mut sent = Vec::new();
        for ((lane, _, gas), (account, balance)) in
            lanes.into_iter().zip(owners.into_iter().zip(balances))
        {
            let keep = fee * U256::from(gas) * U256::from(self.config.top_up_transactions);
            if balance >= keep / U256::from(2) {
                continue;
            }
            let amount = keep - balance;
            if own < amount {
                warn!(
                    lane,
                    "the relayer's own account has too little ETH to top this lane up"
                );
                continue;
            }
            self.settlement.send_eth(account, amount).await?;
            own -= amount;
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
