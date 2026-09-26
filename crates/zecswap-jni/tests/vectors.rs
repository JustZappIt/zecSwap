//! The bindings' operations reproduce `cargo run -p zecswap-client --example vectors`, the known
//! answers the Android port is checked against.

use rand_core::OsRng;
use zecswap::ops::{Swap, railgun_address};
use zecswap_core::{Domain, Payout, PublicShare, ShareProof, SwapContext, derive_maker_share};

const SEED: [u8; 64] = [7; 64];
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
}

#[test]
fn payout_note() {
    let note = swap().payout_note().unwrap();
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
        .accept(context, &maker.public().to_affine_bytes(), &maker_proof)
        .unwrap();

    let (share, rest) = accepted.split_at(64);
    let (proof, viewing) = rest.split_at(64);
    assert_eq!(hex(share), USER_SHARE);
    assert_eq!(hex(viewing), VIEWING_KEYS);
    let payout = Payout {
        user: swap().auth_address().unwrap(),
        note: Some(swap().payout_note().unwrap()[160..].try_into().unwrap()),
    };
    let user = PublicShare::from_affine_bytes(share.try_into().unwrap()).unwrap();
    let proof = ShareProof::from_bytes(proof.try_into().unwrap());
    assert_eq!(
        context.verify_user(&maker.public(), &user, &payout, &proof),
        Ok(())
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
    let refused = swap().accept(other, &maker.public().to_affine_bytes(), &proof);
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
    assert!(Swap::new(&SEED, false, -1).is_err());
}
