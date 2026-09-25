//! `cargo test -p zecswap-e2e --test live -- [scenario ...]`; see scripts/e2e-testnet.sh.

use std::process::ExitCode;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zecswap_maker=info,warn".into()),
        )
        .with_target(false)
        .init();
    let only: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    match runtime.block_on(zecswap_e2e::run(&only)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("setup failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}
