mod economics;
mod flow;
mod gas_alerts;
mod monitoring;
mod notifications;
mod reads;
mod reverse;
mod transactions;

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use futures_util::FutureExt;
use hmac::{Hmac, Mac};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use sha2::Sha256;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use zcash_address::ZcashAddress;
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};
use zecswap_chain::evm::{
    Address, B256, OnChainSwap, Settlement, Stage, Windows, endpoint, swap_id,
};
use zecswap_chain::zcash::{
    AccountUuid, Funds, Lightwalletd, Prover, TxId, UnifiedSpendingKey, Wallet, connect_lazy,
};
use zecswap_core::{JointAccount, Payout, SecretShare, SwapContext, Terms, derive_maker_share};
use zecswap_tokens::server::{Gate, Spend};
use zeroize::Zeroizing;

use crate::config::{Config, Secrets};
use crate::policy::{self, Action, Observation};
use crate::store::{Store, Swap};
use crate::watchtower::{self, Health};
use notifications::{Alert, forward_ending};
use reads::{Confirm, Reads};

// Background observer diagnostics must not expose authenticated URLs or RPC payloads.
fn observer_failure(error: &anyhow::Error) -> &'static str {
    use zecswap_chain::Error;
    match error.downcast_ref::<Error>() {
        Some(Error::Config(_)) => "configuration",
        Some(Error::Lightwalletd(_) | Error::Connection(_)) => "lightwalletd",
        Some(Error::Rejected { .. }) => "transaction_rejected",
        Some(Error::Database(_) | Error::Wallet(_)) => "zcash_wallet",
        Some(
            Error::Contract(_)
            | Error::Reverted(_)
            | Error::Unconfirmed(_)
            | Error::Dropped(_)
            | Error::Late(_),
        ) => "evm_rpc_or_contract",
        Some(Error::Swap(_) | Error::WrongTerms(_)) => "swap_validation",
        Some(Error::Journal(_)) => "database",
        None if error.downcast_ref::<rusqlite::Error>().is_some() => "database",
        None => "operation_failed",
    }
}

pub struct Maker {
    config: Config,
    account: Address,
    /// The contract's window lengths, which every decision on a swap is judged by.
    windows: Windows,
    root: Zeroizing<[u8; 32]>,
    ticket_key: Zeroizing<[u8; 32]>,
    chain_id: u64,
    sweep_to: ZcashAddress,
    store: Store,
    settlement: Settlement,
    zcash: Mutex<Zcash>,
    prover: Prover,
    health: Health,
    zcash_health: Health,
    wallet_snapshots: std::sync::Mutex<HashMap<AccountUuid, WalletSnapshot>>,
    inventory: Option<(AccountUuid, UnifiedSpendingKey)>,
    monitoring: monitoring::Monitoring,
    prices: crate::market::PriceBook,
    /// The gas price quotes last charged the maker's network cost at, and when it was read.
    quote_gas_price: std::sync::Mutex<Option<(u64, u128)>>,
    /// What passes read of the chain, for the rest to use instead of reading it again.
    chain: std::sync::Mutex<reads::ChainCache>,
    telegram: zecswap_alerts::Telegram,
    /// When each failing alert scope started failing, so only a lasting error alerts.
    failing: std::sync::Mutex<HashMap<String, u64>>,
    tokens: Option<Arc<Gate>>,
}

#[derive(Clone, Copy)]
struct WalletSnapshot {
    funds: Funds,
    sweep: Option<(TxId, bool)>,
}

struct Zcash {
    wallet: Option<Wallet>,
    client: Lightwalletd,
}

impl Zcash {
    fn open_wallet(config: &Config) -> Result<Wallet> {
        let wallet = Wallet::open(
            config.data_dir.join("wallet.sqlite"),
            config.network.zcash(),
        )?;
        Ok(match config.confirmations {
            Some(confirmations) => wallet.with_confirmations(confirmations),
            None => wallet,
        })
    }

    fn wallet(&self) -> Result<&Wallet> {
        self.wallet.as_ref().context("Zcash wallet needs reopening")
    }

    async fn sync_with<F, Fut>(&mut self, config: &Config, sync: F) -> Result<bool>
    where
        F: FnOnce(Wallet, Lightwalletd) -> Fut,
        Fut: Future<Output = (Wallet, Result<(), zecswap_chain::Error>)>,
    {
        let wallet = match self.wallet.take() {
            Some(wallet) => wallet,
            None => Self::open_wallet(config)?,
        };
        // Own the wallet inside the unwind boundary: a panic drops the connection and
        // block cache instead of allowing partially updated in-memory state to escape.
        match AssertUnwindSafe(async { sync(wallet, self.client.clone()).await })
            .catch_unwind()
            .await
        {
            Ok((wallet, result)) => {
                self.wallet = Some(wallet);
                match result {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        warn!("acting on the last synced Zcash state: {e}");
                        Ok(false)
                    }
                }
            }
            Err(_) => {
                error!("Zcash sync panicked; reopening wallet and acting on the last synced state");
                self.wallet = Some(Self::open_wallet(config)?);
                Ok(false)
            }
        }
    }
}

/// The maker's own record of a swap.
pub struct Status {
    pub sweep: Option<TxId>,
    /// Settlement has enough confirmations. Retained state is still checked for reorgs.
    pub settled: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MakerError {
    #[error("{0}")]
    Rejected(String),
    #[error("quote is unknown, expired or already accepted")]
    UnknownQuote,
    #[error("swap is unknown")]
    UnknownSwap,
    #[error("cannot fill that amount right now")]
    Unavailable,
    #[error("live market price is unavailable or stale; try again later")]
    PriceUnavailable,
    #[error("watchtower has not completed a recent pass; try again later")]
    WatchtowerUnavailable,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<zecswap_chain::Error> for MakerError {
    fn from(e: zecswap_chain::Error) -> Self {
        MakerError::Internal(e.into())
    }
}

impl Maker {
    pub async fn new(config: Config, secrets: Secrets) -> Result<Self> {
        if let Some(alerts) = &config.gas_alerts {
            alerts.check()?;
        }
        let prices = crate::market::PriceBook::from_env(&config.pricing)?;
        let telegram = zecswap_alerts::Telegram::from_env()?;
        std::fs::create_dir_all(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join("maker.sqlite"))?;
        anyhow::ensure!(
            config.reverse.is_some() || store.pending_reverse_swaps()?.is_empty(),
            "reverse swaps are pending; keep reverse configuration enabled until they settle"
        );
        let network = config.network;
        anyhow::ensure!(
            network.ironwood(),
            "this build's Zcash crates schedule no NU6.3 on {network}, whose Ironwood pool swaps \
             use"
        );
        zecswap_core::SweepIntent::recipient(&config.sweep_to, network.zcash_type())
            .context("sweep_to")?;
        let sweep_to = config.sweep_to.parse().context("parsing sweep_to")?;
        let account = secrets.evm_key.address();
        info!(endpoint = %endpoint(&config.evm_rpc), "settlement RPC");
        let mut settlement =
            Settlement::connect(&config.evm_rpc, config.contract, secrets.evm_key)?;
        settlement.set_reveal_headroom(network.reveal_headroom_wei());
        settlement.reveal_with_margin(config.timing.reveal_margin);
        if let Some(reveals) = &config.reveals {
            zecswap_chain::evm::check_keeps_secrets(&reveals.private_rpc)
                .context("[reveals] private_rpc")?;
            settlement.broadcast_privately(&reveals.private_rpc)?;
        }
        let chain_id = settlement
            .chain_id()
            .await
            .context("reaching the settlement chain")?;
        settlement.check_private_rpc().await?;
        let railgun = settlement.railgun().await?;
        network
            .check(chain_id, railgun, config.token)
            .with_context(|| format!("the maker's config names {network}"))?;
        let windows = settlement
            .windows()
            .await
            .context("the maker's contract must be the windows contract")?;
        config.timing.check(windows)?;
        let mut zcash = Zcash {
            wallet: Some(Zcash::open_wallet(&config)?),
            client: connect_lazy(config.lightwalletd())?,
        };
        let inventory = if let Some(reverse) = &config.reverse {
            reverse.check(windows)?;
            settlement
                .reverse_funding(B256::ZERO)
                .await
                .context("configured contract does not support reverse swaps")?;
            anyhow::ensure!(
                !railgun.is_zero(),
                "reverse swaps require a Railgun deployment"
            );
            let seed = secrets
                .zcash_seed
                .as_ref()
                .context("reverse swaps require MAKER_ZCASH_SEED")?;
            let wallet = zcash
                .wallet
                .as_mut()
                .context("Zcash wallet needs reopening")?;
            Some(wallet.inventory_account(&mut zcash.client, seed).await?)
        } else {
            None
        };
        let tokens = config
            .tokens
            .as_ref()
            .map(Gate::open)
            .transpose()
            .context("[tokens]")?
            .map(Arc::new);
        let maker = Self {
            tokens,
            telegram,
            failing: std::sync::Mutex::default(),
            prices,
            quote_gas_price: std::sync::Mutex::default(),
            chain: std::sync::Mutex::default(),
            monitoring: monitoring::Monitoring::from_env()?,
            inventory,
            store,
            root: secrets.root,
            ticket_key: secrets.ticket_key,
            chain_id,
            sweep_to,
            settlement,
            zcash: Mutex::new(zcash),
            prover: Prover::default(),
            account,
            windows,
            health: Health::new(Duration::from_secs(config.timing.tick)),
            zcash_health: Health::new(Duration::from_secs(config.timing.tick)),
            wallet_snapshots: std::sync::Mutex::new(HashMap::new()),
            config,
        };
        maker.check_swaps().await?;
        Ok(maker)
    }

    /// Refuses to run on live swaps it could no longer act on: opened from another account or
    /// under another root secret, each of its calls would fail, cancels included, and an open
    /// swap nobody deposited into would pay out after `t0`.
    async fn check_swaps(&self) -> Result<()> {
        for swap in self.store.unsettled_swaps()? {
            anyhow::ensure!(
                swap.id == swap_id(self.account, &swap.user_share),
                "swap {} was opened from another EVM account than {}",
                swap.id,
                self.account
            );
            match self.settlement.swap(swap.id, &self.terms(&swap)?).await {
                Ok(_) => {}
                Err(zecswap_chain::Error::WrongTerms(id)) => {
                    anyhow::bail!("swap {id} was opened under another MAKER_ROOT_SECRET")
                }
                Err(e) => return Err(e.into()),
            }
        }
        for swap in self.store.pending_reverse_swaps()? {
            self.check_reverse(&swap)?;
        }
        Ok(())
    }

    pub fn settlement(&self) -> &Settlement {
        &self.settlement
    }

    /// The account that opens swaps and holds the inventory.
    pub fn account(&self) -> Address {
        self.account
    }

    /// What accepts spend tokens through, if they must.
    pub(crate) fn tokens(&self) -> Option<Arc<Gate>> {
        self.tokens.clone()
    }

    /// Swaps waiting on their users to pay in: forward ones with no deposit seen and no cancel
    /// started, and reverse ones whose ZEC the maker has yet to send.
    pub(crate) fn awaiting_deposit(&self) -> Result<usize> {
        let forward = {
            let snapshots = self.wallet_snapshots.lock().unwrap();
            self.store
                .unsettled_swaps()?
                .iter()
                .filter(|swap| {
                    !swap.refund_started
                        && snapshots
                            .get(&swap.zcash_account)
                            .is_none_or(|snapshot| snapshot.funds.total == 0)
                })
                .count()
        };
        let reverse = self
            .store
            .pending_reverse_swaps()?
            .iter()
            .filter(|swap| swap.deposit.is_none())
            .count();
        Ok(forward + reverse)
    }

    /// Refuses another accept while `max_awaiting_deposit` swaps wait on their users to pay in.
    pub(crate) fn admit_another(&self) -> Result<(), MakerError> {
        let Some(cap) = self.config.max_awaiting_deposit else {
            return Ok(());
        };
        if self.awaiting_deposit()? >= cap {
            return Err(MakerError::Unavailable);
        }
        Ok(())
    }

    pub fn listen(&self) -> std::net::SocketAddr {
        self.config.listen
    }

    pub fn info(&self) -> zecswap_api::service::MakerInfo {
        use zecswap_api::service::MakerInfo;

        MakerInfo {
            api_version: 1,
            maker: self.account,
            chain_id: self.chain_id,
            contract: self.config.contract,
            token: self.config.token,
            zcash_network: self.config.network,
            reverse_enabled: self.inventory.is_some(),
            token_return_key: self
                .tokens
                .as_ref()
                .map(|gate| gate.return_key().to_base64()),
        }
    }

    /// New swaps require a completed watchtower pass within the last three tick intervals.
    /// Unavailable at startup until the first pass completes.
    pub fn check_watchtower(&self) -> Result<(), MakerError> {
        self.health.check()?;
        self.zcash_health.check()
    }

    /// The gas price a quote charges the maker's network cost at, read at most once a minute;
    /// none where quotes charge none, or it can't be read.
    pub(crate) async fn quote_gas_price(&self) -> Option<u128> {
        if !self.prices.charges_costs() {
            return None;
        }
        let now = unix_now();
        let cached = *self.quote_gas_price.lock().unwrap();
        if let Some((at, price)) = cached
            && now.saturating_sub(at) < 60
        {
            return Some(price);
        }
        match self.settlement.gas_price().await {
            Ok(price) => {
                *self.quote_gas_price.lock().unwrap() = Some((now, price));
                Some(price)
            }
            Err(e) => {
                warn!("no gas price to charge quotes at: {e:#}");
                None
            }
        }
    }

    pub async fn quote(&self, request: QuoteRequest) -> Result<Quote, MakerError> {
        self.check_watchtower()?;
        self.prices.refresh().await;
        let gas_price = self.quote_gas_price().await;
        let pricing = self
            .prices
            .quote(unix_now(), gas_price)
            .ok_or(MakerError::PriceUnavailable)?;
        let terms = pricing.terms(request.units).ok_or_else(|| {
            MakerError::Rejected(format!(
                "{} units is outside the quotable range",
                request.units
            ))
        })?;
        if request.payout.is_zero() {
            return Err(MakerError::Rejected("payout address is zero".into()));
        }
        if request.payout_note.is_some() && self.settlement.railgun().await?.is_zero() {
            return Err(MakerError::Rejected(
                "this deployment cannot pay into Railgun".into(),
            ));
        }
        let inventory = self.inventory_balance().await?;
        if inventory < terms.amount {
            return Err(MakerError::Unavailable);
        }

        let mut quote_id = [0; 32];
        UnwrapErr(SysRng).fill_bytes(&mut quote_id);
        let expires_at = unix_now() + self.config.timing.quote_ttl;
        self.check_watchtower()?;
        if !pricing.fresh(unix_now()) {
            return Err(MakerError::PriceUnavailable);
        }
        let nonce = self.store.insert_quote(
            quote_id,
            request.payout,
            request.payout_note,
            terms.amount,
            terms.deposit_zat,
            expires_at,
        )?;
        self.record_quote_mark(&quote_id, pricing.mark(pricing.forward_cost()));
        let e = self.maker_share(nonce)?;
        Ok(Quote {
            quote_id: quote_id.into(),
            maker: self.account,
            maker_share: e.public(),
            maker_proof: self.context(quote_id).prove_maker(&e, UnwrapErr(SysRng)),
            chain_id: self.chain_id,
            contract: self.settlement.contract(),
            token: self.config.token,
            amount: terms.amount,
            deposit_zat: terms.deposit_zat,
            expires_at,
            network_cost: pricing.forward_cost(),
        })
    }

    /// With `[tokens]`, `spend` is the accept's token: kept spent once the quote is taken,
    /// spendable again after any refusal before.
    #[tracing::instrument(skip_all, fields(operation = "accept", %quote_id, swap_id = tracing::field::Empty), err(level = "warn"))]
    pub async fn accept(
        &self,
        quote_id: B256,
        acceptance: Acceptance,
        spend: Option<&Spend>,
    ) -> Result<Accepted, MakerError> {
        self.check_watchtower()?;
        self.admit_another()?;
        let mut zcash = self.zcash.lock().await;
        // A request can wait behind a long sync or proof after its first health check.
        // Check again before consuming its quote or importing an account.
        self.check_watchtower()?;
        let Zcash { wallet, client } = &mut *zcash;
        let wallet = wallet.as_mut().ok_or(MakerError::WatchtowerUnavailable)?;
        // Read, and taken only once the acceptance checks out: a malformed one leaves the quote
        // to its user.
        let quote = self
            .store
            .quote(&quote_id.0, unix_now())?
            .ok_or(MakerError::UnknownQuote)?;
        let maker_share = self.maker_share(quote.nonce)?.public();
        let payout = Payout {
            user: quote.payout.into(),
            note: quote.payout_note.map(|note| note.0),
        };
        let token_request = self.token_request(&acceptance)?;
        let Acceptance {
            user_share,
            user_proof,
            viewing_keys,
            ..
        } = acceptance;
        self.context(quote.id)
            .verify_user(&maker_share, &user_share, &payout, &user_proof)
            .map_err(|_| MakerError::Rejected("user share proof does not verify".into()))?;
        let joint = JointAccount::derive(&maker_share, &user_share, &viewing_keys)
            .map_err(|e| MakerError::Rejected(e.to_string()))?;
        let id = swap_id(self.account, &user_share);
        tracing::Span::current().record("swap_id", tracing::field::display(id));
        // Before the import, so a failed read leaves no account watched for nothing.
        let now = self.settlement.now().await?;
        if self.store.take_quote(&quote_id.0, unix_now())?.is_none() {
            return Err(MakerError::UnknownQuote);
        }
        if let Some(spend) = spend {
            spend.keep()?;
        }

        // Watch the deposit address before the user can learn it from the chain.
        let zcash_account = wallet
            .import_joint(client, &joint, &format!("swap {id}"))
            .await?;
        drop(zcash);
        if let Err(e) = self.check_watchtower() {
            self.forget(zcash_account).await;
            return Err(e);
        }
        let timing = &self.config.timing;
        let swap = Swap {
            id,
            quote,
            user_share,
            viewing: viewing_keys,
            zcash_account,
            opened_at: now,
            token: self.config.token,
            t0: now + timing.t0_after,
            t1: now + timing.t1_after,
            sweep: None,
            settled: false,
            refund_started: false,
            ready_sent: false,
            token_request,
            token_return: None,
        };
        // Recorded before `open`, whose outcome can be unknown: the watchtower then settles
        // the swap from what the chain shows.
        let event = self.forward_alert(&swap, Alert::Started);
        if let Err(e) = self.store.insert_swap(&swap, event.as_ref()) {
            self.forget(zcash_account).await;
            return Err(e.into());
        }
        let opened = self.settlement.open(&self.terms(&swap)?).await;
        self.forget_inventory();
        let transaction_hash = self.journal(id, "open", opened)?;
        // The user can only deposit once it sees the swap, which on a slow chain can be minutes
        // after `now` when opens queue behind each other; `cancel_after` counts from here.
        self.store
            .set_opened_at(&id, self.settlement.now().await?)?;
        info!(swap_id = %id, %transaction_hash, outcome = "mined", amount = swap.quote.amount, deposit_zat = swap.quote.deposit_zat, "opened swap");
        Ok(self.accepted(id, swap.t0, swap.t1))
    }

    /// The answer to an accept of `swap_id`, forward or reverse, with the swap's ticket.
    pub(crate) fn accepted(&self, swap_id: B256, t0: u64, t1: u64) -> Accepted {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.ticket_key.as_ref())
            .expect("HMAC takes keys of any length");
        mac.update(swap_id.as_slice());
        Accepted {
            swap_id,
            t0,
            t1,
            ticket: format!("{swap_id}.{}", hex::encode(mac.finalize().into_bytes())),
        }
    }

    pub fn status(&self, id: B256) -> Result<Option<Status>> {
        Ok(self.store.swap(&id)?.map(|swap| Status {
            sweep: swap.sweep,
            settled: swap.settled,
        }))
    }

    /// What `GET /v1/swaps/{id}` shows of a forward swap.
    pub fn swap_status(&self, id: B256) -> Result<Option<zecswap_api::Status>> {
        Ok(self.store.swap(&id)?.map(|swap| zecswap_api::Status {
            swap_id: id,
            token_return: swap.token_return,
        }))
    }

    /// The accept's request for its token back: required where accepts take tokens, refused
    /// where they don't, and one the return key can sign.
    pub(crate) fn token_request(
        &self,
        acceptance: &Acceptance,
    ) -> Result<Option<Vec<u8>>, MakerError> {
        let rejected = |why: &str| MakerError::Rejected(why.into());
        match (&self.tokens, &acceptance.token_request) {
            (None, None) => Ok(None),
            (None, Some(_)) => Err(rejected("this maker hands back no tokens")),
            (Some(_), None) => Err(rejected("an accept asks for its token back")),
            (Some(gate), Some(request)) => gate
                .read_return_request(request)
                .map(Some)
                .map_err(|e| rejected(&e.to_string())),
        }
    }

    /// Hands the swap's token back, once, now the user has paid in or the swap never opened.
    /// Failing here never holds up the swap itself.
    fn return_token(&self, swap: &Swap) {
        let (Some(gate), Some(request), None) =
            (&self.tokens, &swap.token_request, &swap.token_return)
        else {
            return;
        };
        let returned = gate
            .sign_return(request)
            .and_then(|signature| self.store.return_token(&swap.id, &signature));
        if let Err(e) = returned {
            warn!(swap_id = %swap.id, "could not hand the token back: {e:#}");
        }
    }

    /// EVM deadlines run independently of wallet I/O and CPU-heavy scanning/proving.
    pub async fn run(self: Arc<Self>) {
        let maker = &self;
        let watchtower = watchtower::run(
            Duration::from_secs(self.config.timing.tick),
            &self.health,
            |panicked| async move {
                if panicked {
                    maker.record_alert_failure(
                        "watchtower-panic",
                        B256::ZERO,
                        maker
                            .service_alert("The watchtower keeps panicking; see the maker's logs."),
                    );
                }
                let result = maker.tick().await;
                maker.record_alert_failure(
                    "watchtower-pass",
                    B256::ZERO,
                    result.as_ref().err().and_then(|_| {
                        maker.service_alert(
                            "The watchtower's passes keep failing; see the maker's logs.",
                        )
                    }),
                );
                if result.is_ok() {
                    maker.record_alert_failure("watchtower-panic", B256::ZERO, None);
                }
                result
            },
        );
        tokio::join!(
            watchtower,
            self.run_zcash(),
            self.run_notifications(),
            self.run_gas_alerts(),
            self.run_transaction_observer(),
            self.run_flow_observer(),
            zecswap_chain::evm::log_usage(
                self.settlement.meter().clone(),
                Duration::from_secs(3600)
            )
        );
    }

    async fn tick(&self) -> Result<()> {
        let swaps = self.store.watched_swaps()?;
        let pending_reverse = self.store.pending_reverse_swaps()?;
        let mut failed = Vec::new();
        let (mut watched, mut readable) = (Vec::new(), Vec::new());
        for swap in &swaps {
            match self.terms(swap) {
                Ok(terms) => {
                    watched.push(swap);
                    readable.push((swap.id, terms));
                }
                Err(e) => self.forward_failed(swap, &e, &mut failed),
            }
        }
        let by_id: HashMap<_, _> = watched.iter().map(|swap| (swap.id, *swap)).collect();
        // Settled states are read deep to check they hold; one that never showed is read deep,
        // with that block's time, to show it never will.
        let reads = self
            .read_swaps(
                &readable,
                u64::from(self.config.evm_confirmations.get()),
                |id, state, now| {
                    let swap = by_id[&id];
                    let never_opened = state.is_none() && !swap.settled && now >= swap.t1;
                    let settled =
                        state.is_some_and(|s| matches!(s.stage, Stage::Claimed | Stage::Refunded));
                    Confirm {
                        state: settled || never_opened,
                        time: never_opened,
                    }
                },
            )
            .await?;
        if reads.now().is_none() {
            // Nothing to read: a call nodes don't bill keeps the pass a test of the RPC.
            let chain_id = self.settlement.chain_id().await?;
            anyhow::ensure!(
                chain_id == self.chain_id,
                "the settlement RPC serves chain {chain_id}, not {}",
                self.chain_id
            );
        }
        self.set_chain_busy(reads.now().is_some() || !pending_reverse.is_empty());
        for (swap, (_, terms)) in watched.into_iter().zip(&readable) {
            match self.advance(swap, terms, &reads).await {
                Ok(()) => self.record_alert_failure("forward", swap.id, None),
                Err(e) => self.forward_failed(swap, &e, &mut failed),
            }
        }
        for swap in pending_reverse {
            if let Err(e) = self.advance_reverse_claim(&swap).await {
                failed.push(swap.id);
                warn!(swap_id = %swap.id, operation = "reverse_claim", "{e:#}");
            }
        }
        self.record_monitor_errors(failed);
        Ok(())
    }

    fn forward_failed(&self, swap: &Swap, e: &anyhow::Error, failed: &mut Vec<B256>) {
        failed.push(swap.id);
        warn!(swap_id = %swap.id, operation = "forward_watchtower", "{e:#}");
        self.record_alert_failure("forward", swap.id, self.forward_alert(swap, Alert::Stuck));
    }

    async fn run_zcash(self: &Arc<Self>) {
        // Sync freshness is updated when a snapshot is published, not after a long proof.
        let worker_health = Health::new(Duration::from_secs(self.config.timing.tick));
        watchtower::run(
            Duration::from_secs(self.config.timing.tick),
            &worker_health,
            |panicked| {
                let maker = self.clone();
                async move {
                    // The backend scans and proves synchronously. A separate blocking worker
                    // keeps even a long proof off the executor that polls EVM deadlines.
                    let result = tokio::task::spawn_blocking(move || {
                        tokio::runtime::Handle::current().block_on(async {
                            if panicked {
                                maker.zcash.lock().await.wallet = None;
                            }
                            maker.zcash_tick().await
                        })
                    })
                    .await;
                    match result {
                        Ok(result) => result,
                        Err(error) if error.is_panic() => {
                            std::panic::resume_unwind(error.into_panic())
                        }
                        Err(error) => Err(error.into()),
                    }
                }
            },
        )
        .await;
    }

    async fn zcash_tick(&self) -> Result<()> {
        let synced = {
            let mut zcash = self.zcash.lock().await;
            let synced = zcash
                .sync_with(&self.config, |mut wallet, mut client| async move {
                    let result = wallet.sync(&mut client).await;
                    (wallet, result)
                })
                .await?;
            if synced {
                if let Some(wallet) = zcash.wallet.as_mut() {
                    // No reorganisation reopens these any more: stop scanning their addresses.
                    // ZEC sent to one from now on waits for a manual sweep.
                    for (id, account) in self.store.final_swaps()? {
                        if let Err(e) = wallet.forget(account) {
                            warn!(swap_id = %id, "could not stop tracking {account:?}: {e}");
                        }
                        self.store.archive(&id)?;
                    }
                }
                let wallet = zcash.wallet()?;
                let all = wallet.funds_by_account()?;
                let funds = |account| all.get(&account).copied().unwrap_or_default();
                let mut snapshots = HashMap::new();
                for swap in self.store.watched_swaps()? {
                    snapshots.insert(
                        swap.zcash_account,
                        WalletSnapshot {
                            funds: funds(swap.zcash_account),
                            sweep: swap
                                .sweep
                                .map(|txid| {
                                    Ok::<_, zecswap_chain::Error>((
                                        txid,
                                        wallet.is_confirmed(txid)?,
                                    ))
                                })
                                .transpose()?,
                        },
                    );
                }
                for swap in self.store.pending_reverse_swaps()? {
                    snapshots.insert(
                        swap.account,
                        WalletSnapshot {
                            funds: funds(swap.account),
                            sweep: None,
                        },
                    );
                }
                *self.wallet_snapshots.lock().unwrap() = snapshots;
            }
            synced
        };
        self.record_monitor_sync(synced);
        if synced {
            self.zcash_health.completed();
        } else {
            self.zcash_health.failed();
        }
        self.record_alert_failure("zcash-sync", B256::ZERO, (!synced).then(|| self.service_alert("Zcash sync keeps failing; Ethereum settlement carries on. Check lightwalletd.")).flatten());
        anyhow::ensure!(synced, "Zcash sync did not complete");
        for swap in self.store.unsettled_swaps()? {
            if let Err(e) = self.advance_sweep(&swap).await {
                warn!(id = %swap.id, "Zcash sweep: {e:#}");
            }
        }
        if self.config.reverse.is_none() {
            return Ok(());
        }
        let swaps = self.store.watched_reverse_swaps()?;
        if swaps.is_empty() {
            return Ok(());
        }
        let reads = self.reverse_reads(&swaps).await;
        for mut swap in swaps {
            self.queue_alert(self.reverse_alert(&swap, Alert::Started));
            let advanced = match &reads {
                Ok(reads) => self.advance_reverse(&mut swap, synced, reads).await,
                Err(e) => Err(anyhow::anyhow!("reading the reverse swaps: {e:#}")),
            };
            if let Err(e) = advanced {
                warn!(id = %swap.id, "reverse swap: {e:#}");
                self.record_alert_failure(
                    "reverse",
                    swap.id,
                    self.reverse_alert(&swap, Alert::Stuck),
                );
            } else {
                self.record_alert_failure("reverse", swap.id, None);
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "advance"), err(level = "warn"))]
    async fn advance(&self, swap: &Swap, terms: &Terms, reads: &Reads) -> Result<()> {
        let Some(chain) = reads.latest(swap.id, terms)? else {
            if !swap.settled
                && reads.now().is_some_and(|now| now >= swap.t1)
                && reads.confirmed_time().is_some_and(|time| time >= swap.t1)
                && reads
                    .confirmed(swap.id, terms)?
                    .is_some_and(|confirmed| confirmed.is_none())
            {
                info!(id = %swap.id, "the swap never opened");
                self.return_token(swap);
                self.settle(swap, None, Funds::default())?;
            }
            return Ok(());
        };
        if chain.stage == Stage::Refunded && !swap.refund_started {
            self.store.start_refund(&swap.id)?;
        }
        let (funds, sweep_confirmed, synced) = self.wallet_observation(swap);
        let chain_confirmed = matches!(chain.stage, Stage::Claimed | Stage::Refunded)
            && reads
                .confirmed(swap.id, terms)?
                .flatten()
                .is_some_and(|confirmed| {
                    confirmed.stage == chain.stage && confirmed.secret == chain.secret
                });
        if swap.settled {
            let still_settled = match chain.stage {
                Stage::Refunded => chain_confirmed,
                Stage::Claimed => {
                    chain_confirmed
                        && (!synced || (sweep_confirmed == Some(true) && funds.spendable == 0))
                }
                _ => false,
            };
            if still_settled {
                return Ok(());
            }
            self.store.resume(&swap.id)?;
        }
        let now = match reads.now() {
            Some(now) => now,
            // Reopened by its Zcash side, a finalized swap that the pass didn't read: its claim or
            // refund decides what comes next, whatever the time.
            None => self.settlement.now().await?,
        };
        let observation = Observation {
            now,
            wall: unix_now(),
            opened_at: swap.opened_at,
            expected_zat: swap.quote.deposit_zat,
            chain,
            funds,
            synced,
            chain_confirmed,
            cancelling: swap.refund_started,
            readied: swap.ready_sent,
            sweep_confirmed,
            windows: self.windows,
        };
        // Paid in full, if not yet confirmed: the accept's token goes back. An underpaid swap
        // is cancelled like one never paid into.
        if synced && funds.total >= swap.quote.deposit_zat {
            self.return_token(swap);
        }
        let action = policy::decide(&observation, &self.config.timing);
        if action != Action::Wait {
            info!(id = %swap.id, ?action, "watchtower");
        }
        match action {
            Action::Wait => {}
            Action::MarkReady => {
                // Recorded first: a restart that forgot it could cancel behind the ready.
                self.store.mark_ready_sent(&swap.id)?;
                self.journal(
                    swap.id,
                    "ready",
                    self.settlement.ready(swap.id, terms).await,
                )?;
            }
            Action::Refund => {
                self.store.start_refund(&swap.id)?;
                let e = self.maker_share(swap.quote.nonce)?;
                let sent = self.settlement.refund(swap.id, terms, &e).await;
                self.journal(swap.id, "refund", sent)?;
            }
            Action::Sweep => {} // The Zcash worker builds and broadcasts sweeps.
            Action::Settle => self.settle(swap, Some(&observation.chain), observation.funds)?,
        }
        Ok(())
    }

    fn wallet_observation(&self, swap: &Swap) -> (Funds, Option<bool>, bool) {
        if let Some(snapshot) = self.wallet_snapshot(swap.zcash_account) {
            let confirmed = snapshot
                .sweep
                .filter(|(txid, _)| Some(*txid) == swap.sweep)
                .map(|(_, confirmed)| confirmed);
            return (snapshot.funds, confirmed, true);
        }
        (Funds::default(), None, false)
    }

    fn wallet_snapshot(&self, account: AccountUuid) -> Option<WalletSnapshot> {
        self.zcash_health.check().ok()?;
        self.wallet_snapshots.lock().unwrap().get(&account).copied()
    }

    /// Sweeps once the watchtower's last read shows the swap claimed: only a reorganisation
    /// undoes a claim, and the watchtower's next read shows that.
    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "advance_sweep"), err(level = "warn"))]
    async fn advance_sweep(&self, swap: &Swap) -> Result<()> {
        if let Some(Some(chain)) = self.last_read(swap.id)
            && chain.stage == Stage::Claimed
        {
            self.sweep(swap, &chain).await?;
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "sweep"), err(level = "warn"))]
    async fn sweep(&self, swap: &Swap, chain: &OnChainSwap) -> Result<()> {
        let z = chain
            .revealed()?
            .context("a claimed swap stores the user share")?;
        let e = self.maker_share(swap.quote.nonce)?;
        let joint = JointAccount::derive(&e.public(), &swap.user_share, &swap.viewing)?;
        let key = joint.spend_key(&e, &z)?;
        let mut zcash = self.zcash.lock().await;
        let Zcash { wallet, client } = &mut *zcash;
        let wallet = wallet.as_mut().context("Zcash wallet needs reopening")?;
        if let Some(txid) = swap.sweep
            && !wallet.is_mined(txid)?
            && !wallet.is_expired(txid)?
        {
            wallet.broadcast(client, txid).await?;
            return Ok(());
        }
        if wallet.funds(swap.zcash_account)?.spendable == 0 {
            return Ok(());
        }
        let txid = wallet.sweep(&self.prover, swap.zcash_account, &key, &self.sweep_to)?;
        // Recorded first: a sweep that never reaches the network expires, and the policy then
        // sweeps again.
        self.store.record_sweep(&swap.id, txid)?;
        wallet.broadcast(client, txid).await?;
        info!(id = %swap.id, %txid, "swept claimed deposit");
        Ok(())
    }

    fn settle(&self, swap: &Swap, chain: Option<&OnChainSwap>, funds: Funds) -> Result<()> {
        // What the deposit address holds, or its history once the user has taken a refund back.
        let deposited = funds.total > 0
            || self
                .store
                .flow_observation(&self.transaction_scope(), swap.id)
                .ok()
                .flatten()
                .is_some_and(|flow| {
                    flow.transactions
                        .iter()
                        .any(|tx| tx.kind == "deposit" && tx.state != "expired")
                });
        let event = self.forward_alert(swap, Alert::Ended(forward_ending(chain, deposited)));
        // Keep the account, birthday, transaction history and keys for reorg recovery.
        self.store.settle(&swap.id, event.as_ref())
    }

    async fn forget(&self, account: AccountUuid) {
        let mut zcash = self.zcash.lock().await;
        let result = zcash
            .wallet
            .as_mut()
            .context("Zcash wallet needs reopening")
            .and_then(|wallet| Ok(wallet.forget(account)?));
        if let Err(e) = result {
            warn!("could not stop tracking {account:?}: {e}");
        }
    }

    fn maker_share(&self, nonce: u64) -> Result<SecretShare> {
        Ok(derive_maker_share(&self.root, nonce)?)
    }

    /// The terms `open` committed the swap to, which every call on it supplies again.
    fn terms(&self, swap: &Swap) -> Result<Terms> {
        Ok(Terms {
            maker: self.account.into(),
            token: swap.token.into(),
            amount: swap.quote.amount,
            maker_share: self.maker_share(swap.quote.nonce)?.public(),
            user_share: swap.user_share,
            user: swap.quote.payout.into(),
            t0: swap.t0,
            t1: swap.t1,
            payout_note: swap.quote.payout_note.unwrap_or_default().0,
        })
    }

    fn context(&self, quote_id: [u8; 32]) -> SwapContext {
        SwapContext {
            chain_id: self.chain_id,
            contract: self.settlement.contract().into(),
            quote_id,
        }
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

#[cfg(test)]
mod tests;
