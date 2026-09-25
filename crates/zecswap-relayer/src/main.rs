use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser;
use tracing::info;
use zecswap_relayer::{Config, Relayer, api};
use zeroize::Zeroizing;

/// Serves the relayer API. Its key comes from `RELAYER_PRIVATE_KEY`, never a config file.
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
    let relayer = Arc::new(Relayer::new(config, key).await?);

    let listener = tokio::net::TcpListener::bind(relayer.listen()).await?;
    info!("relaying on {}", listener.local_addr()?);
    axum::serve(listener, api::router(relayer))
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}
