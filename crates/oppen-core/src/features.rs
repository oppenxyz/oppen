//! The deterministic numbers an agent decides from (`docs/spec.md` spec F).
//!
//! "The app computes honest deterministic numbers; agents decide. LLM-native
//! shapes only: scalars in bps / $/day / σ-units, enums, percentiles. Never
//! matrices, never pixels, never recommendations."
//!
//! Everything here is arithmetic over readings the gateway fetched. No I/O, so
//! every formula below is driven by fixtures rather than by a live venue — and
//! the ones that are not obviously right (the depth bands, the Parkinson
//! estimator, the funding annualisation) are the ones with worked tests.
//!
//! **The honesty problem this module exists to not have.** Spec F asks for
//! `depth_usd_{bid,ask}_{10,25,50}bps`, and `docs/specs/fair-value.md` §14.5
//! measured that a default `l2Book` cannot reach those bands: the 20-level
//! ladder spans a median of 13.18 bp across the top 50 mainnet symbols and
//! **2.40 bp on BTC** — $22 of an $81,183 price. A number labelled "within
//! 25 bp" computed from a book that stopped at 2.4 bp is the whole book
//! wearing a label it did not earn, and the defect is **invisible on testnet**,
//! where the thin book does span 25 bp. So every band carries whether the
//! ladder actually covered it, and [`BookFeatures`] carries how far each side
//! reached.

pub mod quotes;

use rust_decimal::{Decimal, MathematicalOps};
use serde::Serialize;

use oppen_hl::types::{AssetCtx, Bbo, Candle, L2Book};

use crate::book::{ladder_reach_bps, max_notional_within};

/// Basis points per unit. A price 1% away is 100 bps.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// The bands spec F names.
pub const DEPTH_BANDS_BPS: [u32; 3] = [10, 25, 50];

/// Hours in a year, for the funding annualisation.
const HOURS_PER_YEAR: u32 = 24 * 365;

/// Funding settles on the hour, so this is the length of a funding period.
const FUNDING_PERIOD_S: u64 = 3_600;

/// Bars the realised-vol windows read.
///
/// One-minute bars for the hour and one-hour bars for the day, so each window
/// is measured at a resolution it can actually resolve: 1,440 one-minute bars
/// would be a far larger fetch for the same 24-hour answer.
const BARS_1H: usize = 60;
const BARS_24H: usize = 24;

/// EWMA decay over the bar series.
///
/// 0.94 is the RiskMetrics daily constant, and the reason to take a published
/// one rather than fit a constant here is that a fitted λ is a model this
/// module would then own, re-estimate and defend. Spec F asks for a
/// deterministic number, not a model.
const LAMBDA: Decimal = Decimal::from_parts(94, 0, 0, false, 2);

/// `4 · ln 2`, the Parkinson denominator, to the precision `Decimal` keeps.
///
/// A constant rather than `Decimal::TWO.ln() * 4` so the estimator does not pay
/// a transcendental per bar for a value that never changes.
const FOUR_LN_2: Decimal = Decimal::from_parts(2772588722, 55511110, 0, false, 19);

/// What the book says right now.
///
/// Field order is the wire order (`AGENTS.md` invariant 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BookFeatures {
    /// `(ask − bid) / mid`, in basis points. `None` when either side is empty:
    /// there is no spread across a book with one side.
    pub spread_bps: Option<Decimal>,
    /// Notional at the touch, as `(bid − ask) / (bid + ask)`. Positive means
    /// more size resting on the bid. Measured at the touch alone, because that
    /// is the level always present when both sides are — the deeper picture is
    /// [`BookFeatures::depth`], and it is measured over stated bands.
    pub book_imbalance: Option<Decimal>,
    /// The Stoikov microprice's distance from the mid, in basis points.
    /// Positive means the size-weighted quote sits above the midpoint.
    ///
    /// `None` when no `bbo` frame has arrived for this symbol yet. It is
    /// deliberately not computed from `l2Book`: §14.4 correction 4 measured
    /// that channel at a 5.4 s median push against the 2 s threshold `micro`
    /// is defined by, so a tilt taken from it is stale by construction.
    pub micro_tilt_bps: Option<Decimal>,
    /// How far the bid ladder spans from the touch, in bps. See the module
    /// docs for why this travels with the depth.
    pub bid_reach_bps: Option<Decimal>,
    /// How far the ask ladder spans from the touch, in bps.
    pub ask_reach_bps: Option<Decimal>,
    /// One entry per band in [`DEPTH_BANDS_BPS`], in that order.
    pub depth: Vec<DepthBand>,
}

/// Resting notional within one band of the touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DepthBand {
    pub band_bps: u32,
    pub bid_usd: Decimal,
    pub ask_usd: Decimal,
    /// **Whether these figures mean what the band says.** False when the
    /// ladder stopped short of the band on either side, which makes the number
    /// the whole of that side's book rather than the band's contents — a floor
    /// on the real depth, never the depth itself.
    pub covers_band: bool,
}

/// What funding is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FundingFeatures {
    /// The venue's `funding`, in basis points: what has accrued **so far this
    /// hour**, not the hour's finished rate. Named for what it is, because
    /// [`oppen_hl::types::HourToDateRate1h`] exists to stop exactly this value
    /// being read as something else.
    pub hour_to_date_bps: Decimal,
    /// [`FundingFeatures::hour_to_date_bps`] compounded hourly over a year, as
    /// a percentage — `docs/specs/charts.md` §5.2's definition.
    ///
    /// **It reads low early in the hour**, because the rate it annualises is
    /// an hour-to-date accrual rather than a finished hourly rate: at minute
    /// six it is roughly a tenth of what the hour will settle at.
    /// [`FundingFeatures::next_funding_s`] says how far through the hour the
    /// reading is, and [`FundingFeatures::predicted_apr_pct`] is the venue's
    /// own forward number.
    pub apr_pct: Decimal,
    /// The venue's predicted rate for this asset, compounded the same way.
    /// `None` when `predictedFundings` does not carry Hyperliquid's own leg
    /// for this coin.
    pub predicted_apr_pct: Option<Decimal>,
    /// Seconds until funding settles, **derived** as
    /// `3600 − (epoch_s mod 3600)`.
    ///
    /// Not read from the venue: §14.5 measured `nextFundingTime` naming a
    /// boundary 1,064–1,898 s in the *past*, identically across all 233 coins,
    /// so a `now >= nextFundingTime` trigger built on it fires forever.
    pub next_funding_s: u64,
    /// `(mark − oracle) / oracle`, in basis points.
    pub basis_bps: Option<Decimal>,
}

/// What realised volatility is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolFeatures {
    /// EWMA-Parkinson σ over the last hour of one-minute bars, expressed as
    /// the standard deviation of an hour's return in basis points.
    pub rv_1h_bps: Option<Decimal>,
    /// The same over the last day of one-hour bars, as a day's σ in bps.
    pub rv_24h_bps: Option<Decimal>,
    /// The hour's σ against what the day implies for one hour —
    /// `rv_1h_bps / (rv_24h_bps / √24)`. One means the last hour moved like a
    /// normal hour of this day; two means it moved twice as much.
    pub vol_ratio: Option<Decimal>,
    /// Bars behind each estimate. Spec F asks for `n=` on every stat, and a σ
    /// from four bars is a different claim from one over sixty.
    pub bars_1h: usize,
    pub bars_24h: usize,
}

/// Read one book.
pub fn book_features(book: &L2Book, bbo: Option<&Bbo>) -> BookFeatures {
    let bids = book.bids();
    let asks = book.asks();
    let (best_bid, best_ask) = (bids.first(), asks.first());

    let mid = match (best_bid, best_ask) {
        (Some(bid), Some(ask)) => {
            let mid = (bid.px + ask.px) / Decimal::TWO;
            (mid > Decimal::ZERO).then_some(mid)
        }
        _ => None,
    };

    let spread_bps = match (best_bid, best_ask, mid) {
        (Some(bid), Some(ask), Some(mid)) => (ask.px - bid.px).checked_div(mid).map(|r| r * BPS),
        _ => None,
    };

    let book_imbalance = match (best_bid, best_ask) {
        (Some(bid), Some(ask)) => {
            let (bid_usd, ask_usd) = (bid.px * bid.sz, ask.px * ask.sz);
            (bid_usd - ask_usd).checked_div(bid_usd + ask_usd)
        }
        _ => None,
    };

    let bid_reach_bps = ladder_reach_bps(bids, false);
    let ask_reach_bps = ladder_reach_bps(asks, true);

    let depth = DEPTH_BANDS_BPS
        .iter()
        .map(|&band_bps| {
            let band = Decimal::from(band_bps);
            // Both sides must have spanned the band, or the pair of figures
            // describes two different windows and their sum describes neither.
            let covers_band = bid_reach_bps.is_some_and(|reach| reach >= band)
                && ask_reach_bps.is_some_and(|reach| reach >= band);
            DepthBand {
                band_bps,
                bid_usd: max_notional_within(bids, false, band_bps),
                ask_usd: max_notional_within(asks, true, band_bps),
                covers_band,
            }
        })
        .collect();

    BookFeatures {
        spread_bps,
        book_imbalance,
        micro_tilt_bps: bbo.and_then(micro_tilt_bps),
        bid_reach_bps,
        ask_reach_bps,
        depth,
    }
}

/// The Stoikov microprice's distance from the mid, in bps.
///
/// `(bid·ask_sz + ask·bid_sz) / (bid_sz + ask_sz)` against `(bid + ask)/2`.
/// The weights are crossed on purpose: size resting on the bid pulls the fair
/// quote *up*, because it is the side that has to be consumed first.
fn micro_tilt_bps(bbo: &Bbo) -> Option<Decimal> {
    let (bid, ask) = (bbo.bid()?, bbo.ask()?);
    let size = bid.sz + ask.sz;
    if size <= Decimal::ZERO {
        return None;
    }
    let mid = (bid.px + ask.px) / Decimal::TWO;
    if mid <= Decimal::ZERO {
        return None;
    }
    let micro = (bid.px * ask.sz + ask.px * bid.sz).checked_div(size)?;
    (micro - mid).checked_div(mid).map(|r| r * BPS)
}

/// Read the funding context.
///
/// `predicted` is Hyperliquid's own predicted hourly rate for this coin, where
/// `predictedFundings` carried one.
pub fn funding_features(
    ctx: &AssetCtx,
    predicted_hourly: Option<Decimal>,
    now_ms: u64,
) -> FundingFeatures {
    let hourly = ctx.funding.hour_to_date_1h();
    FundingFeatures {
        hour_to_date_bps: hourly * BPS,
        apr_pct: compound_hourly_to_apr_pct(hourly),
        predicted_apr_pct: predicted_hourly.map(compound_hourly_to_apr_pct),
        next_funding_s: FUNDING_PERIOD_S - (now_ms / 1_000) % FUNDING_PERIOD_S,
        basis_bps: (ctx.oracle_px > Decimal::ZERO)
            .then(|| (ctx.mark_px - ctx.oracle_px).checked_div(ctx.oracle_px))
            .flatten()
            .map(|r| r * BPS),
    }
}

/// `((1 + hourly)^8760 − 1) · 100`.
///
/// Compounded rather than multiplied because that is what `charts.md` §5.2
/// specifies, and because funding is charged on a position that carries its
/// own previous funding.
fn compound_hourly_to_apr_pct(hourly: Decimal) -> Decimal {
    let base = Decimal::ONE + hourly;
    if base <= Decimal::ZERO {
        // A funding rate at or below −100% per hour is not a rate this can
        // annualise: the position is gone inside the hour. Reported as zero
        // would be a lie, so the caller gets the uncompounded reading instead.
        return hourly * Decimal::from(HOURS_PER_YEAR) * Decimal::ONE_HUNDRED;
    }
    match base.checked_powu(u64::from(HOURS_PER_YEAR)) {
        Some(grown) => (grown - Decimal::ONE) * Decimal::ONE_HUNDRED,
        // A rate large enough to overflow a year of compounding is past any
        // number an agent would size against; the linear reading is the
        // honest fallback rather than a saturated one.
        None => hourly * Decimal::from(HOURS_PER_YEAR) * Decimal::ONE_HUNDRED,
    }
}

/// Read realised volatility from two bar series.
///
/// `minutes` is the last hour of one-minute candles, `hours` the last day of
/// one-hour candles, both oldest first.
pub fn vol_features(minutes: &[Candle], hours: &[Candle]) -> VolFeatures {
    let per_minute = ewma_parkinson_sigma(minutes);
    let per_hour = ewma_parkinson_sigma(hours);

    // A per-bar σ scales to a window by √(bars in it).
    let rv_1h_bps = per_minute.and_then(|sigma| scale_bps(sigma, BARS_1H));
    let rv_24h_bps = per_hour.and_then(|sigma| scale_bps(sigma, BARS_24H));

    let vol_ratio = match (rv_1h_bps, rv_24h_bps) {
        (Some(hour), Some(day)) => Decimal::from(BARS_24H as u64)
            .sqrt()
            .and_then(|root| day.checked_div(root))
            .filter(|implied| *implied > Decimal::ZERO)
            .and_then(|implied| hour.checked_div(implied)),
        _ => None,
    };

    VolFeatures {
        rv_1h_bps,
        rv_24h_bps,
        vol_ratio,
        bars_1h: minutes.len(),
        bars_24h: hours.len(),
    }
}

/// σ over one bar, EWMA-weighted across the series, oldest first.
///
/// Parkinson's estimator reads each bar's range rather than its close:
/// `σ² = ln(high/low)² / (4 ln 2)`. It is the right estimator here because a
/// close-to-close σ over sixty one-minute bars throws away everything that
/// happened inside them, which on a perp is most of the movement.
///
/// `None` when no bar carries a usable range — a series of flat bars is a
/// σ of zero and says so, but a series with no high or low at all is not a
/// measurement.
fn ewma_parkinson_sigma(bars: &[Candle]) -> Option<Decimal> {
    let mut variance: Option<Decimal> = None;
    for bar in bars {
        let Some(bar_variance) = parkinson_variance(bar) else {
            continue;
        };
        variance = Some(match variance {
            Some(previous) => LAMBDA * previous + (Decimal::ONE - LAMBDA) * bar_variance,
            // Seeded with the first usable bar rather than with zero, which
            // would make every early estimate a fraction of the truth.
            None => bar_variance,
        });
    }
    variance?.sqrt()
}

/// One bar's Parkinson variance, or `None` when its range is unusable.
fn parkinson_variance(bar: &Candle) -> Option<Decimal> {
    if bar.l <= Decimal::ZERO || bar.h < bar.l {
        return None;
    }
    let log_range = bar.h.checked_div(bar.l)?.checked_ln()?;
    log_range.checked_mul(log_range)?.checked_div(FOUR_LN_2)
}

/// A per-bar σ as a window's σ in basis points.
fn scale_bps(sigma: Decimal, bars: usize) -> Option<Decimal> {
    Decimal::from(bars as u64)
        .sqrt()
        .and_then(|root| sigma.checked_mul(root))
        .map(|window| window * BPS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::{HourToDateRate1h, Level};
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    fn book(bids: &[(&str, &str)], asks: &[(&str, &str)]) -> L2Book {
        let level = |(px, sz): &(&str, &str)| Level {
            px: d(px),
            sz: d(sz),
            n: 1,
        };
        L2Book {
            coin: "TEST".into(),
            time: 1_756_000_000_000,
            levels: [
                bids.iter().map(level).collect(),
                asks.iter().map(level).collect(),
            ],
        }
    }

    fn bbo(bid: Option<(&str, &str)>, ask: Option<(&str, &str)>) -> Bbo {
        let level = |(px, sz): (&str, &str)| Level {
            px: d(px),
            sz: d(sz),
            n: 1,
        };
        Bbo {
            coin: "TEST".into(),
            time: 1_756_000_000_000,
            bbo: [bid.map(level), ask.map(level)],
        }
    }

    fn candle(high: &str, low: &str) -> Candle {
        Candle {
            t: 0,
            t_close: 60_000,
            s: "TEST".into(),
            i: "1m".into(),
            o: d(low),
            c: d(high),
            h: d(high),
            l: d(low),
            v: d("1"),
            n: 1,
        }
    }

    fn ctx(mark: &str, oracle: &str, funding: &str) -> AssetCtx {
        AssetCtx {
            funding: HourToDateRate1h::from_hour_to_date_1h(d(funding)),
            open_interest: d("1000"),
            prev_day_px: d(mark),
            day_ntl_vlm: d("1"),
            premium: Some(Decimal::ZERO),
            oracle_px: d(oracle),
            mark_px: d(mark),
            mid_px: Some(d(mark)),
            impact_pxs: None,
        }
    }

    #[test]
    fn the_spread_is_measured_against_the_mid() {
        let features = book_features(&book(&[("99.9", "1")], &[("100.1", "1")]), None);
        // 0.2 / 100 = 20 bps.
        assert_eq!(features.spread_bps, Some(d("20")));
    }

    #[test]
    fn a_one_sided_book_has_no_spread_and_no_imbalance() {
        let features = book_features(&book(&[("99", "1")], &[]), None);
        assert_eq!(features.spread_bps, None);
        assert_eq!(
            features.book_imbalance, None,
            "nothing to be imbalanced against"
        );
    }

    /// Positive means more resting on the bid.
    #[test]
    fn imbalance_is_signed_toward_the_heavier_side() {
        let heavy_bid = book_features(&book(&[("100", "3")], &[("100", "1")]), None);
        assert_eq!(heavy_bid.book_imbalance, Some(d("0.5")));
        let heavy_ask = book_features(&book(&[("100", "1")], &[("100", "3")]), None);
        assert_eq!(heavy_ask.book_imbalance, Some(d("-0.5")));
    }

    /// **The §14.5 finding, as a test.** A BTC-shaped ladder spans 2.4 bp, so
    /// every band spec F names is out of its reach: the figures are the whole
    /// book and say so, rather than claiming to be a 25 bp measurement.
    #[test]
    fn a_ladder_that_stops_short_does_not_claim_to_cover_the_band() {
        // 20 levels a tenth of a bp apart: about 2 bp of reach on each side.
        let step = |i: u32, up: bool| {
            let delta = d("0.8") * Decimal::from(i);
            let px = if up {
                d("81183") + delta
            } else {
                d("81183") - delta
            };
            (px.to_string(), "0.5".to_string())
        };
        let bids: Vec<_> = (0..20).map(|i| step(i, false)).collect();
        let asks: Vec<_> = (0..20).map(|i| step(i, true)).collect();
        fn as_refs(rows: &[(String, String)]) -> Vec<(&str, &str)> {
            rows.iter()
                .map(|(px, sz)| (px.as_str(), sz.as_str()))
                .collect()
        }
        let features = book_features(&book(&as_refs(&bids), &as_refs(&asks)), None);

        let reach = features.ask_reach_bps.expect("a reach");
        assert!(
            reach < d("25"),
            "a BTC-shaped ladder does not span 25 bp, got {reach}"
        );
        for band in &features.depth {
            assert!(
                !band.covers_band,
                "band {} claims coverage from a {reach} bp ladder",
                band.band_bps
            );
            assert!(band.bid_usd > Decimal::ZERO, "the figure is still the book");
        }
    }

    /// The converse, and the reason the flag is not just always false: a book
    /// that genuinely spans the band says so. This is the testnet shape, where
    /// §14.5 warns the defect is invisible.
    #[test]
    fn a_ladder_that_spans_the_band_says_it_covers_it() {
        let features = book_features(
            &book(
                &[("100", "1"), ("99", "5")],
                &[("100.01", "1"), ("101", "5")],
            ),
            None,
        );
        // A 1% ladder on each side is 100 bp of reach, past every band.
        assert!(features.depth.iter().all(|band| band.covers_band));
    }

    /// Size on the bid pulls the fair quote up, because the bid is what has to
    /// be consumed first.
    #[test]
    fn the_micro_tilt_leans_toward_the_heavier_side() {
        let quote = bbo(Some(("99", "9")), Some(("101", "1")));
        let tilt = micro_tilt_bps(&quote).expect("a tilt");
        assert!(tilt > Decimal::ZERO, "heavy bid tilts up, got {tilt}");

        let flipped = bbo(Some(("99", "1")), Some(("101", "9")));
        assert!(micro_tilt_bps(&flipped).expect("a tilt") < Decimal::ZERO);
    }

    #[test]
    fn a_balanced_quote_has_no_tilt() {
        let quote = bbo(Some(("99", "5")), Some(("101", "5")));
        assert_eq!(micro_tilt_bps(&quote), Some(Decimal::ZERO));
    }

    /// A `bbo` frame with one side empty marks `micro` stale rather than
    /// zero — `docs/specs/fair-value.md` §5.2 requires that of the channel,
    /// and it has to hold of anything reading it.
    #[test]
    fn a_one_sided_quote_has_no_tilt_rather_than_a_zero() {
        assert_eq!(micro_tilt_bps(&bbo(Some(("99", "5")), None)), None);
        assert_eq!(micro_tilt_bps(&bbo(None, Some(("101", "5")))), None);
    }

    /// Without a `bbo` frame the field is absent, not filled from `l2Book`.
    #[test]
    fn no_quote_means_no_tilt_rather_than_a_substitute() {
        let features = book_features(&book(&[("99", "9")], &[("101", "1")]), None);
        assert_eq!(features.micro_tilt_bps, None);
    }

    #[test]
    fn basis_is_the_marks_distance_from_the_oracle() {
        let features = funding_features(&ctx("100.5", "100", "0"), None, 0);
        assert_eq!(features.basis_bps, Some(d("50")));
    }

    /// §14.5: the venue's own `nextFundingTime` points into the past, so this
    /// is derived from the clock and nothing else.
    #[test]
    fn the_next_funding_countdown_is_derived_from_the_hour() {
        // 12:00:00 exactly — a whole period remains.
        let on_the_hour = funding_features(
            &ctx("1", "1", "0"),
            None,
            1_756_000_000_000 / 3_600_000 * 3_600_000,
        );
        assert_eq!(on_the_hour.next_funding_s, 3_600);
        // One second later, one second less.
        let after = funding_features(
            &ctx("1", "1", "0"),
            None,
            1_756_000_000_000 / 3_600_000 * 3_600_000 + 1_000,
        );
        assert_eq!(after.next_funding_s, 3_599);
    }

    /// The units are the whole point: the venue's `funding` is a fraction per
    /// hour, and `hour_to_date_bps` is that in basis points.
    #[test]
    fn funding_carries_the_hour_to_date_rate_in_bps() {
        let features = funding_features(&ctx("1", "1", "0.000125"), None, 0);
        assert_eq!(features.hour_to_date_bps, d("1.250000"));
    }

    /// Compounded, not multiplied: a rate of 1 bp an hour is more than
    /// 8,760 bp a year.
    #[test]
    fn the_apr_compounds_rather_than_multiplying() {
        let features = funding_features(&ctx("1", "1", "0.0001"), None, 0);
        let linear = d("0.0001") * Decimal::from(8_760u32) * Decimal::ONE_HUNDRED;
        assert!(
            features.apr_pct > linear,
            "compounding must exceed {linear}, got {}",
            features.apr_pct
        );
    }

    #[test]
    fn a_predicted_rate_is_annualised_the_same_way() {
        let features = funding_features(&ctx("1", "1", "0.0001"), Some(d("0.0001")), 0);
        assert_eq!(features.predicted_apr_pct, Some(features.apr_pct));
    }

    /// A flat bar has no range, so Parkinson reads zero — which is a real
    /// measurement, not a missing one.
    #[test]
    fn flat_bars_are_a_zero_volatility_not_an_absent_one() {
        let flat: Vec<_> = (0..60).map(|_| candle("100", "100")).collect();
        let features = vol_features(&flat, &flat[..24]);
        assert_eq!(features.rv_1h_bps, Some(Decimal::ZERO));
    }

    #[test]
    fn no_bars_at_all_is_an_absent_measurement() {
        let features = vol_features(&[], &[]);
        assert_eq!(features.rv_1h_bps, None);
        assert_eq!(features.rv_24h_bps, None);
        assert_eq!(features.vol_ratio, None);
        assert_eq!(features.bars_1h, 0);
    }

    /// A worked value, so the estimator is pinned rather than merely
    /// self-consistent. Every bar ranges 100 → 101, so
    /// σ² = ln(1.01)²/(4 ln 2) and the hour's σ is that × √60.
    #[test]
    fn the_parkinson_estimator_matches_its_formula() {
        let bars: Vec<_> = (0..60).map(|_| candle("101", "100")).collect();
        let features = vol_features(&bars, &[]);

        let log_range = d("1.01").checked_ln().expect("ln");
        let expected = (log_range * log_range / FOUR_LN_2).sqrt().expect("sqrt")
            * Decimal::from(60u32).sqrt().expect("sqrt")
            * BPS;
        let got = features.rv_1h_bps.expect("a sigma");
        assert!(
            (got - expected).abs() < d("0.01"),
            "expected about {expected}, got {got}"
        );
    }

    /// The ratio is the hour against what the day implies for one hour, so a
    /// day of identical hours reads about one.
    #[test]
    fn a_steady_market_has_a_vol_ratio_near_one() {
        // Each hour ranges as much as √60 minutes of the same per-minute move,
        // which is what a random walk does.
        let minutes: Vec<_> = (0..60).map(|_| candle("100.1", "100")).collect();
        let hours: Vec<_> = (0..24).map(|_| candle("100.7746", "100")).collect();
        let features = vol_features(&minutes, &hours);
        let ratio = features.vol_ratio.expect("a ratio");
        assert!(
            ratio > d("0.8") && ratio < d("1.25"),
            "a steady market should read near one, got {ratio}"
        );
    }

    /// And an hour that moved far more than the day's average says so — the
    /// number an agent actually sizes off.
    #[test]
    fn a_violent_hour_reads_above_one() {
        let minutes: Vec<_> = (0..60).map(|_| candle("101", "100")).collect();
        let hours: Vec<_> = (0..24).map(|_| candle("100.7746", "100")).collect();
        let ratio = vol_features(&minutes, &hours).vol_ratio.expect("a ratio");
        assert!(
            ratio > d("2"),
            "a tenfold hour should read high, got {ratio}"
        );
    }

    #[test]
    fn every_stat_carries_the_bars_behind_it() {
        let minutes: Vec<_> = (0..7).map(|_| candle("101", "100")).collect();
        let hours: Vec<_> = (0..3).map(|_| candle("101", "100")).collect();
        let features = vol_features(&minutes, &hours);
        assert_eq!((features.bars_1h, features.bars_24h), (7, 3));
    }
}
