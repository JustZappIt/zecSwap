//! Narrow sponsorship of V2 Relay Adapt reverse escrow funding. The ABI and adapt-params
//! encoding match @railgun-community/engine 9.8.0 (abi/V2/RelayAdapt.json and RelayAdaptHelper).

use alloy::network::TransactionBuilder;
use alloy::primitives::{Bytes, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use zecswap_core::{Domain, ReverseOpen, signer};

use super::{Address, B256, IErc20, IZecSwap, Settlement, U256, share_from_words, swap_id};
use crate::Error;

pub const MAX_CALLDATA_BYTES: usize = 64 * 1024;
pub const SEPOLIA_CHAIN_ID: u64 = 11_155_111;

sol! {
    #[sol(rpc)]
    interface IRelayAdapt {
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
        struct Transaction {
            SnarkProof proof;
            bytes32 merkleRoot;
            bytes32[] nullifiers;
            bytes32[] commitments;
            BoundParams boundParams;
            Preimage unshieldPreimage;
        }
        struct Call { address to; bytes data; uint256 value; }
        struct ActionData { bytes31 random; bool requireSuccess; uint256 minGasLimit; Call[] calls; }
        struct Ciphertext { bytes32[3] encryptedBundle; bytes32 shieldKey; }
        struct ShieldRequest { Preimage preimage; Ciphertext ciphertext; }
        function relay(Transaction[] _transactions, ActionData _actionData) external payable;
        function shield(ShieldRequest[] _shieldRequests) external;
        function railgun() external view returns (address);
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct FundingPolicy {
    pub relay_adapt: Address,
    pub token: Address,
    pub maker: Address,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u128,
    /// Escrow-token base units the funding must transfer to `fee_recipient` for the gas.
    pub fee: u128,
    pub fee_recipient: Address,
}

#[derive(Debug, thiserror::Error)]
pub enum FundingError {
    #[error("{0}")]
    Rejected(&'static str),
    #[error(transparent)]
    Chain(#[from] Error),
}

type Result<T> = std::result::Result<T, FundingError>;

/// Only validation can construct one. Submission uses the original, proof-bound bytes.
pub struct ValidatedFunding {
    id: B256,
    contract: Address,
    data: Bytes,
    terms: ReverseOpen,
    min_gas_price: u128,
    policy: FundingPolicy,
}

impl FundingPolicy {
    pub fn validate_config(&self, chain_id: u64, account: Address) -> Result<()> {
        require(
            chain_id == SEPOLIA_CHAIN_ID,
            "funding sponsorship is Sepolia-only",
        )?;
        require(
            !self.relay_adapt.is_zero() && !self.token.is_zero() && !self.maker.is_zero(),
            "funding addresses must be configured",
        )?;
        require(
            self.maker != account,
            "funding relayer must be separate from maker",
        )?;
        require(
            self.fee_recipient == account,
            "the funding fee must pay the submitting relayer",
        )?;
        require(
            self.max_gas_limit > 0 && self.max_gas_price_wei > 0,
            "funding gas limits must be positive",
        )
    }

    /// Checks the complete call tree, its proof binding, and the user's escrow signature.
    /// Cryptographic Railgun proof verification is performed by simulation and on-chain.
    pub fn validate(
        &self,
        domain: Domain,
        id: B256,
        to: Address,
        value: u128,
        data: Bytes,
    ) -> Result<ValidatedFunding> {
        require(
            domain.chain_id == SEPOLIA_CHAIN_ID,
            "funding sponsorship is Sepolia-only",
        )?;
        require(
            to == self.relay_adapt && value == 0,
            "wrong funding destination or ETH value",
        )?;
        require(
            data.len() <= MAX_CALLDATA_BYTES,
            "funding calldata is too large",
        )?;
        let relay = IRelayAdapt::relayCall::abi_decode_validate(&data)
            .map_err(|_| FundingError::Rejected("invalid V2 Relay Adapt calldata"))?;
        require(
            relay.abi_encode() == data,
            "noncanonical Relay Adapt calldata",
        )?;
        let action = &relay._actionData;
        require(
            action.requireSuccess,
            "funding must require every call to succeed",
        )?;
        require(
            action.minGasLimit <= U256::from(self.max_gas_limit),
            "Relay Adapt minimum gas exceeds sponsorship limit",
        )?;
        // Shield remaining token dust even when no remainder is expected.
        require(
            action.calls.len() == 4,
            "expected approve, openReverse, fee, and shield calls",
        )?;
        require(
            action.calls.iter().all(|c| c.value.is_zero()),
            "funding calls cannot send ETH",
        )?;
        let [approve_call, open_call, fee_call, shield_call] = action.calls.as_slice() else {
            unreachable!()
        };
        require(
            approve_call.to == self.token
                && open_call.to == Address::from(domain.contract)
                && fee_call.to == self.token
                && shield_call.to == self.relay_adapt,
            "unexpected funding call target",
        )?;
        let approve = IErc20::approveCall::abi_decode_validate(&approve_call.data)
            .map_err(|_| FundingError::Rejected("invalid token approval"))?;
        let open = IZecSwap::openReverseCall::abi_decode_validate(&open_call.data)
            .map_err(|_| FundingError::Rejected("invalid escrow open"))?;
        let payment = IErc20::transferCall::abi_decode_validate(&fee_call.data)
            .map_err(|_| FundingError::Rejected("invalid relayer fee"))?;
        let shield = IRelayAdapt::shieldCall::abi_decode_validate(&shield_call.data)
            .map_err(|_| FundingError::Rejected("invalid dust shield"))?;
        require(
            approve.abi_encode() == approve_call.data
                && open.abi_encode() == open_call.data
                && payment.abi_encode() == fee_call.data
                && shield.abi_encode() == shield_call.data,
            "noncanonical funding call",
        )?;
        require(
            payment.to == self.fee_recipient && payment.amount >= U256::from(self.fee),
            "funding must pay the relayer its advertised fee",
        )?;
        let words = open.terms;
        require(
            words.maker == self.maker && words.token == self.token && !words.user.is_zero(),
            "wrong escrow maker, token, or user",
        )?;
        require(
            words.amount > 0 && words.amount < (1u128 << 120) && !words.refundNote.is_zero(),
            "invalid escrow amount or refund commitment",
        )?;
        require(
            words.deadline < words.t0 && words.t0 < words.t1,
            "invalid escrow deadlines",
        )?;
        require(
            approve.spender == open_call.to && approve.amount == U256::from(words.amount),
            "approval must match the exact escrow amount and spender",
        )?;
        require(
            shield._shieldRequests.len() == 1,
            "expected one dust shield",
        )?;
        let dust = &shield._shieldRequests[0].preimage;
        require(
            erc20(&dust.token, self.token) && dust.value.is_zero() && !dust.npk.is_zero(),
            "dust shield must return the entire remaining escrow token balance",
        )?;
        let terms = ReverseOpen {
            maker: words.maker.into(),
            user: words.user.into(),
            token: words.token.into(),
            amount: words.amount,
            maker_share: share_from_words(words.makerKey[0], words.makerKey[1])
                .map_err(|_| FundingError::Rejected("invalid maker share"))?,
            user_share: share_from_words(words.userKey[0], words.userKey[1])
                .map_err(|_| FundingError::Rejected("invalid user share"))?,
            t0: words.t0,
            t1: words.t1,
            refund_note: words.refundNote.0,
            deadline: words.deadline,
        };
        require(
            id == swap_id(words.user, &terms.maker_share),
            "wrong reverse swap ID",
        )?;
        let signature = open
            .signature
            .as_ref()
            .try_into()
            .map_err(|_| FundingError::Rejected("invalid escrow signature length"))?;
        require(
            signer(&domain.open_reverse(&terms), signature) == Some(terms.user),
            "escrow is not authorized by its user",
        )?;
        require(
            !relay._transactions.is_empty() && relay._transactions.len() <= 16,
            "expected between one and sixteen Railgun transactions",
        )?;
        let nullifiers: Vec<Vec<B256>> = relay
            ._transactions
            .iter()
            .map(|t| t.nullifiers.clone())
            .collect();
        let adapt_params = keccak256(
            (
                nullifiers,
                U256::from(relay._transactions.len()),
                action.clone(),
            )
                .abi_encode_params(),
        );
        let mut unshields = 0;
        let mut min_gas_price = 0;
        for tx in &relay._transactions {
            let bound = &tx.boundParams;
            require(
                !tx.nullifiers.is_empty() && !tx.commitments.is_empty(),
                "Railgun transaction must spend notes",
            )?;
            require(
                bound.chainID == domain.chain_id
                    && bound.adaptContract == self.relay_adapt
                    && bound.adaptParams == adapt_params,
                "Railgun proof must bind this chain, adapter, and complete action",
            )?;
            min_gas_price = min_gas_price.max(bound.minGasPrice.to::<u128>());
            require(
                min_gas_price <= self.max_gas_price_wei,
                "proof gas price exceeds sponsorship limit",
            )?;
            require(
                bound.unshield <= 1,
                "redirected unshields are not supported",
            )?;
            if bound.unshield == 1 {
                let unshield = &tx.unshieldPreimage;
                require(
                    unshield.npk == self.relay_adapt.into_word()
                        && erc20(&unshield.token, self.token)
                        && !unshield.value.is_zero(),
                    "unshield must fund Relay Adapt with the escrow token",
                )?;
                unshields += 1;
            }
        }
        require(unshields > 0, "funding must unshield from Railgun")?;
        Ok(ValidatedFunding {
            id,
            contract: domain.contract.into(),
            data,
            terms,
            min_gas_price,
            policy: self.clone(),
        })
    }
}

impl Settlement {
    pub async fn check_funding_adapter(&self, adapter: Address) -> Result<()> {
        let railgun = self.railgun().await?;
        let actual = IRelayAdapt::new(adapter, &self.provider)
            .railgun()
            .call()
            .await
            .map_err(|_| {
                FundingError::Rejected("cannot read funding adapter's Railgun deployment")
            })?;
        require(
            !railgun.is_zero() && actual == railgun,
            "funding adapter uses a different Railgun deployment",
        )
    }

    /// Returns a submitted hash immediately; wallets must independently confirm the escrow.
    /// A matching escrow already on-chain is an idempotent success with no new transaction.
    /// There are no automatic send retries: after transport failure reuse the same proof and
    /// reconcile on-chain. Nullifiers and openReverse prevent a second transfer even if the
    /// original submission's outcome is unknown.
    pub async fn sponsor_reverse_funding(
        &self,
        policy: &FundingPolicy,
        request: &ValidatedFunding,
    ) -> Result<Option<B256>> {
        require(
            policy == &request.policy && self.contract() == request.contract,
            "funding policy or deployment changed; validate again",
        )?;
        let account = self
            .account
            .ok_or_else(|| Error::Config("read-only funding connection".into()))?;
        let _sending = self.sending.lock().await;
        let terms = &request.terms;
        if let Some(swap) = self.swap(request.id).await? {
            let funding = self.reverse_funding(request.id).await?;
            require(
                swap.maker == Address::from(terms.user)
                    && swap.user == Address::from(terms.maker)
                    && swap.token == Address::from(terms.token)
                    && swap.amount == terms.amount
                    && swap.maker_share == terms.user_share
                    && swap.user_share == terms.maker_share
                    && swap.t0 == terms.t0
                    && swap.t1 == terms.t1
                    && swap.payout_note.is_none()
                    && funding.is_some_and(|f| f.refund_note.0 == terms.refund_note),
                "existing escrow does not match funding authorization",
            )?;
            return Ok(None);
        }
        require(
            self.now().await? < terms.deadline,
            "funding deadline passed",
        )?;
        let gas_price = self
            .provider
            .get_gas_price()
            .await
            .map_err(|_| Error::Contract("funding gas price unavailable".into()))?
            .max(request.min_gas_price);
        require(
            gas_price <= policy.max_gas_price_wei,
            "network gas price exceeds sponsorship limit",
        )?;
        let mut tx = TransactionRequest::default()
            .with_from(account)
            .with_to(policy.relay_adapt)
            .with_value(U256::ZERO)
            .with_input(request.data.clone())
            .with_chain_id(SEPOLIA_CHAIN_ID)
            .with_gas_limit(policy.max_gas_limit)
            .with_gas_price(gas_price);
        // Pending-state simulation runs real proof, nullifier, signature and token checks.
        // Never surface raw RPC errors: they can contain the complete private payload.
        let estimate = self.provider.estimate_gas(tx.clone()).await.map_err(|_| {
            FundingError::Rejected(
                "funding simulation failed; reconcile escrow before retrying the same proof",
            )
        })?;
        require(
            estimate <= policy.max_gas_limit,
            "funding gas exceeds sponsorship limit",
        )?;
        tx.set_gas_limit(
            estimate
                .saturating_add(estimate / 5)
                .min(policy.max_gas_limit),
        );
        let pending = self.provider.send_transaction(tx).await
            .map_err(|_| Error::Contract("funding submission outcome unknown; reconcile escrow before retrying the same proof".into()))?;
        let hash = *pending.tx_hash();
        tracing::info!(swap_id = %request.id, transaction_hash = %hash, "reverse funding submitted; awaiting escrow confirmation");
        Ok(Some(hash))
    }
}

fn erc20(token: &IRelayAdapt::TokenData, address: Address) -> bool {
    token.tokenType == 0 && token.tokenAddress == address && token.tokenSubID.is_zero()
}

fn require(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(FundingError::Rejected(reason))
    }
}

#[cfg(test)]
mod tests;
