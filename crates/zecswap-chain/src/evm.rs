//! Client for the ZecSwap settlement contract, on whichever EVM chain it is deployed.

mod canonical;
mod events;
pub mod funding;
mod meter;
pub mod railgun;
pub use events::{SwapEvent, SwapEventKind};
pub use meter::{RpcMeter, RpcReport, RpcUsage, endpoint, log_usage};

use std::time::Duration;

use alloy::contract::{CallBuilder, CallDecoder};
use alloy::eips::BlockNumberOrTag;
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
        event ClaimLocked(bytes32 indexed id, uint64 until);
        event Claimed(bytes32 indexed id, uint256 userSecret);
        event PaidOut(bytes32 indexed id, address relayer, uint256 fee);
        event Rescued(bytes32 indexed id, address relayer, uint256 fee);
        event RefundLocked(bytes32 indexed id, uint64 until);
        event Refunded(bytes32 indexed id, uint256 makerSecret);
        struct Swap {
            bytes32 termsHash;
            uint8 stage;
            bool paidOut;
            uint64 claimLockUntil;
            uint64 refundLockUntil;
            uint256 secret;
        }

        function openReverse(ReverseOpenWords terms, bytes signature) external returns (bytes32);
        function reverseFunding(bytes32 id) external view returns (bytes32 refundNote, uint64 blockNumber);
        function readyWithSig(bytes32 id, TermsWords terms, uint64 deadline, bytes signature) external;
        function lockRefundWithSig(bytes32 id, TermsWords terms, uint64 deadline, bytes signature) external;
        function refundPayout(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function deposit(address token, uint256 amount) external;
        function withdraw(address token, uint256 amount, address to) external;
        function open(address token, uint128 amount, uint256[2] makerKey, uint256[2] userKey, address user, uint64 t0, uint64 t1, bytes32 payoutNote) external returns (bytes32 id);
        function ready(bytes32 id, TermsWords terms) external;
        function lockClaim(bytes32 id, TermsWords terms) external;
        function lockClaimWithSig(bytes32 id, TermsWords terms, uint64 deadline, bytes signature) external;
        function claim(bytes32 id, TermsWords terms, uint256 userSecret) external;
        function payout(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function rescue(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint64 nonce, uint64 deadline, bytes signature) external;
        function rescueNonces(bytes32 id) external view returns (uint64);
        function lockRefund(bytes32 id, TermsWords terms) external;
        function refund(bytes32 id, TermsWords terms, uint256 makerSecret) external;
        function getSwap(bytes32 id) external view returns (Swap memory);
        function vaultOf(bytes32 id) external view returns (address);
        function balanceOf(address maker, address token) external view returns (uint256);
        function LOCK_DURATION() external view returns (uint256);
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
    pub claim_lock_until: u64,
    pub refund_lock_until: u64,
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
    pub claim_lock_until: u64,
    pub refund_lock_until: u64,
    /// The share settlement revealed, big-endian; zero until then.
    pub secret: [u8; 32],
    pub paid_out: bool,
}

/// A block's number and time: the time is the clock the contract's deadlines run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub number: u64,
    pub timestamp: u64,
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
    sending: Line,
    meter: RpcMeter,
    /// The contract's Railgun proxy, which it fixes at deployment.
    railgun: OnceCell<Address>,
    /// Whether Multicall3 is deployed, so reads of many swaps or accounts take one call.
    multicall: OnceCell<bool>,
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
            sending: Line::default(),
            meter: meter.clone(),
            railgun: OnceCell::new(),
            multicall: OnceCell::new(),
        }
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

    pub async fn lock_duration(&self) -> Result<u64, Error> {
        let duration = self
            .contract
            .LOCK_DURATION()
            .call()
            .await
            .map_err(Error::contract)?;
        u64::try_from(duration).map_err(Error::contract)
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

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "lock_refund_with_sig", chain = "evm"))]
    pub async fn lock_refund_with_sig(
        &self,
        id: B256,
        terms: &Terms,
        deadline: u64,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self.contract.lockRefundWithSig(
            id,
            terms_words(terms),
            deadline,
            signature.to_vec().into(),
        );
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

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "lock_claim", chain = "evm"))]
    pub async fn lock_claim(&self, id: B256, terms: &Terms) -> Result<B256, Error> {
        let call = self.contract.lockClaim(id, terms_words(terms));
        Ok(self.submit(call).await?.transaction_hash)
    }

    /// Takes the claim lock for the swap's `user`, which signed for it.
    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "lock_claim_with_sig", chain = "evm"))]
    pub async fn lock_claim_with_sig(
        &self,
        id: B256,
        terms: &Terms,
        deadline: u64,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self.contract.lockClaimWithSig(
            id,
            terms_words(terms),
            deadline,
            signature.to_vec().into(),
        );
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
        Ok(self.submit(call).await?.transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "lock_refund", chain = "evm"))]
    pub async fn lock_refund(&self, id: B256, terms: &Terms) -> Result<B256, Error> {
        let call = self.contract.lockRefund(id, terms_words(terms));
        Ok(self.submit(call).await?.transaction_hash)
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
        Ok(self.submit(call).await?.transaction_hash)
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
        let tx = TransactionRequest::default()
            .with_from(account)
            .with_to(to)
            .with_value(wei);
        let pending = send_at_fees(&self.provider, tx).await?;
        Ok(confirmed(pending).await?.transaction_hash)
    }

    async fn submit<P: Provider, D: CallDecoder>(
        &self,
        call: CallBuilder<P, D>,
    ) -> Result<TransactionReceipt, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let call = call.from(account);
        let mut attempts = 1;
        loop {
            // Simulated before its turn, so a call that would fail never holds the line up.
            let failed = match call.estimate_gas().await {
                Ok(_) => {
                    let _sending = self.sending.turn().await;
                    match send_at_fees(&self.provider, call.as_ref().clone()).await {
                        Ok(pending) => return confirmed(pending).await,
                        Err(e) => e,
                    }
                }
                Err(e) => Error::contract(e),
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
    confirmed(send_at_fees(&provider, tx).await?)
        .await?
        .contract_address
        .ok_or_else(|| Error::Contract("deployment created no contract".into()))
}

/// Sends `tx` from a signing provider, at the fees every send pays now.
async fn send_at_fees(
    provider: &DynProvider,
    tx: TransactionRequest,
) -> Result<PendingTransactionBuilder<Ethereum>, Error> {
    let fees = fees(provider).await?;
    let tx = tx
        .with_max_fee_per_gas(fees.max_fee_per_gas)
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
        claim_lock_until: swap.claimLockUntil,
        refund_lock_until: swap.refundLockUntil,
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
        claim_lock_until: state.claim_lock_until,
        refund_lock_until: state.refund_lock_until,
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
