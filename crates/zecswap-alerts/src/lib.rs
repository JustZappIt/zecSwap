//! Telegram alerts for the maker and the relayer: the bot's client, a durable queue of messages
//! in each service's own SQLite database (`outbox`), and how the messages write amounts, times
//! and transactions.

mod outbox;
mod telegram;

use alloy_primitives::B256;

pub use outbox::{
    Notification, NotificationStatus, SCHEMA, deliver, delivered, insert, run, status,
};
pub use telegram::{SendError, Telegram};

/// `amount` base units of a token with `decimals` decimals, exactly.
pub fn units(amount: u128, decimals: u8) -> String {
    let base = 10u128.pow(u32::from(decimals));
    let fraction = format!("{:0width$}", amount % base, width = usize::from(decimals));
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        (amount / base).to_string()
    } else {
        format!("{}.{fraction}", amount / base)
    }
}

/// A time to the minute, in UTC.
pub fn time(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|t| t.format("%-d %b %H:%M UTC").to_string())
        .unwrap_or_else(|| seconds.to_string())
}

/// The transaction on its chain's explorer; none on a chain without one.
pub fn explorer(chain_id: u64, hash: B256) -> Option<String> {
    let explorer = match chain_id {
        1 => "https://etherscan.io",
        11155111 => "https://sepolia.etherscan.io",
        8453 => "https://basescan.org",
        84532 => "https://sepolia.basescan.org",
        _ => return None,
    };
    Some(format!("{explorer}/tx/{hash}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_amounts_do_not_use_floating_point() {
        assert_eq!(units(123456789, 8), "1.23456789");
        assert_eq!(units(1, 6), "0.000001");
        assert_eq!(
            units(u128::MAX, 6),
            "340282366920938463463374607431768.211455"
        );
    }

    #[test]
    fn times_read_to_the_minute() {
        assert_eq!(time(1_791_720_221), "11 Oct 12:03 UTC");
    }

    #[test]
    fn explorer_links_follow_the_chain() {
        let hash = B256::repeat_byte(3);
        assert_eq!(
            explorer(11155111, hash).unwrap(),
            format!("https://sepolia.etherscan.io/tx/{hash}")
        );
        assert_eq!(
            explorer(1, hash).unwrap(),
            format!("https://etherscan.io/tx/{hash}")
        );
        assert!(explorer(31337, hash).is_none());
    }
}
