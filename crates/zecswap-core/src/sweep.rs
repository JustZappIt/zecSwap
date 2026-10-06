//! A sweep pays one independently selected receiver, with no change or other pools.

use orchard::{Address, ValuePool, value::Sign};
use pczt::{
    Pczt,
    roles::verifier::{OrchardError, Verifier},
};
use zcash_address::unified::{self, Container, Encoding};
use zcash_protocol::{PoolType, consensus::NetworkType, value::Zatoshis};

use crate::Error;

#[derive(Clone, Debug)]
pub struct SweepIntent {
    pub recipient: Address,
    pub minimum_received: u64,
    pub maximum_fee: u64,
}

impl SweepIntent {
    pub fn from_address(
        address: &str,
        network: NetworkType,
        minimum_received: u64,
        maximum_fee: u64,
    ) -> Result<Self, Error> {
        let (actual_network, revision, address) = unified::Address::decode(address)
            .map_err(|_| policy("sweep destination must be a unified address"))?;
        // Revision 2 can carry an expiry this check does not read.
        if revision != unified::Revision::R0 {
            return Err(policy("sweep destination must be a revision 0 unified address"));
        }
        if actual_network != network {
            return Err(policy("sweep destination is on another network"));
        }
        let receiver = address
            .items()
            .into_iter()
            .find_map(|receiver| match receiver {
                unified::Receiver::Orchard(bytes) => {
                    Address::from_raw_address_bytes(&bytes).into_option()
                }
                _ => None,
            })
            .ok_or_else(|| policy("sweep destination needs an Orchard receiver"))?;
        let intent = Self {
            recipient: receiver,
            minimum_received,
            maximum_fee,
        };
        intent.validate()?;
        Ok(intent)
    }

    fn validate(&self) -> Result<(), Error> {
        if self.minimum_received == 0
            || Zatoshis::from_u64(self.minimum_received).is_err()
            || Zatoshis::from_u64(self.maximum_fee).is_err()
        {
            return Err(policy("invalid sweep amount or fee limit"));
        }
        Ok(())
    }

    pub(crate) fn verify(&self, pczt: Pczt) -> Result<Pczt, Error> {
        self.validate()?;
        if pczt.has_data_in_pool(PoolType::TRANSPARENT) || pczt.has_data_in_pool(PoolType::SAPLING)
        {
            return Err(policy(
                "a joint sweep cannot contain transparent or Sapling components",
            ));
        }
        let has_orchard = !pczt.orchard().actions().is_empty();
        let has_ironwood = !pczt.ironwood().actions().is_empty();
        let mut totals = Totals::default();
        let mut verifier = Verifier::new(pczt);
        if has_orchard {
            verifier = verifier
                .with_orchard(|bundle| self.bundle(bundle, ValuePool::Orchard, &mut totals))
                .map_err(crate::signer::verifier_error)?;
        }
        if has_ironwood {
            verifier = verifier
                .with_ironwood(|bundle| self.bundle(bundle, ValuePool::Ironwood, &mut totals))
                .map_err(crate::signer::verifier_error)?;
        }
        if totals.received < u128::from(self.minimum_received)
            || totals.fee < 0
            || totals.fee > i128::from(self.maximum_fee)
        {
            return Err(policy(
                "sweep exceeds the authorized fee or pays too little",
            ));
        }
        Ok(verifier.finish())
    }

    fn bundle(
        &self,
        bundle: &orchard::pczt::Bundle,
        pool: ValuePool,
        totals: &mut Totals,
    ) -> Result<(), OrchardError<Error>> {
        bundle.verify_cross_address_restriction()?;
        let mut net = 0i128;
        for action in bundle.actions() {
            let spend = action.spend();
            let output = action.output();
            action.verify_cv_net()?;
            output.verify_note_commitment(spend)?;
            let spent = spend
                .value()
                .ok_or(orchard::pczt::VerifyError::MissingValue)?
                .inner();
            let received = output
                .value()
                .ok_or(orchard::pczt::VerifyError::MissingValue)?
                .inner();
            if spent > 0 && spend.spend_auth_sig().is_some() {
                return Err(OrchardError::Custom(policy(
                    "a sweep cannot include preauthorized real spends",
                )));
            }
            if spent > 0 {
                spend.verify_nullifier(None)?;
                spend.verify_rk(None)?;
            }
            if received > 0 {
                if output.recipient().as_ref() != Some(&self.recipient) {
                    return Err(OrchardError::Custom(policy(
                        "sweep output or change goes to another receiver",
                    )));
                }
                verify_ciphertext(action, pool).map_err(OrchardError::Custom)?;
                totals.received += u128::from(received);
            }
            net += i128::from(spent) - i128::from(received);
        }
        let (magnitude, sign) = bundle.value_sum().magnitude_sign();
        let declared = match sign {
            Sign::Positive => i128::from(magnitude),
            Sign::Negative => -i128::from(magnitude),
        };
        if net != declared {
            return Err(OrchardError::Custom(policy(
                "sweep values do not match the bundle balance",
            )));
        }
        totals.fee += declared;
        Ok(())
    }
}

#[derive(Default)]
struct Totals {
    received: u128,
    fee: i128,
}

fn policy(message: &str) -> Error {
    Error::SweepIntent(message.into())
}

fn verify_ciphertext(action: &orchard::pczt::Action, pool: ValuePool) -> Result<(), Error> {
    use orchard::{
        Note,
        note::{NoteVersion, Rho},
        note_encryption::{IronwoodDomain, OrchardDomain},
    };
    let output = action.output();
    let note = Note::from_parts(
        output
            .recipient()
            .ok_or_else(|| policy("missing sweep output recipient"))?,
        output
            .value()
            .ok_or_else(|| policy("missing sweep output value"))?,
        Rho::from_bytes(&action.spend().nullifier().to_bytes()).unwrap(),
        output
            .rseed()
            .ok_or_else(|| policy("missing sweep output randomness"))?,
        match pool {
            ValuePool::Orchard => NoteVersion::V2,
            ValuePool::Ironwood => NoteVersion::V3,
        },
    )
    .into_option()
    .ok_or_else(|| policy("invalid sweep output note"))?;
    let valid = match pool {
        ValuePool::Orchard => recover(&OrchardDomain::for_pczt_action(action), &note, action),
        ValuePool::Ironwood => recover(&IronwoodDomain::for_pczt_action(action), &note, action),
    };
    if valid {
        Ok(())
    } else {
        Err(policy("sweep output is not decryptable by its receiver"))
    }
}

fn recover<D>(domain: &D, note: &orchard::Note, action: &orchard::pczt::Action) -> bool
where
    D: zcash_note_encryption::Domain<Note = orchard::Note>,
    orchard::pczt::Action: zcash_note_encryption::ShieldedOutput<D>,
{
    D::derive_esk(note)
        .and_then(|esk| {
            zcash_note_encryption::try_output_recovery_with_pkd_esk(
                domain,
                D::get_pk_d(note),
                esk,
                action,
            )
        })
        .is_some()
}
