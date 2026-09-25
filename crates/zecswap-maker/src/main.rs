use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

use zecswap_maker::{Config, Maker, Secrets, api};

#[derive(Parser)]
#[command(about = "ZecSwap maker: quotes, opens swaps on Base, watches deposits and sweeps claims")]
struct Cli {
    #[arg(long, default_value = "maker.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serves quotes and runs the watchtower.
    Serve,
    /// Moves `amount` base units of the payout token into the contract inventory.
    AddInventory {
        amount: u128,
        /// Mint the amount first (test token only).
        #[arg(long)]
        mint: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,zcash_client_backend=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let token = config.token;
    let maker = Arc::new(Maker::new(config, Secrets::from_env()?).await?);

    match cli.command {
        Command::Serve => {
            let listener = tokio::net::TcpListener::bind(maker.listen()).await?;
            info!("serving quotes on {}", listener.local_addr()?);
            tokio::spawn(maker.clone().run());
            axum::serve(listener, api::router(maker))
                .with_graceful_shutdown(async {
                    tokio::signal::ctrl_c().await.ok();
                })
                .await?;
        }
        Command::AddInventory { amount, mint } => {
            let settlement = maker.settlement();
            if mint {
                settlement
                    .mint_test_token(token, settlement.account(), amount)
                    .await?;
            }
            settlement.add_inventory(token, amount).await?;
            let inventory = settlement.balance_of(settlement.account(), token).await?;
            info!("inventory is now {inventory}");
        }
    }
    Ok(())
}
