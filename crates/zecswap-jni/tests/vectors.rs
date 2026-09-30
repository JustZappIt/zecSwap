//! The bindings' operations reproduce `cargo run -p zecswap-client --example vectors`, the known
//! answers the Android port is checked against.

use rand_core::OsRng;
use zecswap::ops::{Swap, railgun_address};
use zecswap_core::{Domain, Payout, PublicShare, ShareProof, SwapContext, derive_maker_share};
use zecswap_railgun::{Keys as RailgunKeys, ShieldCiphertext, ShieldNote};

const SEED: [u8; 64] = [7; 64];
const RAILGUN_SEED: [u8; 64] = [8; 64];
const SEPOLIA: u64 = 11_155_111;
const CONTRACT: [u8; 20] = [0x11; 20];
const RELAYER: [u8; 20] = [0x22; 20];
const SWAP_ID: &str = "0x297f1ca9d44ff7136dbddb0720ecadc040229e22c03ad0f9e4c648212bfc7b66";
const USER_SHARE: &str = "0x0b42629d5b3f787aba7ccde87574c13f6db6b8fccb7c5cfeb3f4e4081c756461311662156525fcaa692aeef5d2362c2a9ab2083cd6d968c618aec72617aa925a";
const VIEWING_KEYS: &str = "0x97afa7272114ce3634144ee0883379f1a01695bbb56f64f99181ddf44369d125c3e134a0b68e43ce055281af3bf7e30850857d77995986765a72dde442b8802f";

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn bytes<const N: usize>(value: &str) -> [u8; N] {
    hex::decode(value.trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap()
}

fn swap() -> Swap<'static> {
    Swap::new(&SEED, false, 0).unwrap()
}

fn maker_share() -> [u8; 64] {
    derive_maker_share(&[9; 32], 0)
        .unwrap()
        .public()
        .to_affine_bytes()
}

fn shield_note(bytes: &[u8]) -> ShieldNote {
    let words: Vec<[u8; 32]> = bytes
        .chunks(32)
        .map(|word| word.try_into().unwrap())
        .collect();
    ShieldNote {
        npk: words[0],
        ciphertext: ShieldCiphertext {
            encrypted_bundle: [words[1], words[2], words[3]],
            shield_key: words[4],
        },
    }
}

#[test]
fn reverse_authorizations_bind_terms_and_separate_actions() {
    use zecswap::ops::{ReverseAction, ReverseTerms};
    use zecswap_core::{NetworkType, ReverseOpen, derive_user_keys, signer};
    let domain = Domain {
        chain_id: SEPOLIA,
        contract: CONTRACT,
    };
    let keys = derive_user_keys(&SEED, NetworkType::Test, 0, 0).unwrap();
    let note = RailgunKeys::from_seed(&RAILGUN_SEED, 0)
        .note(&keys.note_entropy)
        .unwrap();
    let terms = ReverseTerms {
        maker: [3; 20],
        token: [4; 20],
        amount: 50_000_000,
        maker_share: maker_share(),
        ready_deadline: 2_000,
        refund_after: 3_000,
        funding_deadline: 1_000,
    };
    let mut open = ReverseOpen {
        maker: terms.maker,
        user: keys.auth.address(),
        token: terms.token,
        amount: terms.amount,
        maker_share: PublicShare::from_affine_bytes(&terms.maker_share).unwrap(),
        user_share: keys.share.public(),
        t0: terms.ready_deadline,
        t1: terms.refund_after,
        refund_note: note.commitment(),
        deadline: terms.funding_deadline,
    };
    let signature = swap()
        .sign_reverse_open(&RAILGUN_SEED, domain, &terms)
        .unwrap();
    assert_eq!(
        signer(&domain.open_reverse(&open), &signature),
        Some(keys.auth.address())
    );
    let legacy = swap().sign_reverse_open(&SEED, domain, &terms).unwrap();
    assert_ne!(
        signer(&domain.open_reverse(&open), &legacy),
        Some(keys.auth.address()),
        "the refund note is the Railgun seed's"
    );
    open.amount += 1;
    assert_ne!(
        signer(&domain.open_reverse(&open), &signature),
        Some(keys.auth.address())
    );
    let id = [8; 32];
    let ready = swap()
        .sign_reverse_action(domain, &id, 1_100, ReverseAction::Ready)
        .unwrap();
    assert_eq!(
        signer(&domain.ready(&id, 1_100), &ready),
        Some(keys.auth.address())
    );
    assert_ne!(
        signer(&domain.lock_refund(&id, 1_100), &ready),
        Some(keys.auth.address())
    );
    let rescue = swap()
        .sign_refund_rescue(&RAILGUN_SEED, domain, &id, &RELAYER, 20_000)
        .unwrap();
    assert_eq!(
        signer(
            &domain.rescue(&id, &note.commitment(), &RELAYER, 20_000),
            &rescue
        ),
        Some(keys.auth.address())
    );
    assert_ne!(
        signer(&domain.refund_payout(&id, &RELAYER, 20_000), &rescue),
        Some(keys.auth.address())
    );
    let refund = swap()
        .sign_refund_payout(domain, &id, &RELAYER, 20_000)
        .unwrap();
    assert_eq!(
        signer(&domain.refund_payout(&id, &RELAYER, 20_000), &refund),
        Some(keys.auth.address())
    );
    assert_ne!(
        signer(&domain.payout(&id, &RELAYER, 20_000), &refund),
        Some(keys.auth.address())
    );
}

#[test]
fn keys() {
    assert_eq!(hex(&swap().user_share().unwrap()), USER_SHARE);
    assert_eq!(
        hex(&swap().claim_secret().unwrap()),
        "0x01bfb1d4b12d5365d9bc7aa9e7e7f641bdb7cce5c647b29c7077f344111ba6ac"
    );
    assert_eq!(
        hex(&swap().auth_address().unwrap()),
        "0x757de38c2d9880e44ab59827d1622403fbf88ff5"
    );
    assert_eq!(
        railgun_address(&SEED).unwrap(),
        "0zk1qyt5x0c632363rrmd8psxws6n9tscm8gps277gzc0w3s5cg4mdpe9rv7j6fe3z53luahk4ksjwagt68fl2vguye054rxjyqzvhs9usq4rwrk09al6n0677pdrgn"
    );
    assert_eq!(
        railgun_address(&RAILGUN_SEED).unwrap(),
        "0zk1qyrs4qyrd08p6uep0fc2y8njktgcpezts3rpaq6q0ln948ecjkw8prv7j6fe3z53llz8ursderja0juwv5pgnv8x5klmmwkv8q38h9n704h4d4qjyw7n5qk68nx"
    );
}

#[test]
fn payout_note_pays_the_railgun_seeds_wallet_only() {
    let note = swap().payout_note(&RAILGUN_SEED).unwrap();
    let words: Vec<_> = note.chunks(32).map(hex).collect();
    assert_eq!(
        words,
        [
            "0x1f80223263733ae7cb3047ae61b64ee8179674dbbd708cac8a7a8b15a222ba35",
            "0xe9f4508256863b2559259a39333a65684a26d33e9ac0081841cc96ed3d26c2b2",
            "0xc47d927ba9163144d7c83e28031820134fb3f943d073895b3e2521517419ee97",
            "0x7d9d444c12bec55e6b3892ca92fd50c9ba2d7d0783f95545a2f15923f8474c0f",
            "0x02356776cb176876b31960b8ccbf0c0850a76e9a2ef49caa6631bca732d064b2",
            "0x5af6901ba7cb01f49785a29c4a2e57e31af3e53382ce3dd2e35678897515ffc1",
        ]
    );
    let note = shield_note(&note[..160]);
    assert!(
        RailgunKeys::from_seed(&RAILGUN_SEED, 0)
            .open(&note)
            .is_some()
    );
    assert!(RailgunKeys::from_seed(&SEED, 0).open(&note).is_none());
}

/// Swaps accepted before the Railgun seed was separate committed to this note: it never changes.
#[test]
fn payout_note_into_the_swap_seeds_own_wallet_is_unchanged() {
    let note = swap().payout_note(&SEED).unwrap();
    let words: Vec<_> = note.chunks(32).map(hex).collect();
    assert_eq!(
        words,
        [
            "0x11eb0b931cc092fe6876f395a4f8d29cf5c97c43903605253386e008ab4881e3",
            "0xe9f4508256863b2559259a39333a65685dfd7a7c86fa101e959517e06aa798c6",
            "0x63835c785858b4cf0519698cc24ee8d84fb3f943d073895b3e2521517419ee97",
            "0x82981c9149e1977d0ca708cd313450306e87b35a41dc832ec419686055b02e9a",
            "0x02356776cb176876b31960b8ccbf0c0850a76e9a2ef49caa6631bca732d064b2",
            "0x14d061e0bf2b24d75b75adb91c04cc6b82be6b407b5901dbdb86bbbabe7a9acd",
        ]
    );
}

#[test]
fn deposit_account() {
    let [address, ufvk] = swap().deposit_account(&maker_share()).unwrap();
    assert_eq!(
        address,
        "utest1exuj2qh9gcll0zjygvk7c48e5ra40wwtvdgd2u0ygwn2g3kanp47utpgh25m4pqwcjqsqy55zyr7qncw0ct0gpeccytje6tgeyaheah7"
    );
    assert_eq!(
        ufvk,
        "uviewtest130mkztp6gdvnvn0k3y3h820wjn9unqj2lzkt400lgw4velrg6dsy8m4lp3jn22hyupdk4e5z4dch99rrxcme7s2cm8r7805t64r7d7tma2xg0pn93vpu4luedpcrrj4k9jz6kx374g0738x70vu8l0ueh6nzu33aned77x7rwqyd3cxqndnpvyg4ffr8c"
    );
}

#[test]
fn signatures() {
    let domain = Domain {
        chain_id: SEPOLIA,
        contract: CONTRACT,
    };
    let id = bytes::<32>(SWAP_ID);
    assert_eq!(
        hex(&swap().sign_lock_claim(domain, &id, 1_790_000_000).unwrap()),
        "0xc2a0a598fc3027f949c2a3f3fadb3e988a74effa69e09efc86d3db0e89904dfc69a81a0cc62da15628ed8084bb31d05723dfc2da4308dfbeb0ef4e134b7497551b"
    );
    assert_eq!(
        hex(&swap().sign_payout(domain, &id, &RELAYER, 20_000).unwrap()),
        "0x883118048774977816784fb0f34f256a8344caaf134201e0acd9f0b5c1f8e6a3685df4aa58f4d1725f49fbaa106cc594ef0ffe4b8569550860a00ce5291bae5b1c"
    );
}

#[test]
fn accept_proves_against_the_payout_it_quotes() {
    let maker = derive_maker_share(&[9; 32], 0).unwrap();
    let context = SwapContext {
        chain_id: SEPOLIA,
        contract: CONTRACT,
        quote_id: [0x33; 32],
    };
    let maker_proof = context.prove_maker(&maker, OsRng).to_bytes();
    let accepted = swap()
        .accept(
            &RAILGUN_SEED,
            context,
            &maker.public().to_affine_bytes(),
            &maker_proof,
        )
        .unwrap();

    let (share, rest) = accepted.split_at(64);
    let (proof, viewing) = rest.split_at(64);
    assert_eq!(hex(share), USER_SHARE);
    assert_eq!(hex(viewing), VIEWING_KEYS);
    let payout = |seed: &[u8]| Payout {
        user: swap().auth_address().unwrap(),
        note: Some(swap().payout_note(seed).unwrap()[160..].try_into().unwrap()),
    };
    let user = PublicShare::from_affine_bytes(share.try_into().unwrap()).unwrap();
    let proof = ShareProof::from_bytes(proof.try_into().unwrap());
    assert_eq!(
        context.verify_user(&maker.public(), &user, &payout(&RAILGUN_SEED), &proof),
        Ok(())
    );
    assert!(
        context
            .verify_user(&maker.public(), &user, &payout(&SEED), &proof)
            .is_err()
    );
}

#[test]
fn accept_refuses_a_maker_proof_for_another_quote() {
    let maker = derive_maker_share(&[9; 32], 0).unwrap();
    let context = SwapContext {
        chain_id: SEPOLIA,
        contract: CONTRACT,
        quote_id: [0x33; 32],
    };
    let proof = context.prove_maker(&maker, OsRng).to_bytes();
    let other = SwapContext {
        quote_id: [0x44; 32],
        ..context
    };
    let refused = swap().accept(
        &RAILGUN_SEED,
        other,
        &maker.public().to_affine_bytes(),
        &proof,
    );
    assert!(refused.unwrap_err().starts_with("the maker's share proof"));
}

#[test]
fn refund_refuses_a_maker_secret_the_share_does_not_match() {
    let wrong = derive_maker_share(&[9; 32], 1).unwrap().to_be_bytes();
    let refused = swap().sign_refund(&maker_share(), &wrong, &[]);
    assert!(refused.unwrap_err().contains("do not match"));
}

#[test]
fn seeds_are_64_bytes() {
    assert!(Swap::new(&[7; 32], false, 0).is_err());
    assert!(railgun_address(&[7; 32]).is_err());
    assert!(swap().payout_note(&[8; 32]).is_err());
    assert!(Swap::new(&SEED, false, -1).is_err());
}
