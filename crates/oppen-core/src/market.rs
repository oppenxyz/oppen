//! The market as the console shows it (`docs/spec.md` items 30–31).
//!
//! The symmetric half of [`crate::state`]: that projects the *account* from
//! venue responses, this projects the *market*. Both live here rather than in
//! a client so the operator's console and the agent's `get_features` cannot be
//! shown different arithmetic over the same reads (A3), and so the numbers get
//! tested in the crate that has a test culture.
//!
//! **Every decimal leaves as a string.** A float is the wrong container for a
//! price, and `AGENTS.md` invariant 6 wants one serialization rather than
//! whatever `f64` rounds to on the day.

use rust_decimal::Decimal;
use serde::Serialize;

use oppen_hl::types::{AssetCtx, MetaAndAssetCtxs};

/// Percent, for the day's move.
const HUNDRED: Decimal = Decimal::ONE_HUNDRED;
/// Basis points per unit, for the funding rate.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// One row of the markets rail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarketRow {
    pub symbol: String,
    /// The venue's `markPx`, which `docs/specs/fair-value.md` §14.1 says to
    /// consume rather than replicate.
    pub mark_px: String,
    /// **Absent on an asset the venue has stopped quoting** — 56 of 233 on
    /// mainnet when §14.4 correction 2 measured it. There is no substitute:
    /// `allMids` answers for exactly these with a frozen last print, which is
    /// the fabrication H1 exists to refuse. The row still lists; it simply has
    /// no mid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mid_px: Option<String>,
    /// Move against the previous day's close, in percent. Absent when that
    /// close is zero — an infinite move is not a number to render.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_24h_pct: Option<String>,
    /// Funding accrued **so far this hour**, in basis points. Not annualised,
    /// and not `g(premium)`: see [`oppen_hl::types::HourToDateRate1h`].
    pub funding_1h_bps: String,
    pub open_interest: String,
    pub day_volume_usd: String,
    /// Whether the venue is quoting a book at all.
    ///
    /// The rail dims a bookless row rather than dropping it: 38.6% of the
    /// mainnet universe has no book (§14.4 correction 7), and an operator who
    /// cannot find an asset learns less than one who finds it marked
    /// untradeable.
    pub has_book: bool,
}

impl MarketRow {
    /// One row from one asset context.
    ///
    /// Public because the console builds a row from an `activeAssetCtx` frame
    /// as well as from the REST universe: the socket carries every field this
    /// needs, and a second conversion in the desktop crate would be a second
    /// statement of the hour-to-date bps rule that could drift from this one.
    pub fn of(symbol: &str, ctx: &AssetCtx) -> Self {
        MarketRow {
            symbol: symbol.to_owned(),
            mark_px: ctx.mark_px.to_string(),
            mid_px: ctx.mid_px_no_fallback().map(|px| px.to_string()),
            change_24h_pct: change_pct(ctx).map(|pct| pct.to_string()),
            funding_1h_bps: (ctx.funding.hour_to_date_1h() * BPS)
                .round_dp(4)
                .to_string(),
            open_interest: ctx.open_interest.to_string(),
            day_volume_usd: ctx.day_ntl_vlm.to_string(),
            has_book: ctx.has_book(),
        }
    }
}

/// The day's move in percent, or `None` when it cannot be expressed.
///
/// Checked arithmetic throughout: the inputs come off the wire, and a venue
/// that reports something unrepresentable should cost the row its percentage,
/// not the whole rail.
fn change_pct(ctx: &AssetCtx) -> Option<Decimal> {
    if ctx.prev_day_px.is_zero() {
        return None;
    }
    ctx.mark_px
        .checked_sub(ctx.prev_day_px)?
        .checked_div(ctx.prev_day_px)?
        .checked_mul(HUNDRED)
        .map(|pct| pct.round_dp(2))
}

/// Every listed perp, busiest first.
///
/// Sorted by the day's notional volume because the rail is read top-down and
/// the assets an operator means are almost never at the alphabetical end.
/// Ties break on symbol, so the order is one serialization rather than
/// whatever the venue's array happened to hold (`AGENTS.md` invariant 6).
pub fn rows(contexts: &MetaAndAssetCtxs) -> Vec<MarketRow> {
    let mut rows: Vec<(Decimal, MarketRow)> = contexts
        .iter()
        .map(|(info, ctx)| (ctx.day_ntl_vlm, MarketRow::of(&info.name, ctx)))
        .collect();
    rows.sort_by(|(a_vlm, a), (b_vlm, b)| b_vlm.cmp(a_vlm).then_with(|| a.symbol.cmp(&b.symbol)));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// One side of the book, as the console draws it.
///
/// Levels, not a walk: the panel shows resting depth, and a walk is what
/// `preflight` does for a *size*. Truncated by the caller, because how many
/// rows fit is a layout question and not this module's to answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BookLevel {
    pub px: String,
    pub sz: String,
    /// Orders resting at this price, which the venue reports and which says
    /// whether one participant or twenty is behind a level.
    pub n: u32,
}

/// The selected symbol, in the depth this console draws it.
///
/// Assembled from three venue reads. It carries `as_of_ms` because it is
/// fetched on selection rather than on the account tick, so it ages
/// differently from everything beside it and the panel has to be able to say
/// so — spec item 34's rule that staleness is surfaced per feed, applied to a
/// feed that happens to be a REST read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarketSnapshot {
    pub symbol: String,
    pub as_of_ms: u64,
    /// Best bids, best first.
    pub bids: Vec<BookLevel>,
    /// Best asks, best first.
    pub asks: Vec<BookLevel>,
    /// Spec F's book pack. `micro_tilt_bps` is **always absent here**: it is
    /// sourced from `bbo`, and the console holds no socket — see
    /// [`snapshot`].
    pub book: crate::features::BookFeatures,
    pub funding: crate::features::FundingFeatures,
    pub vol: crate::features::VolFeatures,
}

/// How many levels a side sends. Twenty is more than the panel draws and few
/// enough that the payload stays small; the extra rows are what a taller
/// window shows without another read.
const LEVELS: usize = 20;

/// Projects one symbol from the three reads the console makes for it.
///
/// **`bbo` is deliberately not among them.** `micro_tilt_bps` needs the
/// best-bid-offer feed at its ~0.11 s cadence (`fair-value.md` §14.4
/// correction 4); the console has no socket of its own, and computing a
/// microprice from a 5-second-old REST book would be the same substitution
/// that correction warns about. So the field is absent rather than wrong, and
/// the panel renders it as such.
pub fn snapshot(
    symbol: &str,
    book: &oppen_hl::types::L2Book,
    ctx: &AssetCtx,
    predicted_hourly: Option<Decimal>,
    hours: &[oppen_hl::types::Candle],
    now_ms: u64,
) -> MarketSnapshot {
    let level = |l: &oppen_hl::types::Level| BookLevel {
        px: l.px.to_string(),
        sz: l.sz.to_string(),
        n: l.n,
    };
    MarketSnapshot {
        symbol: symbol.to_owned(),
        as_of_ms: now_ms,
        bids: book.bids().iter().take(LEVELS).map(level).collect(),
        asks: book.asks().iter().take(LEVELS).map(level).collect(),
        book: crate::features::book_features(book, None),
        funding: crate::features::funding_features(ctx, predicted_hourly, now_ms),
        // Only the hourly series: `rv_1h_bps` wants minute bars, and fetching
        // both doubles the cost of selecting a symbol for a figure the rail
        // does not show. `vol_features` reports `bars_1h: 0` beside the
        // absent estimate, which is the honest encoding of "not measured".
        vol: crate::features::vol_features(&[], hours),
    }
}

/// One bar as the chart draws it.
///
/// A projection of [`crate::candles::Bar`] and not that type re-exported: the
/// chart wants a bucket's open time and its five numbers, and does not want
/// the inclusive close, the trade count, or any of the invariants
/// [`crate::candles`] enforces on the way in. Shipping the richer type would
/// put fields on the wire that the renderer must then be trusted to ignore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChartBar {
    /// Bucket start, epoch ms. Epoch-aligned to the interval, which
    /// [`crate::candles::bars_from_candles`] has already checked.
    pub time_ms: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
}

impl ChartBar {
    fn of(bar: &crate::candles::Bar) -> Self {
        ChartBar {
            time_ms: bar.open_time_ms,
            open: bar.open.to_string(),
            high: bar.high.to_string(),
            low: bar.low.to_string(),
            close: bar.close.to_string(),
            volume: bar.volume.to_string(),
        }
    }
}

/// A symbol's bars at one interval, split the way the renderer takes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChartSeries {
    pub symbol: String,
    /// The canonical interval, which is not always the one that was asked for
    /// — [`crate::candles::Interval::parse`] canonicalises, so `120s` comes
    /// back `2m`. The chart labels its axis from this, so it has to be what
    /// was actually drawn.
    pub interval: String,
    pub interval_ms: i64,
    /// `max_price_decimals` for the asset. Axis labels never carry more.
    pub price_decimals: u32,
    /// Closed buckets, oldest first.
    pub closed: Vec<ChartBar>,
    /// The bucket now in progress, if the venue's last row is still open.
    /// Drawn with the forming glyph, so it must be told apart from a closed
    /// bar rather than appended to them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forming: Option<ChartBar>,
}

/// Project a `candleSnapshot` response into what the chart draws.
///
/// **The partition check is not skippable, and a failure costs the chart.**
/// [`crate::candles::bars_from_candles`] refuses rows that are the wrong
/// interval, misaligned to the epoch, or not exactly one width wide — a venue
/// that does any of those has stopped partitioning the time axis, and bars
/// drawn from them are a picture of something that did not happen. The error
/// travels to the panel, which says the chart is unavailable and why. A chart
/// that is quietly wrong is worse than one that is quietly absent, and both
/// are worse than one that says which.
pub fn chart(
    symbol: &str,
    interval: crate::candles::Interval,
    candles: &[oppen_hl::types::Candle],
    price_decimals: u32,
    now_ms: u64,
) -> Result<ChartSeries, crate::candles::BarError> {
    let mut bars = crate::candles::bars_from_candles(candles, interval)?;
    // The venue returns the in-progress bucket as the last row, so only the
    // last one can be open — and it is open exactly while now falls inside it.
    // Comparing against the inclusive close rather than the next open is what
    // keeps the final millisecond of a bucket from reading as closed.
    let forming = bars
        .last()
        .is_some_and(|bar| i64::try_from(now_ms).is_ok_and(|now| now <= bar.close_time_ms))
        .then(|| bars.pop())
        .flatten();
    Ok(ChartSeries {
        symbol: symbol.to_owned(),
        interval: interval.to_string(),
        interval_ms: interval.millis(),
        price_decimals,
        closed: bars.iter().map(ChartBar::of).collect(),
        forming: forming.as_ref().map(ChartBar::of),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::HourToDateRate1h;

    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("decimal")
    }

    fn ctx(mark: &str, prev: &str, mid: Option<&str>, vlm: &str) -> AssetCtx {
        AssetCtx {
            funding: HourToDateRate1h::from_hour_to_date_1h(d("0.0000125")),
            open_interest: d("1000"),
            prev_day_px: d(prev),
            day_ntl_vlm: d(vlm),
            premium: None,
            oracle_px: d(mark),
            mark_px: d(mark),
            mid_px: mid.map(d),
            impact_pxs: None,
        }
    }

    /// **A market the venue has stopped quoting still lists, with no mid.**
    /// §14.4 correction 2 found `midPx` null on 56 of 233 mainnet assets, and
    /// correction 3 forbids the obvious fallback: `allMids` answers for
    /// exactly those with a frozen last print. Dropping the row would hide an
    /// asset the operator may hold; inventing a mid is the fabrication H1
    /// exists to refuse.
    #[test]
    fn a_market_with_no_mid_keeps_its_row_and_loses_only_the_mid() {
        let quoted = MarketRow::of("BTC", &ctx("100", "80", Some("100"), "5"));
        assert_eq!(quoted.mid_px.as_deref(), Some("100"));

        let dark = MarketRow::of("FRIEND", &ctx("4.72", "4", None, "1"));
        assert_eq!(dark.mid_px, None, "no substitute exists");
        assert_eq!(dark.mark_px, "4.72", "the mark is still reported");
        assert_eq!(dark.symbol, "FRIEND", "and the row is still there");
    }

    /// Hand-computed, per P6's gate: 100 against a previous close of 80 is
    /// +25%, and 80 against 100 is −20%.
    #[test]
    fn the_days_move_is_measured_against_the_previous_close() {
        assert_eq!(
            MarketRow::of("UP", &ctx("100", "80", Some("100"), "1")).change_24h_pct,
            Some("25.00".to_owned())
        );
        assert_eq!(
            MarketRow::of("DOWN", &ctx("80", "100", Some("80"), "1")).change_24h_pct,
            Some("-20.00".to_owned())
        );
    }

    /// A zero previous close has no percentage. An infinite move is not a
    /// number to render, and rendering it as some large finite one would be
    /// worse — the row keeps every other field.
    #[test]
    fn a_zero_previous_close_costs_the_row_its_percentage_and_nothing_else() {
        let row = MarketRow::of("NEW", &ctx("100", "0", Some("100"), "1"));
        assert_eq!(row.change_24h_pct, None);
        assert_eq!(row.mark_px, "100");
        assert_eq!(row.open_interest, "1000");
    }

    /// Funding is reported in bps and named for what it is: what has accrued
    /// **so far this hour**. 0.0000125 of price is 0.125 bp.
    #[test]
    fn funding_is_the_hour_to_date_accrual_in_bps() {
        assert_eq!(
            MarketRow::of("BTC", &ctx("100", "100", Some("100"), "1")).funding_1h_bps,
            "0.1250"
        );
    }

    const HOUR_MS: u64 = 60 * 60 * 1_000;

    fn bar_at(t: u64, close: &str) -> oppen_hl::types::Candle {
        oppen_hl::types::Candle {
            t,
            t_close: t + HOUR_MS - 1,
            s: "BTC".to_owned(),
            i: "1h".to_owned(),
            o: d("100"),
            c: d(close),
            h: d("110"),
            l: d("90"),
            v: d("7.5"),
            n: 42,
        }
    }

    fn hourly() -> crate::candles::Interval {
        crate::candles::Interval::parse("1h").expect("1h")
    }

    /// The venue hands back the in-progress bucket as an ordinary last row.
    /// Appending it to the closed bars would draw a bucket that has not
    /// finished as though it had, which is the one thing the renderer's
    /// separate `forming` slot exists to prevent.
    #[test]
    fn the_bucket_now_in_progress_is_told_apart_from_the_ones_that_closed() {
        let candles = [bar_at(0, "101"), bar_at(HOUR_MS, "102")];
        // Halfway through the second bucket.
        let series = chart("BTC", hourly(), &candles, 2, HOUR_MS + HOUR_MS / 2).expect("series");

        assert_eq!(series.closed.len(), 1);
        assert_eq!(series.closed[0].time_ms, 0);
        assert_eq!(
            series.forming.as_ref().map(|bar| bar.time_ms),
            Some(HOUR_MS as i64)
        );
    }

    /// The boundary, which is where an off-by-one hides. A bucket is open
    /// through its **inclusive** close, so at that exact millisecond it is
    /// still forming; one millisecond later every bar is closed.
    #[test]
    fn a_bucket_is_open_through_its_last_millisecond_and_closed_after() {
        let candles = [bar_at(0, "101")];

        let last_ms = HOUR_MS - 1;
        assert!(
            chart("BTC", hourly(), &candles, 2, last_ms)
                .expect("series")
                .forming
                .is_some(),
            "still forming on its final millisecond"
        );
        assert!(
            chart("BTC", hourly(), &candles, 2, last_ms + 1)
                .expect("series")
                .forming
                .is_none(),
            "closed once the bucket has ended"
        );
    }

    /// A venue that has stopped partitioning the time axis costs the panel its
    /// chart and says so. Drawing the bars anyway would be a picture of
    /// something that did not happen, and silently dropping them would leave
    /// the operator staring at an empty panel that reads as downtime.
    #[test]
    fn a_broken_partition_refuses_rather_than_drawing_it() {
        let mut misaligned = bar_at(0, "101");
        misaligned.t = 90_000;
        misaligned.t_close = 90_000 + HOUR_MS - 1;

        assert!(matches!(
            chart("BTC", hourly(), &[misaligned], 2, HOUR_MS),
            Err(crate::candles::BarError::Misaligned { .. })
        ));
    }

    /// Every price leaves as a string, like every other decimal this module
    /// ships. The chart is the one consumer that will parse them back to
    /// floats, and it does that at its own edge — the wire stays exact.
    #[test]
    fn the_chart_ships_prices_as_strings_like_everything_else_here() {
        let series = chart("BTC", hourly(), &[bar_at(0, "101.5")], 2, HOUR_MS).expect("series");

        let bar = &series.closed[0];
        assert_eq!(bar.close, "101.5");
        assert_eq!(bar.volume, "7.5");
        assert_eq!(series.interval, "1h");
        assert_eq!(series.interval_ms, HOUR_MS as i64);
    }
}
