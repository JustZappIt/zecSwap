//! The user side of a swap, resumable: progress is saved after every step.

use std::time::Duration;

use alloy_primitives::Address;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use zcash_address::ZcashAddress;
use zcash_protocol::consensus::Parameters;
use zecswap_chain::base::{PrivateKeySigner, Settlement, Stage};
use zecswap_chain::zcash::{AccountUuid, TxId};
use zecswap_client::{MakerApi, User, UserSwap};
use zecswap_core::JointAccount;

use crate::Session;

const STATE: &str = "swap.json";
const NEXT_INDEX: &str = "next-swap-index.json";
const CHAIN_POLL: Duration = Duration::from_secs(15);

pub(crate) struct SwapArgs {
    pub(crate) maker: String,
    pub(crate) base_rpc: String,
    pub(crate) contract: Address,
    pub(crate) token: Address,
    pub(crate) units: u32,
    pub(crate) payout_key: PrivateKeySigner,
}

#[derive(Serialize, Deserialize)]
struct Run {
    swap: UserSwap,
    joint_account: Option<AccountUuid>,
    deposit: Option<[u8; 32]>,
    finished: bool,
}

pub(crate) async fn run(ctx: &mut Session, args: SwapArgs) -> Result<()> {
    let settlement = Settlement::connect(&args.base_rpc, args.contract, args.payout_key)?;
    let network = ctx.wallet.network().network_type();
    let maker = MakerApi::new(args.maker)?;
    let user = User::new(&ctx.store.seed()?, network, settlement, maker, args.token);

    let mut run = match ctx.store.load::<Run>(STATE)? {
        Some(mut run) if !run.finished => {
            println!("resuming swap #{}", run.swap.index);
            if run.deposit.is_none() && user.state(&run.swap).await?.stage == Stage::Refunded {
                println!("the maker cancelled before anything was deposited");
                return finish(ctx, &mut run);
            }
            run
        }
        _ => {
            // The index is spent the moment its share leaves the device.
            let index = ctx.store.load::<u32>(NEXT_INDEX)?.unwrap_or(0);
            ctx.store.save(NEXT_INDEX, &(index + 1))?;
            let swap = user.open(index, args.units).await?;
            println!("swap #{index} opened: {}", swap.swap_id);
            let run = Run {
                swap,
                joint_account: None,
                deposit: None,
                finished: false,
            };
            ctx.store.save(STATE, &run)?;
            run
        }
    };

    match run.deposit.map(TxId::from_bytes) {
        None => {
            let joint = user.verify(&run.swap).await?;
            deposit(ctx, &mut run, &joint).await?;
        }
        Some(txid) if !ctx.wallet.is_mined(txid)? => {
            if let Err(e) = ctx.broadcast(txid).await {
                println!("  rebroadcasting the deposit: {e:#}");
            }
        }
        Some(_) => {}
    }

    loop {
        let chain = user.state(&run.swap).await?;
        let now = user.settlement().now().await?;
        match chain.stage {
            Stage::Ready | Stage::Claimed => return claim(ctx, &user, &mut run).await,
            Stage::Open if now >= chain.t0 => return claim(ctx, &user, &mut run).await,
            Stage::Refunded => return sweep_back(ctx, &user, &mut run).await,
            Stage::Open => println!("  waiting for the maker to confirm the deposit"),
        }
        tokio::time::sleep(CHAIN_POLL).await;
    }
}

async fn deposit(ctx: &mut Session, run: &mut Run, joint: &JointAccount) -> Result<()> {
    if run.joint_account.is_none() {
        let account = ctx
            .wallet
            .import_joint(&mut ctx.client, joint, "swap")
            .await?;
        run.joint_account = Some(account);
        ctx.store.save(STATE, run)?;
    }
    ctx.sync().await?;
    let to: ZcashAddress = joint
        .unified_address(ctx.wallet.network().network_type())
        .parse()?;
    let txid = ctx.pay(&to, run.swap.quote.deposit_zat)?;
    // Saved before broadcasting, so a resumed run never pays twice.
    run.deposit = Some(*txid.as_ref());
    ctx.store.save(STATE, run)?;
    ctx.broadcast(txid).await?;
    println!(
        "deposited {} zat to {to}\n  tx {txid}",
        run.swap.quote.deposit_zat
    );
    Ok(())
}

async fn claim(ctx: &mut Session, user: &User, run: &mut Run) -> Result<()> {
    let amount = user.claim(&run.swap).await?;
    println!("claimed; withdrew {amount}");
    finish(ctx, run)
}

/// The maker refunded and revealed its share, so the deposit is ours to take back.
async fn sweep_back(ctx: &mut Session, user: &User, run: &mut Run) -> Result<()> {
    let (Some(account), Some(deposit)) = (run.joint_account, run.deposit.map(TxId::from_bytes))
    else {
        println!("the maker cancelled before anything was deposited");
        return finish(ctx, run);
    };
    let key = user.refund_key(&run.swap).await?;
    println!("the maker refunded; waiting for the deposit to confirm");
    ctx.sync_until(|ctx| {
        let funds = ctx.wallet.funds(account)?;
        Ok(ctx.wallet.is_mined(deposit)? && funds.spendable == funds.total)
    })
    .await?;
    // Nothing left means an earlier run already swept it.
    if ctx.wallet.funds(account)?.total > 0 {
        let home: ZcashAddress = ctx.wallet.address(ctx.account()?)?.parse()?;
        let txid = ctx.wallet.sweep(&ctx.prover, account, &key, &home)?;
        ctx.broadcast(txid).await?;
        println!("swept the deposit back in {txid}; waiting for it to be mined");
        ctx.sync_until(|ctx| Ok(ctx.wallet.is_mined(txid)?)).await?;
    }
    finish(ctx, run)
}

fn finish(ctx: &mut Session, run: &mut Run) -> Result<()> {
    run.finished = true;
    ctx.store.save(STATE, run)?;
    if let Some(account) = run.joint_account {
        ctx.wallet.forget(account)?;
    }
    Ok(())
}
