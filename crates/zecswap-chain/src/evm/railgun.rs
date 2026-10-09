//! Railgun's V2 contracts as a relayer calls them. Relay Adapt funds reverse escrows
//! (`funding`); the smart wallet's own `transact` carries a user's private send or withdrawal,
//! which a relayer sends as the user's broadcaster: the user proves it with a fee note to the
//! relayer's 0zk address as its first output, and the relayer pays the gas. The ABI is
//! @railgun-community/engine 9.8.0's (abi/V2/RelayAdapt.json, abi/V2.1/RailgunSmartWallet.json);
//! both contracts take the same `Transaction`.

use alloy::consensus::{Transaction as _, TxEnvelope};
use alloy::eips::{BlockId, Decodable2718, Encodable2718};
use alloy::network::TransactionBuilder;
use alloy::primitives::aliases::U120;
use alloy::primitives::{Bytes, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::RpcError;
use zecswap_railgun::{Keys, OutputCiphertext, ShieldNote};

use super::{Address, B256, IErc20, Settlement, U256};
use crate::Error;

/// The most calldata a relayer takes for one Railgun call.
pub const MAX_CALLDATA_BYTES: usize = 64 * 1024;
/// The most Railgun transactions one call may carry.
pub const MAX_TRANSACTIONS: usize = 16;

sol! {
    struct G1Point { uint256 x; uint256 y; }
    struct G2Point { uint256[2] x; uint256[2] y; }
    struct SnarkProof { G1Point a; G2Point b; G1Point c; }
    struct CommitmentCiphertext {
        bytes32[4] ciphertext;
        bytes32 blindedSenderViewingKey;
        bytes32 blindedReceiverViewingKey;
        bytes annotationData;
        bytes memo;
    }
    struct BoundParams {
        uint16 treeNumber;
        uint72 minGasPrice;
        uint8 unshield;
        uint64 chainID;
        address adaptContract;
        bytes32 adaptParams;
        CommitmentCiphertext[] commitmentCiphertext;
    }
    struct TokenData { uint8 tokenType; address tokenAddress; uint256 tokenSubID; }
    struct Preimage { bytes32 npk; TokenData token; uint120 value; }
    struct Ciphertext { bytes32[3] encryptedBundle; bytes32 shieldKey; }
    struct ShieldRequest { Preimage preimage; Ciphertext ciphertext; }
    struct Transaction {
        SnarkProof proof;
        bytes32 merkleRoot;
        bytes32[] nullifiers;
        bytes32[] commitments;
        BoundParams boundParams;
        Preimage unshieldPreimage;
    }

    #[sol(rpc)]
    interface IRailgunSmartWallet {
        function transact(Transaction[] _transactions) external payable;
        function shield(ShieldRequest[] _shieldRequests) external payable;
        function nullifiers(uint256 treeNumber, bytes32 nullifier) external view returns (bool);
    }

    #[sol(rpc)]
    interface IRelayAdapt {
        struct Call { address to; bytes data; uint256 value; }
        struct ActionData { bytes31 random; bool requireSuccess; uint256 minGasLimit; Call[] calls; }
        function relay(Transaction[] _transactions, ActionData _actionData) external payable;
        function shield(ShieldRequest[] _shieldRequests) external;
        function railgun() external view returns (address);
    }
}

/// What a relayer takes as the broadcaster of plain `transact` calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendPolicy {
    /// Railgun's proxy on the relayer's chain, which every send calls.
    pub railgun: Address,
    /// The token the relayer's fee is paid in.
    pub token: Address,
    /// Token base units a call's fee notes must pay the relayer in all.
    pub fee: u128,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// Nothing from this proof was sent, and nothing will be.
    #[error("{0}")]
    Rejected(&'static str),
    /// A note it spends is spent already.
    #[error("these notes are already spent")]
    Spent,
    /// Nothing was sent: the chain could not be read, or the transaction not recorded.
    #[error(transparent)]
    Chain(#[from] Error),
    /// The transaction was signed and recorded, and its broadcast failed: it may still land.
    #[error("the broadcast of {0} failed; it may still land")]
    Unknown(B256),
}

type Result<T, E = SendError> = std::result::Result<T, E>;

/// A `transact` call that passed every check needing neither keys nor the chain. Only
/// `SendPolicy::decode` makes one; it is sent as the original bytes.
pub struct Transact {
    chain_id: u64,
    railgun: Address,
    data: Bytes,
    transactions: Vec<Transaction>,
    min_gas_price: u128,
}

/// A transaction signed and ready to broadcast, as a relayer records it beforehand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub hash: B256,
    pub nonce: u64,
    pub raw: Bytes,
}

impl Signed {
    /// The call it makes: its destination, ETH value and calldata; none for bytes that are not a
    /// signed transaction with a destination.
    pub fn call(&self) -> Option<(Address, u128, Bytes)> {
        let envelope = TxEnvelope::decode_2718(&mut self.raw.as_ref()).ok()?;
        Some((
            envelope.to()?,
            envelope.value().try_into().ok()?,
            envelope.input().clone(),
        ))
    }
}

/// What the chain knows of a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Mined {
        succeeded: bool,
    },
    /// Neither mined nor pending on the node: never received, or dropped.
    Unknown,
}

impl SendPolicy {
    /// Checks the envelope, the canonical encoding and each Railgun transaction's binding: it
    /// spends notes, on this chain, with no adapt contract (only those may call `transact`
    /// directly), and unshields at most normally.
    pub fn decode(&self, chain_id: u64, to: Address, value: u128, data: Bytes) -> Result<Transact> {
        require(
            to == self.railgun && value == 0,
            "wrong destination or ETH value",
        )?;
        require(
            data.len() <= MAX_CALLDATA_BYTES,
            "the calldata is too large",
        )?;
        let call = IRailgunSmartWallet::transactCall::abi_decode_validate(&data)
            .map_err(|_| SendError::Rejected("not a Railgun transact call"))?;
        require(call.abi_encode() == data, "noncanonical transact calldata")?;
        let transactions = call._transactions;
        require(
            !transactions.is_empty() && transactions.len() <= MAX_TRANSACTIONS,
            "expected between one and sixteen Railgun transactions",
        )?;
        let mut min_gas_price = 0;
        for tx in &transactions {
            let bound = &tx.boundParams;
            require(
                !tx.nullifiers.is_empty() && !tx.commitments.is_empty(),
                "each Railgun transaction must spend notes",
            )?;
            require(bound.chainID == chain_id, "a proof is for another chain")?;
            require(
                bound.adaptContract.is_zero() && bound.adaptParams.is_zero(),
                "a proof is bound to an adapt contract",
            )?;
            require(
                bound.unshield <= 1,
                "redirected unshields are not supported",
            )?;
            min_gas_price = min_gas_price.max(bound.minGasPrice.to::<u128>());
        }
        Ok(Transact {
            chain_id,
            railgun: self.railgun,
            data,
            transactions,
            min_gas_price,
        })
    }

    /// Whether `transact` pays this relayer's fee, at least, and its proofs leave the gas price
    /// within the cap: Railgun reverts below each proof's minimum. The fee it pays, if so.
    pub fn check(&self, transact: &Transact, keys: &Keys) -> Result<u128> {
        require(
            transact.min_gas_price <= self.max_gas_price_wei,
            "a proof's minimum gas price is above the relayer's cap",
        )?;
        let paid = transact.fee_paid(keys, self.token);
        require(
            paid >= self.fee,
            "the transaction does not pay the relayer's fee",
        )?;
        Ok(paid)
    }
}

impl Transact {
    /// The calldata's hash: the same bytes are the same send.
    pub fn id(&self) -> B256 {
        keccak256(&self.data)
    }

    /// Every note it spends, as Railgun records them: by tree and nullifier.
    pub fn nullifiers(&self) -> Vec<(u16, B256)> {
        self.transactions
            .iter()
            .flat_map(|tx| {
                let tree = tx.boundParams.treeNumber;
                tx.nullifiers
                    .iter()
                    .map(move |nullifier| (tree, *nullifier))
            })
            .collect()
    }

    /// Whether it pays out of Railgun to a public address: a withdrawal, not a private send.
    pub fn unshields(&self) -> bool {
        self.transactions
            .iter()
            .any(|tx| tx.boundParams.unshield == 1)
    }

    /// What it pays the wallet of `keys` in `token`, read as Railgun's public broadcasters read
    /// their fees: the first output of each Railgun transaction, where wallets put the fee note,
    /// counted if it opens as the note committed to.
    pub fn fee_paid(&self, keys: &Keys, token: Address) -> u128 {
        self.transactions
            .iter()
            .filter_map(|tx| {
                let output = tx.boundParams.commitmentCiphertext.first()?;
                let received = keys.receive(
                    &tx.commitments.first()?.0,
                    &OutputCiphertext {
                        ciphertext: output.ciphertext.map(|word| word.0),
                        blinded_sender_viewing_key: output.blindedSenderViewingKey.0,
                        memo: output.memo.to_vec(),
                    },
                )?;
                (received.token == token.into_word().0).then_some(received.value)
            })
            .fold(0, u128::saturating_add)
    }
}

impl Settlement {
    /// Whether a note `transact` spends is spent already.
    pub async fn spends_spent_notes(&self, transact: &Transact) -> Result<bool, Error> {
        let railgun = IRailgunSmartWallet::new(transact.railgun, &self.provider);
        for (tree, nullifier) in transact.nullifiers() {
            if railgun
                .nullifiers(U256::from(tree), nullifier)
                .call()
                .await
                .map_err(|_| Error::Contract("Railgun's nullifiers are unavailable".into()))?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Sends `transact` as its broadcaster. It is simulated on the pending state at the network's
    /// gas price or the proofs' minimum, whichever is higher, within the policy's caps and this
    /// account's ETH, and sent only if its fee `covers` the gas it is estimated to burn at that
    /// price; then signed, handed to `record`, and broadcast once. No retries: a failed broadcast's
    /// outcome is unknown, and what `record` kept says what to look for.
    pub async fn send_transact(
        &self,
        policy: &SendPolicy,
        transact: &Transact,
        covers: impl FnOnce(u64, u128) -> bool,
        record: impl FnOnce(&Signed) -> Result<(), Error>,
    ) -> Result<B256> {
        require(
            transact.railgun == policy.railgun,
            "the relayer's Railgun deployment changed; post again",
        )?;
        let (Some(account), Some(wallet)) = (self.account, &self.wallet) else {
            return Err(Error::Config("a read-only connection cannot send".into()).into());
        };
        let _sending = self.sending.lock().await;
        let gas_price = self
            .provider
            .get_gas_price()
            .await
            .map_err(|_| Error::Contract("the gas price is unavailable".into()))?
            .max(transact.min_gas_price);
        require(
            gas_price <= policy.max_gas_price_wei,
            "the network's gas price is above the relayer's cap",
        )?;
        let balance = self
            .provider
            .get_balance(account)
            .await
            .map_err(|_| Error::Contract("the relayer's balance is unavailable".into()))?;
        let afford = |gas: u64| balance >= U256::from(gas) * U256::from(gas_price);
        let mut tx = TransactionRequest::default()
            .with_from(account)
            .with_to(policy.railgun)
            .with_value(U256::ZERO)
            .with_input(transact.data.clone())
            .with_chain_id(transact.chain_id)
            .with_gas_limit(policy.max_gas_limit)
            .with_gas_price(gas_price);
        // The simulation runs Railgun's own checks: the proofs, their roots and nullifiers, the
        // gas price. Never surface the node's errors: they can carry the calldata.
        let estimate = match self
            .provider
            .estimate_gas(tx.clone())
            .block(BlockId::pending())
            .await
        {
            Ok(gas) => gas,
            Err(RpcError::ErrorResp(_)) => {
                if !afford(policy.max_gas_limit) {
                    return Err(SendError::Rejected("the relayer is low on ETH"));
                }
                if self.spends_spent_notes(transact).await? {
                    return Err(SendError::Spent);
                }
                return Err(SendError::Rejected("the transaction fails in simulation"));
            }
            Err(_) => return Err(Error::Contract("the simulation is unavailable".into()).into()),
        };
        require(
            estimate <= policy.max_gas_limit,
            "the transaction needs more gas than the relayer's cap",
        )?;
        require(
            covers(estimate, gas_price),
            "the transaction does not pay the relayer's fee",
        )?;
        let gas_limit = estimate
            .saturating_add(estimate / 5)
            .min(policy.max_gas_limit);
        require(afford(gas_limit), "the relayer is low on ETH")?;
        let nonce = self
            .provider
            .get_transaction_count(account)
            .pending()
            .await
            .map_err(|_| Error::Contract("the relayer's nonce is unavailable".into()))?;
        tx.set_gas_limit(gas_limit);
        tx.set_nonce(nonce);
        let envelope = tx
            .build(wallet)
            .await
            .map_err(|_| Error::Contract("cannot sign the transaction".into()))?;
        let signed = Signed {
            hash: *envelope.tx_hash(),
            nonce,
            raw: envelope.encoded_2718().into(),
        };
        record(&signed)?;
        match self.provider.send_raw_transaction(&signed.raw).await {
            Ok(_) => {
                tracing::info!(transaction_hash = %signed.hash, "sent a Railgun transaction as its broadcaster");
                Ok(signed.hash)
            }
            Err(_) => Err(SendError::Unknown(signed.hash)),
        }
    }

    /// Shields `value` of `token` from this account to `note`, as a public wallet funds a
    /// Railgun balance.
    pub async fn shield(
        &self,
        token: Address,
        value: u128,
        note: &ShieldNote,
    ) -> Result<B256, Error> {
        let railgun = self.railgun().await?;
        self.submit(IErc20::new(token, &self.provider).approve(railgun, U256::from(value)))
            .await?;
        let request = ShieldRequest {
            preimage: Preimage {
                npk: note.npk.into(),
                token: TokenData {
                    tokenType: 0,
                    tokenAddress: token,
                    tokenSubID: U256::ZERO,
                },
                value: U120::from(value),
            },
            ciphertext: Ciphertext {
                encryptedBundle: note.ciphertext.encrypted_bundle.map(B256::from),
                shieldKey: note.ciphertext.shield_key.into(),
            },
        };
        let receipt = self
            .submit(IRailgunSmartWallet::new(railgun, &self.provider).shield(vec![request]))
            .await?;
        Ok(receipt.transaction_hash)
    }

    pub async fn transaction_status(&self, hash: B256) -> Result<Status, Error> {
        let unavailable = |_| Error::Contract("the transaction's status is unavailable".into());
        if let Some(receipt) = self
            .provider
            .get_transaction_receipt(hash)
            .await
            .map_err(unavailable)?
        {
            return Ok(Status::Mined {
                succeeded: receipt.status(),
            });
        }
        Ok(
            match self
                .provider
                .get_transaction_by_hash(hash)
                .await
                .map_err(unavailable)?
            {
                Some(_) => Status::Pending,
                None => Status::Unknown,
            },
        )
    }

    /// Whether this account's `nonce` was taken by a transaction mined at least `depth` blocks
    /// ago: a signed transaction the node doesn't know with that nonce can never land.
    pub async fn nonce_taken(&self, nonce: u64, depth: u64) -> Result<bool, Error> {
        let account = self
            .account
            .ok_or_else(|| Error::Config("a read-only connection has no nonce".into()))?;
        let unavailable = |_| Error::Contract("the relayer's nonce is unavailable".into());
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(unavailable)?;
        let Some(block) = latest.checked_sub(depth) else {
            return Ok(false);
        };
        let count = self
            .provider
            .get_transaction_count(account)
            .block_id(block.into())
            .await
            .map_err(unavailable)?;
        Ok(count > nonce)
    }

    /// Broadcasts `signed` again if its nonce is still the account's next: it then replaces
    /// nothing. Whether it was broadcast.
    pub async fn rebroadcast(&self, signed: &Signed) -> Result<bool, Error> {
        let account = self
            .account
            .ok_or_else(|| Error::Config("a read-only connection cannot send".into()))?;
        let _sending = self.sending.lock().await;
        let next = self
            .provider
            .get_transaction_count(account)
            .pending()
            .await
            .map_err(|_| Error::Contract("the relayer's nonce is unavailable".into()))?;
        if next != signed.nonce {
            return Ok(false);
        }
        let _pending = self
            .provider
            .send_raw_transaction(&signed.raw)
            .await
            .map_err(|_| Error::Contract("the broadcast failed again".into()))?;
        tracing::info!(transaction_hash = %signed.hash, "broadcast a Railgun transaction again");
        Ok(true)
    }
}

fn require(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(SendError::Rejected(reason))
    }
}
