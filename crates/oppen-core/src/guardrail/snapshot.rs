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
    Unusable,
}

impl FeedQuality {
    pub(super) fn is_ok(self) -> bool {
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
    /// The book snapshot this decision was taken against (`docs/decisions.md`
    /// R6). Nullable today because no capture policy is decided; the
    /// plumbing exists first because the book at the moment an agent decided
    /// is the one class of data that cannot be backfilled.
    ///
    /// It rides on the market tick rather than on [`super::OrderIntent`] on
    /// purpose: the snapshot is a fact about the market the engine measured
    /// against, so an agent must not be able to choose which one its order is
    /// recorded against.
    pub snapshot: Option<MarketSnapshotRef>,
    /// Daily volatility as a fraction of price — `0.04` is a coin that moves
    /// 4% a day — for spec F's vol-scaled cap.
    ///
    /// **A market fact carried in, never fetched here.** The module doc's rule
    /// is that an evaluation is a pure function of its inputs, so a refusal
    /// can be reproduced from the ledger row that recorded it; an engine that
    /// went and measured its own volatility would make the same order clear
    /// or refuse depending on when it was asked. So the caller supplies it,
    /// exactly as it supplies [`MarketRef::reference_px`].
    ///
    /// `None` when nobody measured it, and that is **not** the same as
    /// "volatility is zero". An agent with [`super::RiskSettings::max_risk_usd`]
    /// set is refused rather than sized against a missing number: a cap that
    /// cannot be computed is a cap that is not enforced, the treatment
    /// [`Exposure::fleet`] already gets. Unset costs nothing when no
    /// vol-scaled cap is configured, which is the default.
    pub sigma_day: Option<Decimal>,
    /// The last hour's realised volatility against what
    /// [`MarketRef::sigma_day`] implies for one hour — `oppen_core::features`'
    /// `vol_ratio`. Three means the last hour moved like three normal hours
    /// of this day.
    ///
    /// It exists because `sigma_day` is measured over twenty-four hourly
    /// bars, so a market that started moving an hour ago has barely shifted
    /// it — one bar in twenty-four — and the cap derived from it stays too
    /// wide for hours. This is the fast half of the same measurement, and the
    /// engine multiplies `sigma_day` by it rather than replacing it, so both
    /// numbers reach the refusal and neither is inferred from the other.
    ///
    /// **It can only tighten.** A ratio below one is a quiet hour, and
    /// widening a cap on a sixty-bar statistic is not something a guardrail
    /// should do, so the engine clamps at one. `None` — nobody measured it —
    /// is the same clamp, not a refusal: unlike [`MarketRef::sigma_day`],
    /// whose absence leaves the cap with no denominator, an unmeasured ratio
    /// leaves a cap that still computes and is merely untightened.
    pub vol_ratio: Option<Decimal>,
}

/// A reference to a stored book snapshot, in the shape the ledger's own
/// `SnapshotRef` chains (`docs/decisions.md` R6): the id is the primary key
/// in the prunable `book_snapshots` table, the hash goes into the chained row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSnapshotRef {
    pub id: String,
    pub hash: String,
}

impl MarketRef {
    /// A clean tick, for tests that have a good price and nothing to report
    /// about divergence. Production ticks come from the feed with the
    /// divergence fields filled in.
    #[cfg(test)]
    pub(super) fn fresh(symbol: impl Into<String>, reference_px: Decimal, as_of_ms: u64) -> Self {
        MarketRef {
            symbol: symbol.into(),
            reference_px: Some(reference_px),
            as_of_ms,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
            sigma_day: None,
            vol_ratio: None,
        }
    }
}

/// One open position in a sub-account. Signed size only — every cap is
/// measured at the reference price, so an entry price is never read here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// Signed size, negative for a short — the venue's `szi`.
    pub szi: Decimal,
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
    /// The account's working orders (spec item 24).
    ///
    /// `Option` so that "no working orders" and "nobody supplied the working
    /// orders" are different values. The engine refuses the second with
    /// [`super::Unevaluable::MissingRestingOrders`], the same fail-closed
    /// treatment [`Exposure::fleet`] gets: a cap measured against a book the
    /// engine cannot see is a cap that is not enforced. Unused on the fleet
    /// aggregate, which is only read for PnL and equity.
    pub resting: Option<RestingExposure>,
}

/// What the agent's working orders would add to its position if they all
/// filled (spec item 24).
///
/// Without this the notional and leverage caps measure only filled positions,
/// and both are bypassable by splitting: at D-c's $100 position cap and 5
/// orders per 5 minutes an agent rests five $100 orders, each of which clears
/// because each is evaluated against a flat book, and holds $500 if they fill.
/// Item 9 already fetches `frontendOpenOrders` on reconcile, so the numbers
/// exist; they simply were not reaching the engine.
///
/// **The caller's obligation, and it is load-bearing.** This must include the
/// orders spec item 7's submit queue has *sent and not yet seen acknowledged*,
/// not only the ones the venue already reports resting. A cleared order is
/// exposure the agent has committed to from the instant it is signed, and the
/// engine cannot see it: a clearance is a value the caller holds, and nothing
/// here knows when the venue took it. Report only the venue's book and the
/// same split works one step earlier — five clearances taken against one flat
/// snapshot each pass the cap alone and breach it together, inside both the
/// freshness window and the order-rate cap.
/// `super::tests::the_position_cap_holds_when_the_caller_reports_what_it_has_in_flight`
/// pins that the cap does hold once this is honoured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestingExposure {
    /// Signed resting size per coin — positive for working buys, negative for
    /// working sells, summed per coin. This is the "everything fills"
    /// reading, which is the one a cap has to be measured against.
    ///
    /// `BTreeMap` for the same reason as `positions`: one serialization
    /// (`AGENTS.md` invariant 6).
    pub szi: BTreeMap<String, Decimal>,
    /// Sum of `|szi| × mark` across every working order in the account,
    /// including symbols the current order does not touch, for the leverage
    /// cap.
    pub notional_usd: Decimal,
}

impl RestingExposure {
    /// An account with nothing working. Named rather than `default()` at call
    /// sites so "the book really is empty" is distinguishable from "nobody
    /// filled this in" — the latter is [`None`], which refuses.
    #[cfg(test)]
    pub(super) fn none() -> Self {
        RestingExposure::default()
    }

    /// Signed resting size for one coin, zero when nothing is working.
    pub(super) fn szi_of(&self, symbol: &str) -> Decimal {
        self.szi.get(symbol).copied().unwrap_or(Decimal::ZERO)
    }
}

/// Milliseconds in a day, for the UTC day-boundary check.
const DAY_MS: u64 = 86_400_000;

impl AccountSnapshot {
    /// Mark-to-market PnL for the day: realized plus unrealized. Negative is
    /// a loss.
    ///
    /// Saturating, because `rust_decimal`'s `+` panics on overflow and a
    /// saturated loss still trips the breaker — the fail-closed direction.
    pub(super) fn day_pnl_usd(&self) -> Decimal {
        self.realized_pnl_today_usd
            .saturating_add(self.unrealized_pnl_usd)
    }

    /// Signed size of one position, zero when flat. Filled size only — the
    /// working book is [`AccountSnapshot::resting`].
    pub(super) fn position_szi(&self, symbol: &str) -> Decimal {
        self.positions
            .get(symbol)
            .map(|p| p.szi)
            .unwrap_or(Decimal::ZERO)
    }

    /// Whether `day_start_ms` is the UTC midnight of the day containing
    /// `now_ms`. A snapshot carrying yesterday's boundary would understate
    /// today's loss, so the engine refuses rather than reinterpreting it.
    pub(super) fn covers_day_of(&self, now_ms: u64) -> bool {
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
            resting: Some(RestingExposure::none()),
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
