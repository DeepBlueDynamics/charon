//! Pricing and the gateway cut (spec 05). The wire unit is millisatoshi (msat).
//!
//! Pricing is cap-based and computed **up front** from `est_input_tokens` plus
//! the `max_tokens` cap — never from actual output.

use serde::{Deserialize, Serialize};

/// Default gateway markup: +10% (basis points).
pub const DEFAULT_MARKUP_BPS: u64 = 1000;
/// Default gateway floor: 21 sat.
pub const DEFAULT_FLOOR_MSAT: u64 = 21_000;

/// A fully-priced quote for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    /// What the provider earns.
    pub provider_msat: u64,
    /// What the gateway keeps (the cut).
    pub gateway_msat: u64,
    /// What the consumer pays (`provider_msat` + `gateway_msat`).
    pub total_msat: u64,
}

/// Per-model rate, from the provider's model card (spec 03/06).
#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub price_msat_per_mtok_in: u64,
    pub price_msat_per_mtok_out: u64,
}

/// Price a session up front (spec 05).
///
/// ```text
/// provider_msat = in_rate  * est_input_tokens / 1e6
///               + out_rate * max_tokens       / 1e6
/// total_msat    = max(provider_msat * (10_000 + markup_bps) / 10_000, floor_msat)
/// gateway_msat  = total_msat - provider_msat
/// ```
pub fn quote(
    rate: Rate,
    est_input_tokens: u32,
    max_tokens: u32,
    markup_bps: u64,
    floor_msat: u64,
) -> Quote {
    let provider_msat = rate.price_msat_per_mtok_in * est_input_tokens as u64 / 1_000_000
        + rate.price_msat_per_mtok_out * max_tokens as u64 / 1_000_000;
    let marked_up = provider_msat * (10_000 + markup_bps) / 10_000;
    let total_msat = marked_up.max(floor_msat);
    Quote {
        provider_msat,
        gateway_msat: total_msat - provider_msat,
        total_msat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_applies_to_tiny_requests() {
        let q = quote(
            Rate { price_msat_per_mtok_in: 200_000, price_msat_per_mtok_out: 600_000 },
            10,
            10,
            DEFAULT_MARKUP_BPS,
            DEFAULT_FLOOR_MSAT,
        );
        assert_eq!(q.total_msat, DEFAULT_FLOOR_MSAT);
        assert_eq!(q.gateway_msat + q.provider_msat, q.total_msat);
    }

    #[test]
    fn markup_is_ten_percent_above_floor() {
        let q = quote(
            Rate { price_msat_per_mtok_in: 0, price_msat_per_mtok_out: 600_000 },
            0,
            1_000_000, // 1M output tokens -> 600_000 msat provider
            DEFAULT_MARKUP_BPS,
            DEFAULT_FLOOR_MSAT,
        );
        assert_eq!(q.provider_msat, 600_000);
        assert_eq!(q.total_msat, 660_000);
        assert_eq!(q.gateway_msat, 60_000);
    }
}
