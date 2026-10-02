//! Signs joint-account spends in a PCZT.
//!
//! `orchard` cannot build a `SpendAuthorizingKey` from a raw scalar, so signatures are
//! made here and applied through the Signer role's external-signature path, which
//! checks each one against the spend's `rk`.

use core::fmt::Debug;

use orchard::ValuePool;
use orchard::primitives::redpallas::{self, SpendAuth};
use pczt::Pczt;
use pczt::roles::signer::{Signer, SpendAuthSignature};
use pczt::roles::verifier::{OrchardError, Verifier};
use rand_core::OsRng;

use crate::{Error, SpendKey, SweepIntent};

type Pending = Vec<(ValuePool, usize, redpallas::SigningKey<SpendAuth>)>;

pub fn sign_pczt_bytes(
    pczt: &[u8],
    keys: &[SpendKey],
    intent: &SweepIntent,
) -> Result<Vec<u8>, Error> {
    let pczt = Pczt::parse(pczt).map_err(pczt_error)?;
    sign_pczt(pczt, keys, intent)?
        .serialize()
        .map_err(pczt_error)
}

/// Signs every Orchard and Ironwood spend in `pczt` with the matching key in `keys`.
///
/// Requires the authorized receiver, minimum receipt and maximum fee, no change to
/// other receivers, and no foreign pools or preauthorized real inputs.
pub fn sign_pczt(pczt: Pczt, keys: &[SpendKey], intent: &SweepIntent) -> Result<Pczt, Error> {
    let pczt = intent.verify(pczt)?;
    let keys: Vec<_> = keys.iter().map(SpendKey::signing_key).collect();
    let has_orchard = !pczt.orchard().actions().is_empty();
    let has_ironwood = !pczt.ironwood().actions().is_empty();

    let mut pending = Pending::new();
    let mut verifier = Verifier::new(pczt);
    if has_orchard {
        verifier = verifier
            .with_orchard(|bundle| match_spends(bundle, ValuePool::Orchard, &keys, &mut pending))
            .map_err(verifier_error)?;
    }
    if has_ironwood {
        verifier = verifier
            .with_ironwood(|bundle| match_spends(bundle, ValuePool::Ironwood, &keys, &mut pending))
            .map_err(verifier_error)?;
    }
    if pending.is_empty() {
        return Err(Error::NothingToSign);
    }

    let mut signer = Signer::new(verifier.finish()).map_err(pczt_error)?;
    let sighash = signer.shielded_sighash();
    for (pool, index, rsk) in pending {
        let signature = (&rsk.sign(OsRng, &sighash)).into();
        signer
            .apply_orchard_spend_auth_signature(&SpendAuthSignature::from_parts(
                pool, index, signature,
            ))
            .map_err(pczt_error)?;
    }
    Ok(signer.finish())
}

fn match_spends(
    bundle: &orchard::pczt::Bundle,
    pool: ValuePool,
    keys: &[redpallas::SigningKey<SpendAuth>],
    pending: &mut Pending,
) -> Result<(), OrchardError<Error>> {
    for (index, action) in bundle.actions().iter().enumerate() {
        let spend = action.spend();
        if spend.spend_auth_sig().is_some() {
            continue;
        }
        let rk: [u8; 32] = spend.rk().into();
        let rsk = spend
            .alpha()
            .as_ref()
            .and_then(|alpha| {
                keys.iter()
                    .map(|key| key.randomize(alpha))
                    .find(|rsk| <[u8; 32]>::from(&redpallas::VerificationKey::from(rsk)) == rk)
            })
            .ok_or(OrchardError::Custom(Error::UnsignedSpend { pool, index }))?;
        pending.push((pool, index, rsk));
    }
    Ok(())
}

pub(crate) fn verifier_error(e: OrchardError<Error>) -> Error {
    match e {
        OrchardError::Custom(e) => e,
        e => pczt_error(e),
    }
}

fn pczt_error(e: impl Debug) -> Error {
    Error::Pczt(format!("{e:?}"))
}
