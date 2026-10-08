use zecswap_api::relayer::{Payout, Sent};
use zecswap_api::reverse::{Authorization, Refund};
use zecswap_chain::evm::{B256, OnChainSwap, Stage};
use zecswap_core::{SecretShare, Terms, signer};
use zecswap_railgun::ShieldNote;

use crate::{Relayer, RelayerError, Result};

impl Relayer {
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "fund_reverse"), err(level = "warn"))]
    pub async fn fund_reverse(&self, request: zecswap_api::reverse::Funding) -> Result<Sent> {
        use zecswap_chain::evm::funding::FundingError;
        let policy = self.config.funding_policy(self.account).ok_or_else(|| {
            RelayerError::Rejected("initial reverse funding sponsorship is disabled".into())
        })?;
        if request.chain_id != self.domain.chain_id {
            return Err(RelayerError::Rejected("wrong funding chain".into()));
        }
        let map_error = |error| match error {
            FundingError::Rejected(reason) => RelayerError::Rejected(reason.into()),
            // Upstream RPC errors can contain request bytes. Do not log them.
            FundingError::Chain(_) => RelayerError::Internal(anyhow::anyhow!(
                "funding RPC unavailable or submission outcome unknown; reconcile escrow before retrying the same proof"
            )),
        };
        let validated = policy
            .validate(
                self.domain,
                request.swap_id,
                request.to,
                request.value,
                request.data,
            )
            .map_err(map_error)?;
        let tx = self
            .settlement
            .sponsor_reverse_funding(&policy, &validated)
            .await
            .map_err(map_error)?;
        Ok(Sent {
            transactions: tx.into_iter().collect(),
        })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "ready_reverse"), err(level = "warn"))]
    pub async fn ready_reverse(&self, request: Authorization) -> Result<Sent> {
        let (swap, terms, _) = self.reverse_swap(request.swap_id, &request.terms).await?;
        self.check_reverse_signature(
            &swap,
            &self.domain.ready(&request.swap_id, request.deadline),
            &request.signature.0,
        )?;
        if swap.stage == Stage::Ready || swap.stage == Stage::Claimed {
            return Ok(Sent {
                transactions: vec![],
            });
        }
        Ok(Sent {
            transactions: vec![
                self.settlement
                    .ready_with_sig(
                        request.swap_id,
                        &terms,
                        request.deadline,
                        &request.signature.0,
                    )
                    .await?,
            ],
        })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "lock_reverse_refund"), err(level = "warn"))]
    pub async fn lock_reverse_refund(&self, request: Authorization) -> Result<Sent> {
        let (swap, terms, _) = self.reverse_swap(request.swap_id, &request.terms).await?;
        self.check_reverse_signature(
            &swap,
            &self.domain.lock_refund(&request.swap_id, request.deadline),
            &request.signature.0,
        )?;
        let now = self.settlement.now().await?;
        if swap.stage == Stage::Refunded
            || swap.refund_lock_until > now.saturating_add(self.config.claim_margin)
        {
            return Ok(Sent {
                transactions: vec![],
            });
        }
        Ok(Sent {
            transactions: vec![
                self.settlement
                    .lock_refund_with_sig(
                        request.swap_id,
                        &terms,
                        request.deadline,
                        &request.signature.0,
                    )
                    .await?,
            ],
        })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "refund_reverse"), err(level = "warn"))]
    pub async fn refund_reverse(&self, request: Refund) -> Result<Sent> {
        if request.swap_id != request.payout.swap_id || request.terms != request.payout.terms {
            return Err(RelayerError::Rejected(
                "refund and payout name different swaps or terms".into(),
            ));
        }
        let (swap, terms, commitment) = self.reverse_swap(request.swap_id, &request.terms).await?;
        let note = self.check_reverse_payout(&swap, commitment, &request.payout)?;
        if swap.stage == Stage::Refunded {
            return self.reverse_refund_payout(request.payout).await;
        }
        let now = self.settlement.now().await?;
        if !matches!(swap.stage, Stage::Open | Stage::Ready)
            || swap.refund_lock_until <= now.saturating_add(self.config.claim_margin)
        {
            return Err(RelayerError::Rejected(
                "no refund lock with enough time left".into(),
            ));
        }
        let secret = SecretShare::from_be_bytes(&request.secret.0)
            .map_err(|e| RelayerError::Rejected(e.to_string()))?;
        if secret.public() != swap.maker_share {
            return Err(RelayerError::Rejected(
                "refund share does not match escrow".into(),
            ));
        }
        if !self.settlement.railgun_accepts(swap.token).await? {
            return Err(RelayerError::Rejected(
                "Railgun is not accepting refunds now".into(),
            ));
        }
        let tx = self
            .settlement
            .refund(request.swap_id, &terms, &secret)
            .await?;
        let mut transactions = vec![tx];
        match self
            .settlement
            .refund_payout(
                request.swap_id,
                &terms,
                &note,
                request.payout.fee,
                &request.payout.signature.0,
            )
            .await
        {
            Ok(tx) => transactions.push(tx),
            Err(e) => {
                tracing::warn!(id = %request.swap_id, "refund payout pending: {e}");
                self.monitor
                    .failed("reverse_refund_payout", crate::monitor::failure_kind(&e));
            }
        }
        Ok(Sent { transactions })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "reverse_refund_payout"), err(level = "warn"))]
    pub async fn reverse_refund_payout(&self, request: Payout) -> Result<Sent> {
        let (swap, terms, commitment) = self.reverse_swap(request.swap_id, &request.terms).await?;
        let note = self.check_reverse_payout(&swap, commitment, &request)?;
        if swap.stage != Stage::Refunded {
            return Err(RelayerError::Rejected("swap has not refunded".into()));
        }
        if swap.paid_out {
            return Ok(Sent {
                transactions: vec![],
            });
        }
        Ok(Sent {
            transactions: vec![
                self.settlement
                    .refund_payout(
                        request.swap_id,
                        &terms,
                        &note,
                        request.fee,
                        &request.signature.0,
                    )
                    .await?,
            ],
        })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "rescue_reverse"), err(level = "warn"))]
    pub async fn rescue_reverse(&self, request: zecswap_api::relayer::Rescue) -> Result<Sent> {
        let (swap, terms, _) = self.reverse_swap(request.swap_id, &request.terms).await?;
        if !swap.paid_out {
            return Err(RelayerError::Rejected("refund has not paid out".into()));
        }
        let authorization = zecswap_core::RescueAuthorization {
            nonce: request.nonce,
            deadline: request.deadline,
        };
        if self.settlement.now().await? > request.deadline
            || self.settlement.rescue_nonce(request.swap_id).await? != request.nonce
        {
            return Err(RelayerError::Rejected(
                "rescue approval is expired or already consumed".into(),
            ));
        }
        self.check_fee(request.fee)?;
        let note = ShieldNote::from(&request.note);
        self.check_reverse_signature(
            &swap,
            &self.domain.rescue(
                &request.swap_id,
                &note.commitment(),
                &self.account.into(),
                request.fee,
                authorization,
            ),
            &request.signature.0,
        )?;
        Ok(Sent {
            transactions: vec![
                self.settlement
                    .rescue(
                        request.swap_id,
                        &terms,
                        &note,
                        request.fee,
                        &request.signature.0,
                        authorization,
                    )
                    .await?,
            ],
        })
    }

    fn check_reverse_payout(
        &self,
        swap: &OnChainSwap,
        commitment: B256,
        request: &Payout,
    ) -> Result<ShieldNote> {
        let note = ShieldNote::from(&request.note);
        if commitment != B256::from(note.commitment()) || request.fee >= swap.amount {
            return Err(RelayerError::Rejected("invalid refund note or fee".into()));
        }
        self.check_fee(request.fee)?;
        self.check_reverse_signature(
            swap,
            &self
                .domain
                .refund_payout(&request.swap_id, &self.account.into(), request.fee),
            &request.signature.0,
        )?;
        Ok(note)
    }

    fn check_reverse_signature(
        &self,
        swap: &OnChainSwap,
        digest: &[u8; 32],
        signature: &[u8; 65],
    ) -> Result<()> {
        if signer(digest, signature) != Some(swap.maker.into()) {
            return Err(RelayerError::Rejected(
                "not signed by the reverse swap's user".into(),
            ));
        }
        Ok(())
    }

    async fn reverse_swap(
        &self,
        id: B256,
        terms: &zecswap_api::Terms,
    ) -> Result<(OnChainSwap, Terms, B256)> {
        let terms = Terms::from(terms);
        // The escrow's ZEC side, its terms' user, is the maker.
        self.admit(terms.token, terms.user)?;
        let swap = self.swap(id, &terms).await?;
        let funding = self
            .settlement
            .reverse_funding(id)
            .await?
            .ok_or_else(|| RelayerError::Rejected("not a reverse swap".into()))?;
        Ok((swap, terms, funding.refund_note))
    }
}
