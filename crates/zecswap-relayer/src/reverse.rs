use zecswap_api::relayer::{Payout, Sent};
use zecswap_api::reverse::{Authorization, Refund};
use zecswap_chain::evm::{B256, Head, OnChainSwap, Reveal, Stage, wall_now};
use zecswap_core::{SecretShare, Terms, signer};
use zecswap_railgun::ShieldNote;

use crate::pricing::SwapFee;
use crate::turns::Step;
use crate::{Relayer, RelayerError, Result};

impl Relayer {
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "fund_reverse"), err(level = "warn"))]
    pub async fn fund_reverse(&self, request: zecswap_api::reverse::Funding) -> Result<Sent> {
        use zecswap_chain::evm::funding::FundingError;
        let mut policy = self.funding.clone().ok_or_else(|| {
            RelayerError::Rejected("initial reverse funding sponsorship is disabled".into())
        })?;
        policy.fee = self.honored_swap_fee(SwapFee::Funding).await;
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
        let lane = &self.lanes.funding;
        let tx = self
            .fund_once(request.swap_id, lane, async {
                lane.sponsor_reverse_funding(&policy, &validated)
                    .await
                    .map_err(map_error)
            })
            .await?;
        Ok(Sent {
            transactions: tx.into_iter().collect(),
        })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "ready_reverse"), err(level = "warn"))]
    pub async fn ready_reverse(&self, request: Authorization) -> Result<Sent> {
        let (swap, terms, _, head) = self.reverse_swap(request.swap_id, &request.terms).await?;
        self.check_reverse_signature(
            &swap,
            &self.domain.ready(&request.swap_id, request.deadline),
            &request.signature.0,
        )?;
        let readied = |swap: &OnChainSwap| matches!(swap.stage, Stage::Ready | Stage::Claimed);
        if readied(&swap) {
            return Ok(Sent {
                transactions: vec![],
            });
        }
        if head.timestamp >= swap.t0 {
            return Err(RelayerError::Rejected(
                "an escrow is readied only before t0".into(),
            ));
        }
        // A refund reveals the user's share, and a ready landing before it would revert it with
        // the share public: none follows a refund sent for the escrow, nor one the claims account
        // may still land from before a restart.
        self.steps.after_earlier(&self.lanes.claims).await?;
        if self.steps.sent(request.swap_id, Step::Refund) {
            return Err(RelayerError::Rejected(
                "a refund was sent for this escrow, which is never readied after one".into(),
            ));
        }
        let (id, locks) = (request.swap_id, &self.lanes.locks);
        let ready = locks.ready_with_sig(id, &terms, request.deadline, &request.signature.0);
        let sent = self.send_once(id, Step::Ready, locks, ready).await;
        let tx = self.unless_landed(id, &terms, sent, readied).await?;
        Ok(Sent {
            transactions: tx.into_iter().collect(),
        })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "refund_reverse"), err(level = "warn"))]
    pub async fn refund_reverse(&self, request: Refund) -> Result<Sent> {
        if request.swap_id != request.payout.swap_id || request.terms != request.payout.terms {
            return Err(RelayerError::Rejected(
                "refund and payout name different swaps or terms".into(),
            ));
        }
        let (swap, terms, commitment, head) =
            self.reverse_swap(request.swap_id, &request.terms).await?;
        let note = self
            .check_reverse_payout(&swap, commitment, &request.payout)
            .await?;
        if swap.stage == Stage::Refunded {
            return self.reverse_refund_payout(request.payout).await;
        }
        let window = self.windows.window(
            Reveal::Refund,
            true,
            swap.stage,
            (swap.t0, swap.t1),
            head.timestamp,
        );
        if !window.leaves(head.timestamp.max(wall_now()), self.config.claim_margin) {
            return Err(RelayerError::Rejected(match swap.stage {
                Stage::Ready => "a ready escrow is its maker's to claim until t1; its user may refund it from t1".into(),
                stage => format!("the escrow is {stage:?}"),
            }));
        }
        // A ready sent for the escrow and still unmined would land first, and revert the refund
        // with the user's share public: the maker's to claim with, the ZEC the maker's to sweep.
        let readies = &self.lanes.locks;
        self.steps.after_earlier(readies).await?;
        if self
            .steps
            .may_land(readies, request.swap_id, Step::Ready)
            .await?
        {
            return Err(RelayerError::Busy(
                "a ready sent for this escrow may still land",
                30,
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
        let (id, claims) = (request.swap_id, &self.lanes.claims);
        let sent = self
            .send_once(id, Step::Refund, claims, claims.refund(id, &terms, &secret))
            .await;
        let refund = self
            .unless_landed(id, &terms, sent, |swap| swap.stage == Stage::Refunded)
            .await?;
        let mut transactions: Vec<B256> = refund.into_iter().collect();
        match self
            .send_refund_payout(&request.payout, &terms, &note)
            .await
        {
            Ok(tx) => transactions.extend(tx),
            Err(e) => {
                tracing::warn!(id = %id, "refund payout pending: {e}");
                self.monitor.failed(
                    "reverse_refund_payout",
                    e.failure_kind().unwrap_or("internal"),
                );
            }
        }
        Ok(Sent { transactions })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "reverse_refund_payout"), err(level = "warn"))]
    pub async fn reverse_refund_payout(&self, request: Payout) -> Result<Sent> {
        let (swap, terms, commitment, _) =
            self.reverse_swap(request.swap_id, &request.terms).await?;
        if swap.stage != Stage::Refunded {
            return Err(RelayerError::Rejected("swap has not refunded".into()));
        }
        // A repeat, once paid out, sends nothing.
        if swap.paid_out {
            return Ok(Sent {
                transactions: vec![],
            });
        }
        let note = self
            .check_reverse_payout(&swap, commitment, &request)
            .await?;
        Ok(Sent {
            transactions: self
                .send_refund_payout(&request, &terms, &note)
                .await?
                .into_iter()
                .collect(),
        })
    }

    /// Pays a refunded escrow back into Railgun: the transaction, or none where one sent before
    /// landed.
    async fn send_refund_payout(
        &self,
        request: &Payout,
        terms: &Terms,
        note: &ShieldNote,
    ) -> Result<Option<B256>> {
        let id = request.swap_id;
        let payout =
            self.settlement
                .refund_payout(id, terms, note, request.fee, &request.signature.0);
        let sent = self
            .send_once(id, Step::RefundPayout, &self.settlement, payout)
            .await;
        self.unless_landed(id, terms, sent, |swap| swap.paid_out)
            .await
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "rescue_reverse"), err(level = "warn"))]
    pub async fn rescue_reverse(&self, request: zecswap_api::relayer::Rescue) -> Result<Sent> {
        let (swap, terms, _, _) = self.reverse_swap(request.swap_id, &request.terms).await?;
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
        self.check_fee(request.fee).await?;
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

    async fn check_reverse_payout(
        &self,
        swap: &OnChainSwap,
        commitment: B256,
        request: &Payout,
    ) -> Result<ShieldNote> {
        let note = ShieldNote::from(&request.note);
        if commitment != B256::from(note.commitment()) || request.fee >= swap.amount {
            return Err(RelayerError::Rejected("invalid refund note or fee".into()));
        }
        self.check_fee(request.fee).await?;
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

    /// The escrow, its terms, the note its refund pays and the newest block it is read as of.
    async fn reverse_swap(
        &self,
        id: B256,
        terms: &zecswap_api::Terms,
    ) -> Result<(OnChainSwap, Terms, B256, Head)> {
        let terms = Terms::from(terms);
        // The escrow's ZEC side, its terms' user, is the maker.
        self.admit(terms.token, terms.user)?;
        let (swap, head) = self.swap(id, &terms).await?;
        let funding = self
            .settlement
            .reverse_funding(id)
            .await?
            .ok_or_else(|| RelayerError::Rejected("not a reverse swap".into()))?;
        Ok((swap, terms, funding.refund_note, head))
    }
}
