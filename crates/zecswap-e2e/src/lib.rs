//! Live end-to-end runs of every swap outcome on an EVM testnet and the Zcash testnet.
//!
//! Each run deploys fresh contracts, starts an attentive and a silent maker and a relayer
//! in-process, and plays every scenario concurrently; see scripts/e2e-testnet.sh. Where
//! Railgun is deployed, the scenarios that pay into it run too.

mod env;
mod scenarios;

use std::time::Instant;

use anyhow::Result;
use tokio::task::JoinSet;

use crate::env::{Env, Settings};

/// Runs the named scenarios, or all of them, concurrently. Returns whether all passed.
pub async fn run(only: &[String]) -> Result<bool> {
    let Some(settings) = Settings::from_env()? else {
        println!("live e2e skipped: set ZECSWAP_E2E_FUNDER_KEY and ZECSWAP_E2E_WALLET");
        return Ok(true);
    };
    let names = scenarios::select(only, settings.has_railgun())?;
    let env = Env::setup(settings, scenarios::needs(&names)).await?;
    let sync = env.spawn_sync();
    let restart = env.spawn_restart();

    let mut runs = JoinSet::new();
    for name in names {
        let env = env.clone();
        runs.spawn(async move {
            let started = Instant::now();
            let result = scenarios::run(env.clone(), name).await;
            match &result {
                Ok(()) => env.log(name, "passed"),
                Err(e) => env.log(name, format!("FAILED: {e:#}")),
            }
            (name, result, started.elapsed())
        });
    }
    let mut results = Vec::new();
    while let Some(finished) = runs.join_next().await {
        results.push(finished?);
    }
    sync.abort();
    restart.abort();

    println!("\n{:<16} {:<7} {:>7}", "scenario", "result", "minutes");
    for (name, result, elapsed) in &results {
        let verdict = if result.is_ok() { "passed" } else { "FAILED" };
        println!(
            "{name:<16} {verdict:<7} {:>7.1}",
            elapsed.as_secs_f64() / 60.0
        );
    }
    for (name, result, _) in &results {
        if let Err(e) = result {
            println!("\n{name}: {e:#}");
        }
    }
    Ok(results.iter().all(|(_, result, _)| result.is_ok()))
}
