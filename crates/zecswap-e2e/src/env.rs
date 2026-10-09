use std::fmt::Display;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
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
/// What a private send or withdrawal pays the relayer, in a fee note: 0.25 of the test token.
pub(crate) const SEND_FEE: u64 = 250_000;
/// The highest gas price the relayer pays for one, which the proofs' minimum may not exceed.
pub(crate) const SEND_MAX_GAS_PRICE: u64 = 200_000_000_000;
/// Gas each account is funded for, at the chain's gas price and `GAS_MARGIN` times over. The
/// deployer's covers both contracts, the silent maker's its inventory and moves, a payout
/// account's a claim lock, claim and withdrawal, the relayer's a lock, claim and payout each.
const DEPLOY_GAS: u64 = 5_000_000;
const MAKER_GAS: u64 = 1_500_000;
const PAYOUT_GAS: u64 = 500_000;
const RELAYED_GAS: u64 = 1_500_000;
/// The most gas the relayer pays for a private send, which takes about 1.1M on a fork and 1.35M
/// on Sepolia.
const SEND_GAS_LIMIT: u64 = 3_000_000;
/// The private sends of one scenario, two Railgun transactions.
const SEND_GAS: u64 = 2 * SEND_GAS_LIMIT;
/// Approving Railgun and shielding one note, about 3.8M gas on Sepolia.
const SHIELD_GAS: u64 = 8_000_000;
/// Each note a send scenario shields into its sender's Railgun wallet: twenty of the test token,
/// room for a send and a fee priced by its gas.
pub(crate) const SHIELDED: u128 = 20_000_000;
/// The notes a send scenario shields: one for the private send, one for the withdrawal.
const SHIELDS: u64 = 2;
const GAS_MARGIN: u128 = 4;
const SYNC_INTERVAL: Duration = Duration::from_secs(15);
/// Testnet's target block interval, in seconds.
const BLOCK_TIME: u64 = 75;

pub(crate) struct Settings {
    evm_rpc: String,
    /// Railgun's proxy on that chain, which enables the Railgun scenarios.
    railgun: Option<Address>,
    /// Where Railgun's wallet SDK reads the chain: it scans logs, so not a node that caps their
    /// range. The fork itself, or a public Sepolia node.
    railgun_rpc: String,
    /// Confirmations a note needs before it counts; the wallets' default, 10, unless set.
    confirmations: Option<NonZeroU32>,
    lightwalletd: String,
    funder: PrivateKeySigner,
    wallet_dir: PathBuf,
    work_dir: PathBuf,
    artifacts: PathBuf,
    workspace: PathBuf,
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
        let evm_rpc = var_or("ZECSWAP_E2E_EVM_RPC", "https://sepolia.base.org");
        Ok(Some(Self {
            railgun_rpc: var_or("ZECSWAP_E2E_RAILGUN_RPC", &evm_rpc),
            evm_rpc,
            railgun,
            confirmations,
            lightwalletd: var_or("ZECSWAP_E2E_LIGHTWALLETD", "https://testnet.zec.rocks:443"),
            funder: funder.parse().context("ZECSWAP_E2E_FUNDER_KEY")?,
            wallet_dir: workspace.join(wallet_dir),
            work_dir,
            artifacts: workspace.join("contracts/out"),
            workspace: workspace.to_path_buf(),
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
                // One wallet pays every deposit, so the last leaves a minute after its open,
                // and a testnet block can take minutes: room for both before calling a swap
                // unpaid.
                cancel_after: 8 * 60,
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
    /// Private Railgun sends the relayer sends as their broadcaster.
    pub(crate) sends: usize,
    pub(crate) deposits: usize,
}

/// What every scenario shares: one fresh deployment, an attentive and a silent maker, a
/// relayer, and a funded Zcash wallet whose treasury account pays the deposits.
pub(crate) struct Env {
    started: Instant,
    pub(crate) network: Network,
    pub(crate) evm_rpc: String,
    pub(crate) railgun_rpc: String,
    pub(crate) contract: Address,
    pub(crate) token: Address,
    pub(crate) relayer_url: String,
    /// A relayer of its own for the private sends, so what it sends is theirs alone.
    pub(crate) sends: Option<SendsNode>,
    pub(crate) workspace: PathBuf,
    pub(crate) work_dir: PathBuf,
    /// The token issuer, which gives each device one accept a day, and the key the makers hand
    /// tokens back under.
    issuer_url: String,
    return_key: String,
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

pub(crate) struct SendsNode {
    pub(crate) relayer_url: String,
    /// That relayer itself, whose ledger records what each send cost and earned.
    pub(crate) relayer: Arc<Relayer>,
    /// The same relayer, key and journal, on an RPC that swallows its first broadcast.
    pub(crate) lossy_relayer_url: String,
    /// The same relayer and key on an empty journal, as after losing it.
    pub(crate) forgetful_relayer_url: String,
    /// The seed of the relayer's own Railgun wallet, which the fee notes pay.
    pub(crate) railgun_seed: [u8; 64],
    /// A public account holding the token to shield, `SHIELDED` a note, and gas.
    pub(crate) shielder: PrivateKeySigner,
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
        let (issuer_url, return_key, gate) = start_issuer(&settings.work_dir).await?;
        let (relayer_url, _) = start_relayer(
            &settings,
            &settings.evm_rpc,
            contract,
            token,
            relayer_key,
            None,
        )
        .await?;
        let sends = match needs.sends {
            0 => None,
            count => {
                let (key, shielder) = (PrivateKeySigner::random(), PrivateKeySigner::random());
                chain
                    .send_eth(key.address(), fund(SEND_GAS * count as u64))
                    .await?;
                let shields = SHIELDS * count as u64;
                chain
                    .send_eth(shielder.address(), fund(SHIELD_GAS * shields))
                    .await?;
                chain
                    .mint_test_token(token, shielder.address(), SHIELDED * u128::from(shields))
                    .await?;
                let mut railgun_seed = [0; 64];
                UnwrapErr(SysRng).fill_bytes(&mut railgun_seed);
                let journal = Some((&railgun_seed, "relayer-sends.sqlite"));
                let (rpc, lossy_rpc) = (&settings.evm_rpc, lossy_rpc(settings.evm_rpc.clone()));
                let (relayer_url, relayer) =
                    start_relayer(&settings, rpc, contract, token, key.clone(), journal).await?;
                let (lossy_relayer_url, _) = start_relayer(
                    &settings,
                    &lossy_rpc.await?,
                    contract,
                    token,
                    key.clone(),
                    journal,
                )
                .await?;
                let forgetful = Some((&railgun_seed, "relayer-sends-forgotten.sqlite"));
                let (forgetful_relayer_url, _) =
                    start_relayer(&settings, rpc, contract, token, key, forgetful).await?;
                Some(SendsNode {
                    relayer_url,
                    relayer,
                    lossy_relayer_url,
                    forgetful_relayer_url,
                    railgun_seed,
                    shielder,
                })
            }
        };
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
            gas_alerts: None,
            max_awaiting_deposit: None,
            tokens: Some(gate(
                "maker",
                settings
                    .work_dir
                    .join(format!("{name}-spent-tokens.sqlite")),
            )),
            network: Chain::Testnet,
            lightwalletd: settings.lightwalletd.clone(),
            evm_rpc: settings.evm_rpc.clone(),
            contract,
            token,
            sweep_to: sweep_to.clone(),
            confirmations: settings.confirmations,
            evm_confirmations: std::num::NonZeroU32::new(2).unwrap(),
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
            railgun_rpc: settings.railgun_rpc,
            contract,
            token,
            relayer_url,
            sends,
            workspace: settings.workspace,
            work_dir: settings.work_dir.clone(),
            issuer_url,
            return_key,
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

    /// What `device` pays for its accepts with.
    pub(crate) fn tokens(&self, device: &str) -> Result<Arc<zecswap_client::Tokens>> {
        let device = zecswap_client::Unattested(device.as_bytes().to_vec());
        let tokens = zecswap_client::Tokens::new(&self.issuer_url, device, 1, &self.return_key)?;
        Ok(Arc::new(tokens))
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
    address: SocketAddr,
    server: JoinHandle<()>,
    shutdown: Option<oneshot::Sender<()>>,
    watchtower: Option<JoinHandle<()>>,
}

impl MakerNode {
    async fn start(config: Config, secrets: Secrets, watching: bool) -> Result<Self> {
        let any_port = SocketAddr::from(([127, 0, 0, 1], 0));
        let running = Running::start(&config, &secrets, watching, any_port).await?;
        Ok(Self {
            config,
            secrets,
            running: Mutex::new(running),
        })
    }

    pub(crate) async fn url(&self) -> String {
        format!("http://{}", self.running.lock().await.address)
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

    /// On the same address, which players keep reaching the maker at.
    async fn restart(&self) -> Result<()> {
        let mut running = self.running.lock().await;
        let watching = running.watchtower.is_some();
        running.stop().await;
        let address = running.address;
        *running = Running::start(&self.config, &self.secrets, watching, address).await?;
        Ok(())
    }
}

impl Running {
    async fn start(
        config: &Config,
        secrets: &Secrets,
        watching: bool,
        address: SocketAddr,
    ) -> Result<Self> {
        let maker = Arc::new(Maker::new(config.clone(), secrets.clone()).await?);
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let router = zecswap_maker::api::router(maker.clone());
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    stopped.await.ok();
                })
                .await
                .ok();
        });
        let watchtower = watching.then(|| tokio::spawn(maker.clone().run()));
        let mut running = Self {
            maker,
            address,
            server,
            shutdown: Some(shutdown),
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
                running.stop().await;
                anyhow::bail!("maker watchtower did not complete its initial pass");
            }
        }
        Ok(running)
    }

    /// Stops the watchtower and the API, closing its idle connections, so that no request
    /// reaches this maker once another replaces it.
    async fn stop(&mut self) {
        if let Some(watchtower) = self.watchtower.take() {
            watchtower.abort();
        }
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).ok();
        }
        if tokio::time::timeout(Duration::from_secs(10), &mut self.server)
            .await
            .is_err()
        {
            self.server.abort();
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
        + RELAYED_GAS * needs.relayed as u64
        + (SEND_GAS + SHIELD_GAS * SHIELDS) * needs.sends as u64;
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

/// Runs a token issuer in-process, with a new key and one accept a device a day; returns its
/// URL, the key the makers hand tokens back under, and what the makers' `[tokens]` hold.
async fn start_issuer(
    work_dir: &Path,
) -> Result<(
    String,
    String,
    impl Fn(&str, PathBuf) -> zecswap_tokens::server::Config,
)> {
    let key = zecswap_tokens::IssuerKey::generate()?;
    let pem = work_dir.join("issuer.pem");
    std::fs::write(&pem, key.to_pem()?)?;
    let returns = zecswap_tokens::IssuerKey::generate()?;
    let return_pem = work_dir.join("return.pem");
    std::fs::write(&return_pem, returns.to_pem()?)?;
    let issuer = zecswap_issuer::Issuer::new(zecswap_issuer::Config {
        listen: "127.0.0.1:0".parse().expect("socket address"),
        name: "zecswap-e2e".into(),
        key: pem,
        data_dir: work_dir.join("issuer"),
        tokens_per_day: 1,
        attestation: zecswap_issuer::Attestation::InsecureTest,
        allow_insecure: true,
    })?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let router = zecswap_issuer::router(Arc::new(issuer));
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    let token_key = key.token_key().to_base64();
    let gate = move |origin: &str, spent: PathBuf| zecswap_tokens::server::Config {
        issuer: "zecswap-e2e".into(),
        origin: origin.into(),
        keys: vec![token_key.clone()],
        return_key: return_pem.clone(),
        spent,
    };
    Ok((url, returns.token_key().to_base64(), gate))
}

/// Runs a relayer in-process on `evm_rpc`, with its own key: it must never be a maker. With the
/// seed of its own Railgun wallet and a journal, it sends private Railgun sends for a fee note,
/// and records what each cost and earned, as the relayer's binary does.
async fn start_relayer(
    settings: &Settings,
    evm_rpc: &str,
    contract: Address,
    token: Address,
    key: PrivateKeySigner,
    sends: Option<(&[u8; 64], &str)>,
) -> Result<(String, Arc<Relayer>)> {
    // Railgun scenarios all swap with the attentive maker, which sends as the funder.
    let config = zecswap_relayer::Config {
        evm_rpc: evm_rpc.into(),
        contract,
        token,
        maker: settings.funder.address(),
        listen: "127.0.0.1:0".parse().expect("socket address"),
        fee: RELAYER_FEE,
        claim_margin: 3 * 60,
        reverse_funding: None,
        railgun_sends: sends.map(|(_, journal)| zecswap_relayer::RailgunSendsConfig {
            fee: SEND_FEE,
            // With an Alchemy key in the environment, sends are priced by their gas as well.
            providers: std::env::var("ALCHEMY_API_KEY")
                .is_ok()
                .then_some(zecswap_prices::Provider::Alchemy)
                .into_iter()
                .collect(),
            fee_margin_bps: 1_000,
            max_gas_limit: SEND_GAS_LIMIT,
            max_gas_price_wei: SEND_MAX_GAS_PRICE,
            journal: settings.work_dir.join(journal),
        }),
    };
    let railgun = sends.map(|(seed, _)| zecswap_railgun::Keys::from_seed(seed, 0));
    let relayer = Arc::new(Relayer::new(config, key, railgun).await?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    tokio::spawn(relayer.clone().run_costs());
    let router = zecswap_relayer::api::router(relayer.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    Ok((url, relayer))
}

/// Passes JSON-RPC through to `upstream`, but swallows the first transaction broadcast, as a
/// connection lost mid-send would: whoever sends through it cannot know if that one went out.
async fn lossy_rpc(upstream: String) -> Result<String> {
    use axum::http::{StatusCode, header};
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let http = reqwest::Client::new();
    let swallowed = Arc::new(AtomicBool::new(false));
    let router = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (http, upstream, swallowed) = (http.clone(), upstream.clone(), swallowed.clone());
        async move {
            let broadcast = serde_json::from_slice::<serde_json::Value>(&body)
                .is_ok_and(|request| request["method"] == "eth_sendRawTransaction");
            let (status, body) = if broadcast && !swallowed.swap(true, Ordering::SeqCst) {
                (StatusCode::BAD_GATEWAY, Vec::new())
            } else {
                match http
                    .post(&upstream)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .send()
                    .await
                {
                    Ok(response) => (
                        response.status(),
                        response.bytes().await.map(Vec::from).unwrap_or_default(),
                    ),
                    Err(_) => (StatusCode::BAD_GATEWAY, Vec::new()),
                }
            };
            (status, [(header::CONTENT_TYPE, "application/json")], body)
        }
    });
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
    UnwrapErr(SysRng).fill_bytes(&mut root[..]);
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
