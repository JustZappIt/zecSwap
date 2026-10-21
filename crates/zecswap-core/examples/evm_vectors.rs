//! Vectors for the contract's differential tests.
//!
//!   evm_vectors [count]   JSON: edge-case and seeded-random scalars with their `[k]·SpendAuthG`
//!   evm_vectors mul <k>   ABI-encoded `(x, y)` for the 32-byte hex scalar `k` (forge ffi)
//!   evm_vectors terms <maker> <token> <amount> <makerKey> <userKey> <user> <t0> <t1> <payoutNote>
//!                         `Terms::hash` of a swap's terms: hex addresses, 64-byte keys and note,
//!                         decimal numbers (forge ffi)
//!   evm_vectors deposit-nonce <chainId> <deposits> <npk> <bundle0> <bundle1> <bundle2>
//!                         <shieldKey> <fee> <relayer>
//!                         `DepositsDomain::deposit_nonce` (forge ffi)
//!   evm_vectors receive-digest <name> <version> <chainId> <token> <from> <to> <value>
//!                         <validAfter> <validBefore> <nonce>
//!                         `TokenDomain::receive_with_authorization` (forge ffi)
//!   evm_vectors deposit-rescue <chainId> <deposits> <from> <note> <relayer> <fee> <nonce>
//!                         <deadline>
//!                         `DepositsDomain::rescue` (forge ffi)
//!   evm_vectors digest <kind> <chainId> <contract> <fields...>
//!                         `Domain`'s digest of `ready`, `payout`, `refund-payout`, `rescue` or
//!                         `open-reverse` (forge ffi)

use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use zecswap_core::{
    DepositsDomain, Domain, PublicShare, ReceiveAuthorization, RescueAuthorization, ReverseOpen,
    SecretShare, Terms, TokenDomain,
};

const Q_MINUS_ONE: &str = "40000000000000000000000000000000224698fc0994a8dd8c46eb2100000000";
const Q_MINUS_TWO: &str = "40000000000000000000000000000000224698fc0994a8dd8c46eb20ffffffff";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode, kind, chain_id, contract, rest @ ..] if mode == "digest" => {
            let domain = Domain {
                chain_id: chain_id.parse().expect("a decimal chain id"),
                contract: fixed(contract),
            };
            let digest = match (kind.as_str(), rest) {
                ("ready", [id, deadline]) => {
                    domain.ready(&fixed(id), deadline.parse().expect("a decimal deadline"))
                }
                ("payout", [id, relayer, fee]) => domain.payout(
                    &fixed(id),
                    &fixed(relayer),
                    fee.parse().expect("a decimal fee"),
                ),
                ("refund-payout", [id, relayer, fee]) => domain.refund_payout(
                    &fixed(id),
                    &fixed(relayer),
                    fee.parse().expect("a decimal fee"),
                ),
                ("rescue", [id, note, relayer, fee, nonce, deadline]) => domain.rescue(
                    &fixed(id),
                    &fixed(note),
                    &fixed(relayer),
                    fee.parse().expect("a decimal fee"),
                    RescueAuthorization {
                        nonce: nonce.parse().expect("a decimal nonce"),
                        deadline: deadline.parse().expect("a decimal deadline"),
                    },
                ),
                (
                    "open-reverse",
                    [
                        maker,
                        user,
                        token,
                        amount,
                        maker_key,
                        user_key,
                        t0,
                        t1,
                        refund_note,
                        deadline,
                    ],
                ) => {
                    let key = |text: &str| {
                        PublicShare::from_affine_bytes(&fixed(text)).expect("a key on the curve")
                    };
                    domain.open_reverse(&ReverseOpen {
                        maker: fixed(maker),
                        user: fixed(user),
                        token: fixed(token),
                        amount: amount.parse().expect("a decimal amount"),
                        maker_share: key(maker_key),
                        user_share: key(user_key),
                        t0: t0.parse().expect("a decimal t0"),
                        t1: t1.parse().expect("a decimal t1"),
                        refund_note: fixed(refund_note),
                        deadline: deadline.parse().expect("a decimal deadline"),
                    })
                }
                _ => panic!("unknown digest"),
            };
            println!("0x{}", hex(&digest));
        }
        [mode, k] if mode == "mul" => {
            let point = share(k.trim_start_matches("0x")).public().to_affine_bytes();
            println!("0x{}", hex(&point));
        }
        [
            mode,
            maker,
            token,
            amount,
            maker_key,
            user_key,
            user,
            t0,
            t1,
            payout_note,
        ] if mode == "terms" => {
            let key = |text: &str| {
                PublicShare::from_affine_bytes(&fixed(text)).expect("a key on the curve")
            };
            let terms = Terms {
                maker: fixed(maker),
                token: fixed(token),
                amount: amount.parse().expect("a decimal amount"),
                maker_share: key(maker_key),
                user_share: key(user_key),
                user: fixed(user),
                t0: t0.parse().expect("a decimal t0"),
                t1: t1.parse().expect("a decimal t1"),
                payout_note: fixed(payout_note),
            };
            println!("0x{}", hex(&terms.hash()));
        }
        [
            mode,
            chain_id,
            deposits,
            npk,
            bundle0,
            bundle1,
            bundle2,
            shield_key,
            fee,
            relayer,
        ] if mode == "deposit-nonce" => {
            let domain = DepositsDomain {
                chain_id: chain_id.parse().expect("a decimal chain id"),
                contract: fixed(deposits),
            };
            let note = [npk, bundle0, bundle1, bundle2, shield_key].map(|word| fixed(word));
            let fee = fee.parse().expect("a decimal fee");
            println!(
                "0x{}",
                hex(&domain.deposit_nonce(&note, fee, &fixed(relayer)))
            );
        }
        [
            mode,
            name,
            version,
            chain_id,
            token,
            from,
            to,
            value,
            valid_after,
            valid_before,
            nonce,
        ] if mode == "receive-digest" => {
            let domain = TokenDomain {
                name: name.clone(),
                version: version.clone(),
                chain_id: chain_id.parse().expect("a decimal chain id"),
                token: fixed(token),
            };
            let digest = domain.receive_with_authorization(&ReceiveAuthorization {
                from: fixed(from),
                to: fixed(to),
                value: value.parse().expect("a decimal value"),
                valid_after: valid_after.parse().expect("a decimal validAfter"),
                valid_before: valid_before.parse().expect("a decimal validBefore"),
                nonce: fixed(nonce),
            });
            println!("0x{}", hex(&digest));
        }
        [
            mode,
            chain_id,
            deposits,
            from,
            note,
            relayer,
            fee,
            nonce,
            deadline,
        ] if mode == "deposit-rescue" => {
            let domain = DepositsDomain {
                chain_id: chain_id.parse().expect("a decimal chain id"),
                contract: fixed(deposits),
            };
            let authorization = RescueAuthorization {
                nonce: nonce.parse().expect("a decimal nonce"),
                deadline: deadline.parse().expect("a decimal deadline"),
            };
            let digest = domain.rescue(
                &fixed(from),
                &fixed(note),
                &fixed(relayer),
                fee.parse().expect("a decimal fee"),
                authorization,
            );
            println!("0x{}", hex(&digest));
        }
        [] => print_vectors(256),
        [count] => print_vectors(count.parse().expect("count must be a number")),
        _ => panic!(
            "usage: evm_vectors [count] | mul <k> | terms <...> | deposit-nonce <...> | receive-digest <...> | deposit-rescue <...>"
        ),
    }
}

fn print_vectors(random: usize) {
    let mut rng = ChaCha20Rng::seed_from_u64(20_260_924);
    let mut shares: Vec<SecretShare> = [Q_MINUS_ONE, Q_MINUS_TWO]
        .iter()
        .map(|k| share(k))
        .collect();
    shares.extend((0..255).map(|bit| {
        let mut k = [0; 32];
        k[31 - bit / 8] = 1 << (bit % 8);
        SecretShare::from_be_bytes(&k).expect("2^bit < q")
    }));
    shares.extend((0..random).map(|_| SecretShare::random(&mut rng)));

    let entries: Vec<String> = shares
        .iter()
        .map(|share| {
            let point = share.public().to_affine_bytes();
            format!(
                "    {{ \"k\": \"0x{}\", \"x\": \"0x{}\", \"y\": \"0x{}\" }}",
                hex(&share.to_be_bytes()),
                hex(&point[..32]),
                hex(&point[32..])
            )
        })
        .collect();
    println!("{{\n  \"vectors\": [\n{}\n  ]\n}}", entries.join(",\n"));
}

fn share(k: &str) -> SecretShare {
    let bytes = unhex(k);
    let mut padded = [0; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    SecretShare::from_be_bytes(&padded).expect("scalar in (0, q)")
}

/// Exactly `N` bytes of `0x` hex.
fn fixed<const N: usize>(text: &str) -> [u8; N] {
    unhex(text.trim_start_matches("0x"))
        .try_into()
        .unwrap_or_else(|_| panic!("{text} is not {N} bytes"))
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
