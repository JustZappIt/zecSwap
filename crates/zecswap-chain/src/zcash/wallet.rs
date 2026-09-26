use std::convert::Infallible;
use std::num::NonZeroU32;
use std::path::Path;

use orchard::keys::SpendAuthorizingKey;
use pczt::Pczt;
use pczt::roles::signer::Signer;
use rand_core::OsRng;
use secrecy::SecretVec;
use zcash_address::ZcashAddress;
use zcash_client_backend::data_api::wallet::input_selection::{
    GreedyInputSelector, LockedInputPolicy, SpendPolicy,
};
use zcash_client_backend::data_api::wallet::{
    ConfirmationsPolicy, create_pczt_from_proposal, extract_and_store_transaction_from_pczt,
    propose_send_max_transfer, propose_transfer,
};
use zcash_client_backend::data_api::{
    Account, AccountBirthday, AccountPurpose, AccountSource, MaxSpendMode, WalletRead, WalletWrite,
};
use zcash_client_backend::fees::standard::SingleOutputChangeStrategy;
use zcash_client_backend::fees::{DustOutputPolicy, StandardFeeRule};
use zcash_client_backend::proposal::Proposal;
use zcash_client_backend::wallet::OvkPolicy;
use zcash_client_backend::zip321::{Payment, TransactionRequest};
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::wallet::init::init_wallet_db;
use zcash_client_sqlite::{AccountUuid, ReceivedNoteId, WalletDb};
use zcash_keys::keys::{UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_primitives::transaction::builder::BundlePadding;
use zcash_protocol::consensus::{BlockHeight, Network, Parameters};
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{ShieldedPool, TxId};
use zecswap_core::{JointAccount, SpendKey, sign_pczt};

use super::cache::MemoryBlockCache;
use super::lightwalletd::{self, Lightwalletd};
use super::prover::Prover;
use crate::Error;

const JOINT_KEY_SOURCE: &str = "zecswap";
const SYNC_BATCH_SIZE: u32 = 1_000;
const ORCHARD_POOLS: [ShieldedPool; 2] = [ShieldedPool::Orchard, ShieldedPool::Ironwood];

type Db = WalletDb<rusqlite::Connection, Network, SystemClock, OsRng>;

/// Value held in the Orchard-protocol pools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Funds {
    /// Everything received and unspent, confirmed or not.
    pub total: u64,
    /// What has enough confirmations to spend now.
    pub spendable: u64,
}

pub struct Wallet {
    db: Db,
    network: Network,
    blocks: MemoryBlockCache,
    confirmations: ConfirmationsPolicy,
}

impl Wallet {
    /// Opens a wallet that spends its own notes after 3 confirmations, and others' after 10.
    pub fn open(path: impl AsRef<Path>, network: Network) -> Result<Self, Error> {
        let mut db =
            WalletDb::for_path(path, network, SystemClock, OsRng).map_err(Error::wallet)?;
        init_wallet_db(&mut db, None)
            .map_err(|e| Error::Wallet(format!("migrating wallet: {e}")))?;
        Ok(Self {
            db,
            network,
            blocks: MemoryBlockCache::default(),
            confirmations: ConfirmationsPolicy::default(),
        })
    }

    /// Spends any note after `confirmations` instead. A deposit then counts sooner, and a
    /// chain reorganization is likelier to undo it after it counted: for testnets, or amounts
    /// small enough to risk.
    pub fn with_confirmations(mut self, confirmations: NonZeroU32) -> Self {
        self.confirmations = ConfirmationsPolicy::new_symmetrical(confirmations, false);
        self
    }

    pub fn network(&self) -> Network {
        self.network
    }

    /// Scans every tracked account up to the chain tip.
    pub async fn sync(&mut self, client: &mut Lightwalletd) -> Result<(), Error> {
        // Without an account there is no birthday to start from, and the wallet would scan
        // every pool's last incomplete shard: for the sparse Ironwood pool, hundreds of
        // thousands of blocks.
        if self.db.get_account_ids()?.is_empty() {
            return Ok(());
        }
        zcash_client_backend::sync::run(
            client,
            &self.network,
            &self.blocks,
            &mut self.db,
            SYNC_BATCH_SIZE,
        )
        .await
        .map_err(Error::wallet)
    }

    /// Starts watching a joint account from the chain tip, so it must be imported before
    /// anything is paid to it.
    pub async fn import_joint(
        &mut self,
        client: &mut Lightwalletd,
        joint: &JointAccount,
        name: &str,
    ) -> Result<AccountUuid, Error> {
        let encoded = joint.ufvk(self.network.network_type());
        let ufvk = UnifiedFullViewingKey::decode(&self.network, &encoded).map_err(Error::Wallet)?;
        let birthday = Self::birthday_at_tip(client).await?;
        // A spending account whose key is assembled outside the wallet, as for hardware signers.
        let purpose = AccountPurpose::Spending { derivation: None };
        let account =
            self.db
                .import_account_ufvk(name, &ufvk, &birthday, purpose, Some(JOINT_KEY_SOURCE))?;
        Ok(account.id())
    }

    /// Adds a seed-derived spending account born at the chain tip.
    pub async fn create_account(
        &mut self,
        client: &mut Lightwalletd,
        seed: &[u8],
        name: &str,
    ) -> Result<(AccountUuid, UnifiedSpendingKey), Error> {
        let birthday = Self::birthday_at_tip(client).await?;
        Ok(self
            .db
            .create_account(name, &SecretVec::new(seed.to_vec()), &birthday, None)?)
    }

    pub fn funds(&self, account: AccountUuid) -> Result<Funds, Error> {
        let summary = self.db.get_wallet_summary(self.confirmations)?;
        let Some(balance) = summary
            .as_ref()
            .and_then(|s| s.account_balances().get(&account))
        else {
            return Ok(Funds::default());
        };
        let pools = [balance.orchard_balance(), balance.ironwood_balance()];
        Ok(Funds {
            total: pools.iter().map(|pool| u64::from(pool.total())).sum(),
            spendable: pools
                .iter()
                .map(|pool| u64::from(pool.spendable_value()))
                .sum(),
        })
    }

    /// The account's Orchard-only unified address.
    pub fn address(&mut self, account: AccountUuid) -> Result<String, Error> {
        let request = UnifiedAddressRequest::ORCHARD;
        let address = match self
            .db
            .get_last_generated_address_matching(account, request)?
        {
            Some(address) => address,
            None => {
                self.db
                    .get_next_available_address(account, request)?
                    .ok_or_else(|| {
                        Error::Wallet("account cannot generate an Orchard address".into())
                    })?
                    .0
            }
        };
        Ok(address.encode(&self.network))
    }

    /// A never-used Orchard-only address of the account.
    pub fn fresh_address(&mut self, account: AccountUuid) -> Result<String, Error> {
        let (address, _) = self
            .db
            .get_next_available_address(account, UnifiedAddressRequest::ORCHARD)?
            .ok_or_else(|| Error::Wallet("account cannot generate an Orchard address".into()))?;
        Ok(address.encode(&self.network))
    }

    /// The first account derived from a seed, as opposed to an imported joint account.
    pub fn derived_account(&self) -> Result<Option<AccountUuid>, Error> {
        for id in self.db.get_account_ids()? {
            let account = self.db.get_account(id)?;
            if matches!(
                account.map(|a| a.source().clone()),
                Some(AccountSource::Derived { .. })
            ) {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Stores a transaction sending everything in a joint account to `to`, authorized by the
    /// combined key; `broadcast` sends it.
    pub fn sweep(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        key: &SpendKey,
        to: &ZcashAddress,
    ) -> Result<TxId, Error> {
        let proposal = propose_send_max_transfer::<_, _, _, Infallible>(
            &mut self.db,
            &self.network,
            account,
            &ORCHARD_POOLS,
            &StandardFeeRule::Zip317,
            to.clone(),
            None,
            MaxSpendMode::Everything,
            self.confirmations,
            &LockedInputPolicy::Exclude,
            None,
        )
        .map_err(Error::wallet)?;
        let sign = |pczt| Ok(sign_pczt(pczt, std::slice::from_ref(key))?);
        self.store_proposal(prover, account, &proposal, sign)
    }

    /// Stores a transaction paying every `(address, zatoshis)` from a seed-derived account;
    /// `broadcast` sends it.
    pub fn pay(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        usk: &UnifiedSpendingKey,
        payments: &[(ZcashAddress, u64)],
    ) -> Result<TxId, Error> {
        let payments = payments
            .iter()
            .map(|(to, amount)| {
                let amount = Zatoshis::from_u64(*amount)
                    .map_err(|_| Error::Wallet(format!("{amount} zatoshis is out of range")))?;
                Ok(Payment::without_memo(to.clone(), amount))
            })
            .collect::<Result<_, Error>>()?;
        let request = TransactionRequest::new(payments).map_err(Error::wallet)?;
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Ironwood,
            DustOutputPolicy::default(),
        );
        let proposal = propose_transfer::<_, _, _, _, Infallible>(
            &mut self.db,
            &self.network,
            account,
            &GreedyInputSelector::new(),
            &change_strategy,
            request,
            self.confirmations,
            &SpendPolicy::default(),
            None,
            None,
        )
        .map_err(Error::wallet)?;
        let ask = SpendAuthorizingKey::from(usk.orchard());
        self.store_proposal(prover, account, &proposal, |pczt| {
            sign_own_spends(pczt, &ask)
        })
    }

    /// Sends a stored transaction to the network. Sending one again is harmless.
    pub async fn broadcast(&mut self, client: &mut Lightwalletd, txid: TxId) -> Result<(), Error> {
        let tx = self
            .db
            .get_transaction(txid)?
            .ok_or_else(|| Error::Wallet(format!("transaction {txid} is not stored")))?;
        let mut raw = Vec::new();
        tx.write(&mut raw).map_err(Error::wallet)?;
        lightwalletd::broadcast(client, raw).await
    }

    pub fn is_mined(&self, txid: TxId) -> Result<bool, Error> {
        Ok(self.db.get_tx_height(txid)?.is_some())
    }

    /// Stops tracking an account whose swap has settled.
    pub fn forget(&mut self, account: AccountUuid) -> Result<(), Error> {
        Ok(self.db.delete_account(account)?)
    }

    async fn birthday_at_tip(client: &mut Lightwalletd) -> Result<AccountBirthday, Error> {
        let tip = lightwalletd::chain_tip(client).await?;
        let treestate = lightwalletd::tree_state(client, birthday_tree_height(tip)?).await?;
        AccountBirthday::from_treestate(treestate, None)
            .map_err(|e| Error::Wallet(format!("account birthday: {e:?}")))
    }

    fn store_proposal(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        proposal: &Proposal<StandardFeeRule, ReceivedNoteId>,
        sign: impl FnOnce(Pczt) -> Result<Pczt, Error>,
    ) -> Result<TxId, Error> {
        let pczt = create_pczt_from_proposal::<_, _, Infallible, _, Infallible, _>(
            &mut self.db,
            &self.network,
            account,
            OvkPolicy::Sender,
            proposal,
            None,
            BundlePadding::DEFAULT,
        )
        .map_err(Error::wallet)?;
        let (pczt, circuit) = prover.prove(pczt)?;
        extract_and_store_transaction_from_pczt::<_, ReceivedNoteId>(
            &mut self.db,
            sign(pczt)?,
            None,
            Some(&circuit.verifying_key),
        )
        .map_err(Error::wallet)
    }
}

fn birthday_tree_height(tip: BlockHeight) -> Result<BlockHeight, Error> {
    // A tree state describes the block BEFORE the birthday. A birthday above the tip
    // can produce an empty scan range after the previous joint account was deleted.
    // https://github.com/zcash/librustzcash/issues/2301
    u32::from(tip)
        .checked_sub(1)
        .map(BlockHeight::from_u32)
        .ok_or_else(|| Error::Wallet("cannot create an account at genesis".into()))
}

fn sign_own_spends(pczt: Pczt, ask: &SpendAuthorizingKey) -> Result<Pczt, Error> {
    let orchard = unsigned_spends(pczt.orchard());
    let ironwood = unsigned_spends(pczt.ironwood());
    let mut signer = Signer::new(pczt).map_err(|e| Error::Wallet(format!("{e:?}")))?;
    for index in orchard {
        signer
            .sign_orchard(index, ask)
            .map_err(|e| Error::Wallet(format!("{e:?}")))?;
    }
    for index in ironwood {
        signer
            .sign_ironwood(index, ask)
            .map_err(|e| Error::Wallet(format!("{e:?}")))?;
    }
    Ok(signer.finish())
}

fn unsigned_spends(bundle: &pczt::orchard::Bundle) -> Vec<usize> {
    bundle
        .actions()
        .iter()
        .enumerate()
        .filter(|(_, action)| action.spend().spend_auth_sig().is_none())
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests;
