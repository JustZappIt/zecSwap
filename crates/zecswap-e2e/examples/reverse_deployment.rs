//! Exercises the deployed reverse protocol with publicly funded test tokens, not Relay Adapt.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::num::NonZeroU32;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use serde::{Deserialize, Serialize};
use zecswap_chain::evm::{Address, Settlement, Stage};
use zecswap_chain::zcash::{Network, Prover, TxId, Wallet, connect};
use zecswap_client::reverse::{ReverseSwap, ReverseUser};
use zecswap_client::{MakerApi, RelayerApi};
use zecswap_core::NetworkType;
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Deployment {
    chain_id: u64,
    contract: Address,
    token: Address,
}

#[derive(Serialize, Deserialize)]
struct State {
    swap: ReverseSwap,
    receive_tx: Option<[u8; 32]>,
}

#[derive(Serialize)]
struct FundingCall {
    to: Address,
    data: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let operation = std::env::args()
        .nth(1)
        .context("use prepare, receive or refund")?;
    ensure!(
        matches!(operation.as_str(), "prepare" | "receive" | "refund"),
        "unknown operation"
    );
    let dir = std::env::var("REVERSE_TEST_DIR").context("REVERSE_TEST_DIR")?;
    let dir = Path::new(&dir);
    fs::create_dir_all(dir)?;
    let seed_path = dir.join("seed");
    if !seed_path.exists() {
        ensure!(operation == "prepare", "no saved reverse test");
        let mut seed = Zeroizing::new([0u8; 32]);
        UnwrapErr(SysRng).fill_bytes(seed.as_mut());
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&seed_path)?
            .write_all(seed.as_ref())?;
    }
    let seed = Zeroizing::new(fs::read(seed_path)?);
    let deployment: Deployment =
        serde_json::from_slice(&fs::read("deployments/sepolia-reverse.json")?)?;
    let settlement =
        Settlement::read_only(&std::env::var("ETH_SEPOLIA_RPC_URL")?, deployment.contract)?;
    ensure!(
        deployment.chain_id == 11155111 && settlement.chain_id().await? == 11155111,
        "testnet only"
    );
    let maker = MakerApi::new(std::env::var("ZECSWAP_MAKER_URL")?)?;
    let info = maker.info().await?;
    ensure!(
        info.reverse_enabled
            && info.contract == deployment.contract
            && info.token == deployment.token,
        "wrong maker deployment"
    );
    let user = ReverseUser::new(
        &seed,
        &seed,
        NetworkType::Test,
        settlement,
        maker.clone(),
        RelayerApi::new(std::env::var("ZECSWAP_RELAYER_URL")?)?,
        deployment.token,
        100_000,
    );
    let state_path = dir.join("state.json");
    let mut state: State = if state_path.exists() {
        serde_json::from_slice(&fs::read(&state_path)?)?
    } else {
        ensure!(operation == "prepare", "no saved reverse test");
        let state = State {
            swap: user.quote(0, 1_000_000).await?,
            receive_tx: None,
        };
        save(&state_path, &state)?;
        state
    };
    let mut wallet = Wallet::open(dir.join("wallet.sqlite"), Network::TestNetwork)?
        .with_confirmations(NonZeroU32::new(3).unwrap());
    let mut client = connect("https://testnet.zec.rocks:443").await?;
    if operation == "prepare" {
        user.accept(&state.swap).await?;
    }
    let account = user
        .watch_deposit(&state.swap, &mut wallet, &mut client)
        .await?;
    if operation == "prepare" {
        let calls = user
            .funding_calls(&state.swap)
            .await?
            .map(|(to, data)| FundingCall {
                to,
                data: format!("0x{}", hex::encode(data)),
            });
        save(&dir.join("funding-calls.json"), &calls)?;
        println!("prepared {}", state.swap.swap_id);
        return Ok(());
    }
    let destination: zcash_address::ZcashAddress =
        std::env::var("REVERSE_TEST_DESTINATION")?.parse()?;
    let prover = Prover::default();
    let until = tokio::time::Instant::now() + Duration::from_secs(3600);
    while tokio::time::Instant::now() < until {
        let chain = user.state(&state.swap).await?;
        if operation == "refund" {
            user.refund(&state.swap).await?;
            if chain.stage == Stage::Refunded && chain.paid_out {
                println!("refunded {}", state.swap.swap_id);
                return Ok(());
            }
        } else {
            wallet.sync(&mut client).await?;
            let funds = wallet.funds(account)?;
            println!("stage {:?}, spendable {} zat", chain.stage, funds.spendable);
            match chain.stage {
                Stage::Open if funds.spendable >= state.swap.quote.terms.deposit_zat => {
                    user.ready(&state.swap, &mut wallet, &mut client, account)
                        .await?;
                }
                Stage::Claimed => {
                    if let Some(bytes) = state.receive_tx
                        && wallet.is_expired(TxId::from_bytes(bytes))?
                    {
                        state.receive_tx = None;
                        save(&state_path, &state)?;
                    }
                    let txid = match state.receive_tx {
                        Some(bytes) => TxId::from_bytes(bytes),
                        None => {
                            let key = user.receive_key(&state.swap).await?;
                            let txid = wallet.sweep(&prover, account, &key, &destination)?;
                            state.receive_tx = Some(*txid.as_ref());
                            save(&state_path, &state)?;
                            txid
                        }
                    };
                    if wallet.is_mined(txid)? {
                        println!("received ZEC: {txid}");
                        return Ok(());
                    }
                    match wallet.broadcast(&mut client, txid).await {
                        Ok(()) => println!("receive submitted: {txid}"),
                        Err(error) => eprintln!("receive {txid}: {error}; continuing to sync"),
                    }
                }
                Stage::Refunded => bail!("swap refunded instead of completing"),
                _ => {}
            }
        }
        println!(
            "maker phase: {:?}",
            maker.reverse_status(state.swap.swap_id).await?.phase
        );
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
    bail!("timed out; saved state can be resumed")
}

fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}
