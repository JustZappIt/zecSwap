use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use rand_core::{OsRng, RngCore};
use tokio::sync::Mutex;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};
use zcash_address::ZcashAddress;
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};
use zecswap_chain::evm::{Address, B256, OnChainSwap, OpenRequest, Settlement, swap_id};
use zecswap_chain::zcash::{AccountUuid, Lightwalletd, Prover, TxId, Wallet, connect};
use zecswap_core::{JointAccount, Payout, SecretShare, SwapContext, derive_maker_share};
use zeroize::Zeroizing;

use crate::config::{Config, Secrets};
use crate::policy::{self, Action, Observation};
use crate::store::{Store, Swap};

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
}

struct Zcash {
    wallet: Wallet,
    client: Lightwalletd,
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
    #[error("cannot fill that amount right now")]
    Unavailable,
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
        std::fs::create_dir_all(&config.data_dir)?;
        let sweep_to = config.sweep_to.parse().context("parsing sweep_to")?;
        let account = secrets.evm_key.address();
        let settlement = Settlement::connect(&config.evm_rpc, config.contract, secrets.evm_key)?;
        let chain_id = settlement
            .chain_id()
            .await
            .context("reaching the settlement chain")?;
        let lock_duration = settlement.lock_duration().await?;
        config.timing.check(lock_duration)?;
        let mut wallet = Wallet::open(config.data_dir.join("wallet.sqlite"), config.network())?;
        if let Some(confirmations) = config.confirmations {
            wallet = wallet.with_confirmations(confirmations);
        }
        let zcash = Zcash {
            wallet,
            client: connect(&config.lightwalletd)
                .await
                .context("reaching lightwalletd")?,
        };
        Ok(Self {
            store: Store::open(&config.data_dir.join("maker.sqlite"))?,
            root: secrets.root,
            chain_id,
            sweep_to,
            settlement,
            zcash: Mutex::new(zcash),
            prover: Prover::default(),
            account,
            lock_duration,
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

    pub async fn quote(&self, request: QuoteRequest) -> Result<Quote, MakerError> {
        let terms = self.config.pricing.terms(request.units).ok_or_else(|| {
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
        let zcash_account = {
            let mut zcash = self.zcash.lock().await;
            let Zcash { wallet, client } = &mut *zcash;
            wallet
                .import_joint(client, &joint, &format!("swap {id}"))
                .await?
        };
        let now = self.settlement.now().await?;
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
        if let Err(e) = self.store.insert_swap(&swap) {
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
        let mut ticker = tokio::time::interval(Duration::from_secs(self.config.timing.tick));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                warn!("watchtower pass failed: {e:#}");
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        // A lightwalletd outage must not stop the Base side: a reveal under a held lock
        // needs nothing from Zcash and can't wait for it.
        let synced = {
            let mut zcash = self.zcash.lock().await;
            let Zcash { wallet, client } = &mut *zcash;
            match wallet.sync(client).await {
                Ok(()) => true,
                Err(e) => {
                    warn!("acting on the last synced Zcash state: {e}");
                    false
                }
            }
        };
        let now = self.settlement.now().await?;
        for swap in self.store.unsettled_swaps()? {
            if let Err(e) = self.advance(&swap, now, synced).await {
                warn!(id = %swap.id, "{e:#}");
            }
        }
        Ok(())
    }

    async fn advance(&self, swap: &Swap, now: u64, synced: bool) -> Result<()> {
        let Some(chain) = self.settlement.swap(swap.id).await? else {
            if now >= swap.t1 {
                info!(id = %swap.id, "the swap never opened");
                self.settle(swap).await?;
            }
            return Ok(());
        };
        let (funds, sweep_mined) = {
            let zcash = self.zcash.lock().await;
            let funds = zcash.wallet.funds(swap.zcash_account)?;
            let sweep_mined = swap
                .sweep
                .map(|txid| zcash.wallet.is_mined(txid))
                .transpose()?;
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
            Action::Settle => self.settle(swap).await?,
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
        let txid = wallet.sweep(&self.prover, swap.zcash_account, &key, &self.sweep_to)?;
        // Recorded first: a sweep that never reaches the network expires, and the policy then
        // sweeps again.
        self.store.record_sweep(&swap.id, txid)?;
        wallet.broadcast(client, txid).await?;
        info!(id = %swap.id, %txid, "swept claimed deposit");
        Ok(())
    }

    async fn settle(&self, swap: &Swap) -> Result<()> {
        self.forget(swap.zcash_account).await;
        self.store.settle(&swap.id)
    }

    async fn forget(&self, account: AccountUuid) {
        if let Err(e) = self.zcash.lock().await.wallet.forget(account) {
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}
