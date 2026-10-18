//! A wallet's deposits, as the reference client plays them: public tokens sent to a deposit
//! address derived from the wallet's seed, shielded into the wallet's own Railgun balance through
//! a relayer, which pays the gas for the fee the deposit's authorization commits to. The relayer
//! is taken on trust for nothing: what lands must be the wallet's own note.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result, bail, ensure};
use tokio::sync::OnceCell;
use zecswap_api::Network;
use zecswap_api::relayer::{DepositTerms, Note, RescueDeposit, ShieldDeposit, deposit_fee};
use zecswap_chain::evm::deposits::{Deployment, Used};
use zecswap_chain::evm::railgun::Status;
use zecswap_chain::evm::{Settlement, Shielded};
use zecswap_core::{
    DepositKeys, DepositsDomain, ReceiveAuthorization, RescueAuthorization, derive_deposit_keys,
};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};
use zeroize::Zeroizing;

use crate::api::{Deposited, RelayerApi};
use crate::user::RAILGUN_WALLET;

/// How long an authorization is valid for once signed, where the relayer takes that long.
const VALIDITY: u64 = 60 * 60;
/// The relayer takes no authorization with less than this left to land.
const LANDING: u64 = 15 * 60;
/// How far the wallet's clock and the relayer's may differ.
const CLOCK_SLACK: u64 = 60;
/// How long a deposit is posted again while the relayer can't send it.
const PATIENCE: Duration = Duration::from_secs(30 * 60);
/// The first wait between posts the relayer asked no wait for, doubling to `LONGEST_WAIT`.
const BACKOFF: Duration = Duration::from_secs(15);
const LONGEST_WAIT: Duration = Duration::from_secs(5 * 60);
const MINED_WITHIN: Duration = Duration::from_secs(10 * 60);
const POLL: Duration = Duration::from_secs(3);
/// Unused deposit addresses in a row past which a wallet restored from its seed stops looking.
pub const GAP: u32 = 20;

/// What a wallet pins of a deposits deployment: never what a relayer says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositDeployment {
    pub chain_id: u64,
    pub contract: Address,
    pub token: Address,
}

/// The most a wallet lets a relayer keep from a deposit: `fee`, and `fee_bps` of what it moves,
/// so the cap grows with the deposit as the relayer's fee does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeCap {
    pub fee: u128,
    pub fee_bps: u32,
}

/// A deposit or rescue that landed, and the note Railgun recorded for the wallet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deposit {
    pub tx: B256,
    pub shielded: Shielded,
}

/// A wallet's deposit addresses, each shielded into the Railgun wallet of its Railgun seed.
pub struct Depositor {
    seed: Zeroizing<Vec<u8>>,
    network: Network,
    account: u32,
    railgun: RailgunKeys,
    chain: Settlement,
    relayer: RelayerApi,
    deployment: DepositDeployment,
    cap: FeeCap,
    contract: OnceCell<Deployment>,
}

impl Depositor {
    /// The deposit addresses of the seed's first account. `chain` reads the chain, sending
    /// nothing: the relayer sends.
    pub fn new(
        seed: &[u8],
        railgun_seed: &[u8],
        network: Network,
        chain: Settlement,
        relayer: RelayerApi,
        deployment: DepositDeployment,
        cap: FeeCap,
    ) -> Self {
        Self {
            seed: Zeroizing::new(seed.to_vec()),
            network,
            account: 0,
            railgun: RailgunKeys::from_seed(railgun_seed, RAILGUN_WALLET),
            chain,
            relayer,
            deployment,
            cap,
            contract: OnceCell::new(),
        }
    }

    /// The deposit addresses of the wallet's ZIP 32 account `account` instead.
    pub fn with_account(mut self, account: u32) -> Self {
        self.account = account;
        self
    }

    pub fn keys(&self, index: u32) -> Result<DepositKeys> {
        Ok(derive_deposit_keys(
            &self.seed,
            self.network.zcash_type(),
            self.account,
            index,
        )?)
    }

    pub fn address(&self, index: u32) -> Result<Address> {
        Ok(self.keys(index)?.key.address().into())
    }

    /// The Railgun wallet deposits shield into.
    pub fn railgun(&self) -> &RailgunKeys {
        &self.railgun
    }

    /// The relayer's deposit terms, for the pinned deployment.
    pub async fn terms(&self) -> Result<DepositTerms> {
        let terms = self.relayer.terms().await?;
        ensure!(
            terms.chain_id == self.deployment.chain_id,
            "the relayer serves another chain"
        );
        let deposits = terms.deposits.context("the relayer takes no deposits")?;
        ensure!(
            (deposits.contract, deposits.token)
                == (self.deployment.contract, self.deployment.token),
            "the relayer takes deposits for another deployment"
        );
        Ok(deposits)
    }

    /// Each address from `first` on, `count` of them: what it holds and whether it deposited.
    pub async fn scan(&self, first: u32, count: u32) -> Result<Vec<Used>> {
        let addresses = (first..first + count)
            .map(|index| self.address(index))
            .collect::<Result<Vec<_>>>()?;
        let DepositDeployment {
            contract, token, ..
        } = self.deployment;
        Ok(self
            .chain
            .deposit_addresses(contract, token, &addresses)
            .await?)
    }

    /// Every address used, as a wallet restored from its seed finds them: scanned `GAP` at a
    /// time until `GAP` in a row are unused.
    pub async fn recover(&self) -> Result<Vec<(u32, Used)>> {
        let (mut used, mut first) = (Vec::new(), 0);
        loop {
            let window = self.scan(first, GAP).await?;
            let found = window.iter().any(Used::used);
            used.extend(
                window
                    .into_iter()
                    .zip(first..)
                    .filter(|(state, _)| state.used())
                    .map(|(state, index)| (index, state)),
            );
            if !found {
                return Ok(used);
            }
            first += GAP;
        }
    }

    /// The address to show for the next deposit: the lowest never shielded from. One holding
    /// under the minimum stays shown until more arrives and it is shielded.
    pub async fn showing(&self) -> Result<u32> {
        let used = self.recover().await?;
        let shielded = |index| {
            used.iter()
                .any(|(used, state)| *used == index && state.deposited)
        };
        Ok((0..).find(|index| !shielded(*index)).unwrap_or(u32::MAX))
    }

    /// Signs a shield of address `index`'s whole balance, valid for an hour, to a note of the
    /// wallet's: the request to persist and then post. None while it holds under the minimum.
    pub async fn sign_shield(&self, index: u32) -> Result<Option<ShieldDeposit>> {
        let terms = self.terms().await?;
        let token = &self.contract().await?.token_domain;
        let keys = self.keys(index)?;
        let from = keys.key.address();
        let value = self
            .chain
            .token_balance(self.deployment.token, from.into())
            .await?;
        if value < terms.min_value {
            return Ok(None);
        }
        let fee = deposit_fee(terms.fee, terms.fee_bps, value);
        let cap = deposit_fee(self.cap.fee, self.cap.fee_bps, value);
        ensure!(
            fee <= cap && fee < value,
            "the relayer asks a fee of {fee} of {value}, over the {cap} this wallet pays"
        );
        let valid_before = now() + validity(&terms);
        let note = self.railgun.note(&keys.shield_entropy(valid_before))?;
        let nonce = self
            .domain()
            .deposit_nonce(&note.words(), fee, &terms.relayer.into());
        let digest = token.receive_with_authorization(&ReceiveAuthorization {
            from,
            to: self.deployment.contract.into(),
            value,
            valid_after: 0,
            valid_before,
            nonce,
        });
        Ok(Some(ShieldDeposit {
            chain_id: self.deployment.chain_id,
            contract: self.deployment.contract,
            from: from.into(),
            value,
            valid_after: 0,
            valid_before,
            note: (&note).into(),
            fee,
            signature: keys.key.sign(&digest).into(),
        }))
    }

    /// Shields address `index`'s balance: signs, hands each request to `keep` to persist before
    /// it is posted, posts it as the relayer's answers say, and waits for it to land. None while
    /// the address holds under the minimum.
    pub async fn shield(
        &self,
        index: u32,
        keep: impl FnMut(&ShieldDeposit) -> Result<()>,
    ) -> Result<Option<Deposit>> {
        let Some(request) = self.sign_shield(index).await? else {
            return Ok(None);
        };
        self.post_shield(index, request, keep).await.map(Some)
    }

    /// Posts `request`, a shield of address `index` signed before, until it lands: the same
    /// bytes again while the relayer can't send it, signed again once its fee or validity went
    /// stale, following whatever deposit of the address the relayer names.
    pub async fn post_shield(
        &self,
        index: u32,
        mut request: ShieldDeposit,
        mut keep: impl FnMut(&ShieldDeposit) -> Result<()>,
    ) -> Result<Deposit> {
        keep(&request)?;
        let give_up = Instant::now() + PATIENCE;
        let (mut wait, mut signed_again) = (BACKOFF, false);
        loop {
            if request.valid_before < now() + LANDING + CLOCK_SLACK {
                request = self.sign_again(index).await?;
                keep(&request)?;
            }
            match self.relayer.shield_deposit(&request).await {
                Deposited::Sent(sent) if !sent.is_empty() => {
                    return self.landed(&sent, request.from, Some(&request.note)).await;
                }
                // Another deposit of the address, which shields to another of the wallet's notes.
                Deposited::Conflict(sent) if !sent.is_empty() => {
                    return self.landed(&sent, request.from, None).await;
                }
                Deposited::Conflict(_) => bail!(
                    "the authorization is used in a transaction the relayer doesn't name: read the address's deposits from the chain"
                ),
                // A stale fee or validity is signed again once.
                Deposited::Refused(reason) if !signed_again => {
                    signed_again = true;
                    request = self
                        .sign_again(index)
                        .await
                        .with_context(|| format!("the relayer refused the deposit: {reason}"))?;
                    keep(&request)?;
                }
                Deposited::Refused(reason) => bail!("the relayer refused the deposit: {reason}"),
                Deposited::Sent(_) => bail!("the relayer sent nothing"),
                Deposited::Retry { reason, after } => {
                    ensure!(
                        Instant::now() < give_up,
                        "the relayer never sent the deposit: {reason}"
                    );
                    signed_again = false;
                    tokio::time::sleep(after.unwrap_or(wait).min(LONGEST_WAIT)).await;
                    wait = (wait * 2).min(LONGEST_WAIT);
                }
            }
        }
    }

    /// Shields again what Railgun sent back to address `index`'s vault, to a note of the wallet's:
    /// none while it holds no more than the relayer's fee.
    pub async fn rescue(
        &self,
        index: u32,
        mut keep: impl FnMut(&RescueDeposit) -> Result<()>,
    ) -> Result<Option<Deposit>> {
        let Some(request) = self.sign_rescue(index).await? else {
            return Ok(None);
        };
        keep(&request)?;
        let give_up = Instant::now() + PATIENCE;
        let mut wait = BACKOFF;
        loop {
            match self.relayer.rescue_deposit(&request).await {
                Deposited::Sent(sent) if !sent.is_empty() => {
                    return self
                        .landed(&sent, request.from, Some(&request.note))
                        .await
                        .map(Some);
                }
                Deposited::Conflict(sent) if !sent.is_empty() => {
                    return self.landed(&sent, request.from, None).await.map(Some);
                }
                Deposited::Retry { reason, after } => {
                    ensure!(
                        Instant::now() < give_up,
                        "the relayer never sent the rescue: {reason}"
                    );
                    tokio::time::sleep(after.unwrap_or(wait).min(LONGEST_WAIT)).await;
                    wait = (wait * 2).min(LONGEST_WAIT);
                }
                other => bail!("the relayer did not rescue: {other:?}"),
            }
        }
    }

    pub async fn sign_rescue(&self, index: u32) -> Result<Option<RescueDeposit>> {
        let terms = self.terms().await?;
        let keys = self.keys(index)?;
        let from = keys.key.address();
        let contract = self.deployment.contract;
        let vault = self.chain.deposit_vault(contract, from.into()).await?;
        let held = self
            .chain
            .token_balance(self.deployment.token, vault)
            .await?;
        // A rescue pays the fee alone.
        let fee = terms.fee;
        ensure!(
            fee <= self.cap.fee,
            "the relayer asks a fee of {fee}, over the {} this wallet pays",
            self.cap.fee
        );
        if held <= fee {
            return Ok(None);
        }
        let nonce = self
            .chain
            .deposit_rescue_nonce(contract, from.into())
            .await?;
        let note = self.railgun.note(&keys.rescue_entropy(nonce))?;
        let authorization = RescueAuthorization {
            nonce,
            deadline: now() + validity(&terms),
        };
        let digest = self.domain().rescue(
            &from,
            &note.commitment(),
            &terms.relayer.into(),
            fee,
            authorization,
        );
        Ok(Some(RescueDeposit {
            chain_id: self.deployment.chain_id,
            contract,
            from: from.into(),
            note: (&note).into(),
            fee,
            nonce,
            deadline: authorization.deadline,
            signature: keys.key.sign(&digest).into(),
        }))
    }

    async fn sign_again(&self, index: u32) -> Result<ShieldDeposit> {
        self.sign_shield(index)
            .await?
            .context("the address no longer holds enough to deposit")
    }

    /// The last of `sent`, once mined, if the pinned contract recorded it as a deposit or rescue
    /// of `from`, to `note` where given, and Railgun as a note of the wallet's: a relayer naming
    /// another transaction is not believed.
    async fn landed(&self, sent: &[B256], from: Address, note: Option<&Note>) -> Result<Deposit> {
        let tx = *sent.last().context("no transaction")?;
        let deadline = Instant::now() + MINED_WITHIN;
        loop {
            match self.chain.transaction_status(tx).await? {
                Status::Mined { succeeded: true } => break,
                Status::Mined { succeeded: false } => bail!(
                    "the deposit {tx} reverted: the relayer holds the address back for an hour"
                ),
                Status::Pending | Status::Unknown => {}
            }
            ensure!(Instant::now() < deadline, "{tx} did not mine");
            tokio::time::sleep(POLL).await;
        }
        let expected = note.map(|note| B256::from(ShieldNote::from(note).commitment()));
        let ours = |commitment: B256| expected.is_none_or(|expected| commitment == expected);
        let recorded = self
            .chain
            .deposits_landed(self.deployment.contract, tx)
            .await?;
        ensure!(
            recorded
                .iter()
                .any(|landed| landed.from == from && ours(landed.note)),
            "{tx} is no deposit of this address to its note"
        );
        let shielded = self
            .chain
            .shielded(tx)
            .await?
            .into_iter()
            .find(|shielded| {
                self.railgun.open(&shielded.note).is_some()
                    && ours(shielded.note.commitment().into())
            })
            .with_context(|| format!("{tx} shielded no note of this wallet's"))?;
        Ok(Deposit { tx, shielded })
    }

    /// The Deposits contract, read once and checked against the pin: its token, and the token's
    /// domain from its own name and version.
    async fn contract(&self) -> Result<&Deployment> {
        self.contract
            .get_or_try_init(|| async {
                let deployment = self.chain.deposits(self.deployment.contract).await?;
                ensure!(
                    deployment.token == self.deployment.token
                        && deployment.token_domain.chain_id == self.deployment.chain_id,
                    "the Deposits contract takes another token or is on another chain"
                );
                Ok(deployment)
            })
            .await
    }

    fn domain(&self) -> DepositsDomain {
        DepositsDomain {
            chain_id: self.deployment.chain_id,
            contract: self.deployment.contract.into(),
        }
    }
}

/// How long to sign for: an hour, or what the relayer takes if that is less.
fn validity(terms: &DepositTerms) -> u64 {
    VALIDITY.min(terms.max_validity.saturating_sub(CLOCK_SLACK))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}
