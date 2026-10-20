//! Client for the ZecSwap settlement contract, on whichever EVM chain it is deployed.

mod canonical;
pub mod deposits;
mod events;
pub mod funding;
mod meter;
pub mod railgun;
pub use events::{SwapEvent, SwapEventKind};
pub use meter::{RpcMeter, RpcReport, RpcUsage, endpoint, log_usage};
use railgun::Signed;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::contract::{CallBuilder, CallDecoder};
use alloy::eips::{BlockNumberOrTag, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::keccak256;
use alloy::providers::bindings::IMulticall3;
use alloy::providers::utils::{EIP1559_BASE_FEE_MULTIPLIER, Eip1559Estimation};
use alloy::providers::{
    DynProvider, MULTICALL3_ADDRESS, PendingTransactionBuilder, Provider, ProviderBuilder,
};
use alloy::rpc::client::ClientBuilder;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::RpcError;
use tokio::sync::{Mutex, MutexGuard, Notify, OnceCell};
use tracing::{info, warn};
use zecswap_core::{PublicShare, ReverseOpen, SecretShare, Terms};
use zecswap_railgun::{ShieldCiphertext, ShieldNote};

use crate::Error;

pub use alloy::eips::BlockId;
pub use alloy::primitives::{Address, B256, U256};
pub use alloy::signers::local::PrivateKeySigner;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(180);
const SEND_ATTEMPTS: u32 = 4;
const SEND_RETRY_DELAY: Duration = Duration::from_secs(3);
/// Reads one `aggregate3` carries, far under any node's gas cap for a call.
const BATCH: usize = 100;
/// The blocks Flashbots Protect tries a transaction for, unless its URL's `blockRange` says.
const PROTECT_BLOCKS: u64 = 25;
/// Past those, the blocks after which one it never landed is taken as dropped: a margin for
/// missed slots and a node a block or two behind.
const PROTECT_MARGIN_BLOCKS: u64 = 5;
/// The gas a claim or refund sent privately may burn, set rather than estimated: an estimate would
/// show the node the share before any builder sees it. Mainnet's claims burn 237k.
const REVEAL_GAS: u64 = 400_000;
/// Blocks a public send waits for before it goes again at the fees then.
const REPRICE_BLOCKS: u64 = 3;
/// Reads in a row that must show a public send's nonce taken before it counts as dropped.
const NONCE_TAKEN_READS: u32 = 3;
/// How long a public send is priced again before its account's turn is let go, its outcome
/// unknown: one priced at the base fee then lands within two blocks, unless its account runs dry.
const LAND_TIMEOUT: Duration = Duration::from_secs(15 * 60);

sol! {
    struct ReverseOpenWords {
        address maker;
        address user;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        uint64 t0;
        uint64 t1;
        bytes32 refundNote;
        uint64 deadline;
    }

    struct ShieldCiphertextWords {
        bytes32[3] encryptedBundle;
        bytes32 shieldKey;
    }

    struct TermsWords {
        address maker;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        address user;
        uint64 t0;
        uint64 t1;
        bytes32 payoutNote;
    }

    // `open` and `rescue` take eight parameters, which the generated binding cannot shorten.
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    interface IZecSwap {
        event Opened(bytes32 indexed id, address indexed maker, address indexed user, address token, uint256 amount, uint256[2] makerKey, uint256[2] userKey, uint64 t0, uint64 t1, bytes32 payoutNote);
        event MarkedReady(bytes32 indexed id);
        event Claimed(bytes32 indexed id, uint256 userSecret);
        event PaidOut(bytes32 indexed id, address relayer, uint256 fee);
        event Rescued(bytes32 indexed id, address relayer, uint256 fee);
        event Refunded(bytes32 indexed id, uint256 makerSecret);
        struct Swap {
            bytes32 termsHash;
            uint8 stage;
            bool paidOut;
            uint256 secret;
        }

        function openReverse(ReverseOpenWords terms, bytes signature) external returns (bytes32);
        function reverseFunding(bytes32 id) external view returns (bytes32 refundNote, uint64 blockNumber);
        function readyWithSig(bytes32 id, TermsWords terms, uint64 deadline, bytes signature) external;
        function refundPayout(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function deposit(address token, uint256 amount) external;
        function withdraw(address token, uint256 amount, address to) external;
        function open(address token, uint128 amount, uint256[2] makerKey, uint256[2] userKey, address user, uint64 t0, uint64 t1, bytes32 payoutNote) external returns (bytes32 id);
        function ready(bytes32 id, TermsWords terms) external;
        function claim(bytes32 id, TermsWords terms, uint256 userSecret) external;
        function payout(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function rescue(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint64 nonce, uint64 deadline, bytes signature) external;
        function rescueNonces(bytes32 id) external view returns (uint64);
        function refund(bytes32 id, TermsWords terms, uint256 makerSecret) external;
        function getSwap(bytes32 id) external view returns (Swap memory);
        function vaultOf(bytes32 id) external view returns (address);
        function balanceOf(address maker, address token) external view returns (uint256);
        function MIN_WINDOW() external view returns (uint256);
        function LATE_WINDOW() external view returns (uint256);
        function RAILGUN() external view returns (address);
    }

    #[sol(rpc)]
    interface IRailgun {
        struct TokenData {
            uint8 tokenType;
            address tokenAddress;
            uint256 tokenSubID;
        }

        struct CommitmentPreimage {
            bytes32 npk;
            TokenData token;
            uint120 value;
        }

        event Shield(uint256 treeNumber, uint256 startPosition, CommitmentPreimage[] commitments, ShieldCiphertextWords[] shieldCiphertext, uint256[] fees);

        function tokenBlocklist(address token) external view returns (bool);
    }

    #[sol(rpc)]
    interface IErc20 {
        event Transfer(address indexed from, address indexed to, uint256 value);
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 amount) external returns (bool);
        function balanceOf(address owner) external view returns (uint256);
        function mint(address to, uint256 amount) external;
    }

    // `shield` takes ten parameters, which the generated binding cannot shorten.
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    interface IDeposits {
        event DepositShielded(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);
        event Rescued(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);
        function shield(address from, uint256 value, uint256 validAfter, uint256 validBefore, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint8 v, bytes32 r, bytes32 s) external;
        function rescue(address from, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint64 nonce, uint64 deadline, bytes signature) external;
        function vaultOf(address from) external view returns (address);
        function deposited(address from) external view returns (bool);
        function rescueNonces(address from) external view returns (uint64);
        function TOKEN() external view returns (address);
        function RAILGUN() external view returns (address);
        function SANCTIONS() external view returns (address);
    }

    /// EIP-3009's, and the EIP-712 domain it signs under.
    #[sol(rpc)]
    interface IEip3009 {
        function name() external view returns (string);
        function version() external view returns (string);
        function DOMAIN_SEPARATOR() external view returns (bytes32);
        function authorizationState(address authorizer, bytes32 nonce) external view returns (bool);
    }

    #[sol(rpc)]
    interface ISanctionsOracle {
        function isSanctioned(address addr) external view returns (bool);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Open,
    Ready,
    Claimed,
    Refunded,
}

/// A mined transaction, as its receipt tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionFacts {
    pub sender: Address,
    pub succeeded: bool,
    pub block_number: u64,
    pub block_hash: B256,
    pub gas_used: u64,
    /// Wei per gas, base fee and tip together.
    pub gas_price: u128,
    /// Whether the configured Railgun proxy emitted anything in it.
    pub uses_railgun: bool,
    /// The escrow token it paid a relayer: what it moved to its own sender, or a sponsored
    /// funding's fee.
    pub paid_to_sender: U256,
}

/// What a transaction paid a relayer in `token`. Its transfers to the sender count, or, where
/// the token logs no transfers (Sepolia's test token logs none), the fees the escrow's `PaidOut`
/// and `Rescued` name the sender for and the transfers a Relay Adapt call makes: a sponsored
/// funding's one transfer is the relayer's fee, which its own account takes while its funding
/// account sends it.
fn paid_to(
    sender: Address,
    token: Address,
    escrow: Address,
    logs: &[alloy::rpc::types::Log],
    input: Option<&[u8]>,
) -> U256 {
    let transferred = logs
        .iter()
        .filter(|log| {
            log.address() == token
                && log.topics().first() == Some(&IErc20::Transfer::SIGNATURE_HASH)
        })
        .filter_map(|log| IErc20::Transfer::decode_log_data(log.data()).ok())
        .filter(|transfer| transfer.to == sender)
        .fold(U256::ZERO, |total, transfer| {
            total.saturating_add(transfer.value)
        });
    let escrow_fees = logs
        .iter()
        .filter(|log| log.address() == escrow)
        .filter_map(|log| match log.topics().first() {
            Some(&IZecSwap::PaidOut::SIGNATURE_HASH) => {
                IZecSwap::PaidOut::decode_log_data(log.data())
                    .ok()
                    .map(|paid| (paid.relayer, paid.fee))
            }
            Some(&IZecSwap::Rescued::SIGNATURE_HASH) => {
                IZecSwap::Rescued::decode_log_data(log.data())
                    .ok()
                    .map(|rescued| (rescued.relayer, rescued.fee))
            }
            _ => None,
        })
        .filter(|(relayer, _)| *relayer == sender)
        .fold(U256::ZERO, |total, (_, fee)| total.saturating_add(fee));
    let relayed = input
        .and_then(|input| railgun::IRelayAdapt::relayCall::abi_decode(input).ok())
        .map_or(U256::ZERO, |relay| {
            relay
                ._actionData
                .calls
                .iter()
                .filter(|call| call.to == token)
                .filter_map(|call| IErc20::transferCall::abi_decode(&call.data).ok())
                .fold(U256::ZERO, |total, payment| {
                    total.saturating_add(payment.amount)
                })
        });
    transferred.max(escrow_fees.saturating_add(relayed))
}

/// A swap as the contract and the terms it opened with describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnChainSwap {
    pub stage: Stage,
    pub maker: Address,
    pub user: Address,
    pub token: Address,
    pub amount: u128,
    pub t0: u64,
    pub t1: u64,
    pub maker_share: PublicShare,
    pub user_share: PublicShare,
    /// The share settlement revealed, big-endian; zero until then.
    pub secret: [u8; 32],
    /// The commitment to the Railgun note a claim pays, for a swap that pays into Railgun.
    pub payout_note: Option<B256>,
    /// Whether a claimed Railgun payout has left.
    pub paid_out: bool,
}

impl OnChainSwap {
    /// The share revealed at settlement: the user's once claimed, the maker's once refunded.
    pub fn revealed(&self) -> Result<Option<SecretShare>, Error> {
        match self.stage {
            Stage::Claimed | Stage::Refunded => Ok(Some(SecretShare::from_be_bytes(&self.secret)?)),
            Stage::Open | Stage::Ready => Ok(None),
        }
    }
}

/// What the contract stores of a swap: its state, and only a hash of its terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapState {
    pub stage: Stage,
    pub terms_hash: B256,
    /// The share settlement revealed, big-endian; zero until then.
    pub secret: [u8; 32],
    pub paid_out: bool,
}

/// A block's number, time and hash: the time is the clock the contract's windows run on, and the
/// hash pins reads to that very block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub number: u64,
    pub timestamp: u64,
    pub hash: B256,
}

/// The contract's two window lengths, fixed at deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// The least time an open leaves before `t0`, and from `t0` to `t1`.
    pub min: u64,
    /// How long from `t1` only the maker may refund a forward swap; after it, its user may claim.
    pub late: u64,
}

/// Which share a call reveals: a claim's (the user's forward, the maker's reverse) or a refund's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    Claim,
    Refund,
}

impl Reveal {
    pub fn name(self) -> &'static str {
        match self {
            Reveal::Claim => "claim",
            Reveal::Refund => "refund",
        }
    }
}

/// Whether the contract takes a reveal now, and until when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// It would revert, its share public.
    Shut,
    /// Until this time, exclusive.
    Until(u64),
    /// From now on, for good.
    Lasting,
}

impl Window {
    /// Whether a reveal sent now lands in time, given the later of the chain's time and the wall
    /// clock as `now`: either can only make a reveal go earlier.
    pub fn leaves(self, now: u64, margin: u64) -> bool {
        match self {
            Window::Shut => false,
            Window::Until(end) => now.saturating_add(margin) < end,
            Window::Lasting => true,
        }
    }
}

impl Windows {
    /// The window, at chain time `now`, of `reveal` on a swap at `stage` with deadlines `t0` and
    /// `t1`, a reverse escrow if `reverse`: as the contract's `_claimable` and `_refundable` judge.
    /// At most one of a swap's two reveals is ever open.
    pub fn window(
        &self,
        reveal: Reveal,
        reverse: bool,
        stage: Stage,
        (t0, t1): (u64, u64),
        now: u64,
    ) -> Window {
        if !matches!(stage, Stage::Open | Stage::Ready) {
            return Window::Shut;
        }
        let late = t1.saturating_add(self.late);
        let open = |yes: bool, window: Window| if yes { window } else { Window::Shut };
        match (reveal, reverse) {
            (Reveal::Claim, false) if now >= t1 => open(now >= late, Window::Lasting),
            (Reveal::Claim, false) => open(stage == Stage::Ready || now >= t0, Window::Until(t1)),
            (Reveal::Claim, true) => open(now < t1 && stage == Stage::Ready, Window::Until(t1)),
            (Reveal::Refund, false) if now >= t1 => open(now < late, Window::Until(late)),
            (Reveal::Refund, false) => open(stage == Stage::Open && now < t0, Window::Until(t0)),
            (Reveal::Refund, true) => open(stage == Stage::Open || now >= t1, Window::Lasting),
        }
    }
}

/// The wall clock, in Unix seconds.
pub fn wall_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A note Railgun recorded shielding, as its `Shield` event reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shielded {
    pub note: ShieldNote,
    pub token: Address,
    /// What the note holds, after Railgun's fee.
    pub value: u128,
    pub fee: u128,
}

/// The contract keys each swap by its maker and user share.
pub fn swap_id(maker: Address, user_share: &PublicShare) -> B256 {
    let mut preimage = [0; 96];
    preimage[12..32].copy_from_slice(maker.as_slice());
    preimage[32..].copy_from_slice(&user_share.to_affine_bytes());
    keccak256(preimage)
}

/// `reverseSwapId`: a reverse escrow's key, from the escrowing user and the maker's share, and
/// never a forward swap's.
pub fn reverse_swap_id(user: Address, maker_share: &PublicShare) -> B256 {
    let mut preimage = [0; 128];
    preimage[12..32].copy_from_slice(user.as_slice());
    preimage[32..96].copy_from_slice(&maker_share.to_affine_bytes());
    preimage[127] = 1;
    keccak256(preimage)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReverseFunding {
    pub refund_note: B256,
    pub block_number: u64,
}

/// Relay Adapt calls these in order, with requireSuccess set, after unshielding `amount`.
pub fn reverse_funding_calls(
    contract: Address,
    terms: &ReverseOpen,
    signature: &[u8; 65],
) -> [(Address, Vec<u8>); 2] {
    let approve = IErc20::approveCall {
        spender: contract,
        amount: U256::from(terms.amount),
    }
    .abi_encode();
    let open = IZecSwap::openReverseCall {
        terms: ReverseOpenWords {
            maker: terms.maker.into(),
            user: terms.user.into(),
            token: terms.token.into(),
            amount: terms.amount,
            makerKey: share_words(&terms.maker_share),
            userKey: share_words(&terms.user_share),
            t0: terms.t0,
            t1: terms.t1,
            refundNote: terms.refund_note.into(),
            deadline: terms.deadline,
        },
        signature: signature.to_vec().into(),
    }
    .abi_encode();
    [(terms.token.into(), approve), (contract, open)]
}

/// A connection to the settlement contract, sending transactions as one account, or reading
/// only. Sends are serialized and read the pending nonce rather than caching it, so a failed
/// send leaves no gap. Settlement calls wait for receipts; sponsored funding returns a hash
/// while pending so the wallet can track inclusion independently.
pub struct Settlement {
    provider: DynProvider,
    contract: IZecSwap::IZecSwapInstance<DynProvider>,
    account: Option<Address>,
    /// The account's key, for transactions recorded before they are broadcast.
    wallet: Option<EthereumWallet>,
    /// Where the account's contract calls and deposits are broadcast instead of the public
    /// mempool.
    private: Option<Private>,
    /// Seconds a claim's or refund's window must still have when its turn to broadcast comes.
    reveal_margin: u64,
    sending: Line,
    meter: RpcMeter,
    /// The contract's Railgun proxy, which it fixes at deployment.
    railgun: OnceCell<Address>,
    /// Whether Multicall3 is deployed, so reads of many swaps or accounts take one call.
    multicall: OnceCell<bool>,
    /// The contract's window lengths, which it fixes at deployment.
    windows: OnceCell<Windows>,
    /// What this account's reveals and cancel locks may pay per gas over its other sends' fee cap.
    reveal_headroom: u128,
}

/// A revert-protecting private RPC: it lands a transaction only if it succeeds, never shows it to
/// the public mempool, and drops one it couldn't land within `blocks` blocks.
struct Private {
    provider: DynProvider,
    blocks: u64,
    /// The least nonce the next contract call sent through it may take: past the last one that
    /// mined, which a node a block behind may not count yet.
    least_nonce: AtomicU64,
}

impl Private {
    /// The blocks after the one a transaction went out in from which, unmined, it is dropped.
    fn window(&self) -> u64 {
        self.blocks + PROTECT_MARGIN_BLOCKS
    }
}

/// A share a call reveals on a swap, whose window must still have `margin` seconds left when the
/// call is broadcast.
#[derive(Clone, Debug)]
struct Check {
    id: B256,
    terms: Terms,
    reveal: Reveal,
    margin: u64,
}

/// The account's sends, one at a time. A turn that ends, whether or not its transaction went
/// out, wakes whoever waits in `Settlement::sent`.
#[derive(Default)]
struct Line {
    turn: Mutex<()>,
    ended: Notify,
}

impl Line {
    async fn turn(&self) -> Turn<'_> {
        Turn {
            _turn: self.turn.lock().await,
            ended: &self.ended,
        }
    }
}

struct Turn<'a> {
    _turn: MutexGuard<'a, ()>,
    ended: &'a Notify,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.ended.notify_one();
    }
}

impl Settlement {
    pub fn connect(
        rpc_url: &str,
        contract: Address,
        signer: PrivateKeySigner,
    ) -> Result<Self, Error> {
        Self::connect_metered(rpc_url, contract, signer, &RpcMeter::default())
    }

    /// As `connect`, counting its requests on `meter`, which other connections may share.
    pub fn connect_metered(
        rpc_url: &str,
        contract: Address,
        signer: PrivateKeySigner,
        meter: &RpcMeter,
    ) -> Result<Self, Error> {
        let account = signer.address();
        let wallet = EthereumWallet::from(signer);
        Ok(Self::new(
            signing_provider(rpc_url, Some(wallet.clone()), meter)?,
            contract,
            Some((account, wallet)),
            meter,
        ))
    }

    /// For a party with no account on the chain, which reads and leaves sending to a relayer.
    pub fn read_only(rpc_url: &str, contract: Address) -> Result<Self, Error> {
        let meter = RpcMeter::default();
        Ok(Self::new(
            signing_provider(rpc_url, None, &meter)?,
            contract,
            None,
            &meter,
        ))
    }

    fn new(
        provider: DynProvider,
        contract: Address,
        account: Option<(Address, EthereumWallet)>,
        meter: &RpcMeter,
    ) -> Self {
        let (account, wallet) = account.unzip();
        Self {
            contract: IZecSwap::new(contract, provider.clone()),
            provider,
            account,
            wallet,
            private: None,
            reveal_margin: 0,
            sending: Line::default(),
            meter: meter.clone(),
            railgun: OnceCell::new(),
            multicall: OnceCell::new(),
            windows: OnceCell::new(),
            reveal_headroom: 0,
        }
    }

    /// Broadcasts this account's contract calls and deposits through `rpc_url` alone, never the
    /// public mempool: Flashbots Protect's, which drops a transaction it couldn't land within its
    /// URL's `blockRange` blocks, or 25. Only the broadcasts go there: every read stays on this
    /// connection's node.
    pub fn broadcast_privately(&mut self, rpc_url: &str) -> Result<(), Error> {
        // Read first: what it finds wrong never echoes the URL.
        let blocks = protect_blocks(&private_url(rpc_url)?)?;
        self.private = Some(Private {
            provider: signing_provider(rpc_url, None, &RpcMeter::default())?,
            blocks,
            least_nonce: AtomicU64::new(0),
        });
        Ok(())
    }

    /// With a private RPC, the blocks after the one a transaction went out in from which,
    /// unmined, it is taken as dropped: those the private RPC tries it for, and a margin.
    pub fn private_window(&self) -> Option<u64> {
        self.private.as_ref().map(Private::window)
    }

    /// Broadcasts this account's claims and refunds only while their window has `seconds` left
    /// when their turn to broadcast comes: one that waited for it may have run the window down,
    /// and a reveal landing after its window closes reverts, its share public, with the other
    /// side's window open. With no margin, the window must merely be open.
    pub fn reveal_with_margin(&mut self, seconds: u64) {
        self.reveal_margin = seconds;
    }

    /// That the private RPC, if any, serves the chain this connection reads.
    pub async fn check_private_rpc(&self) -> Result<(), Error> {
        let Some(private) = &self.private else {
            return Ok(());
        };
        let theirs = private
            .provider
            .get_chain_id()
            .await
            .map_err(|_| Error::Contract("the private RPC is unavailable".into()))?;
        let ours = self.chain_id().await?;
        if theirs != ours {
            return Err(Error::Config(format!(
                "the private RPC serves chain {theirs}, not {ours}"
            )));
        }
        Ok(())
    }

    /// Where this account's signed transactions are broadcast.
    fn broadcaster(&self) -> &DynProvider {
        self.private
            .as_ref()
            .map_or(&self.provider, |private| &private.provider)
    }

    pub fn contract(&self) -> Address {
        *self.contract.address()
    }

    /// The account this connection sends as; none for a read-only one.
    pub fn account(&self) -> Option<Address> {
        self.account
    }

    /// What this connection has asked its node, and every other connection sharing its meter.
    pub fn meter(&self) -> &RpcMeter {
        &self.meter
    }

    pub async fn chain_id(&self) -> Result<u64, Error> {
        self.provider.get_chain_id().await.map_err(Error::contract)
    }

    /// The latest block's timestamp: the clock the contract's deadlines run on.
    pub async fn now(&self) -> Result<u64, Error> {
        Ok(self.head().await?.timestamp)
    }

    /// The latest block, read once for both its number and its time.
    pub async fn head(&self) -> Result<Head, Error> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("no latest block".into()))?;
        Ok(Head {
            number: block.header.number,
            timestamp: block.header.timestamp,
            hash: block.header.hash,
        })
    }

    pub async fn confirmed_now(&self, confirmations: u64) -> Result<u64, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let height = latest.saturating_sub(confirmations - 1);
        let block = self
            .provider
            .get_block_by_number(height.into())
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("confirmed block unavailable".into()))?;
        Ok(block.header.timestamp)
    }

    /// The contract's window lengths, read once: an error for a contract without them, such as
    /// the lock contract deployed before, whose swaps follow other rules and whose swap records
    /// would read wrong here.
    pub async fn windows(&self) -> Result<Windows, Error> {
        self.windows
            .get_or_try_init(|| async {
                let missing = |e| {
                    Error::Contract(format!(
                        "{} has no MIN_WINDOW or LATE_WINDOW, as only the windows contract has: {e}",
                        self.contract()
                    ))
                };
                let (min, late) = tokio::try_join!(
                    async { self.contract.MIN_WINDOW().call().await.map_err(missing) },
                    async { self.contract.LATE_WINDOW().call().await.map_err(missing) },
                )?;
                Ok::<_, Error>(Windows {
                    min: u64::try_from(min).map_err(Error::contract)?,
                    late: u64::try_from(late).map_err(Error::contract)?,
                })
            })
            .await
            .copied()
    }

    /// The swap and the newest block, the one as of the other: a decision on a reveal never takes
    /// one block's time with another's state, as two reads from a load-balanced RPC can.
    pub async fn read(
        &self,
        id: B256,
        terms: &Terms,
    ) -> Result<(Option<OnChainSwap>, Head), Error> {
        let head = self.head().await?;
        Ok((
            self.swap_at(id, terms, BlockId::hash(head.hash)).await?,
            head,
        ))
    }

    /// The swap as of block `at`, read against the terms it should have opened with.
    pub async fn swap_at(
        &self,
        id: B256,
        terms: &Terms,
        at: BlockId,
    ) -> Result<Option<OnChainSwap>, Error> {
        let swap = self
            .contract
            .getSwap(id)
            .block(at)
            .call()
            .await
            .map_err(Error::contract)?;
        on_chain(id, decode_state(swap)?, terms)
    }

    /// The window `reveal` on the swap has now, with the newest block it was judged at: the swap,
    /// whether it is a reverse escrow and the block's time all read as of that block.
    pub async fn reveal_window(
        &self,
        id: B256,
        terms: &Terms,
        reveal: Reveal,
    ) -> Result<(Window, Head), Error> {
        let windows = self.windows().await?;
        let head = self.head().await?;
        let at = BlockId::hash(head.hash);
        let (swap, funding) = tokio::try_join!(self.swap_at(id, terms, at), async {
            let funding = self.contract.reverseFunding(id).block(at).call().await;
            funding.map_err(Error::contract)
        })?;
        let window = swap.map_or(Window::Shut, |swap| {
            let reverse = !funding.refundNote.is_zero();
            windows.window(
                reveal,
                reverse,
                swap.stage,
                (swap.t0, swap.t1),
                head.timestamp,
            )
        });
        Ok((window, head))
    }

    /// The swap, read against the terms it should have opened with: none if it never opened,
    /// `Error::WrongTerms` if it opened on others.
    pub async fn swap(&self, id: B256, terms: &Terms) -> Result<Option<OnChainSwap>, Error> {
        on_chain(id, self.swap_state(id).await?, terms)
    }

    /// What the contract stores of the swap; none if it never opened. Nothing in it says what
    /// the swap pays whom: that takes its terms, and `swap`.
    pub async fn swap_state(&self, id: B256) -> Result<Option<SwapState>, Error> {
        let swap = self
            .contract
            .getSwap(id)
            .call()
            .await
            .map_err(Error::contract)?;
        decode_state(swap)
    }

    pub async fn confirmed_swap(
        &self,
        id: B256,
        terms: &Terms,
        confirmations: u64,
    ) -> Result<Option<OnChainSwap>, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let Some(height) = latest.checked_sub(confirmations - 1) else {
            return Ok(None);
        };
        let swap = self
            .contract
            .getSwap(id)
            .block(height.into())
            .call()
            .await
            .map_err(Error::contract)?;
        on_chain(id, decode_state(swap)?, terms)
    }

    /// What the contract stores of each swap in `ids` at block `at`, in one call for every
    /// hundred where Multicall3 is deployed, else one a swap.
    pub async fn swap_states(
        &self,
        ids: &[B256],
        at: BlockId,
    ) -> Result<Vec<Option<SwapState>>, Error> {
        if !self.multicall_deployed().await? {
            let mut states = Vec::with_capacity(ids.len());
            for id in ids {
                let swap = self
                    .contract
                    .getSwap(*id)
                    .block(at)
                    .call()
                    .await
                    .map_err(Error::contract)?;
                states.push(decode_state(swap)?);
            }
            return Ok(states);
        }
        let calls = ids.iter().map(|id| {
            (
                self.contract(),
                IZecSwap::getSwapCall { id: *id }.abi_encode(),
            )
        });
        self.aggregate(calls.collect(), at)
            .await?
            .into_iter()
            .map(|data| {
                let swap =
                    IZecSwap::getSwapCall::abi_decode_returns(&data).map_err(Error::contract)?;
                decode_state(swap)
            })
            .collect()
    }

    /// The ETH each of `owners` holds, in one call where Multicall3 is deployed.
    pub async fn eth_balances(&self, owners: &[Address]) -> Result<Vec<U256>, Error> {
        if !self.multicall_deployed().await? {
            let mut balances = Vec::with_capacity(owners.len());
            for owner in owners {
                balances.push(self.eth_balance(*owner).await?);
            }
            return Ok(balances);
        }
        let calls = owners.iter().map(|owner| {
            let call = IMulticall3::getEthBalanceCall { addr: *owner };
            (MULTICALL3_ADDRESS, call.abi_encode())
        });
        self.aggregate(calls.collect(), BlockId::latest())
            .await?
            .into_iter()
            .map(|data| {
                IMulticall3::getEthBalanceCall::abi_decode_returns(&data).map_err(Error::contract)
            })
            .collect()
    }

    async fn multicall_deployed(&self) -> Result<bool, Error> {
        self.multicall
            .get_or_try_init(|| async {
                let code = self
                    .provider
                    .get_code_at(MULTICALL3_ADDRESS)
                    .await
                    .map_err(Error::contract)?;
                Ok::<_, Error>(!code.is_empty())
            })
            .await
            .copied()
    }

    /// Each call's answer, in order, from `aggregate3` at `at`; any call that fails fails all.
    async fn aggregate(
        &self,
        calls: Vec<(Address, Vec<u8>)>,
        at: BlockId,
    ) -> Result<Vec<alloy::primitives::Bytes>, Error> {
        let mut answers = Vec::with_capacity(calls.len());
        for batch in calls.chunks(BATCH) {
            let calls = batch
                .iter()
                .map(|(target, data)| IMulticall3::Call3 {
                    target: *target,
                    allowFailure: false,
                    callData: data.clone().into(),
                })
                .collect();
            let request = TransactionRequest::default()
                .with_to(MULTICALL3_ADDRESS)
                .with_input(IMulticall3::aggregate3Call { calls }.abi_encode());
            let returned = self
                .provider
                .call(request)
                .block(at)
                .await
                .map_err(Error::contract)?;
            let results = IMulticall3::aggregate3Call::abi_decode_returns(&returned)
                .map_err(Error::contract)?;
            if results.len() != batch.len() {
                return Err(Error::Contract("Multicall3 answered another count".into()));
            }
            answers.extend(results.into_iter().map(|result| result.returnData));
        }
        Ok(answers)
    }

    /// Railgun's proxy, which payouts shield into; zero where the contract pays accounts only.
    /// The contract fixes it at deployment, so it is read once.
    pub async fn railgun(&self) -> Result<Address, Error> {
        self.railgun
            .get_or_try_init(|| async {
                self.contract
                    .RAILGUN()
                    .call()
                    .await
                    .map_err(Error::contract)
            })
            .await
            .copied()
    }

    /// Whether Railgun takes `token` now: it is there, not paused, and not blocking the token.
    /// A payout that can't leave shouldn't be claimed; the swap unwinds instead.
    pub async fn railgun_accepts(&self, token: Address) -> Result<bool, Error> {
        let railgun = self.railgun().await?;
        if railgun.is_zero() {
            return Ok(false);
        }
        match IRailgun::new(railgun, &self.provider)
            .tokenBlocklist(token)
            .call()
            .await
        {
            Ok(blocked) => Ok(!blocked),
            // Railgun's proxy reverts every call while it is paused.
            Err(e) if e.as_revert_data().is_some() => Ok(false),
            Err(e) => Err(Error::contract(e)),
        }
    }

    /// What a mined transaction did, read from its receipt; none while it is unmined.
    /// `token` is the escrow token, whose transfers to the sender are counted.
    pub async fn transaction_facts(
        &self,
        tx: B256,
        token: Address,
    ) -> Result<Option<TransactionFacts>, Error> {
        let Some(receipt) = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
        else {
            return Ok(None);
        };
        let (Some(block_number), Some(block_hash)) = (receipt.block_number, receipt.block_hash)
        else {
            return Ok(None);
        };
        let railgun = self.railgun().await?;
        let logs = receipt.inner.logs();
        let uses_railgun = !railgun.is_zero() && logs.iter().any(|log| log.address() == railgun);
        // A sponsored reverse funding pays its fee by a call in its Relay Adapt calldata.
        let input = match uses_railgun && receipt.to != Some(self.contract()) {
            true => self
                .provider
                .get_transaction_by_hash(tx)
                .await
                .map_err(Error::contract)?
                .map(|transaction| alloy::consensus::Transaction::input(&transaction).clone()),
            false => None,
        };
        Ok(Some(TransactionFacts {
            sender: receipt.from,
            succeeded: receipt.status(),
            block_number,
            block_hash,
            gas_used: receipt.gas_used,
            gas_price: receipt.effective_gas_price,
            uses_railgun,
            paid_to_sender: paid_to(
                receipt.from,
                token,
                self.contract(),
                logs,
                input.as_deref().map(|input| &input[..]),
            ),
        }))
    }

    /// What a mined transaction burned: its gas, and that gas at the price it paid, in wei.
    /// None for one the node has no receipt of.
    pub async fn transaction_cost(&self, tx: B256) -> Result<Option<(u64, u128)>, Error> {
        Ok(self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
            .map(|receipt| {
                let gas = receipt.gas_used;
                (gas, u128::from(gas) * receipt.effective_gas_price)
            }))
    }

    /// The notes a transaction shielded into Railgun.
    pub async fn shielded(&self, tx: B256) -> Result<Vec<Shielded>, Error> {
        let railgun = self.railgun().await?;
        let receipt = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract(format!("no receipt for {tx}")))?;
        let mut notes = Vec::new();
        for log in receipt.inner.logs() {
            if log.address() != railgun {
                continue;
            }
            let Ok(event) = IRailgun::Shield::decode_log_data(log.data()) else {
                continue;
            };
            for ((preimage, ciphertext), fee) in event
                .commitments
                .iter()
                .zip(&event.shieldCiphertext)
                .zip(&event.fees)
            {
                notes.push(Shielded {
                    note: ShieldNote {
                        npk: preimage.npk.0,
                        ciphertext: ShieldCiphertext {
                            encrypted_bundle: ciphertext.encryptedBundle.map(|word| word.0),
                            shield_key: ciphertext.shieldKey.0,
                        },
                    },
                    token: preimage.token.tokenAddress,
                    value: preimage.value.to(),
                    fee: u128::try_from(*fee).map_err(Error::contract)?,
                });
            }
        }
        Ok(notes)
    }

    /// What `owner` can withdraw from the contract: a maker's inventory or a user's payouts.
    pub async fn balance_of(&self, owner: Address, token: Address) -> Result<u128, Error> {
        let balance = self
            .contract
            .balanceOf(owner, token)
            .call()
            .await
            .map_err(Error::contract)?;
        u128::try_from(balance).map_err(Error::contract)
    }

    pub async fn withdraw(&self, token: Address, amount: u128, to: Address) -> Result<B256, Error> {
        let call = self.contract.withdraw(token, U256::from(amount), to);
        Ok(self.submit(call).await?.transaction_hash)
    }

    /// Opens a swap on `terms`, whose `maker` must be this account.
    #[tracing::instrument(skip_all, fields(operation = "open", chain = "evm"))]
    pub async fn open(&self, terms: &Terms) -> Result<B256, Error> {
        if self.account != Some(terms.maker.into()) {
            return Err(Error::Config(
                "the terms name another account as the maker".into(),
            ));
        }
        let call = self.contract.open(
            terms.token.into(),
            terms.amount,
            share_words(&terms.maker_share),
            share_words(&terms.user_share),
            terms.user.into(),
            terms.t0,
            terms.t1,
            terms.payout_note.into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    pub async fn reverse_funding(&self, id: B256) -> Result<Option<ReverseFunding>, Error> {
        let funding = self
            .contract
            .reverseFunding(id)
            .call()
            .await
            .map_err(Error::contract)?;
        Ok((!funding.refundNote.is_zero()).then_some(ReverseFunding {
            refund_note: funding.refundNote,
            block_number: funding.blockNumber,
        }))
    }

    /// Reads the escrow at a confirmed block, so an API notification never proves funding.
    pub async fn confirmed_reverse_funding(
        &self,
        id: B256,
        confirmations: u64,
    ) -> Result<Option<ReverseFunding>, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let Some(height) = latest.checked_sub(confirmations - 1) else {
            return Ok(None);
        };
        let funding = self
            .contract
            .reverseFunding(id)
            .block(height.into())
            .call()
            .await
            .map_err(Error::contract)?;
        Ok((!funding.refundNote.is_zero()).then_some(ReverseFunding {
            refund_note: funding.refundNote,
            block_number: funding.blockNumber,
        }))
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "ready_with_sig", chain = "evm"))]
    pub async fn ready_with_sig(
        &self,
        id: B256,
        terms: &Terms,
        deadline: u64,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call =
            self.contract
                .readyWithSig(id, terms_words(terms), deadline, signature.to_vec().into());
        Ok(self.submit(call).await?.transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "refund_payout", chain = "evm"))]
    pub async fn refund_payout(
        &self,
        id: B256,
        terms: &Terms,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self.contract.refundPayout(
            id,
            terms_words(terms),
            note.npk.into(),
            ciphertext_words(&note.ciphertext),
            fee,
            signature.to_vec().into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "ready", chain = "evm"))]
    pub async fn ready(&self, id: B256, terms: &Terms) -> Result<B256, Error> {
        let call = self.contract.ready(id, terms_words(terms));
        Ok(self.submit(call).await?.transaction_hash)
    }

    /// Shields a claimed swap's amount to its committed `note`, keeping the `fee` its user
    /// signed for this account.
    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "payout", chain = "evm"))]
    pub async fn payout(
        &self,
        id: B256,
        terms: &Terms,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self.contract.payout(
            id,
            terms_words(terms),
            note.npk.into(),
            ciphertext_words(&note.ciphertext),
            fee,
            signature.to_vec().into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    /// Shields what came back to a swap's vault to a `note` its user signed for.
    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "rescue", chain = "evm"))]
    pub async fn rescue(
        &self,
        id: B256,
        terms: &Terms,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
        authorization: zecswap_core::RescueAuthorization,
    ) -> Result<B256, Error> {
        let call = self.contract.rescue(
            id,
            terms_words(terms),
            note.npk.into(),
            ciphertext_words(&note.ciphertext),
            fee,
            authorization.nonce,
            authorization.deadline,
            signature.to_vec().into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    pub async fn rescue_nonce(&self, id: B256) -> Result<u64, Error> {
        self.contract
            .rescueNonces(id)
            .call()
            .await
            .map_err(Error::contract)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "claim", chain = "evm"))]
    pub async fn claim(
        &self,
        id: B256,
        terms: &Terms,
        user_secret: &SecretShare,
    ) -> Result<B256, Error> {
        let secret = U256::from_be_bytes(user_secret.to_be_bytes());
        let call = self.contract.claim(id, terms_words(terms), secret);
        Ok(self
            .reveal(call, id, terms, Reveal::Claim)
            .await?
            .transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "refund", chain = "evm"))]
    pub async fn refund(
        &self,
        id: B256,
        terms: &Terms,
        maker_secret: &SecretShare,
    ) -> Result<B256, Error> {
        let secret = U256::from_be_bytes(maker_secret.to_be_bytes());
        let call = self.contract.refund(id, terms_words(terms), secret);
        Ok(self
            .reveal(call, id, terms, Reveal::Refund)
            .await?
            .transaction_hash)
    }

    /// Approves and adds `amount` of `token` to this account's inventory.
    pub async fn add_inventory(&self, token: Address, amount: u128) -> Result<(), Error> {
        let erc20 = IErc20::new(token, &self.provider);
        self.submit(erc20.approve(self.contract(), U256::from(amount)))
            .await?;
        self.submit(self.contract.deposit(token, U256::from(amount)))
            .await?;
        Ok(())
    }

    pub async fn token_balance(&self, token: Address, owner: Address) -> Result<u128, Error> {
        let balance = IErc20::new(token, &self.provider)
            .balanceOf(owner)
            .call()
            .await
            .map_err(Error::contract)?;
        u128::try_from(balance).map_err(Error::contract)
    }

    /// Mints a test token; only the testnet token has `mint`.
    pub async fn mint_test_token(
        &self,
        token: Address,
        to: Address,
        amount: u128,
    ) -> Result<(), Error> {
        self.submit(IErc20::new(token, &self.provider).mint(to, U256::from(amount)))
            .await?;
        Ok(())
    }

    /// What gas costs now, in wei: the base fee and the tip the node suggests, the tip every send
    /// pays, so what quotes and fees charge at this price covers their sends.
    pub async fn gas_price(&self) -> Result<u128, Error> {
        self.provider.get_gas_price().await.map_err(Error::contract)
    }

    /// The fee cap a send is filled with now: the ETH it must hold for each gas it may burn,
    /// the base fee twice over and the tip.
    pub async fn max_fee_per_gas(&self) -> Result<u128, Error> {
        Ok(fees(&self.provider).await?.max_fee_per_gas)
    }

    /// Lets this account's reveals, the claims and refunds that carry a share, pay `wei` more per
    /// gas than its other sends may.
    pub fn set_reveal_headroom(&mut self, wei: u128) {
        self.reveal_headroom = wei;
    }

    pub fn reveal_headroom(&self) -> u128 {
        self.reveal_headroom
    }

    pub async fn eth_balance(&self, owner: Address) -> Result<U256, Error> {
        self.provider
            .get_balance(owner)
            .await
            .map_err(Error::contract)
    }

    /// Waits for a turn to send from this account to end, whether its transaction went out or
    /// not: only the account's own sends spend its ETH. Turns that end while no one waits wake
    /// the next wait, once. For one waiter.
    pub async fn sent(&self) {
        self.sending.ended.notified().await;
    }

    pub async fn send_eth(&self, to: Address, wei: U256) -> Result<B256, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let _sending = self.sending.turn().await;
        self.transfer(account, to, wei).await
    }

    /// Sends `to` what this account holds above `keep`, if anything, unless a transaction of its
    /// own is pending: its balance doesn't show what that one spends yet.
    pub async fn send_eth_above(&self, to: Address, keep: U256) -> Result<Option<U256>, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let _sending = self.sending.turn().await;
        let count = || self.provider.get_transaction_count(account);
        let (mined, sent) =
            tokio::try_join!(count().latest(), count().pending()).map_err(Error::contract)?;
        if sent != mined {
            return Ok(None);
        }
        let balance = self.eth_balance(account).await?;
        let Some(wei) = balance.checked_sub(keep).filter(|wei| !wei.is_zero()) else {
            return Ok(None);
        };
        self.transfer(account, to, wei).await?;
        Ok(Some(wei))
    }

    /// Sends `wei` from `account` in a turn its caller holds.
    async fn transfer(&self, account: Address, to: Address, wei: U256) -> Result<B256, Error> {
        let tx = TransactionRequest::default()
            .with_from(account)
            .with_to(to)
            .with_value(wei);
        let pending = send_at_fees(&self.provider, tx, 0).await?;
        Ok(confirmed(pending).await?.transaction_hash)
    }

    async fn submit<P: Provider, D: CallDecoder>(
        &self,
        call: CallBuilder<P, D>,
    ) -> Result<TransactionReceipt, Error> {
        self.submit_held(call, None, 0).await
    }

    /// Sends `call`, which reveals a share on swap `id` in `reveal`'s window, its fee cap this
    /// account's reveal headroom higher: only while the window has the account's reveal margin
    /// left, judged before its turn and again in it.
    async fn reveal<P: Provider, D: CallDecoder>(
        &self,
        call: CallBuilder<P, D>,
        id: B256,
        terms: &Terms,
        reveal: Reveal,
    ) -> Result<TransactionReceipt, Error> {
        let check = Box::new(Check {
            id,
            terms: terms.clone(),
            reveal,
            margin: self.reveal_margin,
        });
        self.submit_held(call, Some(check), self.reveal_headroom)
            .await
    }

    /// Sends `call` in the account's turn, its fee cap `headroom` per gas higher, where `check`'s
    /// window, if any, is read again before it is signed.
    async fn submit_held<P: Provider, D: CallDecoder>(
        &self,
        call: CallBuilder<P, D>,
        check: Option<Box<Check>>,
        headroom: u128,
    ) -> Result<TransactionReceipt, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let call = call.from(account);
        let mut attempts = 1;
        loop {
            // Judged before its turn, so a call that would fail never holds the line up: a reveal
            // by its window first, and sent privately by that alone, as a simulation would show
            // the node its share; any other call by a simulation.
            let simulated = async {
                call.estimate_gas()
                    .await
                    .map(|_| ())
                    .map_err(Error::contract)
            };
            let judged = match &check {
                Some(check) => match self.check_window(check).await {
                    Ok(()) if self.private.is_none() => simulated.await,
                    judged => judged,
                },
                None => simulated.await,
            };
            let failed = match judged {
                Ok(()) => {
                    let _sending = self.sending.turn().await;
                    // Boxed: every send's future holds an attempt's, and requests nest several.
                    let attempt = self.attempt(call.as_ref().clone(), check.as_deref(), headroom);
                    match Box::pin(attempt).await {
                        Ok(ended) => return ended,
                        Err(late @ Error::Late(_)) => return Err(late),
                        Err(e) => e,
                    }
                }
                Err(late @ Error::Late(_)) => return Err(late),
                Err(e) => e,
            };
            // Usually an estimate made on a node that hasn't seen the transaction this one
            // depends on. If an earlier attempt did reach the network, the repeat is
            // harmless: swap moves revert, and token moves stay within this account's own.
            if attempts == SEND_ATTEMPTS {
                warn!(attempt = attempts, outcome = "send_failed", error = %failed, "transaction submission failed");
                return Err(failed);
            }
            warn!(attempt = attempts, max_attempts = SEND_ATTEMPTS, outcome = "retrying", error = %failed, "RPC rejected transaction submission");
            attempts += 1;
            tokio::time::sleep(SEND_RETRY_DELAY).await;
        }
    }

    /// One attempt at `tx`, in the account's turn: how it ended once broadcast, or why it never
    /// was.
    async fn attempt(
        &self,
        tx: TransactionRequest,
        check: Option<&Check>,
        headroom: u128,
    ) -> Result<Result<TransactionReceipt, Error>, Error> {
        if let Some(check) = check {
            self.check_window(check).await?;
        }
        Ok(match &self.private {
            None => self.land_publicly(tx, headroom).await,
            Some(private) => {
                let gas = check.map(|_| REVEAL_GAS);
                let (signed, until) = self.send_privately(private, tx, headroom, gas).await?;
                self.landed_privately(private, &signed, until).await
            }
        })
    }

    /// That `check`'s window is open and has its margin left, read in one block: a reveal that
    /// waited for its turn may have run its window down.
    async fn check_window(&self, check: &Check) -> Result<(), Error> {
        let (window, head) = self
            .reveal_window(check.id, &check.terms, check.reveal)
            .await?;
        if !window.leaves(head.timestamp.max(wall_now()), check.margin) {
            warn!(swap_id = %check.id, reveal = check.reveal.name(), outcome = "late", "the reveal's window is shut or ends too soon to send it; nothing was sent");
            return Err(Error::Late(check.reveal.name()));
        }
        Ok(())
    }

    /// Signs `tx` at the account's next nonce and the fees now, broadcasts it, and holds the
    /// account's turn until it mines: unmined for `REPRICE_BLOCKS` blocks, it goes again at the
    /// same nonce at the fees then, at least an eighth over its last, as nodes ask of a
    /// replacement. A send priced once can sit unmined for most of an hour in a surge, and a
    /// cancel or refund must land inside its window. Every hash sent for the nonce is watched, as
    /// any may be the one that mines.
    async fn land_publicly(
        &self,
        tx: TransactionRequest,
        headroom: u128,
    ) -> Result<TransactionReceipt, Error> {
        let account = self
            .account
            .ok_or_else(|| Error::Config("a read-only connection cannot send".into()))?;
        let gas = self
            .provider
            .estimate_gas(tx.clone())
            .await
            .map_err(Error::contract)?;
        let tx = tx.with_chain_id(self.chain_id().await?).with_gas_limit(gas);
        let now = fees(&self.provider).await?;
        let (mut cap, mut tip) = (now.max_fee_per_gas + headroom, now.max_priority_fee_per_gas);
        let priced = |cap, tip| {
            tx.clone()
                .with_max_fee_per_gas(cap)
                .with_max_priority_fee_per_gas(tip)
        };
        let first = self.sign_next(priced(cap, tip), 0).await?;
        let nonce = first.nonce;
        let mut sent = vec![first.hash];
        match self.provider.send_raw_transaction(&first.raw).await {
            Ok(_) => {}
            // One it answers with an error, it passed on to no one.
            Err(RpcError::ErrorResp(e)) => return Err(Error::Contract(e.to_string())),
            Err(_) => {
                warn!(transaction_hash = %first.hash, outcome = "unknown", "the node's answer was lost; the transaction may still land");
            }
        }
        info!(transaction_hash = %first.hash, outcome = "submitted", "transaction submitted; awaiting receipt");
        let started = Instant::now();
        let mut priced_at = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let mut taken = 0;
        loop {
            tokio::time::sleep(self.provider.client().poll_interval()).await;
            // The nonce before the receipts: whichever of these took it shows its receipt then.
            let mined = self
                .provider
                .get_transaction_count(account)
                .latest()
                .await
                .unwrap_or_default();
            for hash in &sent {
                if let Ok(Some(receipt)) = self.provider.get_transaction_receipt(*hash).await {
                    let transaction_hash = receipt.transaction_hash;
                    if receipt.status() {
                        info!(%transaction_hash, block_number = receipt.block_number, outcome = "mined", "transaction mined successfully");
                        return Ok(receipt);
                    }
                    warn!(%transaction_hash, block_number = receipt.block_number, outcome = "reverted", "transaction reverted on chain");
                    return Err(Error::Reverted(transaction_hash));
                }
            }
            let last = *sent.last().expect("one was sent");
            // Another of the account's transactions took the nonce: none of these can land. Seen
            // on reads in a row, as a load-balanced RPC may answer receipts a block behind.
            if mined > nonce {
                taken += 1;
                if taken == NONCE_TAKEN_READS {
                    warn!(transaction_hash = %last, outcome = "dropped", "another transaction took the nonce");
                    return Err(Error::Dropped(last));
                }
                continue;
            }
            taken = 0;
            if started.elapsed() >= LAND_TIMEOUT {
                warn!(transaction_hash = %last, outcome = "unknown", "receipt unavailable; submission may still mine");
                return Err(Error::Unconfirmed(last));
            }
            let Ok(head) = self.provider.get_block_number().await else {
                continue;
            };
            if head < priced_at + REPRICE_BLOCKS {
                continue;
            }
            let Ok(now) = fees(&self.provider).await else {
                continue;
            };
            cap = (now.max_fee_per_gas + headroom).max(bumped(cap));
            tip = now.max_priority_fee_per_gas.max(bumped(tip));
            let again = self.sign_at(priced(cap, tip), nonce).await?;
            match self.provider.send_raw_transaction(&again.raw).await {
                Ok(_) => {
                    info!(transaction_hash = %again.hash, replaces = %last, max_fee_per_gas = cap, outcome = "repriced", "transaction unmined; sent again at the fees now")
                }
                Err(e) => {
                    warn!(transaction_hash = %again.hash, outcome = "refused", "the node refused the repriced transaction: {e}")
                }
            }
            sent.push(again.hash);
            priced_at = head;
        }
    }

    /// Signs `tx` at the fees every send pays now, its fee cap `headroom` per gas higher, and
    /// broadcasts it through the private RPC alone: what went out, and the block from which,
    /// unmined, it can no longer land. An error if nothing went out. Never surfaces the nodes'
    /// errors, which can carry the transaction.
    async fn send_privately(
        &self,
        private: &Private,
        tx: TransactionRequest,
        headroom: u128,
        gas: Option<u64>,
    ) -> Result<(Signed, u64), Error> {
        let unavailable = |what: &str| Error::Contract(format!("{what} is unavailable"));
        let fees = fees(&self.provider).await?;
        let sent_at = self
            .provider
            .get_block_number()
            .await
            .map_err(|_| unavailable("the latest block"))?;
        let gas = match gas {
            Some(gas) => gas,
            None => self
                .provider
                .estimate_gas(tx.clone())
                .await
                .map_err(|_| unavailable("the simulation"))?,
        };
        let tx = tx
            .with_chain_id(self.chain_id().await?)
            .with_gas_limit(gas)
            .with_max_fee_per_gas(fees.max_fee_per_gas + headroom)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        let least_nonce = private.least_nonce.load(Ordering::Relaxed);
        let signed = self.sign_next(tx, least_nonce).await?;
        let transaction_hash = signed.hash;
        match private.provider.send_raw_transaction(&signed.raw).await {
            Ok(_) => {
                info!(%transaction_hash, outcome = "submitted", "transaction sent privately; awaiting its block");
            }
            // One it answers with an error, it passed on to no one.
            Err(RpcError::ErrorResp(_)) => {
                return Err(Error::Contract(
                    "the private RPC refused the transaction".into(),
                ));
            }
            Err(_) => {
                warn!(%transaction_hash, outcome = "unknown", "the private RPC's answer was lost; the transaction may still land");
            }
        }
        Ok((signed, sent_at + private.window()))
    }

    /// Waits, in the account's turn and reading this connection's node alone, until `signed`
    /// mines or block `until` comes without it: until then it may still land, its nonce with it.
    async fn landed_privately(
        &self,
        private: &Private,
        signed: &Signed,
        until: u64,
    ) -> Result<TransactionReceipt, Error> {
        let transaction_hash = signed.hash;
        loop {
            // The head before the receipt, so the receipt read sees at least that block.
            let past = matches!(self.provider.get_block_number().await, Ok(head) if head >= until);
            if let Ok(Some(receipt)) = self
                .provider
                .get_transaction_receipt(transaction_hash)
                .await
            {
                private
                    .least_nonce
                    .fetch_max(signed.nonce + 1, Ordering::Relaxed);
                if receipt.status() {
                    info!(%transaction_hash, block_number = receipt.block_number, outcome = "mined", "transaction mined successfully");
                    return Ok(receipt);
                }
                warn!(%transaction_hash, block_number = receipt.block_number, outcome = "reverted", "transaction reverted on chain");
                return Err(Error::Reverted(transaction_hash));
            }
            if past {
                warn!(%transaction_hash, outcome = "dropped", "the private RPC dropped the transaction unmined");
                return Err(Error::Dropped(transaction_hash));
            }
            tokio::time::sleep(self.provider.client().poll_interval()).await;
        }
    }

    /// Signs `tx` as this account at its next nonce, or `least_nonce` if more, in a turn its
    /// caller holds.
    async fn sign_next(&self, tx: TransactionRequest, least_nonce: u64) -> Result<Signed, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let nonce = self
            .provider
            .get_transaction_count(account)
            .pending()
            .await
            .map_err(|_| Error::Contract("the account's nonce is unavailable".into()))?
            .max(least_nonce);
        self.sign_at(tx, nonce).await
    }

    /// Signs `tx` as this account at `nonce`.
    async fn sign_at(&self, mut tx: TransactionRequest, nonce: u64) -> Result<Signed, Error> {
        let Some(wallet) = &self.wallet else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        tx.set_nonce(nonce);
        let envelope = tx
            .build(wallet)
            .await
            .map_err(|_| Error::Contract("cannot sign the transaction".into()))?;
        Ok(Signed {
            hash: *envelope.tx_hash(),
            nonce,
            raw: envelope.encoded_2718().into(),
        })
    }
}

/// That Flashbots Protect at `rpc_url` keeps a transaction to its builders until it lands, and
/// never lands one that reverts, as a share revealed in calldata needs: its default endpoint, not
/// `/fast`, which shows TEE searchers whole transactions, failed ones too; the hash alone shared
/// (`hint=hash`), as its default hints share logs as well; and no other parameter but those that
/// say how long and through which builders it tries, and whom its refunds pay (`blockRange`,
/// `builder`, `refund`, `originId`, `auctionTimeout`): never `canRevert`, `useMempool` or
/// `allowTee`. Protect reads parameter names in any case. Never echoes the URL, which can carry
/// a key.
pub fn check_keeps_secrets(rpc_url: &str) -> Result<(), Error> {
    let refuse = |why: &str| Err(Error::Config(format!("the private RPC {why}")));
    let url = private_url(rpc_url)?;
    protect_blocks(&url)?;
    if url.path() != "/" {
        return refuse(
            "must be Flashbots Protect's default endpoint, with no path: /fast shows TEE searchers whole transactions, failed ones too",
        );
    }
    let mut hints = Vec::new();
    for (name, value) in url.query_pairs() {
        match name.to_ascii_lowercase().as_str() {
            "hint" => hints.push(value),
            "blockrange" | "builder" | "refund" | "originid" | "auctiontimeout" => {}
            _ => {
                return refuse(
                    "URL sets a parameter other than hint, blockRange, builder, refund, originId and auctionTimeout",
                );
            }
        }
    }
    if hints.is_empty() || hints.iter().any(|hint| hint != "hash") {
        return refuse(
            "must share a transaction's hash alone, hint=hash: its default hints share logs too",
        );
    }
    Ok(())
}

/// A private RPC's URL, which no error echoes: it can carry a key.
fn private_url(rpc_url: &str) -> Result<reqwest::Url, Error> {
    reqwest::Url::parse(rpc_url)
        .map_err(|_| Error::Config("the private RPC's URL is unreadable".into()))
}

/// The blocks Flashbots Protect at `url` tries a transaction for: its `blockRange`, or 25.
/// Protect reads parameter names in any case, so which of two it would take is anyone's guess.
fn protect_blocks(url: &reqwest::Url) -> Result<u64, Error> {
    let mut ranges = url
        .query_pairs()
        .filter(|(name, _)| name.eq_ignore_ascii_case("blockRange"));
    match (ranges.next(), ranges.next()) {
        (None, _) => Ok(PROTECT_BLOCKS),
        (Some((_, blocks)), None) => blocks
            .parse()
            .ok()
            .filter(|blocks| *blocks > 0)
            .ok_or_else(|| Error::Config("the private RPC's blockRange is not positive".into())),
        (Some(_), Some(_)) => Err(Error::Config(
            "the private RPC's URL sets blockRange more than once".into(),
        )),
    }
}

/// Deploys `init_code`, creation bytecode with any constructor arguments appended.
pub async fn deploy(
    rpc_url: &str,
    signer: PrivateKeySigner,
    init_code: Vec<u8>,
) -> Result<Address, Error> {
    let provider = signing_provider(
        rpc_url,
        Some(EthereumWallet::from(signer)),
        &RpcMeter::default(),
    )?;
    let tx = TransactionRequest::default().with_deploy_code(init_code);
    confirmed(send_at_fees(&provider, tx, 0).await?)
        .await?
        .contract_address
        .ok_or_else(|| Error::Contract("deployment created no contract".into()))
}

/// Sends `tx` from a signing provider, at the fees every send pays now, its fee cap `headroom`
/// per gas higher.
async fn send_at_fees(
    provider: &DynProvider,
    tx: TransactionRequest,
    headroom: u128,
) -> Result<PendingTransactionBuilder<Ethereum>, Error> {
    let fees = fees(provider).await?;
    let tx = tx
        .with_max_fee_per_gas(fees.max_fee_per_gas + headroom)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
    provider.send_transaction(tx).await.map_err(Error::contract)
}

/// The tip the node suggests, the one its gas price is made of, under a cap of the next block's
/// base fee twice over: quotes and fees charge that gas price. alloy's own estimate tips the
/// median of recent blocks' tips, which on Sepolia, where bots tip a flat gwei, ran to a
/// thousand times the node's.
async fn fees(provider: &DynProvider) -> Result<Eip1559Estimation, Error> {
    let history = provider
        .get_fee_history(1, BlockNumberOrTag::Latest, &[])
        .await
        .map_err(Error::contract)?;
    let base_fee = history
        .next_block_base_fee()
        .ok_or_else(|| Error::Contract("the node reports no base fee".into()))?;
    let tip = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(Error::contract)?;
    Ok(eip1559(base_fee, tip))
}

/// The least a send tips. Ethereum's nodes suggest no tip at all when blocks are quiet, and on
/// 2026-10-12 a mainnet claim sent so, its share already public, waited 6m41s to be mined.
const MIN_TIP_WEI: u128 = 20_000_000;

/// A replacement's fee: an eighth over `fee`, past the tenth nodes ask of one.
fn bumped(fee: u128) -> u128 {
    fee + fee / 8 + 1
}

fn eip1559(base_fee: u128, suggested_tip: u128) -> Eip1559Estimation {
    let tip = suggested_tip.max(MIN_TIP_WEI);
    Eip1559Estimation {
        max_fee_per_gas: base_fee * EIP1559_BASE_FEE_MULTIPLIER + tip,
        max_priority_fee_per_gas: tip,
    }
}

fn signing_provider(
    rpc_url: &str,
    wallet: Option<EthereumWallet>,
    meter: &RpcMeter,
) -> Result<DynProvider, Error> {
    let url = rpc_url
        .parse()
        .map_err(|e| Error::Config(format!("RPC URL {rpc_url}: {e}")))?;
    let http = reqwest::Client::builder()
        .timeout(RPC_TIMEOUT)
        .build()
        .map_err(Error::contract)?;
    let client = ClientBuilder::default()
        .layer(meter.layer())
        .http_with_client(http, url);
    let Some(wallet) = wallet else {
        return Ok(ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_client(client)
            .erased());
    };
    // The default nonce cache advances on sends that fail, and every later transaction then
    // queues behind the gap.
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_gas_estimation()
        .with_simple_nonce_management()
        .fetch_chain_id()
        .wallet(wallet)
        .connect_client(client)
        .erased())
}

async fn confirmed(
    pending: PendingTransactionBuilder<Ethereum>,
) -> Result<TransactionReceipt, Error> {
    let transaction_hash = *pending.tx_hash();
    info!(%transaction_hash, outcome = "submitted", "transaction submitted; awaiting receipt");
    let receipt = pending
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .map_err(|error| {
            warn!(%transaction_hash, outcome = "unknown", error = %error, "receipt unavailable; submission may still mine");
            Error::Unconfirmed(transaction_hash)
        })?;
    if receipt.status() {
        info!(%transaction_hash, block_number = receipt.block_number, outcome = "mined", "transaction mined successfully");
        Ok(receipt)
    } else {
        warn!(%transaction_hash, block_number = receipt.block_number, outcome = "reverted", "transaction reverted on chain");
        Err(Error::Reverted(receipt.transaction_hash))
    }
}

fn ciphertext_words(ciphertext: &ShieldCiphertext) -> ShieldCiphertextWords {
    ShieldCiphertextWords {
        encryptedBundle: ciphertext.encrypted_bundle.map(B256::from),
        shieldKey: ciphertext.shield_key.into(),
    }
}

fn terms_words(terms: &Terms) -> TermsWords {
    TermsWords {
        maker: terms.maker.into(),
        token: terms.token.into(),
        amount: terms.amount,
        makerKey: share_words(&terms.maker_share),
        userKey: share_words(&terms.user_share),
        user: terms.user.into(),
        t0: terms.t0,
        t1: terms.t1,
        payoutNote: terms.payout_note.into(),
    }
}

fn share_words(share: &PublicShare) -> [U256; 2] {
    let bytes = share.to_affine_bytes();
    [
        U256::from_be_slice(&bytes[..32]),
        U256::from_be_slice(&bytes[32..]),
    ]
}

fn share_from_words(x: U256, y: U256) -> Result<PublicShare, Error> {
    let mut bytes = [0; 64];
    bytes[..32].copy_from_slice(&x.to_be_bytes::<32>());
    bytes[32..].copy_from_slice(&y.to_be_bytes::<32>());
    Ok(PublicShare::from_affine_bytes(&bytes)?)
}

fn decode_state(swap: IZecSwap::Swap) -> Result<Option<SwapState>, Error> {
    let stage = match swap.stage {
        0 => return Ok(None),
        1 => Stage::Open,
        2 => Stage::Ready,
        3 => Stage::Claimed,
        4 => Stage::Refunded,
        other => return Err(Error::Contract(format!("unknown stage {other}"))),
    };
    Ok(Some(SwapState {
        stage,
        terms_hash: swap.termsHash,
        secret: swap.secret.to_be_bytes(),
        paid_out: swap.paidOut,
    }))
}

/// The swap `state` shows, read against the terms it should have opened with: none if it never
/// opened, `Error::WrongTerms` if it opened on others.
pub fn on_chain(
    id: B256,
    state: Option<SwapState>,
    terms: &Terms,
) -> Result<Option<OnChainSwap>, Error> {
    let Some(state) = state else {
        return Ok(None);
    };
    if state.terms_hash != terms.hash() {
        return Err(Error::WrongTerms(id));
    }
    Ok(Some(OnChainSwap {
        stage: state.stage,
        maker: terms.maker.into(),
        user: terms.user.into(),
        token: terms.token.into(),
        amount: terms.amount,
        t0: terms.t0,
        t1: terms.t1,
        maker_share: terms.maker_share,
        user_share: terms.user_share,
        secret: state.secret,
        payout_note: (terms.payout_note != [0; 32]).then(|| terms.payout_note.into()),
        paid_out: state.paid_out,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use alloy::node_bindings::Anvil;
    use alloy::primitives::address;
    use alloy::rpc::types::Log;
    use zecswap_core::derive_maker_share;

    use super::*;

    pub(super) fn creation(artifacts: &Path, name: &str) -> Vec<u8> {
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(artifacts.join(format!("{name}.sol/{name}.json"))).unwrap(),
        )
        .unwrap();
        alloy::primitives::hex::decode(json["bytecode"]["object"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn every_turn_to_send_wakes_the_wait_for_one_once() {
        let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
            eprintln!("skipped: anvil is not installed");
            return;
        };
        let url = anvil.endpoint();
        let to = Address::repeat_byte(7);
        let woken = async |settlement: &Settlement| {
            tokio::time::timeout(Duration::from_millis(300), settlement.sent())
                .await
                .is_ok()
        };
        let funded = Settlement::connect(
            &url,
            Address::ZERO,
            PrivateKeySigner::from(anvil.keys()[0].clone()),
        )
        .unwrap();
        assert!(!woken(&funded).await, "nothing was sent");
        funded.send_eth(to, U256::from(1)).await.unwrap();
        funded.send_eth(to, U256::from(1)).await.unwrap();
        assert!(woken(&funded).await);
        assert!(!woken(&funded).await, "two sends wake it once");
        // An account with no ETH still takes its turn, and fails in it.
        let empty = Settlement::connect(
            &url,
            Address::ZERO,
            PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap(),
        )
        .unwrap();
        assert!(empty.send_eth(to, U256::from(1)).await.is_err());
        assert!(woken(&empty).await);
    }

    #[tokio::test]
    async fn eth_above_a_floor_waits_out_the_accounts_pending_transactions() {
        let Ok(anvil) = Anvil::new().try_spawn() else {
            eprintln!("skipped: anvil is not installed");
            return;
        };
        let to = Address::repeat_byte(7);
        let key = PrivateKeySigner::from(anvil.keys()[0].clone());
        let account = key.address();
        let settlement = Settlement::connect(&anvil.endpoint(), Address::ZERO, key).unwrap();
        let automine = async |on: bool| {
            settlement
                .provider
                .raw_request::<_, serde_json::Value>("evm_setAutomine".into(), (on,))
                .await
                .unwrap();
        };
        let held = async || settlement.eth_balance(account).await.unwrap();

        automine(false).await;
        let tx = TransactionRequest::default()
            .with_from(account)
            .with_to(to)
            .with_value(U256::from(1));
        let _ = send_at_fees(&settlement.provider, tx, 0).await.unwrap();
        assert_eq!(
            settlement.send_eth_above(to, U256::ZERO).await.unwrap(),
            None
        );

        automine(true).await;
        settlement
            .provider
            .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
            .await
            .unwrap();
        let before = held().await;
        let keep = before / U256::from(2);
        assert_eq!(
            settlement.send_eth_above(to, keep).await.unwrap(),
            Some(before - keep)
        );
        // Its gas came out of what it kept.
        assert!(held().await < keep);
        assert_eq!(settlement.send_eth_above(to, keep).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_call_that_would_fail_never_waits_for_the_line() {
        let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
            eprintln!("skipped: anvil is not installed");
            return;
        };
        let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
        if !artifacts.join("TestToken.sol").exists() {
            eprintln!("skipped: run `forge build` in contracts/");
            return;
        }
        let url = anvil.endpoint();
        let key = PrivateKeySigner::from(anvil.keys()[0].clone());
        let account = key.address();
        let token = deploy(&url, key.clone(), creation(&artifacts, "TestToken"))
            .await
            .unwrap();
        let mut code = creation(&artifacts, "ZecSwap");
        code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
        code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
        code.extend_from_slice(Address::repeat_byte(0x42).into_word().as_slice());
        let contract = deploy(&url, key.clone(), code).await.unwrap();
        let settlement = Settlement::connect(&url, contract, key).unwrap();
        let sent = async || {
            settlement
                .provider
                .get_transaction_count(account)
                .pending()
                .await
                .unwrap()
        };
        let before = sent().await;
        let share = |index| derive_maker_share(&[9; 32], index).unwrap();
        // No swap has these terms.
        let terms = Terms {
            maker: account.into(),
            token: token.into(),
            amount: 1,
            maker_share: share(0).public(),
            user_share: share(1).public(),
            user: account.into(),
            t0: 0,
            t1: 1,
            payout_note: [0; 32],
        };

        let line = settlement.sending.turn().await;
        let refused = tokio::time::timeout(
            Duration::from_secs(30),
            settlement.claim(B256::ZERO, &terms, &share(1)),
        )
        .await;
        assert!(matches!(refused, Ok(Err(_))), "it waited for the line");
        let mint = settlement.mint_test_token(token, account, 1);
        tokio::pin!(mint);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut mint)
                .await
                .is_err(),
            "it sent out of turn"
        );
        drop(line);
        mint.await.unwrap();
        assert_eq!(sent().await, before + 1);
    }

    fn log(address: Address, event: &impl SolEvent) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address,
                data: event.encode_log_data(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn sends_tip_at_least_the_floor_and_whatever_more_the_node_suggests() {
        let base = 116_000_000;
        let quiet = eip1559(base, 0);
        assert_eq!(quiet.max_priority_fee_per_gas, MIN_TIP_WEI);
        assert_eq!(quiet.max_fee_per_gas, 2 * base + MIN_TIP_WEI);
        let busy = eip1559(base, 2_000_000_000);
        assert_eq!(busy.max_priority_fee_per_gas, 2_000_000_000);
        assert_eq!(busy.max_fee_per_gas, 2 * base + 2_000_000_000);
    }

    #[test]
    fn a_relayers_fee_counts_once_whether_or_not_the_token_logs_transfers() {
        let relayer = address!("0x00000000000000000000000000000000000000a1");
        let token = address!("0x00000000000000000000000000000000000000b2");
        let escrow = address!("0x00000000000000000000000000000000000000c3");
        let vault = address!("0x00000000000000000000000000000000000000d4");
        let paid_out = IZecSwap::PaidOut {
            id: B256::ZERO,
            relayer,
            fee: U256::from(100_000),
        };
        let transfer = |to, value: u64| IErc20::Transfer {
            from: vault,
            to,
            value: U256::from(value),
        };
        let paid = |sender, logs: &[Log], input: Option<&[u8]>| {
            paid_to(sender, token, escrow, logs, input)
        };

        // Sepolia's test token logs no transfers: the escrow's event names the fee.
        assert_eq!(
            paid(relayer, &[log(escrow, &paid_out)], None),
            U256::from(100_000)
        );
        // A token that logs them: the transfer and the event are the same fee.
        let logs = [
            log(token, &transfer(relayer, 100_000)),
            log(token, &transfer(escrow, 9_000_000)),
            log(escrow, &paid_out),
        ];
        assert_eq!(paid(relayer, &logs, None), U256::from(100_000));
        assert_eq!(paid(vault, &[log(escrow, &paid_out)], None), U256::ZERO);

        // A sponsored funding pays its fee by a call in its Relay Adapt calldata, to the relayer's
        // own account, which may have sent it or left that to its funding account.
        let call = |data: Vec<u8>| railgun::IRelayAdapt::Call {
            to: token,
            data: data.into(),
            value: U256::ZERO,
        };
        let relay = railgun::IRelayAdapt::relayCall {
            _transactions: vec![],
            _actionData: railgun::IRelayAdapt::ActionData {
                random: Default::default(),
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: vec![
                    call(
                        IErc20::approveCall {
                            spender: escrow,
                            amount: U256::from(5_000_000),
                        }
                        .abi_encode(),
                    ),
                    call(
                        IErc20::transferCall {
                            to: relayer,
                            amount: U256::from(250_000),
                        }
                        .abi_encode(),
                    ),
                ],
            },
        }
        .abi_encode();
        let funding = address!("0x00000000000000000000000000000000000000e5");
        for sender in [relayer, funding] {
            assert_eq!(paid(sender, &[], Some(&relay)), U256::from(250_000));
        }
        let logged = [log(token, &transfer(relayer, 250_000))];
        assert_eq!(paid(relayer, &logged, Some(&relay)), U256::from(250_000));
    }
}
