//! Private Railgun sends and withdrawals through the relayer, proved by Railgun's own wallet SDK
//! as the app proves them (crates/zecswap-railgun/engine/send.cjs), each paying the relayer a
//! fee note to its own 0zk address.

use std::process::Stdio;
use std::time::{Duration, Instant};

use alloy::primitives::aliases::U72;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol_types::SolCall;
use anyhow::{Context as _, Result, bail, ensure};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zecswap_api::relayer::{RailgunSendTerms, RailgunTransact};
use zecswap_chain::evm::railgun::{IRailgunSmartWallet, Status};
use zecswap_chain::evm::{Address, B256, PrivateKeySigner, Settlement, U256};
use zecswap_client::{Broadcast, RelayerApi};
use zecswap_railgun::Keys;

use crate::env::{Env, SHIELDED};

const NAME: &str = "railgun-send";
/// What each send moves, besides the relayer's fee: one of the test token.
const SENT: u128 = 1_000_000;
const MINED_WITHIN: Duration = Duration::from_secs(10 * 60);
/// How long screening may take to clear a note on a live chain.
const SCREENING_WITHIN: Duration = Duration::from_secs(40 * 60);
/// Screening, then a minute or two to sync and prove.
const SDK_WITHIN: Duration = Duration::from_secs(45 * 60);
/// Longer than the relayer waits before broadcasting a lost transaction again.
const REBROADCAST_WAIT: Duration = Duration::from_secs(65);

alloy::sol! {
    #[sol(rpc)]
    interface IRailgunFees {
        function unshieldFee() external view returns (uint120);
    }
}

#[derive(Deserialize)]
struct NewWallet {
    mnemonic: String,
    seed: String,
}

#[derive(Deserialize)]
struct Proved {
    transactions: Vec<RailgunTransact>,
    /// What each pays the relayer, in its token's base units.
    fees: Vec<String>,
}

#[derive(Deserialize)]
struct Balances {
    balances: Vec<Balance>,
}

#[derive(Deserialize)]
struct Balance {
    total: String,
}

/// A wallet shields twice, then sends privately and withdraws through the relayer. Copies altered on
/// the way, and a proof underpaying the fee, are refused with nothing sent. A send whose
/// broadcast is lost is unknown until the relayer broadcasts it again; the same bytes posted
/// again, to either of two relayers on one journal, name it and send nothing new; a second proof
/// of the same notes is told which send spends them, and a relayer that lost its journal finds
/// them spent; and the fees reach the relayer's Railgun wallet, where Railgun's own SDK finds them.
pub(crate) async fn railgun_send(env: &Env) -> Result<()> {
    let node = env
        .sends
        .as_ref()
        .context("no relayer sends Railgun transactions")?;
    let relayer = RelayerApi::new(node.relayer_url.clone())?;
    let all_terms = relayer.terms().await?;
    let terms = all_terms
        .railgun_sends
        .context("the relayer advertises no Railgun sends")?;
    ensure!(
        terms.railgun_address == Keys::from_seed(&node.railgun_seed, 0).address(),
        "the relayer's fee address is not its Railgun wallet"
    );
    let chain = ProviderBuilder::new().connect_http(env.evm_rpc.parse()?);
    let nonce = |account| {
        let chain = chain.clone();
        async move { anyhow::Ok(chain.get_transaction_count(account).await?) }
    };

    let sender: NewWallet = sdk(env, "wallet", None).await?;
    // Kept with the run, as the makers' root secrets are, to look into a failed one.
    let kept = env.work_dir.join("railgun-sender.json");
    std::fs::write(&kept, json!({ "mnemonic": sender.mnemonic }).to_string())?;
    std::fs::set_permissions(&kept, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    let sender_keys = Keys::from_seed(&seed_bytes(&sender.seed)?, 0);
    let from_block = chain.get_block_number().await?;
    let shielder = Settlement::connect(&env.evm_rpc, env.contract, node.shielder.clone())?;
    let shield = || async {
        let mut entropy = [0; 32];
        UnwrapErr(SysRng).fill_bytes(&mut entropy);
        let tx = shielder
            .shield(env.token, SHIELDED, &sender_keys.note(&entropy)?)
            .await?;
        env.log(NAME, format!("shielded {SHIELDED} into the wallet in {tx}"));
        anyhow::Ok(())
    };
    shield().await?;

    let mut recipient_seed = [0; 64];
    UnwrapErr(SysRng).fill_bytes(&mut recipient_seed);
    let recipient = Keys::from_seed(&recipient_seed, 0).address();
    let withdrawn_to = PrivateKeySigner::random().address().to_string();
    let gas_price = shielder.gas_price().await?.min(terms.max_gas_price_wei);
    // All of them spend the shielded note: two priced as the app prices them, one a unit under
    // the fixed fee, and where the relayer prices gas, one paying the fixed fee alone.
    let mut proofs = vec![
        (recipient.as_str(), None),
        (withdrawn_to.as_str(), None),
        (recipient.as_str(), Some(terms.fee - 1)),
    ];
    if terms.fee_per_unit_gas.is_some() {
        proofs.push((recipient.as_str(), Some(terms.fee)));
    }
    let first = prove(env, &sender, from_block, &terms, gas_price, &proofs).await?;
    let [
        (transfer, transfer_fee),
        (withdrawal, _),
        (underpaid, _),
        short @ ..,
    ] = first.as_slice()
    else {
        bail!("the SDK proved {} sends", first.len());
    };
    if terms.fee_per_unit_gas.is_some() {
        ensure!(
            *transfer_fee > terms.fee,
            "a send priced by its gas paid only the fixed fee, {transfer_fee}"
        );
    }

    let sent_before = nonce(all_terms.relayer).await?;
    let short = short.iter().map(|(request, _)| {
        (
            request.clone(),
            "paying the fixed fee alone, short of its gas",
        )
    });
    for (request, what) in altered(transfer, underpaid, env.token, &terms)?
        .into_iter()
        .chain(short)
    {
        match relayer.railgun_transact(&request).await {
            Broadcast::Refused(reason) => env.log(NAME, format!("{what}: refused, {reason}")),
            other => bail!("{what}: {other:?}"),
        }
    }
    ensure!(
        nonce(all_terms.relayer).await? == sent_before,
        "the relayer sent a transaction for a refused request"
    );

    let lossy = RelayerApi::new(node.lossy_relayer_url.clone())?;
    for when in ["", " again"] {
        match lossy.railgun_transact(transfer).await {
            Broadcast::Retry(reason) => {
                env.log(NAME, format!("posted{when}, its broadcast lost: {reason}"))
            }
            other => bail!("posted{when}, its broadcast lost: {other:?}"),
        }
    }
    tokio::time::sleep(REBROADCAST_WAIT).await;
    let Broadcast::Sent(sent) = lossy.railgun_transact(transfer).await else {
        bail!("the lost broadcast was not sent again");
    };
    let transferred = sent[0];
    ensure!(
        relayer.railgun_transact(transfer).await == Broadcast::Sent(sent.clone()),
        "the same bytes posted to the other relayer on the journal did not name the send"
    );
    ensure!(
        relayer.railgun_transact(withdrawal).await == Broadcast::Spent(sent.clone()),
        "a proof of notes already being spent was not told which send spends them"
    );
    // The withdrawal spends a note shielded now: where Railgun screens, the send's change clears
    // only once screening proofs of the send are in (docs/railgun-sends.md, "Screening").
    shield().await?;
    mined(&shielder, sent[0]).await?;
    env.log(NAME, format!("sent privately in {}", sent[0]));

    let second = prove(
        env,
        &sender,
        from_block,
        &terms,
        gas_price,
        &[(withdrawn_to.as_str(), None)],
    )
    .await?;
    let (withdrawal, withdrawal_fee) = &second[0];
    let Broadcast::Sent(withdrawn) = relayer.railgun_transact(withdrawal).await else {
        bail!("the withdrawal was not sent");
    };
    mined(&shielder, withdrawn[0]).await?;
    ensure!(
        relayer.railgun_transact(transfer).await == Broadcast::Sent(sent),
        "the first send's bytes, posted after it mined, did not name it"
    );
    // A relayer that lost its journal finds the notes spent on chain, and sends nothing.
    let forgetful = RelayerApi::new(node.forgetful_relayer_url.clone())?;
    ensure!(
        forgetful.railgun_transact(transfer).await == Broadcast::Spent(Vec::new()),
        "a relayer without the journal did not find the mined send's notes spent"
    );
    ensure!(
        nonce(all_terms.relayer).await? == sent_before + 2,
        "the relayer sent other than the two sends"
    );
    let unshield_bps = IRailgunFees::new(terms.railgun_proxy, &chain)
        .unshieldFee()
        .call()
        .await?
        .to::<u128>();
    let received = shielder
        .token_balance(env.token, withdrawn_to.parse()?)
        .await?;
    ensure!(
        received == SENT - SENT * unshield_bps / 10_000,
        "the withdrawal paid {received}, less Railgun's {unshield_bps} basis points of {SENT}"
    );
    env.log(NAME, format!("withdrew {received} in {}", withdrawn[0]));

    let balances: Balances = sdk(
        env,
        "balances",
        Some(json!({
            "rpc": env.railgun_rpc,
            "artifacts": artifacts(env),
            "token": env.token.to_string(),
            "creationBlock": from_block,
            "seeds": [
                format!("0x{}", hex::encode(node.railgun_seed)),
                format!("0x{}", hex::encode(recipient_seed)),
            ],
        })),
    )
    .await?;
    let [fees, delivered] = balances.balances.as_slice() else {
        bail!("the SDK read {} balances", balances.balances.len());
    };
    ensure!(
        fees.total == (transfer_fee + withdrawal_fee).to_string()
            && delivered.total == SENT.to_string(),
        "Railgun's SDK finds {} in the relayer's wallet and {} in the recipient's",
        fees.total,
        delivered.total
    );

    // The relayer's ledger names both: what each was, the fee it paid, and the gas it burned.
    let started = Instant::now();
    let costed = loop {
        let ledger = serde_json::to_value(node.relayer.sends_snapshot(Some(0))?)?;
        let record = |hash: B256| {
            ledger["sends"]
                .as_array()
                .and_then(|sends| {
                    sends
                        .iter()
                        .find(|send| send["transactionHash"] == json!(hash))
                })
                .filter(|send| !send["kind"].is_null())
                .cloned()
        };
        if let (Some(transfer), Some(withdrawal)) = (record(transferred), record(withdrawn[0])) {
            break [transfer, withdrawal];
        }
        ensure!(
            started.elapsed() < Duration::from_secs(150),
            "the relayer never recorded what its sends cost"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    for ((record, kind), fee) in costed
        .iter()
        .zip(["send", "unshield"])
        .zip([transfer_fee, withdrawal_fee])
    {
        ensure!(
            record["kind"] == kind
                && record["succeeded"] == true
                && record["fee"] == fee.to_string()
                && record["gasUsed"].as_u64().is_some_and(|gas| gas > 21_000),
            "the relayer's ledger recorded the {kind} as {record}"
        );
    }
    env.log(
        NAME,
        format!(
            "the relayer's ledger costs the send at {} gas paying {transfer_fee}, and the withdrawal at {} paying {withdrawal_fee}",
            costed[0]["gasUsed"], costed[1]["gasUsed"]
        ),
    );
    env.log(
        NAME,
        format!(
            "the relayer's Railgun wallet holds both fees, {}",
            fees.total
        ),
    );
    Ok(())
}

/// Each send, `(to, fee)`, proved by Railgun's SDK from the sender's notes as they stand: to a
/// 0zk address a private send, to an account a withdrawal.
/// Each send to `to` proved with the SDK, paying the fee given, or if none the fee the app works
/// out from the terms; with the fee each pays.
async fn prove(
    env: &Env,
    sender: &NewWallet,
    from_block: u64,
    terms: &RailgunSendTerms,
    gas_price: u128,
    sends: &[(&str, Option<u128>)],
) -> Result<Vec<(RailgunTransact, u128)>> {
    let proved: Proved = sdk(
        env,
        "prove",
        Some(json!({
            "rpc": env.railgun_rpc,
            "artifacts": artifacts(env),
            "mnemonic": sender.mnemonic,
            "creationBlock": from_block,
            "token": env.token.to_string(),
            "broadcaster": {
                "railgunAddress": terms.railgun_address,
                "token": terms.token.to_string(),
                "fee": terms.fee.to_string(),
                "feePerUnitGas": terms.fee_per_unit_gas.map(|rate| rate.to_string()),
            },
            "minGasPrice": gas_price.to_string(),
            "waitSeconds": SCREENING_WITHIN.as_secs(),
            "sends": sends.iter().map(|(to, fee)| json!({
                "to": to,
                "amount": SENT.to_string(),
                "fee": fee.map(|fee| fee.to_string()),
            })).collect::<Vec<_>>(),
        })),
    )
    .await?;
    ensure!(proved.transactions.len() == sends.len() && proved.fees.len() == sends.len());
    env.log(
        NAME,
        format!(
            "Railgun's SDK proved {} sends, paying {}",
            sends.len(),
            proved.fees.join(", ")
        ),
    );
    proved
        .transactions
        .into_iter()
        .zip(proved.fees)
        .map(|(transaction, fee)| Ok((transaction, fee.parse()?)))
        .collect()
}

/// The send as it might arrive altered, each of which the relayer must refuse unsent.
fn altered(
    transfer: &RailgunTransact,
    underpaid: &RailgunTransact,
    token: Address,
    terms: &RailgunSendTerms,
) -> Result<Vec<(RailgunTransact, &'static str)>> {
    let call = IRailgunSmartWallet::transactCall::abi_decode(&transfer.data)?;
    let with = |change: &dyn Fn(&mut IRailgunSmartWallet::transactCall)| {
        let mut call = call.clone();
        change(&mut call);
        RailgunTransact {
            data: call.abi_encode().into(),
            ..transfer.clone()
        }
    };
    let bytes = |change: &dyn Fn(&mut Vec<u8>)| {
        let mut data = transfer.data.to_vec();
        change(&mut data);
        RailgunTransact {
            data: data.into(),
            ..transfer.clone()
        }
    };
    let cap = U72::from(terms.max_gas_price_wei + 1);
    Ok(vec![
        (
            RailgunTransact {
                to: token,
                ..transfer.clone()
            },
            "sent to the token",
        ),
        (
            RailgunTransact {
                value: 1,
                ..transfer.clone()
            },
            "carrying ETH",
        ),
        (
            RailgunTransact {
                chain_id: 1,
                ..transfer.clone()
            },
            "for another chain",
        ),
        (bytes(&|data| data.push(0)), "with a byte appended"),
        (
            bytes(&|data| data.resize(terms.max_calldata_bytes + 1, 0)),
            "padded past the calldata cap",
        ),
        (
            with(&|call| call._transactions = vec![call._transactions[0].clone(); 17]),
            "as seventeen transactions",
        ),
        (
            with(&|call| call._transactions[0].nullifiers.clear()),
            "spending no notes",
        ),
        (
            with(&|call| call._transactions[0].boundParams.chainID = 1),
            "proved for another chain",
        ),
        (
            with(&|call| call._transactions[0].boundParams.adaptContract = token),
            "bound to an adapt contract",
        ),
        (
            with(&|call| call._transactions[0].boundParams.unshield = 2),
            "redirecting its unshield",
        ),
        (
            with(&|call| call._transactions[0].boundParams.minGasPrice = cap),
            "asking more than the gas price cap",
        ),
        (
            with(&|call| {
                call._transactions[0].boundParams.commitmentCiphertext[0].ciphertext[1].0[0] ^= 1
            }),
            "its fee note altered",
        ),
        (
            with(&|call| call._transactions[0].proof.a.x += U256::from(1)),
            "its proof altered",
        ),
        (underpaid.clone(), "paying one unit less than the fee"),
    ])
}

async fn mined(chain: &Settlement, tx: B256) -> Result<()> {
    let deadline = Instant::now() + MINED_WITHIN;
    loop {
        match chain.transaction_status(tx).await? {
            Status::Mined { succeeded: true } => return Ok(()),
            Status::Mined { succeeded: false } => bail!("{tx} reverted"),
            Status::Pending | Status::Unknown => {}
        }
        ensure!(Instant::now() < deadline, "{tx} did not mine");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Runs `send.cjs` with `request` on stdin, and reads its JSON answer.
async fn sdk<T: DeserializeOwned>(env: &Env, command: &str, request: Option<Value>) -> Result<T> {
    let engine = env.workspace.join("crates/zecswap-railgun/engine");
    ensure!(
        engine.join("node_modules").exists(),
        "run `npm ci` in {}",
        engine.display()
    );
    let mut child = tokio::process::Command::new("node")
        .arg("send.cjs")
        .arg(command)
        .current_dir(&engine)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("running node")?;
    let mut stdin = child.stdin.take().expect("piped");
    if let Some(request) = request {
        stdin.write_all(request.to_string().as_bytes()).await?;
    }
    drop(stdin);
    let (mut out, mut err) = (child.stdout.take(), child.stderr.take());
    let stdout = tokio::spawn(async move {
        let mut bytes = Vec::new();
        out.as_mut()
            .expect("piped")
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    });
    let stderr = tokio::spawn(async move {
        let mut bytes = Vec::new();
        err.as_mut()
            .expect("piped")
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    });
    let status = tokio::time::timeout(SDK_WITHIN, child.wait()).await;
    if status.is_err() {
        child.kill().await?;
    }
    // Node's messages can name the RPC, whose URL can carry a key.
    let said = String::from_utf8_lossy(&stderr.await??)
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(|word| if word.contains("://") { "<url>" } else { word })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    match status {
        Ok(Ok(status)) if status.success() => Ok(serde_json::from_slice(&stdout.await??)?),
        Ok(Ok(_)) => bail!("send.cjs {command} failed:\n{said}"),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => bail!("send.cjs {command} took over {SDK_WITHIN:?}:\n{said}"),
    }
}

fn artifacts(env: &Env) -> String {
    env.workspace
        .join("target/railgun-artifacts")
        .display()
        .to_string()
}

fn seed_bytes(seed: &str) -> Result<Vec<u8>> {
    Ok(hex::decode(seed.trim_start_matches("0x"))?)
}
