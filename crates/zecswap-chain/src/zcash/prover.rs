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
        let circuit = self.circuit(circuit_version(branch)?);

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

/// The circuit proving both Orchard-protocol pools under `branch`: they share one under any
/// branch.
fn circuit_version(branch: BranchId) -> Result<OrchardCircuitVersion, Error> {
    bundle_version_for_branch(branch, ValuePool::Orchard)
        .map(|version| version.circuit_version())
        .ok_or_else(|| Error::Wallet(format!("{branch:?} has no Orchard protocol")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NU7 keeps Orchard protocol V3: nodes refuse a proof from any other circuit as invalid.
    #[test]
    fn nu7_proves_both_pools_with_the_nu6_3_circuit() {
        let nu7 = circuit_version(BranchId::Nu7).unwrap();
        assert_eq!(nu7, circuit_version(BranchId::Nu6_3).unwrap());
        assert_eq!(nu7, OrchardCircuitVersion::PostNu6_3);
        assert_eq!(
            bundle_version_for_branch(BranchId::Nu7, ValuePool::Ironwood)
                .map(|v| v.circuit_version()),
            Some(nu7)
        );
    }
}
