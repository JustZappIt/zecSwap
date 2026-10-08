mod store;
mod swap;

use std::path::PathBuf;
use std::time::Duration;

use alloy_primitives::Address;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use zcash_address::ZcashAddress;
use zcash_keys::keys::UnifiedSpendingKey;
use zecswap_chain::zcash::{AccountUuid, Lightwalletd, Network, Prover, TxId, Wallet, connect};

use crate::store::Store;

const POLL_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Parser)]
#[command(about = "Test client for ZecSwap: a seed wallet plus the user side of a swap")]
struct Cli {
    #[arg(long, value_enum, default_value_t = Chain::Testnet, global = true)]
    network: Chain,
    #[arg(
        long,
        env = "LIGHTWALLETD_URL",
        default_value = "https://testnet.zec.rocks:443",
        global = true
    )]
    lightwalletd: String,
    #[arg(
        long,
        env = "ZECSWAP_CLI_DIR",
        default_value = "zecswap-cli-data",
        global = true
    )]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Chain {
    Mainnet,
    Testnet,
}

#[derive(Subcommand)]
enum Command {
    /// Creates the wallet with a fresh seed and one account born at the chain tip.
    Init,
    /// Syncs, then prints the account's address and balance.
    Status,
    /// Pays `zatoshis` to `address`.
    Send { address: String, zatoshis: u64 },
    /// Runs the user side of a swap against a maker, resuming an unfinished one. It pays the
    /// account whose key is in `USER_PRIVATE_KEY`, or with `--relayer`, this seed's Railgun
    /// wallet.
    Swap {
        #[arg(
            long,
            env = "ZECSWAP_MAKER_URL",
            default_value = "http://127.0.0.1:8787"
        )]
        maker: String,
        /// The settlement chain's RPC.
        #[arg(long, env = "EVM_RPC_URL")]
        rpc: String,
        #[arg(long, env = "ZECSWAP_CONTRACT")]
        contract: Address,
        #[arg(long, env = "ZECSWAP_TOKEN")]
        token: Address,
        #[arg(long, default_value_t = 1)]
        units: u32,
        /// Pay into Railgun, with this relayer sending the transactions.
        #[arg(long, env = "ZECSWAP_RELAYER_URL")]
        relayer: Option<String>,
        /// The most the relayer may keep, in token base units.
        #[arg(long, default_value_t = 2_000_000)]
        max_fee: u128,
        /// Spend Privacy Pass tokens from this issuer where the maker takes them.
        #[arg(long, env = "ZECSWAP_ISSUER_URL")]
        issuer: Option<String>,
        /// This device's attestation for the issuer: its id, in the issuer's insecure-test mode.
        #[arg(long, env = "ZECSWAP_DEVICE", default_value = "zecswap-cli")]
        device: String,
    },
}

pub(crate) struct Session {
    pub(crate) store: Store,
    pub(crate) wallet: Wallet,
    pub(crate) client: Lightwalletd,
    pub(crate) prover: Prover,
}

impl Session {
    pub(crate) async fn sync(&mut self) -> Result<()> {
        self.wallet.sync(&mut self.client).await.context("syncing")
    }

    /// Syncs every `POLL_INTERVAL` until `done` holds.
    pub(crate) async fn sync_until(
        &mut self,
        mut done: impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<()> {
        loop {
            self.sync().await?;
            if done(self)? {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    pub(crate) fn account(&self) -> Result<AccountUuid> {
        self.wallet
            .derived_account()?
            .context("no wallet yet; run `init`")
    }

    /// Stores a payment from the wallet's account, which `init` creates at ZIP 32 index 0.
    pub(crate) fn pay(&mut self, to: &ZcashAddress, zatoshis: u64) -> Result<TxId> {
        let network = self.wallet.network();
        let usk =
            UnifiedSpendingKey::from_seed(&network, &self.store.seed()?, zip32::AccountId::ZERO)
                .map_err(|e| anyhow::anyhow!("deriving the spending key: {e:?}"))?;
        let account = self.account()?;
        let payments = [(to.clone(), zatoshis)];
        Ok(self.wallet.pay(&self.prover, account, &usk, &payments)?)
    }

    pub(crate) async fn broadcast(&mut self, txid: TxId) -> Result<()> {
        Ok(self.wallet.broadcast(&mut self.client, txid).await?)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let network = match cli.network {
        Chain::Mainnet => Network::MainNetwork,
        Chain::Testnet => Network::TestNetwork,
    };
    std::fs::create_dir_all(&cli.data_dir)?;
    let mut ctx = Session {
        store: Store::new(&cli.data_dir),
        wallet: Wallet::open(cli.data_dir.join("wallet.sqlite"), network)?,
        client: connect(&cli.lightwalletd)
            .await
            .context("connecting to lightwalletd")?,
        prover: Prover::default(),
    };

    match cli.command {
        Command::Init => init(&mut ctx).await,
        Command::Status => status(&mut ctx).await,
        Command::Send { address, zatoshis } => send(&mut ctx, &address, zatoshis).await,
        Command::Swap {
            maker,
            rpc,
            contract,
            token,
            units,
            relayer,
            max_fee,
            issuer,
            device,
        } => {
            let payee = match relayer {
                Some(relayer) => swap::Payee::Railgun { relayer, max_fee },
                None => swap::Payee::Account(
                    std::env::var("USER_PRIVATE_KEY")
                        .context("USER_PRIVATE_KEY is not set")?
                        .parse()
                        .context("USER_PRIVATE_KEY")?,
                ),
            };
            let tokens = match issuer {
                Some(issuer) => {
                    // A test tool takes the return key the maker publishes; an app pins it.
                    let info = zecswap_client::MakerApi::new(maker.clone())?.info().await?;
                    let key = info.token_return_key.context("the maker takes no tokens")?;
                    let device = zecswap_client::Unattested(device.into_bytes());
                    let tokens = zecswap_client::Tokens::new(issuer, device, 5, &key)?;
                    Some(std::sync::Arc::new(tokens))
                }
                None => None,
            };
            let args = swap::SwapArgs {
                maker,
                rpc,
                contract,
                token,
                units,
                payee,
                tokens,
            };
            swap::run(&mut ctx, args).await
        }
    }
}

async fn init(ctx: &mut Session) -> Result<()> {
    let seed = ctx.store.create_seed()?;
    let (account, _) = ctx
        .wallet
        .create_account(&mut ctx.client, &seed, "zecswap-cli")
        .await?;
    println!("address: {}", ctx.wallet.address(account)?);
    Ok(())
}

async fn status(ctx: &mut Session) -> Result<()> {
    ctx.sync().await?;
    let account = ctx.account()?;
    let funds = ctx.wallet.funds(account)?;
    println!("address:   {}", ctx.wallet.address(account)?);
    println!("total:     {} zat", funds.total);
    println!("spendable: {} zat", funds.spendable);
    Ok(())
}

async fn send(ctx: &mut Session, address: &str, zatoshis: u64) -> Result<()> {
    let to: ZcashAddress = address.parse().context("parsing the recipient")?;
    ctx.sync().await?;
    let txid = ctx.pay(&to, zatoshis)?;
    ctx.broadcast(txid).await?;
    println!("sent: {txid}");
    Ok(())
}
