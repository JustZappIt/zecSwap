//! Client for the ZecSwap settlement contract, on whichever EVM chain it is deployed.

mod events;
pub use events::{SwapEvent, SwapEventKind};

use std::time::Duration;

use alloy::contract::{CallBuilder, CallDecoder};
use alloy::eips::BlockNumberOrTag;
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::keccak256;
use alloy::providers::{DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use tokio::sync::Mutex;
use tracing::warn;
use zecswap_core::{PublicShare, ReverseOpen, SecretShare};
use zecswap_railgun::{ShieldCiphertext, ShieldNote};

use crate::Error;

pub use alloy::primitives::{Address, B256, U256};
pub use alloy::signers::local::PrivateKeySigner;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(180);
const SEND_ATTEMPTS: u32 = 4;
const SEND_RETRY_DELAY: Duration = Duration::from_secs(3);

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

    // `open` takes eight parameters, which the generated binding cannot shorten.
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
            address maker;
            uint64 t0;
            uint8 stage;
            bool paidOut;
            address user;
            uint64 t1;
            address token;
            uint64 claimLockUntil;
            uint128 amount;
            uint64 refundLockUntil;
            uint256 makerX;
            uint256 makerY;
            uint256 userX;
            uint256 userY;
            uint256 secret;
            bytes32 payoutNote;
        }

        function openReverse(ReverseOpenWords terms, bytes signature) external returns (bytes32);
        function reverseFunding(bytes32 id) external view returns (bytes32 refundNote, uint64 blockNumber);
        function readyWithSig(bytes32 id, uint64 deadline, bytes signature) external;
        function lockRefundWithSig(bytes32 id, uint64 deadline, bytes signature) external;
        function refundPayout(bytes32 id, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function deposit(address token, uint256 amount) external;
        function withdraw(address token, uint256 amount, address to) external;
        function open(address token, uint128 amount, uint256[2] makerKey, uint256[2] userKey, address user, uint64 t0, uint64 t1, bytes32 payoutNote) external returns (bytes32 id);
        function ready(bytes32 id) external;
        function lockClaim(bytes32 id) external;
        function lockClaimWithSig(bytes32 id, uint64 deadline, bytes signature) external;
        function claim(bytes32 id, uint256 userSecret) external;
        function payout(bytes32 id, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function rescue(bytes32 id, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint64 nonce, uint64 deadline, bytes signature) external;
        function rescueNonces(bytes32 id) external view returns (uint64);
        function lockRefund(bytes32 id) external;
        function refund(bytes32 id, uint256 makerSecret) external;
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
        function approve(address spender, uint256 amount) external returns (bool);
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

pub struct OpenRequest<'a> {
    pub token: Address,
    pub amount: u128,
    pub maker_share: &'a PublicShare,
    pub user_share: &'a PublicShare,
    pub user: Address,
    pub t0: u64,
    pub t1: u64,
    pub payout_note: Option<B256>,
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
/// only. Sends are serialized, and each reads its nonce from the chain once the previous one
/// has its receipt, so a send that fails leaves no gap behind it.
pub struct Settlement {
    provider: DynProvider,
    contract: IZecSwap::IZecSwapInstance<DynProvider>,
    account: Option<Address>,
    sending: Mutex<()>,
}

impl Settlement {
    pub fn connect(
        rpc_url: &str,
        contract: Address,
        signer: PrivateKeySigner,
    ) -> Result<Self, Error> {
        let account = signer.address();
        Ok(Self::new(
            signing_provider(rpc_url, Some(signer))?,
            contract,
            Some(account),
        ))
    }

    /// For a party with no account on the chain, which reads and leaves sending to a relayer.
    pub fn read_only(rpc_url: &str, contract: Address) -> Result<Self, Error> {
        Ok(Self::new(signing_provider(rpc_url, None)?, contract, None))
    }

    fn new(provider: DynProvider, contract: Address, account: Option<Address>) -> Self {
        Self {
            contract: IZecSwap::new(contract, provider.clone()),
            provider,
            account,
            sending: Mutex::new(()),
        }
    }

    pub fn contract(&self) -> Address {
        *self.contract.address()
    }

    /// The account this connection sends as; none for a read-only one.
    pub fn account(&self) -> Option<Address> {
        self.account
    }

    pub async fn chain_id(&self) -> Result<u64, Error> {
        self.provider.get_chain_id().await.map_err(Error::contract)
    }

    /// The latest block's timestamp: the clock the contract's deadlines run on.
    pub async fn now(&self) -> Result<u64, Error> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("no latest block".into()))?;
        Ok(block.header.timestamp)
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

    pub async fn swap(&self, id: B256) -> Result<Option<OnChainSwap>, Error> {
        let swap = self
            .contract
            .getSwap(id)
            .call()
            .await
            .map_err(Error::contract)?;
        decode_swap(swap)
    }

    pub async fn confirmed_swap(
        &self,
        id: B256,
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
        decode_swap(swap)
    }

    /// Where a swap's Railgun payout leaves from, and where Railgun sends it back.
    pub async fn vault_of(&self, id: B256) -> Result<Address, Error> {
        self.contract
            .vaultOf(id)
            .call()
            .await
            .map_err(Error::contract)
    }

    /// Railgun's proxy, which payouts shield into; zero where the contract pays accounts only.
    pub async fn railgun(&self) -> Result<Address, Error> {
        self.contract
            .RAILGUN()
            .call()
            .await
            .map_err(Error::contract)
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

    /// Whether the receipt includes activity emitted by the configured Railgun proxy.
    pub async fn uses_railgun(&self, tx: B256) -> Result<bool, Error> {
        let railgun = self.railgun().await?;
        let receipt = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("transaction receipt unavailable".into()))?;
        Ok(!railgun.is_zero()
            && receipt
                .inner
                .logs()
                .iter()
                .any(|log| log.address() == railgun))
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

    pub async fn open(&self, request: &OpenRequest<'_>) -> Result<B256, Error> {
        let call = self.contract.open(
            request.token,
            request.amount,
            share_words(request.maker_share),
            share_words(request.user_share),
            request.user,
            request.t0,
            request.t1,
            request.payout_note.unwrap_or_default(),
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

    pub async fn ready_with_sig(
        &self,
        id: B256,
        deadline: u64,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        Ok(self
            .submit(
                self.contract
                    .readyWithSig(id, deadline, signature.to_vec().into()),
            )
            .await?
            .transaction_hash)
    }

    pub async fn lock_refund_with_sig(
        &self,
        id: B256,
        deadline: u64,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        Ok(self
            .submit(
                self.contract
                    .lockRefundWithSig(id, deadline, signature.to_vec().into()),
            )
            .await?
            .transaction_hash)
    }

    pub async fn refund_payout(
        &self,
        id: B256,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        Ok(self
            .submit(self.contract.refundPayout(
                id,
                note.npk.into(),
                ciphertext_words(&note.ciphertext),
                fee,
                signature.to_vec().into(),
            ))
            .await?
            .transaction_hash)
    }

    pub async fn ready(&self, id: B256) -> Result<B256, Error> {
        Ok(self.submit(self.contract.ready(id)).await?.transaction_hash)
    }

    pub async fn lock_claim(&self, id: B256) -> Result<B256, Error> {
        Ok(self
            .submit(self.contract.lockClaim(id))
            .await?
            .transaction_hash)
    }

    /// Takes the claim lock for the swap's `user`, which signed for it.
    pub async fn lock_claim_with_sig(
        &self,
        id: B256,
        deadline: u64,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self
            .contract
            .lockClaimWithSig(id, deadline, signature.to_vec().into());
        Ok(self.submit(call).await?.transaction_hash)
    }

    /// Shields a claimed swap's amount to its committed `note`, keeping the `fee` its user
    /// signed for this account.
    pub async fn payout(
        &self,
        id: B256,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self.contract.payout(
            id,
            note.npk.into(),
            ciphertext_words(&note.ciphertext),
            fee,
            signature.to_vec().into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    /// Shields what came back to a swap's vault to a `note` its user signed for.
    pub async fn rescue(
        &self,
        id: B256,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
        authorization: zecswap_core::RescueAuthorization,
    ) -> Result<B256, Error> {
        let call = self.contract.rescue(
            id,
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

    pub async fn claim(&self, id: B256, user_secret: &SecretShare) -> Result<B256, Error> {
        let secret = U256::from_be_bytes(user_secret.to_be_bytes());
        Ok(self
            .submit(self.contract.claim(id, secret))
            .await?
            .transaction_hash)
    }

    pub async fn lock_refund(&self, id: B256) -> Result<B256, Error> {
        Ok(self
            .submit(self.contract.lockRefund(id))
            .await?
            .transaction_hash)
    }

    pub async fn refund(&self, id: B256, maker_secret: &SecretShare) -> Result<B256, Error> {
        let secret = U256::from_be_bytes(maker_secret.to_be_bytes());
        Ok(self
            .submit(self.contract.refund(id, secret))
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

    /// What gas costs now, in wei.
    pub async fn gas_price(&self) -> Result<u128, Error> {
        self.provider.get_gas_price().await.map_err(Error::contract)
    }

    pub async fn eth_balance(&self, owner: Address) -> Result<U256, Error> {
        self.provider
            .get_balance(owner)
            .await
            .map_err(Error::contract)
    }

    pub async fn send_eth(&self, to: Address, wei: U256) -> Result<B256, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let _sending = self.sending.lock().await;
        let tx = TransactionRequest::default()
            .with_from(account)
            .with_to(to)
            .with_value(wei);
        let pending = self
            .provider
            .send_transaction(tx)
            .await
            .map_err(Error::contract)?;
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
        let _sending = self.sending.lock().await;
        let mut attempts = 1;
        loop {
            match call.send().await {
                Ok(pending) => return confirmed(pending).await,
                // Usually an estimate made on a node that hasn't seen the transaction this one
                // depends on. If an earlier attempt did reach the network, the repeat is
                // harmless: swap moves revert, and token moves stay within this account's own.
                Err(e) if attempts < SEND_ATTEMPTS => {
                    warn!("retrying a transaction the RPC rejected: {e}");
                    attempts += 1;
                    tokio::time::sleep(SEND_RETRY_DELAY).await;
                }
                Err(e) => return Err(Error::contract(e)),
            }
        }
    }
}

/// Deploys `init_code`, creation bytecode with any constructor arguments appended.
pub async fn deploy(
    rpc_url: &str,
    signer: PrivateKeySigner,
    init_code: Vec<u8>,
) -> Result<Address, Error> {
    let provider = signing_provider(rpc_url, Some(signer))?;
    let tx = TransactionRequest::default().with_deploy_code(init_code);
    let pending = provider
        .send_transaction(tx)
        .await
        .map_err(Error::contract)?;
    confirmed(pending)
        .await?
        .contract_address
        .ok_or_else(|| Error::Contract("deployment created no contract".into()))
}

fn signing_provider(rpc_url: &str, signer: Option<PrivateKeySigner>) -> Result<DynProvider, Error> {
    let url = rpc_url
        .parse()
        .map_err(|e| Error::Config(format!("RPC URL {rpc_url}: {e}")))?;
    let http = reqwest::Client::builder()
        .timeout(RPC_TIMEOUT)
        .build()
        .map_err(Error::contract)?;
    let Some(signer) = signer else {
        return Ok(ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_reqwest(http, url)
            .erased());
    };
    // The default nonce cache advances on sends that fail, and every later transaction then
    // queues behind the gap.
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_gas_estimation()
        .with_simple_nonce_management()
        .fetch_chain_id()
        .wallet(EthereumWallet::from(signer))
        .connect_reqwest(http, url)
        .erased())
}

async fn confirmed(
    pending: PendingTransactionBuilder<Ethereum>,
) -> Result<TransactionReceipt, Error> {
    let receipt = pending
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .map_err(Error::contract)?;
    if receipt.status() {
        Ok(receipt)
    } else {
        Err(Error::Contract(format!(
            "transaction {} reverted",
            receipt.transaction_hash
        )))
    }
}

fn ciphertext_words(ciphertext: &ShieldCiphertext) -> ShieldCiphertextWords {
    ShieldCiphertextWords {
        encryptedBundle: ciphertext.encrypted_bundle.map(B256::from),
        shieldKey: ciphertext.shield_key.into(),
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

fn decode_swap(swap: IZecSwap::Swap) -> Result<Option<OnChainSwap>, Error> {
    let stage = match swap.stage {
        0 => return Ok(None),
        1 => Stage::Open,
        2 => Stage::Ready,
        3 => Stage::Claimed,
        4 => Stage::Refunded,
        other => return Err(Error::Contract(format!("unknown stage {other}"))),
    };
    Ok(Some(OnChainSwap {
        stage,
        maker: swap.maker,
        user: swap.user,
        token: swap.token,
        amount: swap.amount,
        t0: swap.t0,
        t1: swap.t1,
        claim_lock_until: swap.claimLockUntil,
        refund_lock_until: swap.refundLockUntil,
        maker_share: share_from_words(swap.makerX, swap.makerY)?,
        user_share: share_from_words(swap.userX, swap.userY)?,
        secret: swap.secret.to_be_bytes(),
        payout_note: (!swap.payoutNote.is_zero()).then_some(swap.payoutNote),
        paid_out: swap.paidOut,
    }))
}
