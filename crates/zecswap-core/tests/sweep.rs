//! A note paid to the joint address is swept end to end: a real Halo 2 proof, a signature
//! from the combined key, and a transaction the extractor fully verifies.

use std::sync::OnceLock;

use incrementalmerkletree::Retention;
use orchard::bundle::BundleVersion;
use orchard::circuit::{OrchardCircuitVersion, ProvingKey, VerifyingKey};
use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use orchard::note::ExtractedNoteCommitment;
use orchard::note_encryption::IronwoodDomain;
use orchard::tree::{MerkleHashOrchard, MerklePath};
use orchard::value::NoteValue;
use orchard::{Address, Anchor, Note, ValuePool};
use pczt::Pczt;
use pczt::roles::combiner::Combiner;
use pczt::roles::creator::Creator;
use pczt::roles::io_finalizer::IoFinalizer;
use pczt::roles::prover::Prover;
use pczt::roles::redactor::Redactor;
use pczt::roles::redactor::orchard::OrchardRedactor;
use pczt::roles::spend_finalizer::SpendFinalizer;
use pczt::roles::tx_extractor::TransactionExtractor;
use rand_core::OsRng;
use shardtree::ShardTree;
use shardtree::store::memory::MemoryShardStore;
use zcash_note_encryption::try_note_decryption;
use zcash_primitives::transaction::builder::{BuildConfig, Builder, BundlePadding, PcztResult};
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;
use zecswap_core::{
    Error, JointAccount, SecretShare, SpendKey, SweepIntent, ViewingKeys, sign_pczt,
    sign_pczt_bytes,
};

const DEPOSIT: u64 = 1_000_000;
const FEE: u64 = 10_000;

struct Swap {
    e: SecretShare,
    z: SecretShare,
    joint: JointAccount,
}

impl Swap {
    fn new() -> Self {
        let (e, z) = (SecretShare::random(OsRng), SecretShare::random(OsRng));
        let joint =
            JointAccount::derive(&e.public(), &z.public(), &ViewingKeys::random(OsRng)).unwrap();
        Self { e, z, joint }
    }

    fn spend_key(&self) -> SpendKey {
        self.joint.spend_key(&self.e, &self.z).unwrap()
    }

    /// An IO-finalized PCZT sweeping a fresh deposit to a wallet outside the swap, as
    /// `createPcztFromProposal` hands it over.
    fn sweep(&self) -> Pczt {
        self.sweep_with_outputs(&[(destination(), DEPOSIT - FEE)])
    }

    fn sweep_with_outputs(&self, outputs: &[(Address, u64)]) -> Pczt {
        let fvk = self.joint.fvk();
        let note = receive_ironwood_note(fvk, self.joint.deposit_address());
        let (anchor, path) = single_leaf_witness(&note);

        let mut builder = Builder::new(
            nu6_3_network(),
            BlockHeight::from_u32(10_000_000),
            BuildConfig::Standard {
                sapling_anchor: None,
                orchard_anchor: None,
                ironwood_anchor: Some(anchor),
                orchard_padding: BundlePadding::DEFAULT,
                ironwood_padding: BundlePadding::DEFAULT,
            },
        );
        builder
            .add_ironwood_spend::<zip317::FeeRule>(fvk.clone(), note, path)
            .unwrap();
        for (recipient, amount) in outputs {
            builder
                .add_ironwood_output::<zip317::FeeRule>(
                    None,
                    *recipient,
                    Zatoshis::from_u64(*amount).unwrap(),
                    MemoBytes::empty(),
                )
                .unwrap();
        }
        let PcztResult { pczt_parts, .. } = builder
            .build_for_pczt(OsRng, &zip317::FeeRule::standard())
            .unwrap();
        IoFinalizer::new(Creator::build_from_parts(pczt_parts).unwrap())
            .finalize_io()
            .unwrap()
    }
}

#[test]
fn the_combined_key_sweeps_the_deposit_and_other_keys_are_ignored() {
    let (swap, stranger) = (Swap::new(), Swap::new());
    let keys = [stranger.spend_key(), swap.spend_key()];
    extract(sign_pczt(prove(swap.sweep()), &keys, &intent()).unwrap());
}

#[test]
fn signs_the_redacted_views_the_wallet_sdks_hand_to_signers() {
    let swap = Swap::new();
    for view in [full_signer_view, compact_signer_view] {
        let pczt = swap.sweep();
        let with_proofs = prove(pczt.clone());
        let signed = sign_pczt_bytes(
            &view(pczt).serialize().unwrap(),
            &[swap.spend_key()],
            &intent(),
        );
        let with_signatures = Pczt::parse(&signed.unwrap()).unwrap();
        extract(
            Combiner::new(vec![with_proofs, with_signatures])
                .combine()
                .unwrap(),
        );
    }
}

#[test]
fn another_accounts_key_signs_nothing() {
    let (swap, stranger) = (Swap::new(), Swap::new());
    let result = sign_pczt(swap.sweep(), &[stranger.spend_key()], &intent());
    assert!(matches!(
        result,
        Err(Error::UnsignedSpend {
            pool: ValuePool::Ironwood,
            ..
        })
    ));
}

#[test]
fn preauthorized_real_spends_are_rejected() {
    let swap = Swap::new();
    let signed = sign_pczt(swap.sweep(), &[swap.spend_key()], &intent()).unwrap();
    assert!(matches!(
        sign_pczt(signed, &[swap.spend_key()], &intent()),
        Err(Error::SweepIntent(_))
    ));
}

fn circuit_keys() -> &'static (ProvingKey, VerifyingKey) {
    static KEYS: OnceLock<(ProvingKey, VerifyingKey)> = OnceLock::new();
    KEYS.get_or_init(|| {
        let version = OrchardCircuitVersion::PostNu6_3;
        (ProvingKey::build(version), VerifyingKey::build(version))
    })
}

fn prove(pczt: Pczt) -> Pczt {
    Prover::new(pczt)
        .create_ironwood_proof(&circuit_keys().0)
        .unwrap()
        .finish()
}

fn extract(pczt: Pczt) {
    let pczt = SpendFinalizer::new(pczt).finalize_spends().unwrap();
    let tx = TransactionExtractor::new(pczt)
        .with_orchard(&circuit_keys().1)
        .extract()
        .unwrap();
    assert!(tx.ironwood_bundle().is_some());
}

fn full_signer_view(pczt: Pczt) -> Pczt {
    Redactor::new(pczt)
        .redact_ironwood_with(|mut bundle| clear_spend_secrets(&mut bundle))
        .finish()
}

fn compact_signer_view(pczt: Pczt) -> Pczt {
    Redactor::new(pczt)
        .redact_ironwood_with(|mut bundle| {
            bundle.clear_zkproof();
            bundle.clear_bsk();
            clear_spend_secrets(&mut bundle);
            bundle.compact_resolvable_fields();
            bundle.clear_anchor();
        })
        .finish()
}

fn clear_spend_secrets(bundle: &mut OrchardRedactor<'_>) {
    bundle.redact_actions(|mut action| {
        action.clear_spend_witness();
        action.clear_spend_dummy_sk();
    });
}

fn receive_ironwood_note(fvk: &FullViewingKey, to: Address) -> Note {
    let version = BundleVersion::ironwood_v3();
    let mut builder = orchard::builder::Builder::new(
        orchard::builder::BundleType::DEFAULT,
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
    let (bundle, meta): (orchard::Bundle<_, i64>, _) = builder.build(&mut OsRng).unwrap().unwrap();
    let action = &bundle.actions()[meta.output_action_index(0).unwrap()];
    let ivk = fvk.to_ivk(Scope::External).prepare();
    try_note_decryption(&IronwoodDomain::for_action(action), &ivk, action)
        .unwrap()
        .0
}

fn single_leaf_witness(note: &Note) -> (Anchor, MerklePath) {
    let cmx: ExtractedNoteCommitment = note.commitment().into();
    let leaf = MerkleHashOrchard::from_cmx(&cmx);
    let mut tree =
        ShardTree::<_, 32, 16>::new(MemoryShardStore::<MerkleHashOrchard, u32>::empty(), 100);
    tree.append(leaf, Retention::Marked).unwrap();
    tree.checkpoint(9_999_999).unwrap();
    let path = tree
        .witness_at_checkpoint_depth(0.into(), 0)
        .unwrap()
        .unwrap();
    (path.root(leaf).into(), path.into())
}

fn destination() -> Address {
    let sk = SpendingKey::from_bytes([1; 32]).unwrap();
    FullViewingKey::from(&sk).address_at(0u32, Scope::External)
}

fn nu6_3_network() -> LocalNetwork {
    LocalNetwork {
        overwinter: Some(BlockHeight::from_u32(1)),
        sapling: Some(BlockHeight::from_u32(2)),
        blossom: Some(BlockHeight::from_u32(3)),
        heartwood: Some(BlockHeight::from_u32(4)),
        canopy: Some(BlockHeight::from_u32(5)),
        nu5: Some(BlockHeight::from_u32(6)),
        nu6: Some(BlockHeight::from_u32(7)),
        nu6_1: Some(BlockHeight::from_u32(8)),
        nu6_2: Some(BlockHeight::from_u32(9)),
        nu6_3: Some(BlockHeight::from_u32(10)),
    }
}

fn intent() -> SweepIntent {
    SweepIntent {
        recipient: destination(),
        minimum_received: DEPOSIT - FEE,
        maximum_fee: FEE,
    }
}

#[test]
fn sweep_intent_rejects_another_recipient_and_excess_fees() {
    let swap = Swap::new();
    let other = Swap::new().joint.deposit_address();
    for intent in [
        SweepIntent {
            recipient: other,
            ..intent()
        },
        SweepIntent {
            maximum_fee: FEE - 1,
            ..intent()
        },
        SweepIntent {
            minimum_received: DEPOSIT - FEE + 1,
            ..intent()
        },
    ] {
        assert!(matches!(
            sign_pczt(swap.sweep(), &[swap.spend_key()], &intent),
            Err(Error::SweepIntent(_))
        ));
    }
}

#[test]
fn redacted_authorization_metadata_is_rejected() {
    let swap = Swap::new();
    let pczt = Redactor::new(swap.sweep())
        .redact_ironwood_with(|mut bundle| {
            bundle.redact_actions(|mut action| action.clear_output_value());
        })
        .finish();
    assert!(sign_pczt(pczt, &[swap.spend_key()], &intent()).is_err());
}

#[test]
fn change_to_a_different_receiver_is_rejected() {
    let swap = Swap::new();
    let other = Swap::new().joint.deposit_address();
    let pczt = swap.sweep_with_outputs(&[(destination(), DEPOSIT - FEE - 1), (other, 1)]);
    let authorized = SweepIntent {
        minimum_received: DEPOSIT - FEE - 1,
        ..intent()
    };
    assert!(matches!(
        sign_pczt(pczt, &[swap.spend_key()], &authorized),
        Err(Error::SweepIntent(_))
    ));
}

#[test]
fn corrupted_encrypted_output_is_rejected_before_signing() {
    let swap = Swap::new();
    let pczt = swap.sweep();
    let mut ciphertext = None;
    pczt::roles::verifier::Verifier::new(pczt.clone())
        .with_ironwood::<(), _>(|bundle| {
            ciphertext = bundle
                .actions()
                .iter()
                .find(|a| a.output().value().unwrap().inner() > 0)
                .map(|a| a.output().encrypted_note().enc_ciphertext);
            Ok(())
        })
        .unwrap();
    let ciphertext = ciphertext.unwrap();
    let mut bytes = pczt.serialize().unwrap();
    let offset = bytes
        .windows(ciphertext.len())
        .position(|w| w == ciphertext)
        .unwrap();
    bytes[offset + ciphertext.len() - 1] ^= 1;
    assert!(sign_pczt_bytes(&bytes, &[swap.spend_key()], &intent()).is_err());
}
