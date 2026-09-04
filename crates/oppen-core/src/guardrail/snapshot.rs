//! The inputs the engine reasons from, and the freshness facts that decide
//! whether it may reason from them at all.
//!
//! These are plain values rather than handles to a live feed on purpose. An
//! evaluation is a pure function of (config, intent, asset, market, exposure,
//! clock), so a refusal can be reproduced exactly from the ledger row that
//! recorded it — the same determinism requirement `docs/specs/fair-value.md`
//! §6.5 places on the quant engine.

use std::collections::BTreeMap;
use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// How much a derived number can be trusted.
///
/// The four states come from `docs/specs/fair-value.md` §13.4 divergence 24,
/// which keeps them because §7's guardrail rule is written against them: an
/// order priced off anything other than `Ok` is refused in the core. Defined
/// here rather than in the fair-value module because the guardrail is the
/// consumer that makes the distinction load-bearing; the fair-value engine
/// should produce this type or convert into it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedQuality {
    #[default]
    Ok,
    /// Not enough observations yet — below `weight_min_obs`.
    Warmup,
    /// Some components are unconstructible and the estimate renormalized
    /// over what is left. §14.4 correction 7: this is the common case, not
    /// the exception — 38.6% of the mainnet universe has no book.
    Degraded,
    /// Nothing survived.
    Unusable,
}

impl FeedQuality {
    pub fn is_ok(self) -> bool {
        matches!(self, FeedQuality::Ok)
    }
}

impl fmt::Display for FeedQuality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeedQuality::Ok => f.write_str("ok"),
            FeedQuality::Warmup => f.write_str("warming up"),
            FeedQuality::Degraded => f.write_str("degraded"),
            FeedQuality::Unusable => f.write_str("unusable"),
        }
    }
}

/// The market's view of one symbol at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketRef {
    pub symbol: String,
    /// The price notional and slippage are measured against — the venue's
    /// `markPx`, which `docs/specs/fair-value.md` §14.1 says to consume and
    /// not replicate.
    ///
    /// `Option`, because §14.4 correction 2 found `midPx` is `null` on 56 of
    /// 233 mainnet assets, and correction 3 forbids the obvious fallback:
    /// `allMids` answers for exactly those assets with a frozen `markPx`
    /// (FRIEND reads 4.72 against an oracle of 0.47734). A missing price is
    /// a refusal.
    pub reference_px: Option<Decimal>,
    pub as_of_ms: u64,
    pub quality: FeedQuality,
    /// How far the reconstructed mark currently sits from the venue's
    /// `markPx`, in bps, and since when it has been outside tolerance.
    /// `None` when no divergence is being tracked.
    pub mark_divergence_bps: Option<Decimal>,
    pub mark_divergent_since_ms: Option<u64>,
}

impl MarketRef {
    /// A clean tick. Used by callers that have a good price and nothing to
    /// report about divergence.
    pub fn fresh(symbol: impl Into<String>, reference_px: Decimal, as_of_ms: u64) -> Self {
        MarketRef {
            symbol: symbol.into(),
            reference_px: Some(reference_px),
            as_of_ms,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
        }
    }
}

/// One open position in a sub-account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// Signed size, negative for a short — the venue's `szi`.
    pub szi: Decimal,
    pub entry_px: Option<Decimal>,
}

/// One sub-account's state at one instant, which by D1 is one agent's.
///
/// The same type carries the fleet aggregate for the account-wide half of
/// spec item 25; there `positions` is unused and the totals are sums across
/// sub-accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    pub as_of_ms: u64,
    /// False until the reconnect reconcile of spec item 9 has completed. The
    /// caps are measured against position sizes, so an unreconciled account
    /// means the engine does not know what it is measuring.
    pub reconciled: bool,
    pub equity_usd: Decimal,
    /// High-water mark of equity, for the drawdown budget.
    pub peak_equity_usd: Decimal,
    pub realized_pnl_today_usd: Decimal,
    pub unrealized_pnl_usd: Decimal,
    /// Start of the UTC day the PnL above is measured from.
    pub day_start_ms: u64,
    /// Sum of `|szi| × mark` across open positions, as the venue values it.
    pub total_position_notional_usd: Decimal,
    /// Keyed by coin. `BTreeMap` so anything derived from it — utilization,
    /// a refusal, a ledger row — has one serialization (`AGENTS.md`
    /// invariant 6).
    pub positions: BTreeMap<String, PositionSnapshot>,
}

/// Milliseconds in a day, for the UTC day-boundary check.
const DAY_MS: u64 = 86_400_000;

impl AccountSnapshot {
    /// Mark-to-market PnL for the day: realized plus unrealized. Negative is
    /// a loss.
    ///
    /// Saturating, because `rust_decimal`'s `+` panics on overflow and a
    /// saturated loss still trips the breaker — the fail-closed direction.
    pub fn day_pnl_usd(&self) -> Decimal {
        self.realized_pnl_today_usd
            .saturating_add(self.unrealized_pnl_usd)
    }

    /// Signed size of one position, zero when flat.
    pub fn position_szi(&self, symbol: &str) -> Decimal {
        self.positions
            .get(symbol)
            .map(|p| p.szi)
            .unwrap_or(Decimal::ZERO)
    }

    /// Whether `day_start_ms` is the UTC midnight of the day containing
    /// `now_ms`. A snapshot carrying yesterday's boundary would understate
    /// today's loss, so the engine refuses rather than reinterpreting it.
    pub fn covers_day_of(&self, now_ms: u64) -> bool {
        now_ms >= self.day_start_ms
            && self.day_start_ms % DAY_MS == 0
            && now_ms - self.day_start_ms < DAY_MS
    }
}

/// Everything spec item 25 needs: the agent's own sub-account, and the fleet
/// aggregate when account-wide limits are configured.
///
/// `fleet` is `Option` because an operator who has set no account-wide limits
/// need not compute the aggregate. The engine refuses with
/// [`super::Unevaluable::MissingFleetState`] if limits are set and it is
/// absent — a configured limit that cannot be checked is a limit that is not
/// enforced, which is the failure this whole module exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposure {
    pub agent: AccountSnapshot,
    pub fleet: Option<AccountSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(day_start_ms: u64) -> AccountSnapshot {
        AccountSnapshot {
            as_of_ms: 0,
            reconciled: true,
            equity_usd: Decimal::from(1_000),
            peak_equity_usd: Decimal::from(1_000),
            realized_pnl_today_usd: Decimal::ZERO,
            unrealized_pnl_usd: Decimal::ZERO,
            day_start_ms,
            total_position_notional_usd: Decimal::ZERO,
            positions: BTreeMap::new(),
        }
    }

    #[test]
    fn the_day_window_must_be_a_utc_midnight_containing_now() {
        // 2026-09-03T00:00:00Z
        let midnight = 1_788_998_400_000u64;
        assert_eq!(midnight % DAY_MS, 0);
        let s = snapshot(midnight);
        assert!(s.covers_day_of(midnight));
        assert!(s.covers_day_of(midnight + DAY_MS - 1));
        assert!(!s.covers_day_of(midnight + DAY_MS));
        assert!(!s.covers_day_of(midnight - 1));
        assert!(!snapshot(midnight + 1).covers_day_of(midnight + 2));
    }
}
