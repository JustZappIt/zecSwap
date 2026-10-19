//! The Deposits contract, as a relayer sends what wallets sign for it: a deposit address's
//! EIP-3009 authorization to shield its tokens into its owner's Railgun balance, and a rescue of
//! what Railgun sent back to the address's vault. Wallets read it too: a vault's code says its
//! address has deposited, and its rescue nonce is what a rescue signs.

use alloy::eips::{BlockId, Encodable2718};
use alloy::network::TransactionBuilder;
use alloy::primitives::Bytes;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::RpcError;
use zecswap_core::{RescueAuthorization, TokenDomain};
use zecswap_railgun::ShieldNote;

use super::railgun::{Later, SendError, Signed, require};
use super::{
    Address, B256, IDeposits, IEip3009, IErc20, ISanctionsOracle, Settlement, U256,
    ciphertext_words, fees,
};
use crate::Error;

/// A Deposits deployment, as it and its token say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployment {
    pub token: Address,
    pub railgun: Address,
    /// The oracle depositors are screened against; none where nobody is, on testnets.
    pub sanctions: Option<Address>,
    /// The token's EIP-712 domain, from its own `name()` and `version()`.
    pub token_domain: TokenDomain,
}

/// What a relayer sends deposits within.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositPolicy {
    pub contract: Address,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u128,
}

/// A deposit address's signed shield, as `shield` takes it.
pub struct Shield<'a> {
    pub from: Address,
    pub value: u128,
    pub valid_after: u64,
    pub valid_before: u64,
    pub note: &'a ShieldNote,
    pub fee: u128,
    pub signature: &'a [u8; 65],
}

/// A deposit address's signed rescue, as `rescue` takes it.
pub struct Rescue<'a> {
    pub from: Address,
    pub note: &'a ShieldNote,
    pub fee: u128,
    pub authorization: RescueAuthorization,
    pub signature: &'a [u8; 65],
}

/// A deposit address as a wallet scans it: what it holds, and whether it has deposited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    pub balance: u128,
    pub deposited: bool,
}

impl Used {
    /// Whether the address has been given out: it holds tokens or has deposited.
    pub fn used(&self) -> bool {
        self.balance > 0 || self.deposited
    }
}

/// A call to the Deposits contract, as its calldata tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Shield {
        from: Address,
        value: u128,
        fee: u128,
        valid_before: u64,
    },
    Rescue {
        from: Address,
        fee: u128,
        deadline: u64,
    },
}

/// A deposit or rescue the Deposits contract recorded: whose, and the note it shielded to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landed {
    pub from: Address,
    /// `noteCommitment(npk, ciphertext)`, as `ShieldNote::commitment` computes it.
    pub note: B256,
    pub fee: u128,
    pub relayer: Address,
}

impl Shield<'_> {
    pub fn calldata(&self) -> Bytes {
        let signature = self.signature;
        IDeposits::shieldCall {
            from: self.from,
            value: U256::from(self.value),
            validAfter: U256::from(self.valid_after),
            validBefore: U256::from(self.valid_before),
            npk: self.note.npk.into(),
            ciphertext: ciphertext_words(&self.note.ciphertext),
            fee: self.fee,
            v: signature[64],
            r: B256::from_slice(&signature[..32]),
            s: B256::from_slice(&signature[32..64]),
        }
        .abi_encode()
        .into()
    }
}

impl Rescue<'_> {
    pub fn calldata(&self) -> Bytes {
        IDeposits::rescueCall {
            from: self.from,
            npk: self.note.npk.into(),
            ciphertext: ciphertext_words(&self.note.ciphertext),
            fee: self.fee,
            nonce: self.authorization.nonce,
            deadline: self.authorization.deadline,
            signature: self.signature.to_vec().into(),
        }
        .abi_encode()
        .into()
    }
}

impl Call {
    /// What `data` asks of the Deposits contract; none for anything else.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if let Ok(call) = IDeposits::shieldCall::abi_decode(data) {
            return Some(Call::Shield {
                from: call.from,
                value: call.value.try_into().ok()?,
                fee: call.fee,
                valid_before: call.validBefore.try_into().unwrap_or(u64::MAX),
            });
        }
        let call = IDeposits::rescueCall::abi_decode(data).ok()?;
        Some(Call::Rescue {
            from: call.from,
            fee: call.fee,
            deadline: call.deadline,
        })
    }

    /// The first time, in Unix seconds, from which it can no longer land: its authorization's
    /// `validBefore`, or past its rescue's deadline.
    pub fn expires_at(&self) -> u64 {
        match *self {
            Call::Shield { valid_before, .. } => valid_before,
            Call::Rescue { deadline, .. } => deadline.saturating_add(1),
        }
    }
}

impl Settlement {
    /// The Deposits deployment at `contract`: its token, Railgun and sanctions oracle, and the
    /// token's EIP-712 domain, which must be the one its `DOMAIN_SEPARATOR()` is.
    pub async fn deposits(&self, contract: Address) -> Result<Deployment, Error> {
        let deposits = IDeposits::new(contract, &self.provider);
        let token = deposits.TOKEN().call().await.map_err(Error::contract)?;
        let railgun = deposits.RAILGUN().call().await.map_err(Error::contract)?;
        let sanctions = deposits.SANCTIONS().call().await.map_err(Error::contract)?;
        let erc20 = IEip3009::new(token, &self.provider);
        let token_domain = TokenDomain {
            name: erc20.name().call().await.map_err(Error::contract)?,
            version: erc20.version().call().await.map_err(Error::contract)?,
            chain_id: self.chain_id().await?,
            token: token.into(),
        };
        let separator = erc20
            .DOMAIN_SEPARATOR()
            .call()
            .await
            .map_err(Error::contract)?;
        if separator != B256::from(token_domain.separator()) {
            return Err(Error::Contract(format!(
                "{token}'s DOMAIN_SEPARATOR is not that of its name {:?} and version {:?}",
                token_domain.name, token_domain.version
            )));
        }
        Ok(Deployment {
            token,
            railgun,
            sanctions: (!sanctions.is_zero()).then_some(sanctions),
            token_domain,
        })
    }

    /// What each of `owners` holds of `token` and whether it has deposited through `contract`,
    /// in one call for every fifty where Multicall3 is deployed.
    pub async fn deposit_addresses(
        &self,
        contract: Address,
        token: Address,
        owners: &[Address],
    ) -> Result<Vec<Used>, Error> {
        let balance = |data: &[u8]| {
            IErc20::balanceOfCall::abi_decode_returns(data)
                .map_err(Error::contract)
                .and_then(|balance| u128::try_from(balance).map_err(Error::contract))
        };
        let deposited = |data: &[u8]| {
            IDeposits::depositedCall::abi_decode_returns(data).map_err(Error::contract)
        };
        if !self.multicall_deployed().await? {
            let mut used = Vec::with_capacity(owners.len());
            for owner in owners {
                used.push(Used {
                    balance: self.token_balance(token, *owner).await?,
                    deposited: IDeposits::new(contract, &self.provider)
                        .deposited(*owner)
                        .call()
                        .await
                        .map_err(Error::contract)?,
                });
            }
            return Ok(used);
        }
        let calls = owners.iter().flat_map(|owner| {
            [
                (token, IErc20::balanceOfCall { owner: *owner }.abi_encode()),
                (
                    contract,
                    IDeposits::depositedCall { from: *owner }.abi_encode(),
                ),
            ]
        });
        let answers = self.aggregate(calls.collect(), BlockId::latest()).await?;
        answers
            .chunks(2)
            .map(|pair| {
                Ok(Used {
                    balance: balance(&pair[0])?,
                    deposited: deposited(&pair[1])?,
                })
            })
            .collect()
    }

    /// The deposits and rescues `contract` recorded in `tx`; none while it is unmined.
    pub async fn deposits_landed(&self, contract: Address, tx: B256) -> Result<Vec<Landed>, Error> {
        let Some(receipt) = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
        else {
            return Ok(Vec::new());
        };
        Ok(receipt
            .inner
            .logs()
            .iter()
            .filter(|log| log.address() == contract)
            .filter_map(|log| match log.topics().first() {
                Some(&IDeposits::DepositShielded::SIGNATURE_HASH) => {
                    let event = IDeposits::DepositShielded::decode_log(&log.inner).ok()?;
                    Some((event.from, event.note, event.fee, event.relayer))
                }
                Some(&IDeposits::Rescued::SIGNATURE_HASH) => {
                    let event = IDeposits::Rescued::decode_log(&log.inner).ok()?;
                    Some((event.from, event.note, event.fee, event.relayer))
                }
                _ => None,
            })
            .map(|(from, note, fee, relayer)| Landed {
                from,
                note,
                fee: fee.saturating_to(),
                relayer,
            })
            .collect())
    }

    pub async fn has_code(&self, address: Address) -> Result<bool, Error> {
        let code = self
            .provider
            .get_code_at(address)
            .await
            .map_err(Error::contract)?;
        Ok(!code.is_empty())
    }

    /// Whether `authorizer` has used `nonce` on `token`, landing or cancelling an authorization.
    pub async fn authorization_used(
        &self,
        token: Address,
        authorizer: Address,
        nonce: B256,
    ) -> Result<bool, Error> {
        IEip3009::new(token, &self.provider)
            .authorizationState(authorizer, nonce)
            .call()
            .await
            .map_err(Error::contract)
    }

    pub async fn sanctioned(&self, oracle: Address, address: Address) -> Result<bool, Error> {
        ISanctionsOracle::new(oracle, &self.provider)
            .isSanctioned(address)
            .call()
            .await
            .map_err(Error::contract)
    }

    /// Where `from`'s deposits shield from, and Railgun sends them back to.
    pub async fn deposit_vault(&self, contract: Address, from: Address) -> Result<Address, Error> {
        IDeposits::new(contract, &self.provider)
            .vaultOf(from)
            .call()
            .await
            .map_err(Error::contract)
    }

    /// The nonce `from`'s next rescue signs.
    pub async fn deposit_rescue_nonce(
        &self,
        contract: Address,
        from: Address,
    ) -> Result<u64, Error> {
        IDeposits::new(contract, &self.provider)
            .rescueNonces(from)
            .call()
            .await
            .map_err(Error::contract)
    }

    /// Sends `data` to the Deposits contract within `policy`. It is simulated on the pending state
    /// from this account, at the fees every send pays, and sent only if its fee `covers` the gas
    /// it is estimated to burn at the network's gas price; then signed, at a nonce of at least
    /// `least_nonce`, handed to `record`, and broadcast once, through the private RPC if there is
    /// one. No retries: a failed broadcast's outcome is unknown, and what `record` kept says what
    /// to look for.
    pub async fn send_deposit(
        &self,
        policy: &DepositPolicy,
        data: Bytes,
        least_nonce: u64,
        covers: impl FnOnce(u64, u128) -> bool,
        record: impl FnOnce(&Signed) -> Result<(), Error>,
    ) -> Result<B256, SendError> {
        let (Some(account), Some(wallet)) = (self.account, &self.wallet) else {
            return Err(Error::Config("a read-only connection cannot send".into()).into());
        };
        let gas_price = self
            .provider
            .get_gas_price()
            .await
            .map_err(|_| Error::Contract("the gas price is unavailable".into()))?;
        if gas_price > policy.max_gas_price_wei {
            return Err(SendError::Later(Later::GasPrice));
        }
        let fees = fees(&self.provider).await?;
        let balance = self
            .provider
            .get_balance(account)
            .await
            .map_err(|_| Error::Contract("the relayer's balance is unavailable".into()))?;
        let afford = |gas: u64| balance >= U256::from(gas) * U256::from(fees.max_fee_per_gas);
        let mut tx = TransactionRequest::default()
            .with_from(account)
            .with_to(policy.contract)
            .with_value(U256::ZERO)
            .with_input(data)
            .with_chain_id(self.chain_id().await?)
            .with_gas_limit(policy.max_gas_limit)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        // Never surface the node's errors: they can carry the signature.
        let estimate = match self
            .provider
            .estimate_gas(tx.clone())
            .block(BlockId::pending())
            .await
        {
            Ok(gas) => gas,
            Err(RpcError::ErrorResp(_)) if !afford(policy.max_gas_limit) => {
                return Err(SendError::Later(Later::Eth));
            }
            Err(RpcError::ErrorResp(_)) => {
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
            "the deposit's fee does not cover its gas",
        )?;
        let gas_limit = estimate
            .saturating_add(estimate / 5)
            .min(policy.max_gas_limit);
        if !afford(gas_limit) {
            return Err(SendError::Later(Later::Eth));
        }
        let _sending = self.sending.turn().await;
        let nonce = self
            .provider
            .get_transaction_count(account)
            .pending()
            .await
            .map_err(|_| Error::Contract("the relayer's nonce is unavailable".into()))?
            .max(least_nonce);
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
        match self.broadcaster().send_raw_transaction(&signed.raw).await {
            Ok(_) => {
                tracing::info!(transaction_hash = %signed.hash, "sent a deposit");
                Ok(signed.hash)
            }
            // A private RPC that answers with an error never passes the transaction on.
            Err(RpcError::ErrorResp(_)) if self.private_blocks().is_some() => {
                Err(SendError::Refused(signed.hash))
            }
            Err(_) => Err(SendError::Unknown(signed.hash)),
        }
    }

    /// Sends `amount` of `token` from this account to `to`.
    pub async fn transfer_token(
        &self,
        token: Address,
        to: Address,
        amount: u128,
    ) -> Result<B256, Error> {
        let erc20 = super::IErc20::new(token, &self.provider);
        Ok(self
            .submit(erc20.transfer(to, U256::from(amount)))
            .await?
            .transaction_hash)
    }
}
