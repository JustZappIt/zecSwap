use std::fmt::Display;
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
use zecswap_chain::base::{Address, PrivateKeySigner, Settlement, U256, deploy};
use zecswap_chain::zcash::{AccountUuid, Lightwalletd, Network, Prover, Wallet, connect};
use zecswap_maker::policy::Timing;
use zecswap_maker::pricing::Pricing;
use zecswap_maker::{Chain, Config, Maker, Secrets};
use zeroize::Zeroizing;

/// Short enough to exercise lock expiry in one run, long enough to land any reveal.
const LOCK_DURATION: u64 = 10 * 60;
/// One unit of the test token costs about 202 000 zatoshis; the rest covers the fee.
pub(crate) const NOTE_ZAT: u64 = 300_000;
const INVENTORY: u128 = 1_000_000_000;
const MAKER_GAS_WEI: u64 = 2_000_000_000_000_000;
const PAYOUT_GAS_WEI: u64 = 500_000_000_000_000;
const SYNC_INTERVAL: Duration = Duration::from_secs(15);

pub(crate) struct Settings {
    base_rpc: String,
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
        Ok(Some(Self {
            base_rpc: var_or("ZECSWAP_E2E_BASE_RPC", "https://sepolia.base.org"),
            lightwalletd: var_or("ZECSWAP_E2E_LIGHTWALLETD", "https://testnet.zec.rocks:443"),
            funder: funder.parse().context("ZECSWAP_E2E_FUNDER_KEY")?,
            wallet_dir: workspace.join(wallet_dir),
            work_dir,
            artifacts: workspace.join("contracts/out"),
        }))
    }
}

/// What every scenario shares: one fresh deployment, an attentive and a silent maker, and
/// a funded Zcash wallet whose treasury account pays the deposits.
pub(crate) struct Env {
    started: Instant,
    pub(crate) network: Network,
    pub(crate) base_rpc: String,
    pub(crate) contract: Address,
    pub(crate) token: Address,
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
    pub(crate) async fn setup(
        settings: Settings,
        players: usize,
        deposits: usize,
    ) -> Result<Arc<Self>> {
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
        let mut wallet = Wallet::open(settings.wallet_dir.join("wallet.sqlite"), network)?;
        let mut client = connect(&settings.lightwalletd).await?;
        wallet.sync(&mut client).await?;
        let treasury = wallet
            .derived_account()?
            .context("the wallet has no seed-derived account")?;
        let treasury_key = UnifiedSpendingKey::from_seed(&network, &seed, zip32::AccountId::ZERO)
            .map_err(|e| anyhow::anyhow!("deriving the treasury key: {e:?}"))?;
        let (contract, token) = deploy_contracts(&settings, players).await?;
        log(
            started,
            "setup",
            format!("deployed ZecSwap {contract} and token {token}"),
        );
        let chain = Settlement::connect(&settings.base_rpc, contract, settings.funder.clone())?;
        let silent_key = PrivateKeySigner::random();
        chain
            .send_eth(silent_key.address(), U256::from(MAKER_GAS_WEI))
            .await?;
        for maker in [chain.account(), silent_key.address()] {
            chain.mint_test_token(token, maker, INVENTORY).await?;
        }
        chain.add_inventory(token, INVENTORY).await?;
        Settlement::connect(&settings.base_rpc, contract, silent_key.clone())?
            .add_inventory(token, INVENTORY)
            .await?;
        let mut payout_keys = Vec::with_capacity(players);
        for _ in 0..players {
            let key = PrivateKeySigner::random();
            chain
                .send_eth(key.address(), U256::from(PAYOUT_GAS_WEI))
                .await?;
            payout_keys.push(key);
        }
        log(
            started,
            "setup",
            "funded both makers and every payout account",
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
            network: Chain::Testnet,
            lightwalletd: settings.lightwalletd.clone(),
            base_rpc: settings.base_rpc.clone(),
            contract,
            token,
            sweep_to: sweep_to.clone(),
            data_dir: settings.work_dir.join(name),
            listen: "127.0.0.1:0".parse().expect("socket address"),
            pricing: pricing(),
            timing: timing(),
        };
        let attentive_config = maker_config("attentive-maker");
        let attentive_secrets = Secrets {
            base_key: settings.funder.clone(),
            root: maker_root(&attentive_config.data_dir)?,
        };
        let attentive = MakerNode::start(attentive_config, attentive_secrets, true).await?;
        let silent_config = maker_config("silent-maker");
        let silent_secrets = Secrets {
            base_key: silent_key,
            root: maker_root(&silent_config.data_dir)?,
        };
        let silent = MakerNode::start(silent_config, silent_secrets, false).await?;

        Ok(Arc::new(Self {
            started,
            network,
            base_rpc: settings.base_rpc,
            contract,
            token,
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
    pub(crate) fn spawn_restart(self: &Arc<Self>, after: Duration) -> JoinHandle<()> {
        let env = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            env.log("setup", "restarting the attentive maker");
            if let Err(e) = env.attentive.restart().await {
                env.log("setup", format!("maker restart failed: {e:#}"));
            }
        })
    }
}

/// A maker running in-process: its API on a local port and, unless silent, its watchtower.
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

    /// Starts the watchtower of a maker that has been silent so far.
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
        Ok(Self {
            maker,
            url,
            server,
            watchtower,
        })
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

async fn deploy_contracts(settings: &Settings, players: usize) -> Result<(Address, Address)> {
    // What setup sends to the other accounts, plus a margin for its own gas.
    let needed = U256::from(MAKER_GAS_WEI + PAYOUT_GAS_WEI * players as u64) * U256::from(2);
    let funder = settings.funder.address();
    let balance = Settlement::connect(&settings.base_rpc, Address::ZERO, settings.funder.clone())?
        .eth_balance(funder)
        .await?;
    ensure!(
        balance >= needed,
        "the funder {funder} holds {balance} wei on Base and needs {needed}"
    );
    let token = deploy(
        &settings.base_rpc,
        settings.funder.clone(),
        creation_code(&settings.artifacts, "TestToken")?,
    )
    .await?;
    let mut code = creation_code(&settings.artifacts, "ZecSwap")?;
    code.extend_from_slice(&U256::from(LOCK_DURATION).to_be_bytes::<32>());
    let contract = deploy(&settings.base_rpc, settings.funder.clone(), code).await?;
    Ok((contract, token))
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
        price_per_zec: 500_000_000,
        spread_bps: 100,
        unit: 1_000_000,
        max_units: 20,
    }
}

/// `t0` leaves room for ten confirmations of a deposit; `t1` follows soon after so the
/// refund scenarios finish within the hour.
fn timing() -> Timing {
    Timing {
        quote_ttl: 300,
        t0_after: 35 * 60,
        t1_after: 40 * 60,
        cancel_after: 3 * 60,
        t0_margin: 5 * 60,
        reveal_margin: 2 * 60,
        tick: 15,
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
