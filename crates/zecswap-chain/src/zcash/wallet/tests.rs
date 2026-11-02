use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

use incrementalmerkletree::Position;
use incrementalmerkletree::frontier::Frontier;
use orchard::builder::BundleType;
use orchard::bundle::BundleVersion;
use orchard::keys::Scope;
use orchard::note_encryption::COMPACT_NOTE_SIZE;
use orchard::tree::MerkleHashOrchard;
use orchard::value::NoteValue;
use orchard::{Anchor, Bundle};
use tempfile::TempDir;
use zcash_client_backend::data_api::WalletCommitmentTrees;
use zcash_client_backend::data_api::chain::{
    BlockCache, ChainState, CommitmentTreeRoot, scan_cached_blocks,
};
use zcash_client_backend::proto::compact_formats::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx,
};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade};
use zcash_protocol::memo::MemoBytes;
use zecswap_core::{SecretShare, ViewingKeys};

use super::*;

const TIP: u32 = 4_395_130;
const DEPOSIT: u64 = 1_000_000;

/// One prover for every test, so the circuit's keys are built once.
static PROVER: LazyLock<Prover> = LazyLock::new(Prover::default);

/// Mainnet's NU7 activation height, as the crates schedule it.
fn nu7() -> u32 {
    Network::MainNetwork
        .activation_height(NetworkUpgrade::Nu7)
        .unwrap()
        .into()
}

fn random_joint() -> JointAccount {
    JointAccount::derive(
        &SecretShare::random(UnwrapErr(SysRng)).public(),
        &SecretShare::random(UnwrapErr(SysRng)).public(),
        &ViewingKeys::random(UnwrapErr(SysRng)),
    )
    .unwrap()
}

fn import_at(wallet: &mut Wallet, joint: &JointAccount, birthday: u32) -> AccountUuid {
    let ufvk =
        UnifiedFullViewingKey::decode(&wallet.network, &joint.ufvk(wallet.network.network_type()))
            .unwrap();
    let birthday = AccountBirthday::from_parts(
        ChainState::empty(BlockHeight::from_u32(birthday - 1), BlockHash([0; 32])),
        None,
    );
    wallet
        .db
        .import_account_ufvk(
            "test joint",
            &ufvk,
            &birthday,
            AccountPurpose::Spending { derivation: None },
            Some(JOINT_KEY_SOURCE),
        )
        .unwrap()
        .id()
}

fn previous_swap(scanned: bool) -> (TempDir, Wallet) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wallet.sqlite");
    let mut wallet = Wallet::open(&path, Network::TestNetwork).unwrap();
    let old = import_at(&mut wallet, &random_joint(), 4_395_100);
    if scanned {
        // Synthetic scan metadata only: no incident database or real keys are needed.
        let conn = rusqlite::Connection::open(&path).unwrap();
        for height in 4_395_100..=4_395_105 {
            conn.execute(
                "INSERT INTO blocks (height, hash, time, sapling_tree,
                 sapling_commitment_tree_size, orchard_commitment_tree_size,
                 ironwood_commitment_tree_size) VALUES (?1, zeroblob(32), 0, x'000000', 0, 0, 0)",
                [height],
            )
            .unwrap();
        }
        conn.execute_batch(
            "DELETE FROM scan_queue;
             INSERT INTO scan_queue VALUES (280000, 4395100, 0), (4395100, 4395106, 10);",
        )
        .unwrap();
    }
    wallet.forget(old).unwrap();
    assert!(wallet.db.get_account_ids().unwrap().is_empty());
    // Sync downloads subtree roots before updating the tip. One completed shard is enough.
    wallet
        .db
        .put_orchard_subtree_roots(
            0,
            &[CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(4_094_022),
                MerkleHashOrchard::from_bytes(&[0; 32]).unwrap(),
            )],
        )
        .unwrap();
    (dir, wallet)
}

#[test]
fn future_birthday_panics_only_with_retained_blocks_at_unchanged_tip() {
    for (scanned, birthday, tip, panics) in [
        (true, TIP + 1, TIP, true),
        (false, TIP + 1, TIP, false),
        (true, TIP, TIP, false),
        (true, TIP + 1, TIP + 1, false),
    ] {
        let (dir, mut wallet) = previous_swap(scanned);
        import_at(&mut wallet, &random_joint(), birthday);
        // `sync` skips every tip that panics here.
        assert!(!panics || wallet.born_above(BlockHeight::from_u32(tip)).unwrap());
        if panics {
            let conn = rusqlite::Connection::open(dir.path().join("wallet.sqlite")).unwrap();
            let ranges: Vec<(u32, u32, u32)> = conn
                .prepare("SELECT * FROM scan_queue ORDER BY block_range_start")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(
                ranges,
                [
                    (280000, 4395100, 0),
                    (4395100, 4395106, 10),
                    (4395106, 4395131, 0)
                ]
            );
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            wallet
                .db
                .update_chain_tip(BlockHeight::from_u32(tip))
                .unwrap();
        }));
        assert_eq!(
            result.is_err(),
            panics,
            "scanned={scanned}, birthday={birthday}, tip={tip}"
        );
        if let Err(panic) = result {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(
                message,
                Some("Split point is within the range of to_insert")
            );
        }
    }
}

#[test]
fn an_imported_joint_account_is_recognised_by_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut wallet = Wallet::open(dir.path().join("wallet.sqlite"), Network::TestNetwork).unwrap();
    let joint = random_joint();
    let account = import_at(&mut wallet, &joint, 4_395_100);
    assert!(wallet.tracks_joint(account, &joint).unwrap());
    assert!(!wallet.tracks_joint(account, &random_joint()).unwrap());
}

#[test]
fn one_summary_reads_every_accounts_funds() {
    let (_dir, mut wallet) = previous_swap(true);
    let accounts = [
        import_at(&mut wallet, &random_joint(), 4_395_106),
        import_at(&mut wallet, &random_joint(), 4_395_106),
    ];
    wallet
        .db
        .update_chain_tip(BlockHeight::from_u32(4_395_106))
        .unwrap();
    let all = wallet.funds_by_account().unwrap();
    assert_eq!(all.len(), accounts.len());
    for account in accounts {
        assert_eq!(all[&account], wallet.funds(account).unwrap());
    }
    wallet.forget(accounts[0]).unwrap();
    assert_eq!(wallet.funds(accounts[0]).unwrap(), Funds::default());
}

#[test]
fn a_birthday_above_the_tip_imports_and_syncs_from_the_next_block() {
    // Scanned to 4,395,105, so the import needs no rewind of the note trees.
    let (_dir, mut wallet) = previous_swap(true);
    import_at(&mut wallet, &random_joint(), 4_395_106);
    assert!(wallet.born_above(BlockHeight::from_u32(4_395_105)).unwrap());
    assert!(!wallet.born_above(BlockHeight::from_u32(4_395_106)).unwrap());
    wallet
        .db
        .update_chain_tip(BlockHeight::from_u32(4_395_106))
        .unwrap();
}

#[test]
fn sweep_finality_uses_scanned_confirmations_and_rewinds_after_a_reorg() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wallet.sqlite");
    let mut wallet = Wallet::open(&path, Network::TestNetwork)
        .unwrap()
        .with_confirmations(NonZeroU32::new(3).unwrap());
    let account = import_at(&mut wallet, &random_joint(), TIP);
    let conn = rusqlite::Connection::open(&path).unwrap();
    for height in TIP..=TIP + 20 {
        conn.execute(
            "INSERT INTO blocks (height, hash, time, sapling_tree,
             sapling_commitment_tree_size, orchard_commitment_tree_size,
             ironwood_commitment_tree_size) VALUES (?1, zeroblob(32), 0, x'000000', 0, 0, 0)",
            [height],
        )
        .unwrap();
    }
    let txid = TxId::from_bytes([1; 32]);
    conn.execute(
        "INSERT INTO transactions (txid, mined_height, min_observed_height) VALUES (?1, ?2, ?2)",
        rusqlite::params![txid.as_ref(), TIP],
    )
    .unwrap();
    let scanned_to = |height: u32| {
        conn.execute("DELETE FROM scan_queue", []).unwrap();
        conn.execute(
            "INSERT INTO scan_queue VALUES (?1, ?2, 10)",
            [TIP, height + 1],
        )
        .unwrap();
        // Advertised tip and downloaded blocks alone must not count as scanned depth.
        conn.execute(
            "INSERT INTO scan_queue VALUES (?1, ?2, 20)",
            [height + 1, TIP + 21],
        )
        .unwrap();
    };
    scanned_to(TIP);
    assert!(wallet.is_mined(txid).unwrap());
    assert!(!wallet.is_confirmed(txid).unwrap());
    scanned_to(TIP + 1);
    assert!(!wallet.is_confirmed(txid).unwrap());
    scanned_to(TIP + 2);
    assert!(wallet.is_confirmed(txid).unwrap());
    drop(wallet);
    let wallet = Wallet::open(&path, Network::TestNetwork)
        .unwrap()
        .with_confirmations(NonZeroU32::new(3).unwrap());
    assert!(wallet.is_confirmed(txid).unwrap());
    assert!(wallet.db.get_account(account).unwrap().is_some());
    scanned_to(TIP + 1);
    assert!(!wallet.is_confirmed(txid).unwrap());
    conn.execute("UPDATE transactions SET mined_height = NULL", [])
        .unwrap();
    assert!(!wallet.is_mined(txid).unwrap());
    assert!(!wallet.is_confirmed(txid).unwrap());
    assert!(!wallet.is_confirmed(TxId::from_bytes([2; 32])).unwrap());
}

/// `count` chained blocks from `tip`, the first holding an Ironwood deposit of `DEPOSIT` to `to`,
/// as lightwalletd serves them, and the chain's state after the block at each index.
fn deposit_blocks(
    tip: u32,
    to: orchard::Address,
    count: u8,
) -> (Vec<CompactBlock>, impl Fn(u8) -> ChainState) {
    let version = BundleVersion::ironwood_v3();
    let mut builder = orchard::builder::Builder::new(
        BundleType::DEFAULT,
        version,
        version.default_flags(),
        Anchor::empty_tree(),
    )
    .unwrap();
    builder
        .add_output(
            None,
            to,
            NoteValue::from_raw(DEPOSIT),
            MemoBytes::empty().into_bytes(),
        )
        .unwrap();
    let (bundle, meta): (Bundle<_, i64>, _) =
        builder.build(&mut UnwrapErr(SysRng)).unwrap().unwrap();
    let action = &bundle.actions()[meta.output_action_index(0).unwrap()];
    let note = action.encrypted_note();
    let deposit = CompactTx {
        txid: vec![9; 32],
        ironwood_actions: vec![CompactOrchardAction {
            nullifier: action.nullifier().to_bytes().to_vec(),
            cmx: action.cmx().to_bytes().to_vec(),
            ephemeral_key: note.epk_bytes.to_vec(),
            ciphertext: note.enc_ciphertext.0[..COMPACT_NOTE_SIZE].to_vec(),
        }],
        ..Default::default()
    };
    let blocks = (0..count)
        .map(|i| CompactBlock {
            height: u64::from(tip + u32::from(i)),
            hash: vec![i + 1; 32],
            prev_hash: vec![i; 32],
            vtx: if i == 0 {
                vec![deposit.clone()]
            } else {
                vec![]
            },
            chain_metadata: Some(ChainMetadata {
                ironwood_commitment_tree_size: 1,
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
    let deposit = MerkleHashOrchard::from_cmx(action.cmx());
    let state = move |i: u8| {
        ChainState::new(
            BlockHeight::from_u32(tip + u32::from(i)),
            BlockHash([i + 1; 32]),
            Frontier::empty(),
            Frontier::empty(),
            Frontier::from_parts(Position::from(0), deposit, vec![]).unwrap(),
        )
    };
    (blocks, state)
}

/// A wallet on `network` whose joint account `deposit_blocks` pays at `tip`, with `count` blocks
/// cached from `tip` and the first two scanned.
struct Funded {
    _dir: TempDir,
    wallet: Wallet,
    joint: JointAccount,
    key: SpendKey,
    account: AccountUuid,
    tip: u32,
    state: Box<dyn Fn(u8) -> ChainState>,
}

impl Funded {
    async fn new(network: Network, tip: u32, count: u8) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut wallet = Wallet::open(dir.path().join("wallet.sqlite"), network)
            .unwrap()
            .with_confirmations(NonZeroU32::new(1).unwrap());
        let (e, z) = (
            SecretShare::random(UnwrapErr(SysRng)),
            SecretShare::random(UnwrapErr(SysRng)),
        );
        let joint = JointAccount::derive(
            &e.public(),
            &z.public(),
            &ViewingKeys::random(UnwrapErr(SysRng)),
        )
        .unwrap();
        let account = import_at(&mut wallet, &joint, tip);
        let (blocks, state) = deposit_blocks(tip, joint.deposit_address(), count);
        wallet.blocks.insert(blocks).await.unwrap();
        let mut funded = Self {
            _dir: dir,
            wallet,
            key: joint.spend_key(&e, &z).unwrap(),
            joint,
            account,
            tip,
            state: Box::new(state),
        };
        funded.scan(tip + 1, 0, 2);
        assert_eq!(funded.spendable(), DEPOSIT);
        funded
    }

    /// Moves the chain tip to `chain_tip` and scans `count` cached blocks from the one at `from`.
    fn scan(&mut self, chain_tip: u32, from: u8, count: usize) {
        self.wallet
            .db
            .update_chain_tip(BlockHeight::from_u32(chain_tip))
            .unwrap();
        let prior = match from.checked_sub(1) {
            Some(before) => (self.state)(before),
            None => ChainState::empty(BlockHeight::from_u32(self.tip - 1), BlockHash([0; 32])),
        };
        scan_cached_blocks(
            &self.wallet.network,
            &self.wallet.blocks,
            &mut self.wallet.db,
            BlockHeight::from_u32(self.tip + u32::from(from)),
            &prior,
            count,
        )
        .unwrap();
    }

    fn sweep(&mut self, to: &ZcashAddress) -> TxId {
        self.wallet
            .sweep(&PROVER, self.account, &self.key, to)
            .unwrap()
    }

    fn spendable(&self) -> u64 {
        self.wallet.funds(self.account).unwrap().spendable
    }
}

#[tokio::test]
async fn the_other_side_cannot_read_where_a_sweep_went() {
    let mut funded = Funded::new(Network::TestNetwork, TIP, 2).await;
    let home = random_joint();
    let to = home
        .unified_address(funded.wallet.network.network_type())
        .parse()
        .unwrap();
    let txid = funded.sweep(&to);

    let tx = funded.wallet.db.get_transaction(txid).unwrap().unwrap();
    let bundles = [tx.orchard_bundle(), tx.ironwood_bundle()];
    let received: u64 = bundles
        .iter()
        .flatten()
        .flat_map(|bundle| bundle.decrypt_outputs_with_keys(&[home.fvk().to_ivk(Scope::External)]))
        .map(|(_, _, note, _, _)| note.value().inner())
        .sum();
    assert_eq!(
        received,
        DEPOSIT - funded.wallet.fee(txid).unwrap().unwrap()
    );
    let joint_ovks =
        [Scope::External, Scope::Internal].map(|scope| funded.joint.fvk().to_ovk(scope));
    for bundle in bundles.iter().flatten() {
        assert!(
            bundle.recover_outputs_with_ovks(&joint_ovks).is_empty(),
            "the joint viewing key reads where the sweep went"
        );
    }
}

/// A sweep built under NU6.3's rules can't be mined once NU7 activates. Left to the builder's
/// expiry, 40 blocks on, it would hold the deposit and read as pending through NU7's first 40
/// blocks.
#[tokio::test]
async fn a_sweep_unmined_at_nu7_expires_with_it_and_is_built_again_under_nu7() {
    let nu7 = nu7();
    let mut funded = Funded::new(Network::MainNetwork, nu7 - 3, 4).await;
    let to = random_joint()
        .unified_address(funded.wallet.network.network_type())
        .parse()
        .unwrap();
    let before = funded.sweep(&to);
    let tx = funded.wallet.db.get_transaction(before).unwrap().unwrap();
    assert_eq!(tx.consensus_branch_id(), BranchId::Nu6_3);
    assert_eq!(tx.expiry_height(), BlockHeight::from_u32(nu7 - 1));
    assert_eq!(funded.spendable(), 0);

    // The chain reaches NU7 without the sweep.
    funded.scan(nu7, 2, 2);
    assert!(!funded.wallet.is_mined(before).unwrap());
    assert!(funded.wallet.is_expired(before).unwrap());
    assert_eq!(funded.spendable(), DEPOSIT);

    let after = funded.sweep(&to);
    let tx = funded.wallet.db.get_transaction(after).unwrap().unwrap();
    assert_eq!(tx.consensus_branch_id(), BranchId::Nu7);
    assert_eq!(tx.expiry_height(), BlockHeight::from_u32(nu7 + 1 + 120));
}

/// Only an expiry that would outlive a scheduled upgrade stops before it, and nothing stops one
/// built at the upgrade's own height.
#[test]
fn an_expiry_stops_before_an_upgrade_only_when_it_would_outlive_it() {
    let nu7 = BlockHeight::from_u32(nu7());
    let mainnet = Network::MainNetwork;
    assert_eq!(expiry_before_upgrade(&mainnet, nu7 - 100), Some(nu7 - 60));
    assert_eq!(expiry_before_upgrade(&mainnet, nu7 - 1), Some(nu7 - 1));
    assert_eq!(expiry_before_upgrade(&mainnet, nu7), None);
}
