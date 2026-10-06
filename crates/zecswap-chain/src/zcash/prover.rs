use std::sync::{Arc, Mutex};

use orchard::ValuePool;
use orchard::circuit::{OrchardCircuitVersion, ProvingKey, VerifyingKey};
use pczt::Pczt;
use pczt::roles::prover::Prover as PcztProver;
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use zcash_primitives::transaction::components::orchard::bundle_version_for_branch;
use zcash_protocol::consensus::BranchId;

use crate::Error;

pub(crate) struct Circuit {
    pub(crate) proving_key: ProvingKey,
    pub(crate) verifying_key: VerifyingKey,
}

/// Creates Orchard and Ironwood proofs, building each circuit's keys the first time a
/// transaction needs them.
#[derive(Default)]
pub struct Prover {
    circuits: Mutex<Vec<(OrchardCircuitVersion, Arc<Circuit>)>>,
}

impl Prover {
    /// Proves `pczt` and returns the circuit whose verifying key checks it.
    pub(crate) fn prove(&self, pczt: Pczt) -> Result<(Pczt, Arc<Circuit>), Error> {
        let branch =
            BranchId::try_from(*pczt.global().consensus_branch_id()).map_err(Error::wallet)?;
        // Both Orchard-protocol pools use the same circuit under any one branch.
        let version = bundle_version_for_branch(branch, ValuePool::Orchard)
            .ok_or_else(|| Error::Wallet(format!("{branch:?} has no Orchard protocol")))?
            .circuit_version();
        let circuit = self.circuit(version);

        let mut prover = PcztProver::new(pczt);
        if prover.requires_sapling_proofs() {
            return Err(Error::Wallet(
                "Sapling spends and outputs are not supported".into(),
            ));
        }
        if prover.requires_orchard_proof() {
            prover = prover
                .create_orchard_proof(UnwrapErr(SysRng), &circuit.proving_key)
                .map_err(|e| Error::Wallet(format!("Orchard proof: {e:?}")))?;
        }
        if prover.requires_ironwood_proof() {
            prover = prover
                .create_ironwood_proof(UnwrapErr(SysRng), &circuit.proving_key)
                .map_err(|e| Error::Wallet(format!("Ironwood proof: {e:?}")))?;
        }
        Ok((prover.finish(), circuit))
    }

    fn circuit(&self, version: OrchardCircuitVersion) -> Arc<Circuit> {
        let mut circuits = self
            .circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, circuit)) = circuits.iter().find(|(v, _)| *v == version) {
            return circuit.clone();
        }
        let circuit = Arc::new(Circuit {
            proving_key: ProvingKey::build(version),
            verifying_key: VerifyingKey::build(version),
        });
        circuits.push((version, circuit.clone()));
        circuit
    }
}
