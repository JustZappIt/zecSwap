//! Known answers a wallet's port must reproduce (`docs/local/android.md`, section 6):
//!   cargo run -p zecswap-client --example vectors

use alloy_primitives::{Address, address};
use zecswap_chain::evm::swap_id;
use zecswap_core::{Domain, NetworkType, derive_maker_share, derive_user_keys};
use zecswap_railgun::{Keys, ShieldNote};

const SEED: [u8; 64] = [7; 64];
const RAILGUN_SEED: [u8; 64] = [8; 64];
const MAKER: Address = address!("09eD1F966745Be18C711C346242c0974DAd7c3e5");
const CONTRACT: Address = address!("1111111111111111111111111111111111111111");
const RELAYER: Address = address!("2222222222222222222222222222222222222222");
const SEPOLIA: u64 = 11_155_111;

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", ::hex::encode(bytes))
}

fn main() {
    let keys = derive_user_keys(&SEED, NetworkType::Test, 0, 0).expect("keys");
    let maker_share = derive_maker_share(&[9; 32], 0).expect("maker share");
    let id = swap_id(MAKER, &keys.share.public());
    println!("seed = 64 bytes of 0x07, network Test, account 0, index 0");
    println!(
        "  userShare    {}",
        hex(&keys.share.public().to_affine_bytes())
    );
    println!("  userSecret   {}", hex(&keys.share.to_be_bytes()));
    println!("  viewingKeys  {}", hex(&keys.viewing.to_bytes()));
    println!("  authAddress  {}", hex(&keys.auth.address()));
    println!("  swapId       {id} (maker {MAKER})");
    println!(
        "maker share from derive_maker_share(root = 32 bytes of 0x09, nonce 0)\n  makerShare   {}",
        hex(&maker_share.public().to_affine_bytes())
    );

    let note = print_note("the seed itself", &SEED, &keys.note_entropy);
    let railgun_note = print_note(
        "a Railgun seed of 64 bytes of 0x08",
        &RAILGUN_SEED,
        &keys.note_entropy,
    );

    let domain = Domain {
        chain_id: SEPOLIA,
        contract: CONTRACT.into(),
    };
    let deadline = 1_790_000_000;
    let fee = 20_000;
    let lock = domain.lock_claim(&id.0, deadline);
    let payout = domain.payout(&id.0, &RELAYER.into(), fee);
    println!("EIP-712 on chain {SEPOLIA} at {CONTRACT}, signed by authAddress");
    println!("  LockClaim(swapId, deadline {deadline})");
    println!("    digest     {}", hex(&lock));
    println!("    signature  {}", hex(&keys.auth.sign(&lock)));
    println!("  Payout(swapId, relayer {RELAYER}, fee {fee})");
    println!("    digest     {}", hex(&payout));
    println!("    signature  {}", hex(&keys.auth.sign(&payout)));

    let reverse_id = swap_id(keys.auth.address().into(), &maker_share.public());
    let reverse = zecswap_core::ReverseOpen {
        maker: MAKER.into(),
        user: keys.auth.address(),
        token: [0x33; 20],
        amount: 50_000_000,
        maker_share: maker_share.public(),
        user_share: keys.share.public(),
        t0: 1_790_003_600,
        t1: 1_790_007_200,
        refund_note: note.commitment(),
        deadline,
    };
    println!("reverse swapId {reverse_id}");
    for (name, digest) in [
        ("OpenReverse", domain.open_reverse(&reverse)),
        ("Ready", domain.ready(&reverse_id.0, deadline)),
        ("LockRefund", domain.lock_refund(&reverse_id.0, deadline)),
        (
            "RefundPayout",
            domain.refund_payout(&reverse_id.0, &RELAYER.into(), fee),
        ),
    ] {
        println!("  {name} {}", hex(&keys.auth.sign(&digest)));
    }
    let separate = zecswap_core::ReverseOpen {
        refund_note: railgun_note.commitment(),
        ..reverse
    };
    let rescue = domain.rescue(
        &reverse_id.0,
        &railgun_note.commitment(),
        &RELAYER.into(),
        fee,
    );
    println!("refunded into the Railgun seed's wallet instead");
    println!(
        "  OpenReverse {}",
        hex(&keys.auth.sign(&domain.open_reverse(&separate)))
    );
    println!("  Rescue {}", hex(&keys.auth.sign(&rescue)));
}

/// Prints Railgun wallet 0 of `seed` and the note built from `entropy` that pays it.
fn print_note(seed_name: &str, seed: &[u8], entropy: &[u8; 32]) -> ShieldNote {
    let wallet = Keys::from_seed(seed, 0);
    let note = wallet.note(entropy).expect("note");
    println!("Railgun wallet 0 of {seed_name}, and the note swap 0 pays into it");
    println!("  0zk          {}", wallet.address());
    println!("  npk          {}", hex(&note.npk));
    for (i, word) in note.ciphertext.encrypted_bundle.iter().enumerate() {
        println!("  bundle[{i}]    {}", hex(word));
    }
    println!("  shieldKey    {}", hex(&note.ciphertext.shield_key));
    println!("  payoutNote   {}", hex(&note.commitment()));
    note
}
