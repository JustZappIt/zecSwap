use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use tracing::info;
use zecswap_issuer::{Config, Issuer, router};
use zecswap_tokens::IssuerKey;

#[derive(Parser)]
#[command(about = "ZecSwap token issuer: signs each genuine device its day's tokens, blind")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serves the issuer API.
    Serve {
        #[arg(long, default_value = "issuer.toml")]
        config: PathBuf,
    },
    /// Writes a new signing key, readable by its owner alone, and prints its public half for
    /// the maker's `[tokens]` keys.
    Keygen { path: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Keygen { path } => {
            let key = IssuerKey::generate()?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("creating {}", path.display()))?
                .write_all(key.to_pem()?.as_bytes())?;
            println!("{}", key.token_key().to_base64());
        }
        Command::Serve { config } => {
            let issuer = Arc::new(Issuer::new(Config::load(&config)?)?);
            let listener = tokio::net::TcpListener::bind(issuer.listen()).await?;
            info!("issuing on {}", listener.local_addr()?);
            axum::serve(listener, router(issuer))
                .with_graceful_shutdown(async {
                    tokio::signal::ctrl_c().await.ok();
                })
                .await?;
        }
    }
    Ok(())
}
