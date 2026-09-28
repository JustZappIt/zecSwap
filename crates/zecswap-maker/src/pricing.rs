use serde::Deserialize;

const ZATOSHIS_PER_ZEC: u128 = 100_000_000;
const BASIS_POINTS: u128 = 10_000;

/// Fixed denominations: a user sells ZEC for a whole number of `unit`s of the token, so the
/// payout amounts on Base don't fingerprint individual swaps.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// Token base units per whole ZEC, before the spread.
    pub price_per_zec: u128,
    pub spread_bps: u16,
    /// Token base units per denomination.
    pub unit: u128,
    pub max_units: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    /// Token base units the user receives.
    pub amount: u128,
    /// Zatoshis the user deposits, rounded up in the maker's favour.
    pub deposit_zat: u64,
}

impl Pricing {
    pub fn reverse_terms(&self, units: u32) -> Option<Terms> {
        if units == 0
            || units > self.max_units
            || self.price_per_zec == 0
            || self.spread_bps >= 10_000
        {
            return None;
        }
        let amount = self.unit.checked_mul(units.into())?;
        if amount >= (1u128 << 120) {
            return None;
        }
        let deposit = amount
            .checked_mul(ZATOSHIS_PER_ZEC)?
            .checked_mul(BASIS_POINTS - u128::from(self.spread_bps))?
            / self.price_per_zec.checked_mul(BASIS_POINTS)?;
        (deposit > 0).then_some(Terms {
            amount,
            deposit_zat: deposit.try_into().ok()?,
        })
    }

    pub fn terms(&self, units: u32) -> Option<Terms> {
        if units == 0 || units > self.max_units {
            return None;
        }
        let amount = self.unit.checked_mul(units.into())?;
        let net_price = self
            .price_per_zec
            .checked_mul(BASIS_POINTS.checked_sub(self.spread_bps.into())?)?;
        if net_price == 0 {
            return None;
        }
        let deposit = amount
            .checked_mul(ZATOSHIS_PER_ZEC * BASIS_POINTS)?
            .div_ceil(net_price);
        Some(Terms {
            amount,
            deposit_zat: deposit.try_into().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> Pricing {
        Pricing {
            price_per_zec: 40_000_000,
            spread_bps: 100,
            unit: 50_000_000,
            max_units: 20,
        }
    }

    #[test]
    fn deposit_covers_the_amount_at_the_net_price() {
        let terms = pricing().terms(1).unwrap();
        assert_eq!(terms.amount, 50_000_000);
        // 50 / (40 × 0.99) ZEC, rounded up to the next zatoshi.
        assert_eq!(terms.deposit_zat, 126_262_627);
        assert_eq!(pricing().terms(3).unwrap().amount, 150_000_000);
    }

    #[test]
    fn rejects_out_of_range_units_and_spreads() {
        assert_eq!(pricing().terms(0), None);
        assert_eq!(pricing().terms(21), None);
        let all_spread = Pricing {
            spread_bps: 10_000,
            ..pricing()
        };
        assert_eq!(all_spread.terms(1), None);
        let free = Pricing {
            price_per_zec: 0,
            ..pricing()
        };
        assert_eq!(free.terms(1), None);
    }

    #[test]
    fn reverse_rounds_zec_down_and_keeps_spread_in_the_makers_favour() {
        let terms = pricing().reverse_terms(1).unwrap();
        assert_eq!(terms.amount, 50_000_000);
        assert_eq!(terms.deposit_zat, 123_750_000);
        assert!(terms.deposit_zat < pricing().terms(1).unwrap().deposit_zat);
        assert_eq!(pricing().reverse_terms(0), None);
        assert_eq!(pricing().reverse_terms(21), None);
        assert_eq!(
            Pricing {
                spread_bps: 10_000,
                ..pricing()
            }
            .reverse_terms(1),
            None
        );
        assert_eq!(
            Pricing {
                price_per_zec: 0,
                ..pricing()
            }
            .reverse_terms(1),
            None
        );
        assert_eq!(
            Pricing {
                unit: u128::MAX,
                ..pricing()
            }
            .reverse_terms(2),
            None
        );
    }
}
