use anyhow::{Context, Result, ensure};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use zecswap_api::reverse::{self, Phase};
use zecswap_api::{Acceptance, Accepted, Quote};
use zecswap_chain::evm::{B256, OnChainSwap, Stage, reverse_swap_id};
use zecswap_core::{JointAccount, Payout};

use super::{Maker, MakerError, Spend, Zcash, unix_now};
use crate::store::ReverseSwap;

impl Maker {
    pub async fn reverse_inventory(&self) -> Result<(String, zecswap_chain::zcash::Funds)> {
        let (account, _) = self.inventory.as_ref().context("reverse swaps disabled")?;
        let mut zcash = self.zcash.lock().await;
        let Zcash { wallet, client } = &mut *zcash;
        let wallet = wallet.as_mut().context("Zcash wallet needs reopening")?;
        wallet.sync(client).await?;
        Ok((wallet.address(*account)?, wallet.funds(*account)?))
    }

    pub async fn reverse_quote(
        &self,
        request: reverse::QuoteRequest,
    ) -> Result<reverse::Quote, MakerError> {
        self.check_watchtower()?;
        let config = self
            .config
            .reverse
            .as_ref()
            .ok_or(MakerError::Unavailable)?;
        let (account, _) = self.inventory.as_ref().ok_or(MakerError::Unavailable)?;
        if request.user.is_zero() || request.refund_note.is_zero() {
            return Err(MakerError::Rejected(
                "user and refund note must be nonzero".into(),
            ));
        }
        self.prices.refresh().await;
        let pricing = self
            .prices
            .quote(unix_now())
            .ok_or(MakerError::PriceUnavailable)?;
        let terms = pricing
            .policy
            .reverse_terms(request.units)
            .ok_or_else(|| MakerError::Rejected("amount is outside the quotable range".into()))?;
        let zcash = self.zcash.lock().await;
        let available = zcash.wallet()?.funds(*account)?.spendable;
        if available
            < self
                .reverse_reserved()?
                .saturating_add(terms.deposit_zat)
                .saturating_add(config.fee_reserve_zat)
        {
            return Err(MakerError::Unavailable);
        }
        drop(zcash);
        if self.settlement.railgun().await?.is_zero() {
            return Err(MakerError::Unavailable);
        }
        let now = self.settlement.now().await?;
        let mut id = [0; 32];
        UnwrapErr(SysRng).fill_bytes(&mut id);
        self.check_watchtower()?;
        if !pricing.fresh(unix_now()) {
            return Err(MakerError::PriceUnavailable);
        }
        let quote = self.store.insert_reverse_quote(|nonce| {
            let share = self.maker_share(nonce)?;
            Ok(reverse::Quote {
                terms: Quote {
                    quote_id: id.into(),
                    maker: self.account,
                    maker_share: share.public(),
                    maker_proof: self.context(id).prove_maker(&share, UnwrapErr(SysRng)),
                    chain_id: self.chain_id,
                    contract: self.settlement.contract(),
                    token: self.config.token,
                    amount: terms.amount,
                    deposit_zat: terms.deposit_zat,
                    expires_at: unix_now() + self.config.timing.quote_ttl,
                },
                user: request.user,
                refund_note: request.refund_note,
                funding_deadline: now + config.funding_window,
                ready_deadline: now + config.ready_after,
                refund_after: now + config.refund_after,
            })
        })?;
        self.record_quote_mark(&quote.terms.quote_id.0, pricing.mark());
        Ok(quote)
    }

    /// With `[tokens]`, `spend` is the accept's token: kept spent once the quote is taken,
    /// spendable again otherwise, a repeated accept of the same swap included.
    #[tracing::instrument(skip_all, fields(operation = "accept_reverse", quote_id = %id, swap_id = tracing::field::Empty), err(level = "warn"))]
    pub async fn accept_reverse(
        &self,
        id: B256,
        acceptance: Acceptance,
        spend: Option<&Spend>,
    ) -> Result<Accepted, MakerError> {
        self.check_watchtower()?;
        let config = self
            .config
            .reverse
            .as_ref()
            .ok_or(MakerError::Unavailable)?;
        let (inventory, _) = self.inventory.as_ref().ok_or(MakerError::Unavailable)?;
        let (nonce, quote) = self
            .store
            .reverse_quote(id)?
            .ok_or(MakerError::UnknownQuote)?;
        self.context(id.0)
            .verify_user(
                &quote.terms.maker_share,
                &acceptance.user_share,
                &Payout {
                    user: quote.user.into(),
                    note: Some(quote.refund_note.0),
                },
                &acceptance.user_proof,
            )
            .map_err(|_| MakerError::Rejected("user share proof does not verify".into()))?;
        self.token_request(&acceptance)?;
        let swap_id = reverse_swap_id(quote.user, &quote.terms.maker_share);
        tracing::Span::current().record("swap_id", tracing::field::display(swap_id));
        let mut zcash = self.zcash.lock().await;
        self.check_watchtower()?;
        if let Some(existing) = self.store.reverse_swap(swap_id)? {
            if existing.acceptance.user_share != acceptance.user_share
                || existing.acceptance.viewing_keys.to_bytes() != acceptance.viewing_keys.to_bytes()
            {
                return Err(MakerError::Rejected(
                    "quote was accepted with other keys".into(),
                ));
            }
            return Ok(Accepted {
                swap_id,
                t0: quote.ready_deadline,
                t1: quote.refund_after,
            });
        }
        if unix_now() >= quote.terms.expires_at {
            return Err(MakerError::UnknownQuote);
        }
        self.admit_another()?;
        let reserved = self.reverse_reserved()?;
        let Zcash { wallet, client } = &mut *zcash;
        let wallet = wallet.as_mut().ok_or(MakerError::WatchtowerUnavailable)?;
        if wallet.funds(*inventory)?.spendable
            < reserved
                .saturating_add(quote.terms.deposit_zat)
                .saturating_add(config.fee_reserve_zat)
        {
            return Err(MakerError::Unavailable);
        }
        let joint = JointAccount::derive(
            &quote.terms.maker_share,
            &acceptance.user_share,
            &acceptance.viewing_keys,
        )
        .map_err(|e| MakerError::Rejected(e.to_string()))?;
        let account = wallet
            .import_joint(client, &joint, &format!("reverse {swap_id}"))
            .await?;
        let swap = ReverseSwap {
            id: swap_id,
            nonce,
            quote,
            acceptance,
            account,
            deposit: None,
            sweep: None,
            settled: false,
            token_return: None,
        };
        let event = self.reverse_alert(
            &swap,
            "accepted",
            "Bridge accepted; awaiting user USDC escrow funding.",
        );
        if let Err(error) = self
            .store
            .insert_reverse_swap(&swap, unix_now(), event.as_ref())
        {
            wallet.forget(account)?;
            return Err(error.into());
        }
        if let Some(spend) = spend {
            spend.keep()?;
        }
        tracing::info!(%swap_id, outcome = "accepted", "reverse swap accepted; awaiting escrow funding");
        Ok(Accepted {
            swap_id,
            t0: swap.quote.ready_deadline,
            t1: swap.quote.refund_after,
        })
    }

    pub(super) fn reverse_reserved(&self) -> Result<u64> {
        let fee = self
            .config
            .reverse
            .as_ref()
            .map_or(0, |config| config.fee_reserve_zat);
        self.store
            .pending_reverse_swaps()?
            .iter()
            .filter(|swap| swap.deposit.is_none())
            .try_fold(0u64, |sum, swap| {
                sum.checked_add(swap.quote.terms.deposit_zat)
                    .and_then(|sum| sum.checked_add(fee))
                    .context("reverse inventory reservation overflow")
            })
    }

    pub async fn reverse_status(&self, id: B256) -> Result<Option<reverse::Status>> {
        let Some(swap) = self.store.reverse_swap(id)? else {
            return Ok(None);
        };
        let chain = self.settlement.swap(id, &swap.terms()).await?;
        let now = self.settlement.now().await?;
        let phase = match chain {
            None if swap.settled => Phase::Expired,
            None => Phase::AwaitingFunding,
            Some(chain) => {
                self.verify_reverse(&swap).await?;
                match chain.stage {
                    Stage::Claimed => Phase::ZecAvailable,
                    Stage::Refunded if chain.paid_out => Phase::Refunded,
                    Stage::Refunded => Phase::Refunding,
                    _ if chain.refund_lock_until != 0 => Phase::Refunding,
                    Stage::Open if now >= chain.t0 => Phase::RefundAvailable,
                    Stage::Ready if now >= chain.t1 && now >= chain.claim_lock_until => {
                        Phase::RefundAvailable
                    }
                    Stage::Ready => Phase::Claiming,
                    Stage::Open if swap.deposit.is_some() => Phase::AwaitingReady,
                    Stage::Open => {
                        let confirmations = self
                            .config
                            .reverse
                            .as_ref()
                            .context("reverse swaps disabled")?
                            .evm_confirmations
                            .get();
                        if self
                            .settlement
                            .confirmed_reverse_funding(id, confirmations.into())
                            .await?
                            .is_some()
                        {
                            Phase::SendingZec
                        } else {
                            Phase::ConfirmingFunding
                        }
                    }
                }
            }
        };
        Ok(Some(reverse::Status {
            swap_id: id,
            phase,
            deposit_txid: swap.deposit.map(|id| id.to_string()),
            ready_deadline: swap.quote.ready_deadline,
            refund_after: swap.quote.refund_after,
            token_return: swap.token_return,
        }))
    }

    /// The escrow, whose terms reading it checked, is for this maker on this deployment and
    /// refunds to the quoted note.
    async fn verify_reverse(&self, swap: &ReverseSwap) -> Result<()> {
        self.check_reverse(swap)?;
        let terms = &swap.quote;
        let funding = self
            .settlement
            .reverse_funding(swap.id)
            .await?
            .context("not a reverse escrow")?;
        ensure!(
            funding.refund_note == terms.refund_note,
            "reverse refund note differs from the quote"
        );
        Ok(())
    }

    /// The swap was quoted by this maker, on this deployment and under this root secret.
    pub(super) fn check_reverse(&self, swap: &ReverseSwap) -> Result<()> {
        let quote = &swap.quote.terms;
        ensure!(
            quote.chain_id == self.chain_id && quote.contract == self.settlement.contract(),
            "reverse swap {} is on another deployment",
            swap.id
        );
        ensure!(
            quote.maker == self.account,
            "reverse swap {} was quoted from another EVM account than {}",
            swap.id,
            self.account
        );
        ensure!(
            quote.token == self.config.token,
            "reverse swap {} escrows another token than {}",
            swap.id,
            self.config.token
        );
        ensure!(
            quote.maker_share == self.maker_share(swap.nonce)?.public(),
            "reverse swap {} was quoted under another MAKER_ROOT_SECRET",
            swap.id
        );
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "advance_reverse"), err(level = "warn"))]
    pub(super) async fn advance_reverse(&self, swap: &mut ReverseSwap, synced: bool) -> Result<()> {
        let config = self
            .config
            .reverse
            .as_ref()
            .context("reverse swaps disabled")?;
        let terms = swap.terms();
        let Some(chain) = self.settlement.swap(swap.id, &terms).await? else {
            if !swap.settled
                && swap.deposit.is_none()
                && self
                    .settlement
                    .confirmed_now(config.evm_confirmations.get().into())
                    .await?
                    > swap.quote.funding_deadline
                && self
                    .settlement
                    .confirmed_swap(swap.id, &terms, config.evm_confirmations.get().into())
                    .await?
                    .is_none()
            {
                self.finish_reverse(swap, super::notifications::outcome(None, true))
                    .await?;
            }
            return Ok(());
        };
        // The escrow holds the user's payment from `openReverse` on: the token goes back.
        self.return_reverse_token(swap);
        if swap.settled {
            let confirmed = self
                .settlement
                .confirmed_swap(swap.id, &terms, config.evm_confirmations.get().into())
                .await?;
            if confirmed.as_ref().is_some_and(|confirmed| {
                confirmed.stage == chain.stage && confirmed.secret == chain.secret
            }) {
                match chain.stage {
                    Stage::Claimed => return Ok(()),
                    Stage::Refunded if swap.deposit.is_none() => return Ok(()),
                    Stage::Refunded => {
                        if let Some(txid) = swap.sweep
                            && self.zcash.lock().await.wallet()?.is_confirmed(txid)?
                        {
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
            swap.settled = false;
            self.store.save_reverse_swap(swap)?;
        }
        if swap.deposit.is_none()
            && self
                .settlement
                .confirmed_now(config.evm_confirmations.get().into())
                .await?
                > swap.quote.ready_deadline
        {
            self.finish_reverse(
                swap,
                "Bridge expired without maker funding. Escrow refund may still be pending.",
            )
            .await?;
            return Ok(());
        }
        self.verify_reverse(swap).await?;
        match chain.stage {
            Stage::Claimed | Stage::Refunded => {
                let confirmed = self
                    .settlement
                    .confirmed_swap(swap.id, &terms, config.evm_confirmations.get().into())
                    .await?;
                if !confirmed.is_some_and(|confirmed| confirmed == chain) {
                    return Ok(());
                }
                if chain.stage == Stage::Claimed {
                    self.finish_reverse(swap, super::notifications::outcome(Some(&chain), true))
                        .await?;
                } else {
                    self.recover_reverse(swap, &chain, synced).await?;
                }
            }
            Stage::Ready => {} // The independent EVM loop handles claim deadlines.
            Stage::Open => {
                let now = self.settlement.now().await?;
                if !synced
                    || chain.refund_lock_until != 0
                    || now.saturating_add(config.deposit_margin) >= chain.t0
                {
                    return Ok(());
                }
                let Some(funding) = self
                    .settlement
                    .confirmed_reverse_funding(swap.id, config.evm_confirmations.get().into())
                    .await?
                else {
                    return Ok(());
                };
                ensure!(
                    funding.refund_note == swap.quote.refund_note,
                    "confirmed refund commitment differs"
                );
                if self
                    .settlement
                    .confirmed_swap(swap.id, &terms, config.evm_confirmations.get().into())
                    .await?
                    .as_ref()
                    != Some(&chain)
                {
                    return Ok(());
                }
                self.queue_alert(self.reverse_alert(swap, "funded", "User USDC escrow funding confirmed; maker is preparing or confirming the ZEC deposit."));
                self.fund_reverse(swap).await?;
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "advance_reverse_claim"), err(level = "warn"))]
    pub(super) async fn advance_reverse_claim(&self, swap: &ReverseSwap) -> Result<()> {
        let Some(snapshot) = self.wallet_snapshot(swap.account) else {
            return Ok(());
        };
        if snapshot.funds.spendable < swap.quote.terms.deposit_zat {
            return Ok(());
        }
        let terms = swap.terms();
        let Some(chain) = self.settlement.swap(swap.id, &terms).await? else {
            return Ok(());
        };
        if chain.stage != Stage::Ready {
            return Ok(());
        }
        self.verify_reverse(swap).await?;
        let now = self.settlement.now().await?;
        if now.saturating_add(self.config.timing.reveal_margin) < chain.claim_lock_until {
            let sent = self
                .settlement
                .claim(swap.id, &terms, &self.maker_share(swap.nonce)?)
                .await;
            self.journal(swap.id, "claim", sent)?;
        } else if now >= chain.claim_lock_until
            && now >= chain.refund_lock_until
            && !(chain.claim_lock_until > chain.refund_lock_until
                && now < chain.claim_lock_until.saturating_add(self.lock_duration))
        {
            let sent = self.settlement.lock_claim(swap.id, &terms).await;
            self.journal(swap.id, "lock_claim", sent)?;
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "fund_reverse"), err(level = "warn"))]
    async fn fund_reverse(&self, swap: &mut ReverseSwap) -> Result<()> {
        let (inventory, key) = self
            .inventory
            .as_ref()
            .context("reverse inventory unavailable")?;
        let mut zcash = self.zcash.lock().await;
        let Zcash { wallet, client } = &mut *zcash;
        let wallet = wallet.as_mut().context("Zcash wallet needs reopening")?;
        if let Some(txid) = swap.deposit {
            if wallet.is_expired(txid)? {
                swap.deposit = None;
                self.store.save_reverse_swap(swap)?;
            } else {
                if !wallet.is_mined(txid)? {
                    wallet.broadcast(client, txid).await?;
                }
                return Ok(());
            }
        }
        let joint = JointAccount::derive(
            &swap.quote.terms.maker_share,
            &swap.acceptance.user_share,
            &swap.acceptance.viewing_keys,
        )?;
        let address = joint
            .unified_address(match self.config.network {
                crate::Chain::Mainnet => zecswap_core::NetworkType::Main,
                crate::Chain::Testnet => zecswap_core::NetworkType::Test,
            })
            .parse()?;
        let txid = wallet.pay(
            &self.prover,
            *inventory,
            key,
            &[(address, swap.quote.terms.deposit_zat)],
        )?;
        swap.deposit = Some(txid);
        self.store.save_reverse_swap(swap)?;
        wallet.broadcast(client, txid).await?;
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "recover_reverse"), err(level = "warn"))]
    async fn recover_reverse(
        &self,
        swap: &mut ReverseSwap,
        chain: &OnChainSwap,
        synced: bool,
    ) -> Result<()> {
        if swap.deposit.is_none() {
            return self
                .finish_reverse(swap, super::notifications::outcome(Some(chain), true))
                .await;
        }
        if !synced {
            return Ok(());
        }
        let mut zcash = self.zcash.lock().await;
        let Zcash { wallet, client } = &mut *zcash;
        let wallet = wallet.as_mut().context("Zcash wallet needs reopening")?;
        if let Some(txid) = swap.sweep {
            if wallet.is_confirmed(txid)? {
                drop(zcash);
                return self
                    .finish_reverse(swap, super::notifications::outcome(Some(chain), true))
                    .await;
            }
            if wallet.is_expired(txid)? {
                swap.sweep = None;
                self.store.save_reverse_swap(swap)?;
            } else {
                if !wallet.is_mined(txid)? {
                    wallet.broadcast(client, txid).await?;
                }
                return Ok(());
            }
        }
        if wallet.funds(swap.account)?.spendable == 0 {
            if let Some(txid) = swap.deposit
                && wallet.is_expired(txid)?
            {
                drop(zcash);
                return self
                    .finish_reverse(swap, super::notifications::outcome(Some(chain), true))
                    .await;
            }
            return Ok(());
        }
        let joint = JointAccount::derive(
            &swap.quote.terms.maker_share,
            &swap.acceptance.user_share,
            &swap.acceptance.viewing_keys,
        )?;
        let key = joint.spend_key(
            &self.maker_share(swap.nonce)?,
            &chain.revealed()?.context("refund share missing")?,
        )?;
        let txid = wallet.sweep(&self.prover, swap.account, &key, &self.sweep_to)?;
        swap.sweep = Some(txid);
        self.store.save_reverse_swap(swap)?;
        wallet.broadcast(client, txid).await?;
        Ok(())
    }

    /// As `return_token` does for a forward swap.
    fn return_reverse_token(&self, swap: &mut ReverseSwap) {
        let (Some(gate), Some(request), None) = (
            &self.tokens,
            &swap.acceptance.token_request,
            &swap.token_return,
        ) else {
            return;
        };
        let returned = gate
            .read_return_request(request)
            .and_then(|request| gate.sign_return(&request))
            .and_then(|signature| {
                swap.token_return = Some(signature);
                self.store.save_reverse_swap(swap)
            });
        if let Err(e) = returned {
            swap.token_return = None;
            tracing::warn!(swap_id = %swap.id, "could not hand the token back: {e:#}");
        }
    }

    #[tracing::instrument(skip_all, fields(swap_id = %swap.id, operation = "finish_reverse"), err(level = "warn"))]
    async fn finish_reverse(&self, swap: &mut ReverseSwap, detail: &str) -> Result<()> {
        let event = self.reverse_alert(swap, "finished", detail);
        swap.settled = true;
        self.store
            .save_reverse_swap_with_notification(swap, event.as_ref())?;
        // Retain the account and transaction history for recovery after a reorg.
        Ok(())
    }
}
