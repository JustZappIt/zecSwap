mod monitoring;
mod notifications;
mod reverse;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use futures_util::FutureExt;
use rand_core::{OsRng, RngCore};
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use zcash_address::ZcashAddress;
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};
use zecswap_chain::evm::{Address, B256, OnChainSwap, OpenRequest, Settlement, swap_id};
use zecswap_chain::zcash::{
    AccountUuid, Lightwalletd, Prover, TxId, UnifiedSpendingKey, Wallet, connect,
};
use zecswap_core::{JointAccount, Payout, SecretShare, SwapContext, derive_maker_share};
use zeroize::Zeroizing;

use crate::config::{Config, Secrets};
use crate::policy::{self, Action, Observation};
use crate::store::{Store, Swap};
use crate::watchtower::{self, Health};

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
    inventory: Option<(AccountUuid, UnifiedSpendingKey)>,
    monitoring: monitoring::Monitoring,
    prices: crate::market::PriceBook,
    telegram: crate::telegram::Telegram,
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
    /// Nothing is left to do: the sweep is mined, the swap was refunded, or it never opened.
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
            client: connect(&config.lightwalletd)
                .await
                .context("reaching lightwalletd")?,
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
        Ok(Self {
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
            config,
        })
    }

    pub fn settlement(&self) -> &Settlement {
        &self.settlement
    }

    /// The account that opens swaps and holds the inventory.
    pub fn account(&self) -> Address {
        self.account
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
        }
    }

    /// New swaps require a completed watchtower pass within the last three tick intervals.
    /// Unavailable at startup until the first pass completes.
    pub fn check_watchtower(&self) -> Result<(), MakerError> {
        self.health.check()
    }

    pub async fn quote(&self, request: QuoteRequest) -> Result<Quote, MakerError> {
        self.health.check()?;
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
        OsRng.fill_bytes(&mut quote_id);
        let expires_at = unix_now() + self.config.timing.quote_ttl;
        self.health.check()?;
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
            maker_proof: self.context(quote_id).prove_maker(&e, OsRng),
            chain_id: self.chain_id,
            contract: self.settlement.contract(),
            token: self.config.token,
            amount: terms.amount,
            deposit_zat: terms.deposit_zat,
            expires_at,
        })
    }

    pub async fn accept(
        &self,
        quote_id: B256,
        acceptance: Acceptance,
    ) -> Result<Accepted, MakerError> {
        self.health.check()?;
        let mut zcash = self.zcash.lock().await;
        // A request can wait behind a long sync or proof after its first health check.
        // Check again before consuming its quote or importing an account.
        self.health.check()?;
        let Zcash { wallet, client } = &mut *zcash;
        let wallet = wallet.as_mut().ok_or(MakerError::WatchtowerUnavailable)?;
        let quote = self
            .store
            .take_quote(&quote_id.0, unix_now())?
            .ok_or(MakerError::UnknownQuote)?;
        let maker_share = self.maker_share(quote.nonce)?.public();
        let payout = Payout {
            user: quote.payout.into(),
            note: quote.payout_note.map(|note| note.0),
        };
        let Acceptance {
            user_share,
            user_proof,
            viewing_keys,
        } = acceptance;
        self.context(quote.id)
            .verify_user(&maker_share, &user_share, &payout, &user_proof)
            .map_err(|_| MakerError::Rejected("user share proof does not verify".into()))?;
        let joint = JointAccount::derive(&maker_share, &user_share, &viewing_keys)
            .map_err(|e| MakerError::Rejected(e.to_string()))?;
        let id = swap_id(self.account, &user_share);

        // Watch the deposit address before the user can learn it from the chain.
        let zcash_account = wallet
            .import_joint(client, &joint, &format!("swap {id}"))
            .await?;
        drop(zcash);
        let now = self.settlement.now().await?;
        if let Err(e) = self.health.check() {
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
            t1: now + timing.t1_after,
            sweep: None,
            settled: false,
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
        self.settlement
            .open(&OpenRequest {
                token: self.config.token,
                amount: swap.quote.amount,
                maker_share: &maker_share,
                user_share: &user_share,
                user: swap.quote.payout,
                t0: now + timing.t0_after,
                t1: swap.t1,
                payout_note: swap.quote.payout_note,
            })
            .await?;
        // The user can only deposit once it sees the swap, which on a slow chain can be minutes
        // after `now` when opens queue behind each other; `cancel_after` counts from here.
        self.store
            .set_opened_at(&id, self.settlement.now().await?)?;
        info!(%id, amount = swap.quote.amount, deposit_zat = swap.quote.deposit_zat, "opened swap");
        Ok(Accepted { swap_id: id })
    }

    pub fn status(&self, id: B256) -> Result<Option<Status>> {
        Ok(self.store.swap(&id)?.map(|swap| Status {
            sweep: swap.sweep,
            settled: swap.settled,
        }))
    }

    /// The watchtower: every tick, sync and take the next step for every unsettled swap.
    pub async fn run(self: Arc<Self>) {
        let maker = &self;
        let watchtower = watchtower::run(
            Duration::from_secs(self.config.timing.tick),
            &self.health,
            |panicked| async move {
                if panicked {
                    maker.zcash.lock().await.wallet = None;
                    maker.record_alert_failure("watchtower-panic", B256::ZERO, maker.service_alert("Watchtower panicked; wallet will reopen and processing will retry. Check maker logs."));
                }
                let result = maker.tick().await;
                maker.record_alert_failure("watchtower-pass", B256::ZERO, result.as_ref().err().and_then(|_| maker.service_alert("Watchtower pass failed; bridge processing will retry. Check maker health and logs.")));
                if result.is_ok() {
                    maker.record_alert_failure("watchtower-panic", B256::ZERO, None);
                }
                result
            },
        );
        tokio::join!(watchtower, self.run_notifications());
    }

    async fn tick(&self) -> Result<()> {
        // A lightwalletd outage must not stop the Base side: a reveal under a held lock
        // needs nothing from Zcash and can't wait for it.
        let synced = {
            let mut zcash = self.zcash.lock().await;
            zcash
                .sync_with(&self.config, |mut wallet, mut client| async move {
                    let result = wallet.sync(&mut client).await;
                    (wallet, result)
                })
                .await?
        };
        self.record_monitor_sync(synced);
        self.record_alert_failure("zcash-sync", B256::ZERO, (!synced).then(|| self.service_alert("Zcash wallet sync failed; watchtower is using the last synced state and retrying. Check lightwalletd and maker logs.")).flatten());
        let now = self.settlement.now().await?;
        let mut failed = Vec::new();
        for swap in self.store.unsettled_swaps()? {
            self.queue_alert(self.forward_alert(
                &swap,
                "accepted",
                "Bridge accepted; maker is monitoring escrow and ZEC deposit.",
            ));
            if let Err(e) = self.advance(&swap, now, synced).await {
                failed.push(swap.id);
                warn!(id = %swap.id, "{e:#}");
                self.record_alert_failure("forward", swap.id, self.forward_alert(&swap, "error", "Bridge processing error: watchtower could not advance this swap. Check maker logs; funds remain subject to escrow deadlines."));
            } else {
                self.record_alert_failure("forward", swap.id, None);
            }
        }
        for mut swap in self.store.pending_reverse_swaps()? {
            self.queue_alert(self.reverse_alert(
                &swap,
                "accepted",
                "Bridge accepted; maker is monitoring USDC escrow funding.",
            ));
            if let Err(e) = self.advance_reverse(&mut swap, synced).await {
                failed.push(swap.id);
                warn!(id = %swap.id, "reverse swap: {e:#}");
                self.record_alert_failure("reverse", swap.id, self.reverse_alert(&swap, "error", "Bridge processing error: watchtower could not advance this swap. Check maker logs; funds remain subject to escrow deadlines."));
            } else {
                self.record_alert_failure("reverse", swap.id, None);
            }
        }
        self.record_monitor_errors(failed);
        Ok(())
    }

    async fn advance(&self, swap: &Swap, now: u64, synced: bool) -> Result<()> {
        let Some(chain) = self.settlement.swap(swap.id).await? else {
            if now >= swap.t1 {
                info!(id = %swap.id, "the swap never opened");
                self.settle(swap, None).await?;
            }
            return Ok(());
        };
        let (funds, sweep_mined) = {
            let zcash = self.zcash.lock().await;
            let wallet = zcash.wallet()?;
            let funds = wallet.funds(swap.zcash_account)?;
            let sweep_mined = swap.sweep.map(|txid| wallet.is_mined(txid)).transpose()?;
            (funds, sweep_mined)
        };
        let observation = Observation {
            now,
            opened_at: swap.opened_at,
            expected_zat: swap.quote.deposit_zat,
            chain,
            funds,
            synced,
            sweep_mined,
            lock_duration: self.lock_duration,
        };
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
                self.settlement.ready(swap.id).await?;
            }
            Action::LockRefund => {
                self.settlement.lock_refund(swap.id).await?;
            }
            Action::Refund => {
                let e = self.maker_share(swap.quote.nonce)?;
                self.settlement.refund(swap.id, &e).await?;
            }
            Action::Sweep => self.sweep(swap, &observation.chain).await?,
            Action::Settle => self.settle(swap, Some(&observation.chain)).await?,
        }
        Ok(())
    }

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
        let txid = wallet.sweep(&self.prover, swap.zcash_account, &key, &self.sweep_to)?;
        // Recorded first: a sweep that never reaches the network expires, and the policy then
        // sweeps again.
        self.store.record_sweep(&swap.id, txid)?;
        wallet.broadcast(client, txid).await?;
        info!(id = %swap.id, %txid, "swept claimed deposit");
        Ok(())
    }

    async fn settle(&self, swap: &Swap, chain: Option<&OnChainSwap>) -> Result<()> {
        let event = self.forward_alert(swap, "finished", notifications::outcome(chain, false));
        self.forget(swap.zcash_account).await;
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
