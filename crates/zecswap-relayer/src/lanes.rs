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
/// A bound on a payout's gas, the largest send of the relayer's own account: 0.88M on Ethereum,
/// 1.23M on Sepolia's repriced state.
pub(crate) const PAYOUT_GAS: u64 = 1_500_000;
/// Lanes are topped up after each of their sends, which alone spend their ETH, and this often
/// besides, as the fee cap their float is kept at follows the base fee.
const TOP_UP_INTERVAL: Duration = Duration::from_secs(120);

/// Claims and refunds; claim and refund locks, and readies; Railgun sends; fundings; deposits,
/// whose fees in the relayer's token collect in their account.
pub(crate) struct Lanes {
    pub(crate) claims: Settlement,
    pub(crate) locks: Settlement,
    pub(crate) sends: Settlement,
    pub(crate) funding: Settlement,
    pub(crate) deposits: Settlement,
}

impl Lanes {
    pub(crate) fn connect(
        rpc: &str,
        contract: Address,
        key: &PrivateKeySigner,
    ) -> anyhow::Result<Self> {
        Self::connect_metered(rpc, contract, key, &RpcMeter::default())
    }

    /// The lanes as `config` has them, counting their requests on `meter`: claims and refunds
    /// with their network's reveal headroom, broadcast only with `claim_margin` of their lock
    /// left, and they and deposits through their private RPCs, where it names them on the chain
    /// they read.
    pub(crate) async fn of(
        config: &Config,
        key: &PrivateKeySigner,
        meter: &RpcMeter,
    ) -> anyhow::Result<Self> {
        let mut lanes = Self::connect_metered(&config.evm_rpc, config.contract, key, meter)?;
        lanes
            .claims
            .set_reveal_headroom(config.network.reveal_headroom_wei());
        lanes.claims.reveal_with_margin(config.claim_margin);
        let private = [
            (
                &mut lanes.claims,
                config.reveals.as_ref().map(|reveals| &reveals.private_rpc),
            ),
            (
                &mut lanes.deposits,
                config
                    .deposits
                    .as_ref()
                    .and_then(|deposits| deposits.private_rpc.as_ref()),
            ),
        ];
        for (lane, rpc) in private {
            if let Some(rpc) = rpc {
                lane.broadcast_privately(rpc)?;
                lane.check_private_rpc().await?;
            }
        }
        Ok(lanes)
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
            deposits: lane("deposits")?,
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
        if let Some(deposits) = &config.deposits {
            used.push(("deposits", &self.deposits, deposits.max_gas_limit));
        }
        used
    }

    fn all(&self) -> [(&'static str, &Settlement); 5] {
        [
            ("claims", &self.claims),
            ("locks", &self.locks),
            ("sends", &self.sends),
            ("funding", &self.funding),
            ("deposits", &self.deposits),
        ]
    }
}

/// Sends each lane's tokens, the deposit fees it collected, and then its ETH, all but its gas, to
/// `to`. Run with the relayer stopped: its next top-up would fill them again.
pub async fn drain_lanes(
    config: &Config,
    key: &PrivateKeySigner,
    to: Address,
) -> anyhow::Result<()> {
    let lanes = Lanes::connect(&config.evm_rpc, config.contract, key)?;
    for (lane, settlement) in lanes.all() {
        let account = settlement.account().context("a lane sends")?;
        let tokens = settlement.token_balance(config.token, account).await?;
        if tokens > 0 {
            let hash = settlement.transfer_token(config.token, to, tokens).await?;
            info!(lane, %account, tokens, %hash, "drained a lane's tokens");
        }
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
                () = self.lanes.deposits.sent() => {}
            }
        }
    }

    /// Keeps each lane at what `top_up_transactions` of its largest transactions must hold at
    /// the fee cap it fills them with now, a reveal's on the claims lane: tops up one under half
    /// of it, from what the relayer's own account holds beyond as many payouts, and takes back
    /// what one holds beyond twice it, filled while gas cost more. Every balance is read in one
    /// call.
    pub(crate) async fn top_up(&self) -> anyhow::Result<Vec<(&'static str, U256)>> {
        let fee = U256::from(self.settlement.max_fee_per_gas().await?);
        let transactions = U256::from(self.config.top_up_transactions);
        let lanes = self.lanes.used(&self.config);
        let mut owners = lanes
            .iter()
            .map(|(_, settlement, _)| settlement.account().context("a lane sends"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        owners.push(self.account);
        let mut balances = self.settlement.eth_balances(&owners).await?;
        let own = balances
            .pop()
            .context("no balance for the relayer's own account")?;
        // Payouts, rescues and refund payouts go from the own account: a lane never takes their ETH.
        let mut spare = own.saturating_sub(fee * U256::from(PAYOUT_GAS) * transactions);
        let mut short = Vec::new();
        for ((lane, settlement, gas), (account, balance)) in
            lanes.into_iter().zip(owners.into_iter().zip(balances))
        {
            let keep =
                (fee + U256::from(settlement.reveal_headroom())) * U256::from(gas) * transactions;
            if balance > keep * U256::from(2) {
                let taken = match lane {
                    "deposits" => self.take_back_deposit_eth(keep).await?,
                    // A reveal sent privately before a restart may hold the next nonce.
                    _ if self.steps.after_earlier(settlement).await.is_err() => None,
                    _ => settlement.send_eth_above(self.account, keep).await?,
                };
                if let Some(amount) = taken {
                    info!(lane, %account, %amount, "took back a lane's ETH");
                    spare += amount;
                }
            } else if balance < keep / U256::from(2) {
                short.push((lane, account, keep - balance));
            }
        }
        let mut sent = Vec::new();
        for (lane, account, amount) in short {
            if spare < amount {
                warn!(
                    lane,
                    "the relayer's own account has too little ETH beyond its payouts' to top this lane up"
                );
                continue;
            }
            self.settlement.send_eth(account, amount).await?;
            spare -= amount;
            info!(lane, %account, %amount, "topped up a lane");
            sent.push((lane, amount));
        }
        Ok(sent)
    }

    /// The account that signed a recorded send: sends recorded before the lanes were signed by
    /// the relayer's own account.
    pub(crate) fn sender_of(&self, signed: &Signed) -> &Settlement {
        let signer = signed.signer();
        [&self.lanes.sends, &self.lanes.deposits]
            .into_iter()
            .find(|lane| signer.is_some() && signer == lane.account())
            .unwrap_or(&self.settlement)
    }

    pub(crate) fn lane_accounts(&self) -> BTreeMap<&'static str, Address> {
        self.lanes
            .used(&self.config)
            .into_iter()
            .filter_map(|(lane, settlement, _)| Some((lane, settlement.account()?)))
            .collect()
    }
}
