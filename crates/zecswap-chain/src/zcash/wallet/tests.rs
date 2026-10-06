use std::panic::{AssertUnwindSafe, catch_unwind};

use tempfile::TempDir;
use zcash_client_backend::data_api::WalletCommitmentTrees;
use zcash_client_backend::data_api::chain::{ChainState, CommitmentTreeRoot};
use zcash_protocol::consensus::BlockHeight;
use zecswap_core::{SecretShare, ViewingKeys};

use super::*;

const TIP: u32 = 4_395_130;

fn import_at(wallet: &mut Wallet, birthday: u32) -> AccountUuid {
    let joint = JointAccount::derive(
        &SecretShare::random(UnwrapErr(SysRng)).public(),
        &SecretShare::random(UnwrapErr(SysRng)).public(),
        &ViewingKeys::random(UnwrapErr(SysRng)),
    )
    .unwrap();
    let ufvk =
        UnifiedFullViewingKey::decode(&wallet.network, &joint.ufvk(wallet.network.network_type()))
            .unwrap();
    let birthday = AccountBirthday::from_parts(
        ChainState::empty(
            BlockHeight::from_u32(birthday - 1),
            zcash_primitives::block::BlockHash([0; 32]),
        ),
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
    let old = import_at(&mut wallet, 4_395_100);
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
                orchard::tree::MerkleHashOrchard::from_bytes(&[0; 32]).unwrap(),
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
        import_at(&mut wallet, birthday);
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
fn a_birthday_above_the_tip_imports_and_syncs_from_the_next_block() {
    // Scanned to 4,395,105, so the import needs no rewind of the note trees.
    let (_dir, mut wallet) = previous_swap(true);
    import_at(&mut wallet, 4_395_106);
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
    let account = import_at(&mut wallet, TIP);
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
