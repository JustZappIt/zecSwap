mod flow;
mod gas_alerts;
mod monitoring;
mod notifications;
mod reverse;
mod transactions;

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use futures_util::FutureExt;
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use zcash_address::ZcashAddress;
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};
use zecswap_chain::evm::{Address, B256, OnChainSwap, Settlement, Stage, swap_id};
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

// Background observer diagnostics must not expose authenticated URLs or RPC payloads.
fn observer_failure(error: &anyhow::Error) -> &'static str {
    use zecswap_chain::Error;
    match error.downcast_ref::<Error>() {
        Some(Error::Config(_)) => "configuration",
        Some(Error::Lightwalletd(_) | Error::Connection(_)) => "lightwalletd",
        Some(Error::Rejected { .. }) => "transaction_rejected",
        Some(Error::Database(_) | Error::Wallet(_)) => "zcash_wallet",
        Some(Error::Contract(_)) => "evm_rpc_or_contract",
        Some(Error::Swap(_) | Error::WrongTerms(_)) => "swap_validation",
        None if error.downcast_ref::<rusqlite::Error>().is_some() => "database",
        None => "operation_failed",
    }
}

pub struct Maker {
    config: Config,
    account: Address,
    lock_duration: u64,
    root: Zeroizing<[u8; 32]>,
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
    telegram: crate::telegram::Telegram,
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
        let wallet = Wallet::open(config.data_dir.join("wallet.sqlite"), config.network())?;
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
        let telegram = crate::telegram::Telegram::from_env()?;
        std::fs::create_dir_all(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join("maker.sqlite"))?;
        anyhow::ensure!(
            config.reverse.is_some() || store.pending_reverse_swaps()?.is_empty(),
            "reverse swaps are pending; keep reverse configuration enabled until they settle"
        );
        let sweep_to = config.sweep_to.parse().context("parsing sweep_to")?;
        let account = secrets.evm_key.address();
        let settlement = Settlement::connect(&config.evm_rpc, config.contract, secrets.evm_key)?;
        let chain_id = settlement
            .chain_id()
            .await
            .context("reaching the settlement chain")?;
        let lock_duration = settlement.lock_duration().await?;
        config.timing.check(lock_duration)?;
        let mut zcash = Zcash {
            wallet: Some(Zcash::open_wallet(&config)?),
            client: connect_lazy(&config.lightwalletd)?,
        };
        let inventory = if let Some(reverse) = &config.reverse {
            reverse.check()?;
            settlement
                .reverse_funding(B256::ZERO)
                .await
                .context("configured contract does not support reverse swaps")?;
            anyhow::ensure!(
                !settlement.railgun().await?.is_zero(),
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
            prices,
            monitoring: monitoring::Monitoring::from_env()?,
            inventory,
            store,
            root: secrets.root,
            chain_id,
            sweep_to,
            settlement,
            zcash: Mutex::new(zcash),
            prover: Prover::default(),
            account,
            lock_duration,
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

    /// Refuses another accept while `max_awaiting_deposit` swaps wait on their users to pay
    /// in: forward ones with no deposit seen and no cancel started, and reverse ones whose ZEC
    /// the maker has yet to send.
    pub(crate) fn admit_another(&self) -> Result<(), MakerError> {
        let Some(cap) = self.config.max_awaiting_deposit else {
            return Ok(());
        };
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
        if forward + reverse >= cap {
            return Err(MakerError::Unavailable);
        }
        Ok(())
    }

    pub fn listen(&self) -> std::net::SocketAddr {
        self.config.listen
    }

    pub fn info(&self) -> zecswap_api::service::MakerInfo {
        use crate::config::Chain;
        use zecswap_api::service::{MakerInfo, ZcashNetwork};

        MakerInfo {
            api_version: 1,
            maker: self.account,
            chain_id: self.chain_id,
            contract: self.config.contract,
            token: self.config.token,
            zcash_network: match self.config.network {
                Chain::Mainnet => ZcashNetwork::Mainnet,
                Chain::Testnet => ZcashNetwork::Testnet,
            },
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

    pub async fn quote(&self, request: QuoteRequest) -> Result<Quote, MakerError> {
        self.check_watchtower()?;
        self.prices.refresh().await;
        let pricing = self
            .prices
            .quote(unix_now())
            .ok_or(MakerError::PriceUnavailable)?;
        let terms = pricing.policy.terms(request.units).ok_or_else(|| {
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
        let inventory = self
            .settlement
            .balance_of(self.account, self.config.token)
            .await?;
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
            token_request,
            token_return: None,
        };
        // Recorded before `open`, whose outcome can be unknown: the watchtower then settles
        // the swap from what the chain shows.
        let event = self.forward_alert(
            &swap,
            "accepted",
            "Bridge accepted; awaiting escrow and user ZEC deposit.",
        );
        if let Err(e) = self.store.insert_swap(&swap, event.as_ref()) {
            self.forget(zcash_account).await;
            return Err(e.into());
        }
        let transaction_hash = self.settlement.open(&self.terms(&swap)?).await?;
        // The user can only deposit once it sees the swap, which on a slow chain can be minutes
        // after `now` when opens queue behind each other; `cancel_after` counts from here.
        self.store
            .set_opened_at(&id, self.settlement.now().await?)?;
        info!(swap_id = %id, %transaction_hash, outcome = "mined", amount = swap.quote.amount, deposit_zat = swap.quote.deposit_zat, "opened swap");
        Ok(Accepted {
            swap_id: id,
            t0: swap.t0,
            t1: swap.t1,
        })
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
                        maker.service_alert(
                            "EVM watchtower panicked; processing will retry. Check maker logs.",
                        ),
                    );
                }
                let result = maker.tick().await;
                maker.record_alert_failure("watchtower-pass", B256::ZERO, result.as_ref().err().and_then(|_| maker.service_alert("Watchtower pass failed; bridge processing will retry. Check maker health and logs.")));
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
            self.run_flow_observer()
        );
    }

    async fn tick(&self) -> Result<()> {
        let now = self.settlement.now().await?;
        let mut failed = Vec::new();
        for swap in self.store.watched_swaps()? {
            if let Err(e) = self.advance(&swap, now).await {
                failed.push(swap.id);
                warn!(swap_id = %swap.id, operation = "forward_watchtower", "{e:#}");
                self.record_alert_failure("forward", swap.id, self.forward_alert(&swap, "error", "Bridge processing error: watchtower could not advance this swap. Check maker logs; funds remain subject to escrow deadlines."));
            } else {
                self.record_alert_failure("forward", swap.id, None);
            }
        }
        for swap in self.store.pending_reverse_swaps()? {
            if let Err(e) = self.advance_reverse_claim(&swap).await {
                failed.push(swap.id);
                warn!(swap_id = %swap.id, operation = "reverse_claim", "{e:#}");
            }
        }
        self.record_monitor_errors(failed);
        Ok(())
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
                let mut snapshots = HashMap::new();
                for swap in self.store.watched_swaps()? {
                    snapshots.insert(
                        swap.zcash_account,
                        WalletSnapshot {
                            funds: wallet.funds(swap.zcash_account)?,
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
                            funds: wallet.funds(swap.account)?,
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
        self.record_alert_failure("zcash-sync", B256::ZERO, (!synced).then(|| self.service_alert("Zcash wallet sync failed; EVM settlement continues independently. Check lightwalletd and maker logs.")).flatten());
        anyhow::ensure!(synced, "Zcash sync did not complete");
        for swap in self.store.unsettled_swaps()? {
            if let Err(e) = self.advance_sweep(&swap).await {
                warn!(id = %swap.id, "Zcash sweep: {e:#}");
            }
        }
        for mut swap in self
            .store
            .watched_reverse_swaps()?
            .into_iter()
            .filter(|_| self.config.reverse.is_some())
        {
            self.queue_alert(self.reverse_alert(
                &swap,
                "accepted",
                "Bridge accepted; maker is monitoring USDC escrow funding.",
            ));
            if let Err(e) = self.advance_reverse(&mut swap, synced).await {
                warn!(id = %swap.id, "reverse swap: {e:#}");
                self.record_alert_failure("reverse", swap.id, self.reverse_alert(&swap, "error", "Bridge processing error: watchtower could not advance this swap. Check maker logs; funds remain subject to escrow deadlines."));
            } else {
                self.record_alert_failure("reverse", swap.id, None);
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "advance"), err(level = "warn"))]
    async fn advance(&self, swap: &Swap, now: u64) -> Result<()> {
        let confirmations = u64::from(self.config.evm_confirmations.get());
        let terms = self.terms(swap)?;
        let Some(chain) = self.settlement.swap(swap.id, &terms).await? else {
            if !swap.settled
                && now >= swap.t1
                && self.settlement.confirmed_now(confirmations).await? >= swap.t1
                && self
                    .settlement
                    .confirmed_swap(swap.id, &terms, confirmations)
                    .await?
                    .is_none()
            {
                info!(id = %swap.id, "the swap never opened");
                self.return_token(swap);
                self.settle(swap, None)?;
            }
            return Ok(());
        };
        if chain.refund_lock_until != 0 || chain.stage == Stage::Refunded {
            self.store.start_refund(&swap.id)?;
        }
        let (funds, sweep_confirmed, synced) = self.wallet_observation(swap);
        let chain_confirmed = if matches!(chain.stage, Stage::Claimed | Stage::Refunded) {
            self.settlement
                .confirmed_swap(swap.id, &terms, confirmations)
                .await?
                .is_some_and(|confirmed| {
                    confirmed.stage == chain.stage && confirmed.secret == chain.secret
                })
        } else {
            false
        };
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
        let observation = Observation {
            now,
            opened_at: swap.opened_at,
            expected_zat: swap.quote.deposit_zat,
            chain,
            funds,
            synced,
            chain_confirmed,
            cancelling: swap.refund_started,
            sweep_confirmed,
            lock_duration: self.lock_duration,
        };
        // Paid in full, if not yet confirmed: the accept's token goes back. An underpaid swap
        // is cancelled like one never paid into.
        if synced && funds.total >= swap.quote.deposit_zat {
            self.return_token(swap);
        }
        if synced && funds.total > 0 {
            self.queue_alert(self.forward_alert(swap, "funded", &format!(
                "User ZEC deposit observed: {} zat total; {} zat spendable. Confirmation and escrow checks continue.", funds.total, funds.spendable
            )));
        }
        let action = policy::decide(&observation, &self.config.timing);
        if action != Action::Wait {
            info!(id = %swap.id, ?action, "watchtower");
        }
        match action {
            Action::Wait => {}
            Action::MarkReady => {
                self.settlement.ready(swap.id, &terms).await?;
            }
            Action::LockRefund => {
                self.store.start_refund(&swap.id)?;
                self.settlement.lock_refund(swap.id, &terms).await?;
            }
            Action::Refund => {
                self.store.start_refund(&swap.id)?;
                let e = self.maker_share(swap.quote.nonce)?;
                self.settlement.refund(swap.id, &terms, &e).await?;
            }
            Action::Sweep => {} // The Zcash worker builds and broadcasts sweeps.
            Action::Settle => self.settle(swap, Some(&observation.chain))?,
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

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "advance_sweep"), err(level = "warn"))]
    async fn advance_sweep(&self, swap: &Swap) -> Result<()> {
        if let Some(chain) = self.settlement.swap(swap.id, &self.terms(swap)?).await?
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

    fn settle(&self, swap: &Swap, chain: Option<&OnChainSwap>) -> Result<()> {
        let event = self.forward_alert(swap, "finished", notifications::outcome(chain, false));
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
