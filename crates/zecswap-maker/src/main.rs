use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing::info;

use zecswap_maker::{Config, Maker, Secrets, api};

#[derive(Parser)]
#[command(about = "ZecSwap maker: quotes, opens swaps, watches deposits and sweeps claims")]
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
            let watchtower = tokio::spawn(maker.clone().run());
            let server = async move {
                axum::serve(listener, api::router(maker))
                    .with_graceful_shutdown(async {
                        tokio::signal::ctrl_c().await.ok();
                    })
                    .await
            };
            supervise(watchtower, server).await?;
        }
        Command::AddInventory { amount, mint } => {
            let settlement = maker.settlement();
            if mint {
                settlement
                    .mint_test_token(token, maker.account(), amount)
                    .await?;
            }
            settlement.add_inventory(token, amount).await?;
            let inventory = settlement.balance_of(maker.account(), token).await?;
            info!("inventory is now {inventory}");
        }
    }
    Ok(())
}

async fn supervise(
    mut watchtower: tokio::task::JoinHandle<()>,
    server: impl Future<Output = std::io::Result<()>>,
) -> Result<()> {
    tokio::select! {
        biased;
        result = &mut watchtower => {
            match result {
                Ok(()) => bail!("watchtower task ended unexpectedly"),
                Err(e) if e.is_panic() => bail!("watchtower task panicked"),
                Err(_) => bail!("watchtower task was cancelled"),
            }
        }
        result = server => {
            watchtower.abort();
            result.context("quote API stopped")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn watchtower_exit_stops_serving_with_an_error() {
        for panics in [false, true] {
            let watchtower = tokio::spawn(async move {
                assert!(!panics, "injected watchtower panic");
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                supervise(watchtower, std::future::pending()),
            )
            .await
            .expect("API kept running after watchtower exited");
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn stopping_server_aborts_watchtower() {
        let watchtower = tokio::spawn(std::future::pending());
        let abort = watchtower.abort_handle();
        supervise(watchtower, async { Ok(()) }).await.unwrap();
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }
}
