use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use clap::Parser;
use tracing::info;
use zecswap_railgun::Keys;
use zecswap_relayer::{Config, Relayer, api};
use zeroize::Zeroizing;

/// Serves the relayer API. Its key comes from `RELAYER_PRIVATE_KEY`, and with `[railgun_sends]`
/// its Railgun wallet's 64-byte BIP-39 seed from `RELAYER_RAILGUN_SEED`, in hex: never a config
/// file, and never the maker's.
#[derive(Parser)]
#[command(about = "ZecSwap relayer: sends the transactions of users with no account on the chain")]
struct Cli {
    #[arg(long, default_value = "relayer.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let key = Zeroizing::new(
        std::env::var("RELAYER_PRIVATE_KEY").context("RELAYER_PRIVATE_KEY is not set")?,
    )
    .parse()
    .context("RELAYER_PRIVATE_KEY")?;
    let railgun = match config.railgun_sends {
        Some(_) => Some(railgun_keys()?),
        None => None,
    };
    let relayer = Arc::new(Relayer::new(config, key, railgun).await?);
    tokio::spawn(relayer.clone().run_costs());

    let listener = tokio::net::TcpListener::bind(relayer.listen()).await?;
    info!("relaying on {}", listener.local_addr()?);
    axum::serve(listener, api::router(relayer))
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}

fn railgun_keys() -> Result<Keys> {
    let seed = Zeroizing::new(
        std::env::var("RELAYER_RAILGUN_SEED").context("RELAYER_RAILGUN_SEED is not set")?,
    );
    let seed = Zeroizing::new(
        alloy_primitives::hex::decode(seed.trim()).context("RELAYER_RAILGUN_SEED is not hex")?,
    );
    ensure!(
        seed.len() == 64,
        "RELAYER_RAILGUN_SEED must be a 64-byte BIP-39 seed"
    );
    Ok(Keys::from_seed(&seed, 0))
}
