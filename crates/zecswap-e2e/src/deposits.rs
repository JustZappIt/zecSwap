//! Public test tokens sent to a fresh wallet's deposit address, and shielded into that wallet's
//! Railgun balance through the relayer, as a wallet with no ETH deposits: the reference client
//! signs, the relayer sends and is paid its fee, and Railgun's own wallet SDK finds the note,
//! spendable once screening clears it where Railgun screens.

use std::time::{Duration, Instant};

use alloy::providers::ProviderBuilder;
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use zecswap_api::Network;
use zecswap_chain::evm::Settlement;
use zecswap_client::{DepositDeployment, Deposited, Depositor, FeeCap, RelayerApi};
use zecswap_railgun::Keys;

use crate::env::{DEPOSIT_MIN_VALUE, Env, MAX_RELAYER_FEE};
use crate::sends::{NewWallet, SCREENING_WITHIN, artifacts, sdk, seed_bytes};

const NAME: &str = "deposit";
/// What arrives at the deposit address: twelve of the test token.
const DEPOSITED: u128 = 12_000_000;

alloy::sol! {
    #[sol(rpc)]
    interface IRailgunFees {
        function shieldFee() external view returns (uint120);
    }
}

#[derive(Deserialize)]
struct Spendable {
    address: String,
    spendable: String,
}

/// A fresh wallet's deposit address is paid under the minimum and then over it, and shields it all
/// into the wallet's Railgun balance less the relayer's fee and Railgun's; the same request posted
/// again names the same transaction; the next address is shown; Railgun's SDK finds the note
/// spendable in the wallet; and the relayer's ledger costs the deposit.
pub(crate) async fn deposit(env: &Env) -> Result<()> {
    let contract = env.deposits.context("no Deposits contract on this chain")?;
    let wallet: NewWallet = sdk(env, "wallet", None).await?;
    // Kept with the run, as the makers' root secrets are, to look into a failed one.
    let kept = env.work_dir.join("depositor.json");
    std::fs::write(&kept, json!({ "mnemonic": wallet.mnemonic }).to_string())?;
    std::fs::set_permissions(&kept, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let seed = seed_bytes(&wallet.seed)?;
    let chain = Settlement::read_only(&env.evm_rpc, contract)?;
    let chain_id = chain.chain_id().await?;
    let from_block = chain.head().await?.number;
    let railgun = chain.railgun().await?;
    let relayer = RelayerApi::new(env.relayer_url.clone())?;
    let depositor = Depositor::new(
        &seed,
        &seed,
        Network::Testnet,
        chain,
        relayer.clone(),
        DepositDeployment {
            chain_id,
            contract,
            token: env.token,
        },
        FeeCap {
            fee: MAX_RELAYER_FEE,
            fee_bps: 50,
        },
    );
    let address = depositor.address(0)?;
    let funder = Settlement::connect(&env.evm_rpc, env.contract, env.funder.clone())?;
    let under = u128::from(DEPOSIT_MIN_VALUE) - 1;
    funder.mint_test_token(env.token, address, under).await?;
    ensure!(
        depositor.sign_shield(0).await?.is_none() && depositor.showing().await? == 0,
        "an address under the minimum was signed for, or is no longer shown"
    );
    funder
        .mint_test_token(env.token, address, DEPOSITED - under)
        .await?;
    env.log(
        NAME,
        format!("paid {DEPOSITED} to deposit address {address}"),
    );

    let mut requests = Vec::new();
    let deposit = depositor
        .shield(0, |request| {
            requests.push(request.clone());
            Ok(())
        })
        .await?
        .context("the deposit address held under the minimum")?;
    let request = requests.last().context("no request was kept")?;
    let provider = ProviderBuilder::new().connect_http(env.evm_rpc.parse()?);
    let shield_bps = IRailgunFees::new(railgun, &provider)
        .shieldFee()
        .call()
        .await?
        .to::<u128>();
    let shielded = DEPOSITED - request.fee;
    ensure!(
        deposit.shielded.value == shielded - shielded * shield_bps / 10_000
            && deposit.shielded.token == env.token,
        "Railgun took {} of {}, not {shielded} less its {shield_bps} basis points",
        deposit.shielded.value,
        deposit.shielded.token
    );
    env.log(
        NAME,
        format!(
            "shielded {} in {}, the relayer keeping {}",
            deposit.shielded.value, deposit.tx, request.fee
        ),
    );
    ensure!(
        relayer.shield_deposit(request).await == Deposited::Sent(vec![deposit.tx]),
        "the same request posted again did not name the deposit"
    );
    ensure!(
        depositor.showing().await? == 1,
        "the shielded address is still the one shown"
    );

    let found: Spendable = sdk(
        env,
        "spendable",
        Some(json!({
            "rpc": env.railgun_rpc,
            "artifacts": artifacts(env),
            "token": env.token.to_string(),
            "creationBlock": from_block,
            "seed": wallet.seed,
            "needed": deposit.shielded.value.to_string(),
            "waitSeconds": SCREENING_WITHIN.as_secs(),
        })),
    )
    .await?;
    ensure!(
        found.address == Keys::from_seed(&seed, 0).address()
            && found.spendable == deposit.shielded.value.to_string(),
        "Railgun's SDK finds {} spendable in {}",
        found.spendable,
        found.address
    );
    env.log(
        NAME,
        format!("Railgun's SDK finds {} spendable", found.spendable),
    );

    let started = Instant::now();
    let record = loop {
        let ledger = serde_json::to_value(env.relayer.deposits_snapshot(Some(0))?)?;
        let record = ledger["deposits"].as_array().and_then(|records| {
            records
                .iter()
                .find(|record| record["transactionHash"] == json!(deposit.tx))
                .filter(|record| !record["succeeded"].is_null())
                .cloned()
        });
        if let Some(record) = record {
            break record;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(150),
            "the relayer never recorded what the deposit cost"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    ensure!(
        record["succeeded"] == true
            && record["value"] == DEPOSITED.to_string()
            && record["fee"] == request.fee.to_string(),
        "the relayer's ledger recorded the deposit as {record}"
    );
    env.log(
        NAME,
        format!(
            "the relayer's ledger costs the deposit at {} gas, earning {}",
            record["gasUsed"], request.fee
        ),
    );
    Ok(())
}
