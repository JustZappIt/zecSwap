//! Deposits from this seed's deposit addresses into its Railgun wallet, resumable: each signed
//! request is saved before it is posted, and posted again after an interruption.

use alloy_primitives::Address;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use zecswap_api::Network;
use zecswap_api::relayer::ShieldDeposit;
use zecswap_chain::evm::Settlement;
use zecswap_client::{Deposit, DepositDeployment, Depositor, FeeCap, RelayerApi};

use crate::store::Store;

const PENDING: &str = "deposit.json";

pub(crate) struct DepositArgs {
    pub(crate) relayer: String,
    pub(crate) rpc: String,
    pub(crate) contract: Address,
    pub(crate) token: Address,
    pub(crate) max_fee: u128,
    pub(crate) max_fee_bps: u32,
}

/// A shield signed and saved, not yet known to have landed.
#[derive(Serialize, Deserialize)]
struct Pending {
    index: u32,
    request: ShieldDeposit,
}

pub(crate) async fn run(store: &Store, network: Network, args: DepositArgs) -> Result<()> {
    let seed = store.seed()?;
    let chain = Settlement::read_only(&args.rpc, args.contract)?;
    let deployment = DepositDeployment {
        chain_id: chain.chain_id().await?,
        contract: args.contract,
        token: args.token,
    };
    let cap = FeeCap {
        fee: args.max_fee,
        fee_bps: args.max_fee_bps,
    };
    let relayer = RelayerApi::new(args.relayer)?;
    let wallet = Depositor::new(&seed, &seed, network, chain, relayer, deployment, cap);
    let keep = |index| {
        move |request: &ShieldDeposit| {
            let pending = Pending {
                index,
                request: request.clone(),
            };
            store.save(PENDING, &pending)
        }
    };

    if let Some(pending) = store.load::<Pending>(PENDING)? {
        println!("resuming the deposit from address #{}", pending.index);
        let index = pending.index;
        report(
            index,
            &wallet
                .post_shield(index, pending.request, keep(index))
                .await?,
        );
        store.remove(PENDING)?;
    }
    for (index, used) in wallet.recover().await? {
        if used.balance == 0 {
            continue;
        }
        match wallet.shield(index, keep(index)).await? {
            Some(deposit) => {
                report(index, &deposit);
                store.remove(PENDING)?;
            }
            None => println!(
                "address #{index} holds {}, under the relayer's minimum",
                used.balance
            ),
        }
    }
    let index = wallet.showing().await?;
    println!("deposit to {} (address #{index})", wallet.address(index)?);
    Ok(())
}

fn report(index: u32, deposit: &Deposit) {
    println!(
        "shielded {} from address #{index} into this seed's Railgun wallet in {}",
        deposit.shielded.value, deposit.tx
    );
}
