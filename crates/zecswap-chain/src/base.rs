//! Client for the ZecSwap settlement contract.

use std::time::Duration;

use alloy::contract::{CallBuilder, CallDecoder};
use alloy::eips::BlockNumberOrTag;
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::keccak256;
use alloy::providers::{DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use tokio::sync::Mutex;
use tracing::warn;
use zecswap_core::{PublicShare, SecretShare};

use crate::Error;

pub use alloy::primitives::{Address, B256, U256};
pub use alloy::signers::local::PrivateKeySigner;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(180);
const SEND_ATTEMPTS: u32 = 4;
const SEND_RETRY_DELAY: Duration = Duration::from_secs(3);

sol! {
    // `open` takes seven parameters, which the generated binding cannot shorten.
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    interface IZecSwap {
        struct Swap {
            address maker;
            uint64 t0;
            uint8 stage;
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
        }


        function deposit(address token, uint256 amount) external;
        function withdraw(address token, uint256 amount, address to) external;
        function open(address token, uint128 amount, uint256[2] makerKey, uint256[2] userKey, address user, uint64 t0, uint64 t1) external returns (bytes32 id);
        function ready(bytes32 id) external;
        function lockClaim(bytes32 id) external;
        function claim(bytes32 id, uint256 userSecret) external;
        function lockRefund(bytes32 id) external;
        function refund(bytes32 id, uint256 makerSecret) external;
        function getSwap(bytes32 id) external view returns (Swap memory);
        function balanceOf(address maker, address token) external view returns (uint256);
        function LOCK_DURATION() external view returns (uint256);
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
}

/// The contract keys each swap by its user share.
pub fn swap_id(user_share: &PublicShare) -> B256 {
    keccak256(user_share.to_affine_bytes())
}

/// A connection to the settlement contract, sending transactions as one account. Sends are
/// serialized, and each reads its nonce from the chain once the previous one has its receipt,
/// so a send that fails leaves no gap behind it.
pub struct Settlement {
    provider: DynProvider,
    contract: IZecSwap::IZecSwapInstance<DynProvider>,
    account: Address,
    sending: Mutex<()>,
}

impl Settlement {
    pub fn connect(
        rpc_url: &str,
        contract: Address,
        signer: PrivateKeySigner,
    ) -> Result<Self, Error> {
        let account = signer.address();
        let provider = signing_provider(rpc_url, signer)?;
        let contract = IZecSwap::new(contract, provider.clone());
        Ok(Self {
            provider,
            contract,
            account,
            sending: Mutex::new(()),
        })
    }

    pub fn contract(&self) -> Address {
        *self.contract.address()
    }

    pub fn account(&self) -> Address {
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
        }))
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
        );
        Ok(self.submit(call).await?.transaction_hash)
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

    pub async fn eth_balance(&self, owner: Address) -> Result<U256, Error> {
        self.provider
            .get_balance(owner)
            .await
            .map_err(Error::contract)
    }

    pub async fn send_eth(&self, to: Address, wei: U256) -> Result<B256, Error> {
        let _sending = self.sending.lock().await;
        let tx = TransactionRequest::default().with_to(to).with_value(wei);
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
    let provider = signing_provider(rpc_url, signer)?;
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

fn signing_provider(rpc_url: &str, signer: PrivateKeySigner) -> Result<DynProvider, Error> {
    let url = rpc_url
        .parse()
        .map_err(|e| Error::Config(format!("Base RPC URL {rpc_url}: {e}")))?;
    let http = reqwest::Client::builder()
        .timeout(RPC_TIMEOUT)
        .build()
        .map_err(Error::contract)?;
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
