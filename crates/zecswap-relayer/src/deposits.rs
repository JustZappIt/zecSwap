//! Deposits: a deposit address's tokens shielded into its owner's Railgun balance, on the owner's
//! EIP-3009 authorization of them to the Deposits contract, whose nonce commits to the note, the
//! fee and the relayer's deposits account; and rescues of what Railgun sent back to the address's
//! vault. The relayer checks all it can without the chain, then takes the address's turn, checks
//! the chain and simulates, and records each send before its broadcast, so the same authorization
//! never sends twice. Once one mines, what it cost and earned is recorded beside it, for the
//! monitor, with a Telegram message where alerts are on. The relayer never watches deposit
//! addresses: wallets do, and post when one holds enough.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{Context as _, ensure};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use tracing::{info, warn};
use zecswap_alerts::{Notification, Telegram, explorer, time, units};
use zecswap_api::Network;
use zecswap_api::relayer::{DepositTerms, RescueDeposit, Sent, ShieldDeposit, deposit_fee};
use zecswap_chain::evm::deposits::{Call, Deployment, DepositPolicy, Rescue, Shield};
use zecswap_chain::evm::railgun::{Later, SendError, Signed, Status};
use zecswap_chain::evm::{Settlement, endpoint};
use zecswap_core::{DepositsDomain, ReceiveAuthorization, RescueAuthorization, signer};
use zecswap_prices::{Asset, Unpriced};
use zecswap_railgun::ShieldNote;

use crate::pricing::SwapFee;
use crate::sends::{Fate, REVERTED_COOLDOWN, Sending, UNMINED_RECHECK, covers, now, word};
use crate::{Config, DepositsConfig, Relayer, RelayerError, Result};

/// How long an authorization must still be valid when posted: time to check, send and land it.
pub(crate) const LANDING_MARGIN: u64 = 15 * 60;
/// How long past an authorization's `validAfter` must be: USDC takes it only in a block after
/// it, and a block of the current slot is stamped up to a slot before now.
const VALID_AFTER_MARGIN: u64 = 2 * 60;
/// A recorded deposit that can't land within this many seconds is never broadcast again.
const EXPIRING: u64 = 60;
/// What a `503` asks to wait, in seconds: for gas to fall, for the account deposits are sent from
/// to be topped up (the lanes' interval), for deposits on their way to mine.
const GAS_PRICE_WAIT: u64 = 300;
const ETH_WAIT: u64 = 120;
const PENDING_WAIT: u64 = 30;
/// Railgun notes hold at most uint120.
const MAX_VALUE: u128 = (1 << 120) - 1;
/// Past the private RPC's blocks, those after which a deposit it never landed is taken as dropped:
/// a margin for missed slots.
const PRIVATE_MARGIN_BLOCKS: u64 = 5;
const SLOT: u64 = 12;
/// The newest deposits read for one sent privately that may still land, or the last mined.
const NEWEST: usize = 8;
/// The most deposits one export carries; older ones need a later `since`.
const LEDGER_LIMIT: usize = 1000;
const SHIELD: &str = "shield_deposit";
const RESCUE: &str = "rescue_deposit";

pub(crate) struct Deposits {
    config: DepositsConfig,
    policy: DepositPolicy,
    domain: DepositsDomain,
    deployment: Deployment,
    /// The account deposits are sent from, which their nonces and signatures name.
    account: Address,
    journal: Journal,
    pub(crate) telegram: Telegram,
    /// When each long-unmined deposit's receipt was last looked for.
    unmined: Mutex<HashMap<B256, u64>>,
    /// One deposit at a time from the count of those on their way to its broadcast, so the count
    /// holds however many arrive at once.
    sending: tokio::sync::Mutex<()>,
}

/// Which of the contract's calls a deposit is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Shield,
    Rescue,
}

/// What a request asks, as the chain must have it before it is sent.
enum Asked {
    /// The address holds `value`.
    Shield { value: u128 },
    /// The vault's next rescue is `nonce`.
    Rescue { nonce: u64 },
}

/// A request checked as far as it can be without the chain, to send in its address's turn.
struct Checked {
    asked: Asked,
    from: Address,
    /// A shield's authorization nonce, a rescue's nonce: what the same request posted again
    /// shares.
    id: B256,
    fee: u128,
    call: Bytes,
    /// Where the relayer prices gas, the rate the fee must cover it at.
    rate: Option<u128>,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Shield => "shield",
            Kind::Rescue => "rescue",
        }
    }

    fn named(name: &str) -> Option<Self> {
        match name {
            "shield" => Some(Kind::Shield),
            "rescue" => Some(Kind::Rescue),
            _ => None,
        }
    }
}

impl Asked {
    fn kind(&self) -> Kind {
        match self {
            Asked::Shield { .. } => Kind::Shield,
            Asked::Rescue { .. } => Kind::Rescue,
        }
    }
}

impl Deposits {
    /// Takes deposits through `config.contract` once it is the one the relayer's config says:
    /// in the relayer's token, into the settlement's Railgun, and on mainnet screening
    /// depositors. `account` is the one they are sent from.
    pub(crate) async fn open(
        config: &DepositsConfig,
        relayer: &Config,
        settlement: &Settlement,
        account: Address,
        railgun: Address,
        telegram: Telegram,
    ) -> anyhow::Result<Self> {
        ensure!(
            config.max_gas_limit > 0 && config.max_gas_price_wei > 0,
            "deposits gas caps must be positive"
        );
        ensure!(config.max_pending > 0, "max_pending must be positive");
        check_private(config, relayer.network)?;
        ensure!(config.fee_bps < 10_000, "fee_bps must be under 10000");
        ensure!(
            config.max_validity > LANDING_MARGIN,
            "max_validity must be over the {LANDING_MARGIN} seconds an authorization must have left"
        );
        let least = deposit_fee(config.fee.into(), config.fee_bps, config.min_value.into());
        ensure!(
            u128::from(config.min_value) > least,
            "min_value must be more than its fee, {least}"
        );
        let deployment = settlement
            .deposits(config.contract)
            .await
            .context("reading the Deposits contract")?;
        ensure!(
            deployment.token == relayer.token,
            "the Deposits contract takes {}, not the relayer's token {}",
            deployment.token,
            relayer.token
        );
        ensure!(
            !railgun.is_zero() && deployment.railgun == railgun,
            "the Deposits contract shields into {}, not the settlement's Railgun {railgun}",
            deployment.railgun
        );
        if relayer.network == Network::Mainnet {
            let oracle = deployment
                .sanctions
                .context("on mainnet the Deposits contract must screen depositors")?;
            ensure!(
                settlement.has_code(oracle).await?,
                "the sanctions oracle {oracle} has no code"
            );
        }
        info!(
            contract = %config.contract,
            %account,
            token = deployment.token_domain.name,
            sanctions = ?deployment.sanctions,
            private = ?config.private_rpc.as_deref().map(endpoint),
            alerts = telegram.enabled(),
            "taking deposits"
        );
        Ok(Self {
            policy: DepositPolicy {
                contract: config.contract,
                max_gas_limit: config.max_gas_limit,
                max_gas_price_wei: config.max_gas_price_wei.into(),
            },
            domain: DepositsDomain {
                chain_id: deployment.token_domain.chain_id,
                contract: config.contract.into(),
            },
            deployment,
            account,
            journal: Journal::open(&config.journal)?,
            telegram,
            config: config.clone(),
            unmined: Mutex::default(),
            sending: tokio::sync::Mutex::new(()),
        })
    }

    /// The terms, at `fee` before its percentage.
    pub(crate) fn terms(&self, fee: u128, fee_expires_at: Option<u64>) -> DepositTerms {
        DepositTerms {
            contract: self.policy.contract,
            relayer: self.account,
            token: self.deployment.token,
            fee,
            fee_bps: self.config.fee_bps,
            fee_expires_at,
            min_value: self.config.min_value.into(),
            max_validity: self.config.max_validity,
            max_gas_price_wei: self.policy.max_gas_price_wei,
        }
    }

    fn check_deployment(&self, chain_id: u64, request: (u64, Address)) -> Result<()> {
        if request != (chain_id, self.policy.contract) {
            return Err(RelayerError::Rejected(
                "wrong chain or Deposits contract".into(),
            ));
        }
        Ok(())
    }
}

/// Mainnet deposits go through a private RPC, and deposits through one go one at a time.
pub(crate) fn check_private(config: &DepositsConfig, network: Network) -> anyhow::Result<()> {
    match config.private_rpc {
        None => ensure!(
            network != Network::Mainnet,
            "on mainnet deposits go through a revert-protecting private RPC: set private_rpc"
        ),
        Some(_) => ensure!(
            config.max_pending == 1,
            "with private_rpc max_pending must be 1: a deposit the private RPC drops holds back every later nonce"
        ),
    }
    Ok(())
}

/// What a `503` that a send waits on asks.
pub(crate) fn later(later: Later) -> RelayerError {
    let wait = match later {
        Later::GasPrice => GAS_PRICE_WAIT,
        Later::Eth => ETH_WAIT,
    };
    RelayerError::Busy(later.reason(), wait)
}

impl Relayer {
    /// Shields a deposit address's tokens on its authorization: the transaction, or the one a
    /// repeat of the same authorization sent before.
    #[tracing::instrument(skip_all, fields(operation = SHIELD, from = %request.from), err(level = "warn"))]
    pub async fn shield_deposit(self: &Arc<Self>, request: ShieldDeposit) -> Result<Sent> {
        let checked = match self.check_shield(&request).await {
            Ok(checked) => checked,
            Err(error) => return self.observe(SHIELD, Err(error)),
        };
        self.deposit_in_turn(SHIELD, checked).await
    }

    /// Shields what Railgun sent back to a deposit address's vault again, to the note it signed.
    #[tracing::instrument(skip_all, fields(operation = RESCUE, from = %request.from), err(level = "warn"))]
    pub async fn rescue_deposit(self: &Arc<Self>, request: RescueDeposit) -> Result<Sent> {
        let checked = match self.check_rescue(&request).await {
            Ok(checked) => checked,
            Err(error) => return self.observe(RESCUE, Err(error)),
        };
        self.deposit_in_turn(RESCUE, checked).await
    }

    fn deposits(&self) -> Result<&Deposits> {
        self.deposits
            .as_ref()
            .ok_or_else(|| RelayerError::Rejected("this relayer takes no deposits".into()))
    }

    /// Requests for one address take turns.
    async fn deposit_in_turn(
        self: &Arc<Self>,
        operation: &'static str,
        checked: Checked,
    ) -> Result<Sent> {
        let relayer = Arc::clone(self);
        let key = checked.from.into_word();
        self.in_line(
            operation,
            key,
            async move { relayer.send_deposit(checked).await },
            Self::observe_sending,
        )
        .await
    }

    /// Everything a shield must be that needs no RPC: the deployment, its bounds and signature,
    /// and a fee the relayer honors.
    async fn check_shield(&self, request: &ShieldDeposit) -> Result<Checked> {
        let deposits = self.deposits()?;
        deposits.check_deployment(self.domain.chain_id, (request.chain_id, request.contract))?;
        let config = &deposits.config;
        let refuse = |reason: String| Err(RelayerError::Rejected(reason));
        let now = now();
        if request.value < u128::from(config.min_value) {
            return refuse(format!("a deposit moves at least {}", config.min_value));
        }
        if request.value > MAX_VALUE {
            return refuse("a deposit moves at most what a Railgun note holds".into());
        }
        if request.fee >= request.value {
            return refuse("the fee leaves nothing to shield".into());
        }
        if request.valid_after.saturating_add(VALID_AFTER_MARGIN) > now {
            return refuse("the authorization's validAfter is not two minutes past: sign 0".into());
        }
        if request.valid_before < now.saturating_add(LANDING_MARGIN) {
            return refuse("the authorization expires too soon to land: sign it again".into());
        }
        if request.valid_before > now.saturating_add(config.max_validity) {
            return refuse(format!(
                "the authorization is valid for longer than the relayer takes, {} seconds",
                config.max_validity
            ));
        }
        let note = ShieldNote::from(&request.note);
        let nonce =
            deposits
                .domain
                .deposit_nonce(&note.words(), request.fee, &deposits.account.into());
        let digest =
            deposits
                .deployment
                .token_domain
                .receive_with_authorization(&ReceiveAuthorization {
                    from: request.from.into(),
                    to: request.contract.into(),
                    value: request.value,
                    valid_after: request.valid_after,
                    valid_before: request.valid_before,
                    nonce,
                });
        if signer(&digest, &request.signature.0) != Some(request.from.into()) {
            return refuse(
                "not signed by the deposit address for this note, fee and relayer".into(),
            );
        }
        let least = deposit_fee(
            self.honored_swap_fee(SwapFee::Deposit).await,
            config.fee_bps,
            request.value,
        );
        if request.fee < least {
            return refuse(format!(
                "a fee of {} is less than the relayer's {least}: fetch the terms again and sign again",
                request.fee
            ));
        }
        let call = Shield {
            from: request.from,
            value: request.value,
            valid_after: request.valid_after,
            valid_before: request.valid_before,
            note: &note,
            fee: request.fee,
            signature: &request.signature.0,
        }
        .calldata();
        Ok(Checked {
            asked: Asked::Shield {
                value: request.value,
            },
            from: request.from,
            id: nonce.into(),
            fee: request.fee,
            call,
            rate: self.honored_deposit_rate().await?,
        })
    }

    async fn check_rescue(&self, request: &RescueDeposit) -> Result<Checked> {
        let deposits = self.deposits()?;
        deposits.check_deployment(self.domain.chain_id, (request.chain_id, request.contract))?;
        let refuse = |reason: &str| Err(RelayerError::Rejected(reason.into()));
        let now = now();
        if request.deadline < now.saturating_add(LANDING_MARGIN) {
            return refuse("the rescue expires too soon to land: sign it again");
        }
        if request.deadline > now.saturating_add(deposits.config.max_validity) {
            return refuse("the rescue is valid for longer than the relayer takes");
        }
        let note = ShieldNote::from(&request.note);
        let authorization = RescueAuthorization {
            nonce: request.nonce,
            deadline: request.deadline,
        };
        let digest = deposits.domain.rescue(
            &request.from.into(),
            &note.commitment(),
            &deposits.account.into(),
            request.fee,
            authorization,
        );
        if signer(&digest, &request.signature.0) != Some(request.from.into()) {
            return refuse("not signed by the deposit address for this note, fee and relayer");
        }
        let least = self.honored_swap_fee(SwapFee::Deposit).await;
        if request.fee < least {
            return Err(RelayerError::Rejected(format!(
                "a fee of {} is less than the relayer's {least}: fetch the terms again and sign again",
                request.fee
            )));
        }
        let call = Rescue {
            from: request.from,
            note: &note,
            fee: request.fee,
            authorization,
            signature: &request.signature.0,
        }
        .calldata();
        Ok(Checked {
            asked: Asked::Rescue {
                nonce: request.nonce,
            },
            from: request.from,
            id: U256::from(request.nonce).into(),
            fee: request.fee,
            call,
            rate: self.honored_deposit_rate().await?,
        })
    }

    /// Where gas prices fees, the rate a deposit's fee must cover its gas at: none can be sent
    /// while there is none.
    async fn honored_deposit_rate(&self) -> Result<Option<u128>> {
        match &self.pricing {
            Some(pricing) => Ok(Some(
                pricing
                    .honored_rate(now())
                    .await
                    .ok_or(RelayerError::Unpriced)?,
            )),
            None => Ok(None),
        }
    }

    /// In the address's turn: what the journal says of its earlier deposits, then the chain, then
    /// the simulation, and the journaled send.
    async fn send_deposit(&self, checked: Checked) -> Result<Sending> {
        let deposits = self.deposits()?;
        let (chain_id, now) = (self.domain.chain_id, now());
        let kind = checked.asked.kind();
        let (mut in_flight, mut unknown, mut cooling) = (Vec::new(), false, 0);
        for entry in deposits.journal.of(chain_id, checked.from)? {
            let same = entry.kind == kind && entry.id == checked.id;
            match self.deposit_fate(deposits, &entry).await? {
                Fate::Pending | Fate::Landed if same => {
                    return Ok(Sending::Again(Sent {
                        transactions: vec![entry.signed.hash],
                    }));
                }
                Fate::Pending => in_flight.push(entry.signed.hash),
                Fate::Landed => {}
                Fate::Reverted => {
                    let left = (entry.sent_at + REVERTED_COOLDOWN).saturating_sub(now);
                    cooling = cooling.max(left);
                }
                Fate::Dead => deposits.journal.forget(chain_id, entry.signed.hash)?,
                Fate::Unknown => unknown = true,
            }
        }
        if unknown {
            return Err(RelayerError::Unsettled(
                "a deposit of this address has no known outcome yet; post the same request again later",
            ));
        }
        if cooling > 0 {
            return Err(RelayerError::Reverted(cooling));
        }
        if !in_flight.is_empty() {
            return Err(RelayerError::Conflict(
                in_flight,
                "another deposit of this address is on its way",
            ));
        }

        // What an address without the funds fails is read first: it costs one read.
        let lane = &self.lanes.deposits;
        let refuse = |reason: &str| Err(RelayerError::Rejected(reason.into()));
        let (contract, token) = (deposits.policy.contract, deposits.deployment.token);
        match checked.asked {
            Asked::Shield { value } => {
                if lane.token_balance(token, checked.from).await? < value {
                    return refuse("the deposit address holds less than it authorizes");
                }
            }
            Asked::Rescue { nonce } => {
                if lane.deposit_rescue_nonce(contract, checked.from).await? != nonce {
                    return refuse("the rescue's nonce is not its vault's next");
                }
                let vault = lane.deposit_vault(contract, checked.from).await?;
                if !lane.has_code(vault).await?
                    || lane.token_balance(token, vault).await? <= checked.fee
                {
                    return refuse("the address's vault holds nothing to rescue beyond the fee");
                }
            }
        }
        if lane.has_code(checked.from).await? {
            return refuse("the deposit address has code: deposits are from plain accounts");
        }
        if let Asked::Shield { .. } = checked.asked
            && lane
                .authorization_used(token, checked.from, checked.id)
                .await?
        {
            return Err(RelayerError::Conflict(vec![], "the authorization is used"));
        }
        if let Some(oracle) = deposits.deployment.sanctions
            && lane.sanctioned(oracle, checked.from).await?
        {
            return refuse("the deposit address is sanctioned");
        }

        let _sending = deposits.sending.lock().await;
        let least_nonce = if lane.private_blocks().is_some() {
            self.private_nonce(deposits)
                .await?
                .ok_or(RelayerError::Busy(
                    "the relayer has a deposit on its way, and sends one at a time",
                    PENDING_WAIT,
                ))?
        } else {
            let (mined, sent) = lane.nonces().await?;
            if sent.saturating_sub(mined) >= deposits.config.max_pending {
                return Err(RelayerError::Busy(
                    "the relayer has as many deposits on their way as it sends at once",
                    PENDING_WAIT,
                ));
            }
            0
        };
        let (from, id, fee, rate) = (checked.from, checked.id, checked.fee, checked.rate);
        let sent = lane
            .send_deposit(
                &deposits.policy,
                checked.call,
                least_nonce,
                |gas, gas_price| covers(fee, rate, gas, gas_price),
                |signed| {
                    deposits
                        .journal
                        .record(chain_id, from, kind, id, signed)
                        .map_err(|e| zecswap_chain::Error::Journal(e.to_string()))
                },
            )
            .await;
        let tx = match sent {
            Ok(tx) => tx,
            Err(SendError::Refused(hash)) => {
                deposits.journal.forget(chain_id, hash)?;
                return Err(RelayerError::Busy(
                    "the private RPC refused the deposit: post it again",
                    PENDING_WAIT,
                ));
            }
            Err(error) => return Err(crate::sends::refused(error)),
        };
        info!(%tx, kind = kind.name(), "sent a deposit");
        Ok(Sending::New(Sent {
            transactions: vec![tx],
        }))
    }

    /// What became of a recorded deposit: as its ledger has it once mined, else as the chain.
    async fn deposit_fate(&self, deposits: &Deposits, entry: &Entry) -> Result<Fate> {
        let chain_id = self.domain.chain_id;
        if let Some(succeeded) = deposits.journal.outcome(chain_id, entry.signed.hash)? {
            return Ok(if succeeded {
                Fate::Landed
            } else {
                Fate::Reverted
            });
        }
        let expired = entry
            .signed
            .call()
            .and_then(|(_, _, data)| Call::decode(&data))
            .is_none_or(|call| call.expires_at() <= now().saturating_add(EXPIRING));
        match self.lanes.deposits.private_blocks() {
            Some(blocks) => self.private_fate(entry, expired, blocks).await,
            None => self.fate(&entry.signed, entry.sent_at, expired).await,
        }
    }

    /// What became of a deposit sent through the private RPC: the public node knows of it only
    /// once it mines, and the private RPC never sends again one it dropped, so it is never
    /// broadcast again.
    async fn private_fate(&self, entry: &Entry, expired: bool, blocks: u64) -> Result<Fate> {
        let dropped =
            now().saturating_sub(entry.sent_at) >= (blocks + PRIVATE_MARGIN_BLOCKS) * SLOT;
        let lane = &self.lanes.deposits;
        Ok(match lane.transaction_status(entry.signed.hash).await? {
            Status::Mined { succeeded: true } => Fate::Landed,
            Status::Mined { succeeded: false } => Fate::Reverted,
            Status::Pending => Fate::Pending,
            Status::Unknown if expired || dropped => Fate::Dead,
            Status::Unknown => Fate::Pending,
        })
    }

    /// With deposits sent privately, the least nonce the next may take: past the last one mined,
    /// which a node a block behind may not count yet. None while one may still land: the public
    /// node can't see it, and the next would take its nonce.
    async fn private_nonce(&self, deposits: &Deposits) -> Result<Option<u64>> {
        let chain_id = self.domain.chain_id;
        for entry in deposits.journal.newest(chain_id, NEWEST)? {
            match self.deposit_fate(deposits, &entry).await? {
                Fate::Pending | Fate::Unknown => return Ok(None),
                Fate::Landed | Fate::Reverted => return Ok(Some(entry.signed.nonce + 1)),
                Fate::Dead => deposits.journal.forget(chain_id, entry.signed.hash)?,
            }
        }
        Ok(Some(0))
    }

    /// Takes back what the deposits account holds beyond `keep`; never while a deposit it sent
    /// privately may still land, whose nonce the transfer would take.
    pub(crate) async fn take_back_deposit_eth(&self, keep: U256) -> anyhow::Result<Option<U256>> {
        let lane = &self.lanes.deposits;
        let private = self
            .deposits
            .as_ref()
            .filter(|_| lane.private_blocks().is_some());
        let Some(deposits) = private else {
            return Ok(lane.send_eth_above(self.account, keep).await?);
        };
        let _sending = deposits.sending.lock().await;
        if self.private_nonce(deposits).await?.is_none() {
            return Ok(None);
        }
        Ok(lane.send_eth_above(self.account, keep).await?)
    }

    /// Records each deposit's receipt once it mined, a bounded number a pass, and values them.
    pub(crate) async fn deposit_cost_pass(&self) -> anyhow::Result<()> {
        let Some(deposits) = &self.deposits else {
            return Ok(());
        };
        let chain_id = self.domain.chain_id;
        let token = deposits.deployment.token;
        let now = now();
        for entry in deposits.journal.unread(chain_id, 20)? {
            let hash = entry.signed.hash;
            let looked = deposits.unmined.lock().unwrap().get(&hash).copied();
            if looked.is_some_and(|at| now.saturating_sub(at) < UNMINED_RECHECK) {
                continue;
            }
            let Some(facts) = self.settlement.transaction_facts(hash, token).await? else {
                if now.saturating_sub(entry.sent_at) >= UNMINED_RECHECK {
                    deposits.unmined.lock().unwrap().insert(hash, now);
                }
                continue;
            };
            deposits.unmined.lock().unwrap().remove(&hash);
            let Some(block_time) = self.settlement.block_time(facts.block_hash).await? else {
                continue;
            };
            let call = entry
                .signed
                .call()
                .and_then(|(_, _, data)| Call::decode(&data));
            let alert = deposits.telegram.enabled().then(|| {
                deposit_alert(
                    self.config.network,
                    chain_id,
                    hash,
                    call,
                    facts.succeeded,
                    block_time,
                )
            });
            let (value, fee) = match call {
                Some(Call::Shield { value, fee, .. }) => (Some(value), Some(fee)),
                Some(Call::Rescue { fee, .. }) => (None, Some(fee)),
                None => (None, None),
            };
            deposits.journal.save_cost(
                chain_id,
                hash,
                &Cost {
                    value,
                    fee,
                    succeeded: facts.succeeded,
                    block_number: facts.block_number,
                    block_time,
                    gas_used: facts.gas_used,
                    gas_price: facts.gas_price,
                },
                alert.as_ref(),
            )?;
        }
        if let Some(history) = &self.history {
            for (hash, at) in deposits.journal.unvalued(chain_id, 10)? {
                match (
                    history.usd_at(Asset::Eth, at).await,
                    history.usd_at(Asset::Usdc, at).await,
                ) {
                    (Ok(eth), Ok(token)) => deposits.journal.value(chain_id, hash, &eth, &token)?,
                    (Err(Unpriced::Now(error)), _) | (_, Err(Unpriced::Now(error))) => {
                        warn!(
                            operation = "deposit_costs",
                            "price history unavailable: {error}"
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// The deposits since `since` (30 days ago if none), with what each cost and earned; none
    /// where the relayer takes no deposits.
    pub fn deposits_snapshot(
        &self,
        since: Option<u64>,
    ) -> anyhow::Result<Option<DepositsSnapshot>> {
        let Some(deposits) = &self.deposits else {
            return Ok(None);
        };
        let generated_at = now();
        let since = since.unwrap_or(generated_at.saturating_sub(30 * 86_400));
        let (records, truncated) =
            deposits
                .journal
                .ledger(self.domain.chain_id, since, LEDGER_LIMIT)?;
        let config = &deposits.config;
        Ok(Some(DepositsSnapshot {
            schema_version: 1,
            generated_at,
            since,
            relayer: deposits.account,
            chain_id: self.domain.chain_id,
            contract: config.contract,
            token: deposits.deployment.token,
            fee: config.fee.to_string(),
            fee_bps: config.fee_bps,
            fee_gas: config.fee_gas,
            fee_per_unit_gas: self
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.rate(generated_at))
                .map(|rate| rate.to_string()),
            fee_margin_bps: self.pricing.as_ref().map(|pricing| pricing.margin_bps),
            min_value: config.min_value.to_string(),
            max_gas_limit: config.max_gas_limit,
            max_gas_price_wei: config.max_gas_price_wei.to_string(),
            limit: LEDGER_LIMIT,
            truncated,
            deposits: records,
        }))
    }
}

/// A mined deposit's message: what it moved and paid the relayer, or that it reverted, when it
/// mined, and the transaction. No addresses.
pub(crate) fn deposit_alert(
    network: Network,
    chain_id: u64,
    hash: B256,
    call: Option<Call>,
    succeeded: bool,
    at: u64,
) -> Notification {
    // The relayer's token is USDC, as its pricing takes it.
    let usdc = |amount| format!("{} USDC", units(amount, 6));
    let (what, moved) = match call {
        Some(Call::Shield { value, fee, .. }) => (
            "deposit",
            Some(format!("{} made private · fee {}", usdc(value), usdc(fee))),
        ),
        Some(Call::Rescue { fee, .. }) => ("deposit rescue", Some(format!("fee {}", usdc(fee)))),
        None => ("deposit", None),
    };
    let (title, detail) = if succeeded {
        (what.to_owned(), moved)
    } else {
        let lost = "Nothing moved; the relayer paid the gas and earned no fee.";
        (format!("{what} reverted"), Some(lost.to_owned()))
    };
    let network = network.name();
    let detail = detail.map(|line| format!("{line}\n")).unwrap_or_default();
    let link = explorer(chain_id, hash).unwrap_or_else(|| hash.to_string());
    Notification {
        key: format!("{network}:{chain_id}:deposit:{hash}"),
        text: format!("ZecSwap {network} · {title}\n{detail}{}\n{link}", time(at)),
        created_at: now(),
    }
}

/// What the relayer's deposits since a time cost and earned, newest first.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositsSnapshot {
    schema_version: u32,
    generated_at: u64,
    since: u64,
    /// The account deposits are sent from, where their fees collect.
    relayer: Address,
    chain_id: u64,
    contract: Address,
    token: Address,
    /// The fee's floor before its percentage, the gas it is priced for, and its percentage.
    fee: String,
    fee_bps: u32,
    fee_gas: u64,
    /// The gas-based part's rate now and its margin, where fees are priced by gas.
    fee_per_unit_gas: Option<String>,
    fee_margin_bps: Option<u32>,
    min_value: String,
    max_gas_limit: u64,
    max_gas_price_wei: String,
    limit: usize,
    truncated: bool,
    deposits: Vec<DepositRecord>,
}

/// A deposit, and once it mined, what it moved, burned and was paid; USD values at its block's
/// time, from Alchemy's history, once read.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositRecord {
    transaction_hash: B256,
    sent_at: u64,
    /// `shield` or `rescue`.
    kind: String,
    /// Token base units a shield moved in; none for a rescue.
    value: Option<String>,
    /// Token base units it pays the relayer: earned only if it succeeded.
    fee: Option<String>,
    succeeded: Option<bool>,
    block_number: Option<u64>,
    block_time: Option<u64>,
    gas_used: Option<u64>,
    gas_price_wei: Option<String>,
    eth_usd: Option<String>,
    token_usd: Option<String>,
}

/// Every deposit, recorded before its broadcast: the signed transaction and what it asks; and
/// once it mined, what it cost and earned.
pub(crate) struct Journal(Mutex<Connection>);

struct Entry {
    kind: Kind,
    id: B256,
    signed: Signed,
    sent_at: u64,
}

/// A mined deposit, as its receipt and its own bytes tell it.
struct Cost {
    value: Option<u128>,
    fee: Option<u128>,
    succeeded: bool,
    block_number: u64,
    block_time: u64,
    gas_used: u64,
    gas_price: u128,
}

impl Journal {
    fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS deposits (
                 chain_id INTEGER NOT NULL,
                 tx_hash BLOB NOT NULL,
                 depositor BLOB NOT NULL,
                 kind TEXT NOT NULL,
                 id BLOB NOT NULL,
                 nonce INTEGER NOT NULL,
                 raw BLOB NOT NULL,
                 sent_at INTEGER NOT NULL,
                 PRIMARY KEY (chain_id, tx_hash)
             );
             CREATE INDEX IF NOT EXISTS deposits_by_depositor ON deposits (chain_id, depositor);
             CREATE TABLE IF NOT EXISTS deposit_costs (
                 chain_id INTEGER NOT NULL,
                 tx_hash BLOB NOT NULL,
                 value TEXT,
                 fee TEXT,
                 succeeded INTEGER NOT NULL,
                 block_number INTEGER NOT NULL,
                 block_time INTEGER NOT NULL,
                 gas_used INTEGER NOT NULL,
                 gas_price TEXT NOT NULL,
                 eth_usd TEXT,
                 token_usd TEXT,
                 PRIMARY KEY (chain_id, tx_hash)
             );",
        )?;
        db.execute_batch(zecswap_alerts::SCHEMA)?;
        Ok(Self(Mutex::new(db)))
    }

    /// `depositor`'s recorded deposits, newest first.
    fn of(&self, chain_id: u64, depositor: Address) -> rusqlite::Result<Vec<Entry>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT kind, id, tx_hash, nonce, raw, sent_at FROM deposits
             WHERE chain_id = ?1 AND depositor = ?2 ORDER BY sent_at DESC",
        )?;
        query
            .query_map(params![chain_id as i64, depositor.as_slice()], entry)?
            .collect()
    }

    /// Whether the deposit `hash` succeeded, once its receipt is recorded.
    fn outcome(&self, chain_id: u64, hash: B256) -> rusqlite::Result<Option<bool>> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT succeeded FROM deposit_costs WHERE chain_id = ?1 AND tx_hash = ?2",
                params![chain_id as i64, hash.as_slice()],
                |row| row.get(0),
            )
            .optional()
    }

    fn record(
        &self,
        chain_id: u64,
        depositor: Address,
        kind: Kind,
        id: B256,
        signed: &Signed,
    ) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO deposits (chain_id, tx_hash, depositor, kind, id, nonce, raw, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                chain_id as i64,
                signed.hash.as_slice(),
                depositor.as_slice(),
                kind.name(),
                id.as_slice(),
                signed.nonce as i64,
                signed.raw.as_ref(),
                now() as i64,
            ],
        )?;
        Ok(())
    }

    /// The newest recorded deposits of every address, newest first.
    fn newest(&self, chain_id: u64, limit: usize) -> rusqlite::Result<Vec<Entry>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT kind, id, tx_hash, nonce, raw, sent_at FROM deposits WHERE chain_id = ?1
             ORDER BY sent_at DESC, nonce DESC LIMIT ?2",
        )?;
        query
            .query_map(params![chain_id as i64, limit as i64], entry)?
            .collect()
    }

    fn forget(&self, chain_id: u64, hash: B256) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM deposits WHERE chain_id = ?1 AND tx_hash = ?2",
            params![chain_id as i64, hash.as_slice()],
        )?;
        Ok(())
    }

    /// Deposits whose receipt hasn't been read, newest first.
    fn unread(&self, chain_id: u64, limit: usize) -> rusqlite::Result<Vec<Entry>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT d.kind, d.id, d.tx_hash, d.nonce, d.raw, d.sent_at FROM deposits d
             WHERE d.chain_id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM deposit_costs c WHERE c.chain_id = d.chain_id AND c.tx_hash = d.tx_hash
             ) ORDER BY d.sent_at DESC LIMIT ?2",
        )?;
        query
            .query_map(params![chain_id as i64, limit as i64], entry)?
            .collect()
    }

    /// Records a mined deposit's cost, and queues its message with it: once, however often read.
    fn save_cost(
        &self,
        chain_id: u64,
        hash: B256,
        cost: &Cost,
        alert: Option<&Notification>,
    ) -> rusqlite::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let saved = tx.execute(
            "INSERT OR IGNORE INTO deposit_costs (chain_id, tx_hash, value, fee, succeeded,
                 block_number, block_time, gas_used, gas_price)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                chain_id as i64,
                hash.as_slice(),
                cost.value.map(|value| value.to_string()),
                cost.fee.map(|fee| fee.to_string()),
                cost.succeeded,
                cost.block_number as i64,
                cost.block_time as i64,
                cost.gas_used as i64,
                cost.gas_price.to_string(),
            ],
        )?;
        if saved == 1 {
            zecswap_alerts::insert(&tx, alert)?;
        }
        tx.commit()
    }

    fn unvalued(&self, chain_id: u64, limit: usize) -> rusqlite::Result<Vec<(B256, u64)>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT tx_hash, block_time FROM deposit_costs WHERE chain_id = ?1 AND eth_usd IS NULL
             ORDER BY block_time DESC LIMIT ?2",
        )?;
        query
            .query_map(params![chain_id as i64, limit as i64], |row| {
                Ok((word(row, 0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect()
    }

    fn value(
        &self,
        chain_id: u64,
        hash: B256,
        eth_usd: &str,
        token_usd: &str,
    ) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE deposit_costs SET eth_usd = ?3, token_usd = ?4
             WHERE chain_id = ?1 AND tx_hash = ?2 AND eth_usd IS NULL",
            params![chain_id as i64, hash.as_slice(), eth_usd, token_usd],
        )?;
        Ok(())
    }

    fn ledger(
        &self,
        chain_id: u64,
        since: u64,
        limit: usize,
    ) -> rusqlite::Result<(Vec<DepositRecord>, bool)> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT d.tx_hash, d.sent_at, d.kind, c.value, c.fee, c.succeeded, c.block_number,
                 c.block_time, c.gas_used, c.gas_price, c.eth_usd, c.token_usd
             FROM deposits d LEFT JOIN deposit_costs c
                 ON c.chain_id = d.chain_id AND c.tx_hash = d.tx_hash
             WHERE d.chain_id = ?1 AND d.sent_at >= ?2 ORDER BY d.sent_at DESC LIMIT ?3",
        )?;
        let unsigned = |row: &Row<'_>, index| -> rusqlite::Result<Option<u64>> {
            Ok(row.get::<_, Option<i64>>(index)?.map(|value| value as u64))
        };
        let mut records = query
            .query_map(
                params![chain_id as i64, since as i64, limit as i64 + 1],
                |row| {
                    Ok(DepositRecord {
                        transaction_hash: word(row, 0)?,
                        sent_at: row.get::<_, i64>(1)? as u64,
                        kind: row.get(2)?,
                        value: row.get(3)?,
                        fee: row.get(4)?,
                        succeeded: row.get(5)?,
                        block_number: unsigned(row, 6)?,
                        block_time: unsigned(row, 7)?,
                        gas_used: unsigned(row, 8)?,
                        gas_price_wei: row.get(9)?,
                        eth_usd: row.get(10)?,
                        token_usd: row.get(11)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = records.len() > limit;
        records.truncate(limit);
        Ok((records, truncated))
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap()
    }
}

fn entry(row: &Row<'_>) -> rusqlite::Result<Entry> {
    let kind: String = row.get(0)?;
    Ok(Entry {
        kind: Kind::named(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("no deposit kind {kind}").into(),
            )
        })?,
        id: word(row, 1)?,
        signed: Signed {
            hash: word(row, 2)?,
            nonce: row.get::<_, i64>(3)? as u64,
            raw: row.get::<_, Vec<u8>>(4)?.into(),
        },
        sent_at: row.get::<_, i64>(5)? as u64,
    })
}

impl Deposits {
    #[cfg(test)]
    pub(crate) fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Records a shield as `send_deposit` does before broadcasting it.
    #[cfg(test)]
    pub(crate) fn record_shield(
        &self,
        chain_id: u64,
        from: Address,
        id: B256,
        signed: &Signed,
    ) -> rusqlite::Result<()> {
        self.journal
            .record(chain_id, from, Kind::Shield, id, signed)
    }

    pub(crate) fn outbox(&self) -> &Mutex<Connection> {
        &self.journal.0
    }
}
