use std::fmt::Display;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
use rand_core::{OsRng, RngCore};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use zcash_address::ZcashAddress;
use zcash_keys::keys::UnifiedSpendingKey;
use zecswap_chain::evm::{Address, PrivateKeySigner, Settlement, U256, deploy};
use zecswap_chain::zcash::{AccountUuid, Lightwalletd, Network, Prover, Wallet, connect};
use zecswap_maker::policy::Timing;
use zecswap_maker::pricing::Pricing;
use zecswap_maker::{Chain, Config, Maker, Secrets};
use zecswap_relayer::Relayer;
use zeroize::Zeroizing;

/// Short enough to exercise lock expiry in one run, long enough to land any reveal.
const LOCK_DURATION: u64 = 10 * 60;
/// One unit of the test token costs about 202 000 zatoshis; the rest covers the fee.
pub(crate) const NOTE_ZAT: u64 = 300_000;
const INVENTORY: u128 = 1_000_000_000;
/// What the relayer keeps from each Railgun payout: 0.02 of the test token.
pub(crate) const RELAYER_FEE: u64 = 20_000;
/// Gas each account is funded for, at the chain's gas price and `GAS_MARGIN` times over. The
/// deployer's covers both contracts, the silent maker's its inventory and moves, a payout
/// account's a claim lock, claim and withdrawal, the relayer's a lock, claim and payout each.
const DEPLOY_GAS: u64 = 5_000_000;
const MAKER_GAS: u64 = 1_500_000;
const PAYOUT_GAS: u64 = 500_000;
const RELAYED_GAS: u64 = 1_500_000;
const GAS_MARGIN: u128 = 4;
const SYNC_INTERVAL: Duration = Duration::from_secs(15);
/// Testnet's target block interval, in seconds.
const BLOCK_TIME: u64 = 75;

pub(crate) struct Settings {
    evm_rpc: String,
    /// Railgun's proxy on that chain, which enables the Railgun scenarios.
    railgun: Option<Address>,
    /// Confirmations a note needs before it counts; the wallets' default, 10, unless set.
    confirmations: Option<NonZeroU32>,
    lightwalletd: String,
    funder: PrivateKeySigner,
    wallet_dir: PathBuf,
    work_dir: PathBuf,
    artifacts: PathBuf,
}

impl Settings {
    /// `None` unless the two required variables are set, so an unconfigured run skips.
    pub(crate) fn from_env() -> Result<Option<Self>> {
        let (Ok(funder), Ok(wallet_dir)) = (
            std::env::var("ZECSWAP_E2E_FUNDER_KEY"),
            std::env::var("ZECSWAP_E2E_WALLET"),
        ) else {
            return Ok(None);
        };
        // Tests run from the package directory, so relative paths are taken from the workspace.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/<name> sits two levels below the workspace");
        let work_dir = std::env::var("ZECSWAP_E2E_DIR")
            .map(|dir| workspace.join(dir))
            .unwrap_or_else(|_| {
                workspace
                    .join("target/zecswap-e2e")
                    .join(unix_now().to_string())
            });
        let railgun = std::env::var("ZECSWAP_E2E_RAILGUN")
            .ok()
            .filter(|address| !address.is_empty())
            .map(|address| address.parse())
            .transpose()
            .context("ZECSWAP_E2E_RAILGUN")?;
        let confirmations = std::env::var("ZECSWAP_E2E_CONFIRMATIONS")
            .ok()
            .filter(|count| !count.is_empty())
            .map(|count| count.parse())
            .transpose()
            .context("ZECSWAP_E2E_CONFIRMATIONS")?;
        Ok(Some(Self {
            evm_rpc: var_or("ZECSWAP_E2E_EVM_RPC", "https://sepolia.base.org"),
            railgun,
            confirmations,
            lightwalletd: var_or("ZECSWAP_E2E_LIGHTWALLETD", "https://testnet.zec.rocks:443"),
            funder: funder.parse().context("ZECSWAP_E2E_FUNDER_KEY")?,
            wallet_dir: workspace.join(wallet_dir),
            work_dir,
            artifacts: workspace.join("contracts/out"),
        }))
    }

    pub(crate) fn has_railgun(&self) -> bool {
        self.railgun.is_some()
    }

    /// The run's deadlines, sized to how long a deposit takes to confirm. With the wallets'
    /// 10 confirmations they are the client's and maker's defaults.
    fn pace(&self) -> Pace {
        let confirmations = self.confirmations.map_or(10, NonZeroU32::get);
        let confirm = u64::from(confirmations) * BLOCK_TIME;
        let t0_after = 10 * 60 + 2 * confirm;
        Pace {
            timing: Timing {
                quote_ttl: 300,
                t0_after,
                t1_after: t0_after + 5 * 60,
                cancel_after: 3 * 60,
                t0_margin: 5 * 60,
                reveal_margin: 2 * 60,
                tick: 15,
            },
            min_time_to_t0: t0_after - 10 * 60,
            restart_after: Duration::from_secs((confirm * 3 / 5).max(2 * 60)),
        }
    }
}

struct Pace {
    timing: Timing,
    /// The soonest `t0` the user accepts: time for a deposit to confirm.
    min_time_to_t0: u64,
    /// When the attentive maker restarts: after the deposits are in, before they confirm.
    restart_after: Duration,
}

/// What each run needs of its scenarios.
pub(crate) struct Needs {
    /// Scenarios paid to an account, each funded for its own transactions.
    pub(crate) accounts: usize,
    /// Scenarios paid into Railgun, whose transactions the relayer pays for.
    pub(crate) relayed: usize,
    pub(crate) deposits: usize,
}

/// What every scenario shares: one fresh deployment, an attentive and a silent maker, a
/// relayer, and a funded Zcash wallet whose treasury account pays the deposits.
pub(crate) struct Env {
    started: Instant,
    pub(crate) network: Network,
    pub(crate) evm_rpc: String,
    pub(crate) contract: Address,
    pub(crate) token: Address,
    pub(crate) relayer_url: String,
    pub(crate) min_time_to_t0: u64,
    restart_after: Duration,
    pub(crate) zcash: Mutex<Zcash>,
    pub(crate) prover: Prover,
    pub(crate) treasury: AccountUuid,
    pub(crate) treasury_key: UnifiedSpendingKey,
    pub(crate) seed: Vec<u8>,
    pub(crate) attentive: MakerNode,
    pub(crate) silent: MakerNode,
    payout_keys: std::sync::Mutex<Vec<PrivateKeySigner>>,
}

pub(crate) struct Zcash {
    pub(crate) wallet: Wallet,
    pub(crate) client: Lightwalletd,
}

impl Env {
    pub(crate) async fn setup(settings: Settings, needs: Needs) -> Result<Arc<Self>> {
        let started = Instant::now();
        let network = Network::TestNetwork;
        std::fs::create_dir_all(&settings.work_dir)?;
        log(
            started,
            "setup",
            format!("working in {}", settings.work_dir.display()),
        );

        let seed = hex::decode(
            std::fs::read_to_string(settings.wallet_dir.join("seed"))
                .context("reading the wallet seed")?
                .trim(),
        )?;
        let pace = settings.pace();
        let mut wallet = Wallet::open(settings.wallet_dir.join("wallet.sqlite"), network)?;
        if let Some(confirmations) = settings.confirmations {
            wallet = wallet.with_confirmations(confirmations);
        }
        let mut client = connect(&settings.lightwalletd).await?;
        wallet.sync(&mut client).await?;
        let treasury = wallet
            .derived_account()?
            .context("the wallet has no seed-derived account")?;
        let treasury_key = UnifiedSpendingKey::from_seed(&network, &seed, zip32::AccountId::ZERO)
            .map_err(|e| anyhow::anyhow!("deriving the treasury key: {e:?}"))?;
        let (contract, token, gas_price) = deploy_contracts(&settings, &needs).await?;
        let deposits = needs.deposits;
        log(
            started,
            "setup",
            format!("deployed ZecSwap {contract} and token {token}"),
        );
        let chain = Settlement::connect(&settings.evm_rpc, contract, settings.funder.clone())?;
        let fund = |gas: u64| U256::from(u128::from(gas) * gas_price * GAS_MARGIN);
        let silent_key = PrivateKeySigner::random();
        chain
            .send_eth(silent_key.address(), fund(MAKER_GAS))
            .await?;
        for maker in [settings.funder.address(), silent_key.address()] {
            chain.mint_test_token(token, maker, INVENTORY).await?;
        }
        chain.add_inventory(token, INVENTORY).await?;
        Settlement::connect(&settings.evm_rpc, contract, silent_key.clone())?
            .add_inventory(token, INVENTORY)
            .await?;
        let mut payout_keys = Vec::with_capacity(needs.accounts);
        for _ in 0..needs.accounts {
            let key = PrivateKeySigner::random();
            chain.send_eth(key.address(), fund(PAYOUT_GAS)).await?;
            payout_keys.push(key);
        }
        let relayer_key = PrivateKeySigner::random();
        if needs.relayed > 0 {
            chain
                .send_eth(
                    relayer_key.address(),
                    fund(RELAYED_GAS * needs.relayed as u64),
                )
                .await?;
        }
        let relayer_url = start_relayer(&settings, contract, relayer_key).await?;
        log(
            started,
            "setup",
            "funded both makers, the relayer and every payout account",
        );

        // Base first: it is quick and fails fast, while the split spends ZEC and waits.
        let prover = Prover::default();
        if deposits > 0 {
            split_treasury(
                started,
                &mut wallet,
                &mut client,
                &prover,
                treasury,
                &treasury_key,
                deposits,
            )
            .await?;
            log(
                started,
                "setup",
                format!("the treasury holds {deposits} deposit notes"),
            );
        }
        let sweep_to = wallet.fresh_address(treasury)?;

        let maker_config = |name: &str| Config {
            reverse: None,
            network: Chain::Testnet,
            lightwalletd: settings.lightwalletd.clone(),
            evm_rpc: settings.evm_rpc.clone(),
            contract,
            token,
            sweep_to: sweep_to.clone(),
            confirmations: settings.confirmations,
            data_dir: settings.work_dir.join(name),
            listen: "127.0.0.1:0".parse().expect("socket address"),
            pricing: pricing(),
            timing: pace.timing.clone(),
        };
        let attentive_config = maker_config("attentive-maker");
        let attentive_secrets = Secrets {
            zcash_seed: None,
            evm_key: settings.funder.clone(),
            root: maker_root(&attentive_config.data_dir)?,
        };
        let attentive = MakerNode::start(attentive_config, attentive_secrets, true).await?;
        let silent_config = maker_config("silent-maker");
        let silent_secrets = Secrets {
            zcash_seed: None,
            evm_key: silent_key,
            root: maker_root(&silent_config.data_dir)?,
        };
        let silent = MakerNode::start(silent_config, silent_secrets, true).await?;

        Ok(Arc::new(Self {
            started,
            network,
            evm_rpc: settings.evm_rpc,
            contract,
            token,
            relayer_url,
            min_time_to_t0: pace.min_time_to_t0,
            restart_after: pace.restart_after,
            zcash: Mutex::new(Zcash { wallet, client }),
            prover,
            treasury,
            treasury_key,
            seed,
            attentive,
            silent,
            payout_keys: std::sync::Mutex::new(payout_keys),
        }))
    }

    pub(crate) fn log(&self, who: &str, message: impl Display) {
        log(self.started, who, message);
    }

    pub(crate) fn payout_key(&self) -> PrivateKeySigner {
        self.payout_keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .expect("one payout account per scenario")
    }

    /// Keeps the shared user wallet at the chain tip while scenarios read it.
    pub(crate) fn spawn_sync(self: &Arc<Self>) -> JoinHandle<()> {
        let env = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SYNC_INTERVAL).await;
                let mut zcash = env.zcash.lock().await;
                let Zcash { wallet, client } = &mut *zcash;
                if let Err(e) = wallet.sync(client).await {
                    env.log("sync", format!("retrying after: {e}"));
                }
            }
        })
    }

    /// Restarts the attentive maker from its stored state partway through, which every
    /// in-flight swap then has to survive.
    pub(crate) fn spawn_restart(self: &Arc<Self>) -> JoinHandle<()> {
        let env = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(env.restart_after).await;
            env.log("setup", "restarting the attentive maker");
            if let Err(e) = env.attentive.restart().await {
                env.log("setup", format!("maker restart failed: {e:#}"));
            }
        })
    }
}

/// A maker running in-process: its API on a local port and a controllable watchtower.
pub(crate) struct MakerNode {
    config: Config,
    secrets: Secrets,
    running: Mutex<Running>,
}

struct Running {
    maker: Arc<Maker>,
    url: String,
    server: JoinHandle<()>,
    watchtower: Option<JoinHandle<()>>,
}

impl MakerNode {
    async fn start(config: Config, secrets: Secrets, watching: bool) -> Result<Self> {
        let running = Running::start(&config, &secrets, watching).await?;
        Ok(Self {
            config,
            secrets,
            running: Mutex::new(running),
        })
    }

    pub(crate) async fn url(&self) -> String {
        self.running.lock().await.url.clone()
    }

    pub(crate) async fn maker(&self) -> Arc<Maker> {
        self.running.lock().await.maker.clone()
    }

    /// Simulates a maker outage after it accepts a swap, before the deposit.
    pub(crate) async fn sleep(&self) {
        if let Some(watchtower) = self.running.lock().await.watchtower.take() {
            watchtower.abort();
            let _ = watchtower.await;
        }
    }

    /// Restarts the watchtower after a simulated outage.
    pub(crate) async fn wake(&self) {
        let mut running = self.running.lock().await;
        if running.watchtower.is_none() {
            running.watchtower = Some(tokio::spawn(running.maker.clone().run()));
        }
    }

    async fn restart(&self) -> Result<()> {
        let mut running = self.running.lock().await;
        let watching = running.watchtower.is_some();
        running.stop();
        *running = Running::start(&self.config, &self.secrets, watching).await?;
        Ok(())
    }
}

impl Running {
    async fn start(config: &Config, secrets: &Secrets, watching: bool) -> Result<Self> {
        let maker = Arc::new(Maker::new(config.clone(), secrets.clone()).await?);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let router = zecswap_maker::api::router(maker.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        let watchtower = watching.then(|| tokio::spawn(maker.clone().run()));
        let mut running = Self {
            maker,
            url,
            server,
            watchtower,
        };
        if watching {
            let ready = tokio::time::timeout(Duration::from_secs(120), async {
                while running.maker.check_watchtower().is_err() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
            if ready.is_err() {
                running.stop();
                anyhow::bail!("maker watchtower did not complete its initial pass");
            }
        }
        Ok(running)
    }

    fn stop(&mut self) {
        self.server.abort();
        if let Some(watchtower) = self.watchtower.take() {
            watchtower.abort();
        }
    }
}

/// Pays the treasury `count` notes of its own, so concurrent deposits never wait for each
/// other's change to confirm.
async fn split_treasury(
    started: Instant,
    wallet: &mut Wallet,
    client: &mut Lightwalletd,
    prover: &Prover,
    treasury: AccountUuid,
    key: &UnifiedSpendingKey,
    count: usize,
) -> Result<()> {
    let needed = NOTE_ZAT * count as u64 + 50_000;
    // ZEC from another wallet needs ten confirmations before it can be spent.
    loop {
        let funds = wallet.funds(treasury)?;
        ensure!(
            funds.total >= needed,
            "the treasury needs {needed} zatoshis and holds {}: fund {}",
            funds.total,
            wallet.address(treasury)?
        );
        if funds.spendable >= needed {
            break;
        }
        log(
            started,
            "setup",
            format!("waiting for the treasury's {} zat to confirm", funds.total),
        );
        tokio::time::sleep(SYNC_INTERVAL * 4).await;
        wallet.sync(client).await?;
    }
    let payments = (0..count)
        .map(|_| {
            Ok((
                wallet.fresh_address(treasury)?.parse::<ZcashAddress>()?,
                NOTE_ZAT,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let txid = wallet.pay(prover, treasury, key, &payments)?;
    wallet.broadcast(client, txid).await?;
    loop {
        tokio::time::sleep(SYNC_INTERVAL).await;
        wallet.sync(client).await?;
        let funds = wallet.funds(treasury)?;
        if wallet.is_mined(txid)? && funds.spendable == funds.total {
            return Ok(());
        }
    }
}

/// Deploys both contracts once the funder is known to cover the run; returns the gas price
/// the run's funding is priced at.
async fn deploy_contracts(settings: &Settings, needs: &Needs) -> Result<(Address, Address, u128)> {
    let funder = settings.funder.address();
    let chain = Settlement::connect(&settings.evm_rpc, Address::ZERO, settings.funder.clone())?;
    let gas_price = chain.gas_price().await?;
    let gas = DEPLOY_GAS
        + MAKER_GAS * 2
        + PAYOUT_GAS * needs.accounts as u64
        + RELAYED_GAS * needs.relayed as u64;
    let needed = U256::from(u128::from(gas) * gas_price * GAS_MARGIN);
    let balance = chain.eth_balance(funder).await?;
    ensure!(
        balance >= needed,
        "the funder {funder} holds {balance} wei and needs {needed} at {gas_price} wei per gas"
    );
    let token = deploy(
        &settings.evm_rpc,
        settings.funder.clone(),
        creation_code(&settings.artifacts, "TestToken")?,
    )
    .await?;
    let mut code = creation_code(&settings.artifacts, "ZecSwap")?;
    code.extend_from_slice(&U256::from(LOCK_DURATION).to_be_bytes::<32>());
    code.extend_from_slice(&settings.railgun.unwrap_or_default().into_word().0);
    let contract = deploy(&settings.evm_rpc, settings.funder.clone(), code).await?;
    Ok((contract, token, gas_price))
}

/// Runs a relayer in-process, with its own key: it must never be a maker.
async fn start_relayer(
    settings: &Settings,
    contract: Address,
    key: PrivateKeySigner,
) -> Result<String> {
    let config = zecswap_relayer::Config {
        evm_rpc: settings.evm_rpc.clone(),
        contract,
        listen: "127.0.0.1:0".parse().expect("socket address"),
        fee: RELAYER_FEE,
        claim_margin: 3 * 60,
    };
    let relayer = Arc::new(Relayer::new(config, key).await?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let router = zecswap_relayer::api::router(relayer);
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    Ok(url)
}

fn creation_code(artifacts: &Path, name: &str) -> Result<Vec<u8>> {
    let path = artifacts.join(format!("{name}.sol/{name}.json"));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading {} (run `forge build` first)", path.display()))?;
    let artifact: serde_json::Value = serde_json::from_slice(&bytes)?;
    let code = artifact["bytecode"]["object"]
        .as_str()
        .context("the artifact has no bytecode")?;
    Ok(hex::decode(code.trim_start_matches("0x"))?)
}

fn pricing() -> Pricing {
    Pricing {
        market: None,
        price_per_zec: 500_000_000,
        spread_bps: 100,
        unit: 1_000_000,
        max_units: 20,
    }
}

/// Kept beside the maker's data so an interrupted run's swaps can still be settled.
fn maker_root(dir: &Path) -> Result<Zeroizing<[u8; 32]>> {
    let path = dir.join("root");
    let mut root = Zeroizing::new([0; 32]);
    if let Ok(hex) = std::fs::read_to_string(&path) {
        hex::decode_to_slice(hex.trim(), &mut root[..])?;
        return Ok(root);
    }
    std::fs::create_dir_all(dir)?;
    OsRng.fill_bytes(&mut root[..]);
    std::fs::write(&path, hex::encode(&root[..]))?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(root)
}

fn log(started: Instant, who: &str, message: impl Display) {
    let minutes = started.elapsed().as_secs_f64() / 60.0;
    println!("{minutes:>6.1}m  {who:<16} {message}");
}

fn var_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}
