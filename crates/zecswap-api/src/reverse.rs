use alloy_primitives::{Address, B256, FixedBytes};
use serde::{Deserialize, Serialize};
use zecswap_core::{PublicShare, ReverseOpen};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuoteRequest {
    pub units: u32,
    pub user: Address,
    pub refund_note: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub terms: crate::Quote,
    pub user: Address,
    pub refund_note: B256,
    pub funding_deadline: u64,
    pub ready_deadline: u64,
    pub refund_after: u64,
}

impl Quote {
    pub fn open(&self, user_share: PublicShare) -> ReverseOpen {
        ReverseOpen {
            maker: self.terms.maker.into(),
            user: self.user.into(),
            token: self.terms.token.into(),
            amount: self.terms.amount,
            maker_share: self.terms.maker_share,
            user_share,
            t0: self.ready_deadline,
            t1: self.refund_after,
            refund_note: self.refund_note.0,
            deadline: self.funding_deadline,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Authorization {
    pub swap_id: B256,
    pub deadline: u64,
    pub signature: FixedBytes<65>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Refund {
    pub swap_id: B256,
    pub secret: B256,
    pub payout: crate::relayer::Payout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    AwaitingFunding,
    ConfirmingFunding,
    SendingZec,
    AwaitingReady,
    Claiming,
    ZecAvailable,
    RefundAvailable,
    Refunding,
    Refunded,
    Expired,
}

/// Advisory progress only. The wallet verifies escrow and the ZEC deposit independently.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub swap_id: B256,
    pub phase: Phase,
    pub deposit_txid: Option<String>,
    pub ready_deadline: u64,
    pub refund_after: u64,
}
