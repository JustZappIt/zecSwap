use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;

use super::{B256, IZecSwap, Settlement};
use crate::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapEventKind {
    Opened,
    Ready,
    ClaimLocked,
    Claimed,
    PaidOut,
    Rescued,
    RefundLocked,
    Refunded,
}

impl SwapEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Ready => "ready",
            Self::ClaimLocked => "claim_locked",
            Self::Claimed => "claimed",
            Self::PaidOut => "paid_out",
            Self::Rescued => "rescued",
            Self::RefundLocked => "refund_locked",
            Self::Refunded => "refunded",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Opened => "Escrow opened / funded",
            Self::Ready => "Ready to claim",
            Self::ClaimLocked => "Claim lock",
            Self::Claimed => "Escrow claimed",
            Self::PaidOut => "Railgun payout",
            Self::Rescued => "Railgun payout recovery",
            Self::RefundLocked => "Refund lock",
            Self::Refunded => "Escrow refunded",
        }
    }

    pub fn notify(self) -> bool {
        !matches!(self, Self::Ready | Self::ClaimLocked | Self::RefundLocked)
    }
}

#[derive(Clone, Debug)]
pub struct SwapEvent {
    pub id: B256,
    pub kind: SwapEventKind,
    pub transaction_hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub log_index: u64,
}

fn signatures() -> Vec<B256> {
    vec![
        IZecSwap::Opened::SIGNATURE_HASH,
        IZecSwap::MarkedReady::SIGNATURE_HASH,
        IZecSwap::ClaimLocked::SIGNATURE_HASH,
        IZecSwap::Claimed::SIGNATURE_HASH,
        IZecSwap::PaidOut::SIGNATURE_HASH,
        IZecSwap::Rescued::SIGNATURE_HASH,
        IZecSwap::RefundLocked::SIGNATURE_HASH,
        IZecSwap::Refunded::SIGNATURE_HASH,
    ]
}

fn decode(log: &Log) -> Option<SwapEvent> {
    if log.removed {
        return None;
    }
    let signature = *log.topics().first()?;
    macro_rules! event {
        ($($name:ident => $kind:ident),+ $(,)?) => {
            $(if signature == IZecSwap::$name::SIGNATURE_HASH {
                let decoded = IZecSwap::$name::decode_log_data(log.data()).ok()?;
                return Some(SwapEvent {
                    id: decoded.id,
                    kind: SwapEventKind::$kind,
                    transaction_hash: log.transaction_hash?,
                    block_number: log.block_number?,
                    block_hash: log.block_hash?,
                    log_index: log.log_index?,
                });
            })+
        };
    }
    event! {
        Opened => Opened, MarkedReady => Ready, ClaimLocked => ClaimLocked,
        Claimed => Claimed, PaidOut => PaidOut, Rescued => Rescued,
        RefundLocked => RefundLocked, Refunded => Refunded,
    }
    None
}

impl Settlement {
    pub async fn confirmed_height(&self, confirmations: u64) -> Result<u64, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        Ok(self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?
            .saturating_sub(confirmations - 1))
    }

    pub async fn block_timestamp(&self, height: u64) -> Result<u64, Error> {
        let block = self
            .provider
            .get_block_by_number(height.into())
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("block unavailable".into()))?;
        Ok(block.header.timestamp)
    }

    /// The timestamp of the block `hash` names; none for a block the node doesn't know, such
    /// as one a reorganisation dropped.
    pub async fn block_time(&self, hash: B256) -> Result<Option<u64>, Error> {
        Ok(self
            .provider
            .get_block_by_hash(hash)
            .await
            .map_err(Error::contract)?
            .map(|block| block.header.timestamp))
    }

    /// Ten-block windows also work with providers that cap log ranges on their free tier.
    pub async fn swap_events(&self, from: u64, to: u64) -> Result<Vec<SwapEvent>, Error> {
        if to < from || to - from >= 10 {
            return Err(Error::Contract(
                "event range must contain 1 to 10 blocks".into(),
            ));
        }
        let logs = self
            .provider
            .get_logs(
                &Filter::new()
                    .address(self.contract())
                    .event_signature(signatures())
                    .from_block(from)
                    .to_block(to),
            )
            .await
            .map_err(Error::contract)?;
        let mut events = Vec::new();
        for log in logs
            .iter()
            .filter(|log| log.address() == self.contract() && !log.removed)
        {
            let event =
                decode(log).ok_or_else(|| Error::Contract("incomplete settlement event".into()))?;
            if !(from..=to).contains(&event.block_number) {
                return Err(Error::Contract(
                    "settlement event outside requested range".into(),
                ));
            }
            events.push(event);
        }
        events.sort_by_key(|event| (event.block_number, event.log_index));
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Log {
        serde_json::from_value(serde_json::json!({
            "address": "0xbd9A37F47A988AEFc4D80395727F41feb698e225",
            "topics": [IZecSwap::Claimed::SIGNATURE_HASH, B256::repeat_byte(1)],
            "data": format!("0x{}", "0".repeat(64)),
            "blockNumber": "0xb4783d", "blockHash": B256::repeat_byte(2),
            "transactionHash": B256::repeat_byte(3), "transactionIndex": "0x0",
            "logIndex": "0x1", "removed": false
        }))
        .unwrap()
    }

    #[test]
    fn only_mined_complete_events_have_transaction_references() {
        let mut log = log();
        let event = decode(&log).unwrap();
        assert_eq!(event.id, B256::repeat_byte(1));
        assert_eq!(event.transaction_hash, B256::repeat_byte(3));
        assert_eq!(event.kind, SwapEventKind::Claimed);
        log.removed = true;
        assert!(decode(&log).is_none());
        log.removed = false;
        log.transaction_hash = None;
        assert!(decode(&log).is_none());
    }

    #[test]
    fn malformed_events_are_not_transactions() {
        let mut log = log();
        log.inner.data.data = Default::default();
        assert!(decode(&log).is_none());
        assert_eq!(signatures().len(), 8);
    }

    #[test]
    fn every_settlement_event_keeps_only_the_public_transaction_reference() {
        use super::super::{Address, U256};
        let id = B256::repeat_byte(1);
        let cases = [
            (
                IZecSwap::Opened {
                    id,
                    maker: Address::repeat_byte(1),
                    user: Address::repeat_byte(2),
                    token: Address::repeat_byte(3),
                    amount: U256::from(123),
                    makerKey: [U256::ZERO; 2],
                    userKey: [U256::ZERO; 2],
                    t0: 100,
                    t1: 200,
                    payoutNote: B256::ZERO,
                }
                .encode_log_data(),
                SwapEventKind::Opened,
            ),
            (
                IZecSwap::MarkedReady { id }.encode_log_data(),
                SwapEventKind::Ready,
            ),
            (
                IZecSwap::ClaimLocked { id, until: 100 }.encode_log_data(),
                SwapEventKind::ClaimLocked,
            ),
            (
                IZecSwap::Claimed {
                    id,
                    userSecret: U256::from(99),
                }
                .encode_log_data(),
                SwapEventKind::Claimed,
            ),
            (
                IZecSwap::PaidOut {
                    id,
                    relayer: Address::ZERO,
                    fee: U256::ZERO,
                }
                .encode_log_data(),
                SwapEventKind::PaidOut,
            ),
            (
                IZecSwap::Rescued {
                    id,
                    relayer: Address::ZERO,
                    fee: U256::ZERO,
                }
                .encode_log_data(),
                SwapEventKind::Rescued,
            ),
            (
                IZecSwap::RefundLocked { id, until: 100 }.encode_log_data(),
                SwapEventKind::RefundLocked,
            ),
            (
                IZecSwap::Refunded {
                    id,
                    makerSecret: U256::from(88),
                }
                .encode_log_data(),
                SwapEventKind::Refunded,
            ),
        ];
        for (data, kind) in cases {
            let mut log = log();
            log.inner.data = data;
            let event = decode(&log).unwrap();
            assert_eq!(event.id, id);
            assert_eq!(event.kind, kind);
            assert_eq!(event.transaction_hash, B256::repeat_byte(3));
        }
    }

    #[tokio::test]
    async fn oversized_rpc_windows_and_zero_confirmations_are_rejected_before_io() {
        let settlement =
            Settlement::read_only("http://127.0.0.1:1", super::super::Address::repeat_byte(1))
                .unwrap();
        assert!(settlement.swap_events(1, 11).await.is_err());
        assert!(settlement.swap_events(2, 1).await.is_err());
        assert!(settlement.confirmed_height(0).await.is_err());
    }
}
