//! Walking the order book (`docs/spec.md` item 20).
//!
//! Two questions an agent has to answer before it sizes an order, and neither
//! is answerable from the mid alone:
//!
//! * what would this size actually fill at, and
//! * how big could it go before the fill got worse than N basis points.
//!
//! Both are arithmetic over the resting depth, so both live here rather than in
//! the gateway: item 20 wants them, and spec F's `depth_usd_*` and
//! `book_imbalance` want the same walk. The gateway does the I/O and this does
//! the counting.
//!
//! **Everything here is an estimate of a book that has already moved.** The
//! levels are a snapshot, the venue matches against the live book, and nothing
//! reserves depth between the walk and the order. So the numbers describe what
//! *was* resting, and are named to say so.

use rust_decimal::Decimal;

use oppen_hl::types::{L2Book, Level};

/// Basis points per unit. A price 1% away is 100 bps.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// The depth bands item 20 names.
pub const SIZE_BANDS_BPS: [u32; 3] = [5, 10, 25];

/// What a walk of the book found for one order.
///
/// Field order is the wire order (`AGENTS.md` invariant 6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BookWalk {
    /// Best price on the side that would fill this order, or `None` when that
    /// side is empty.
    pub top_px: Option<Decimal>,
    /// Size-weighted price the walked portion would fill at.
    pub avg_px: Option<Decimal>,
    /// How much of the requested size the resting depth covers. Less than the
    /// request means the book ran out, and the venue would fill the rest
    /// against whatever arrives — or not at all.
    pub filled_sz: Decimal,
    /// True when the book could not cover the whole size.
    pub exhausts_book: bool,
    /// `|avg_px - top_px| / top_px`, in basis points: the cost of crossing
    /// this much depth, measured from the touch rather than the mid so it is
    /// the slippage the order causes rather than the spread it pays.
    pub slip_bps: Option<Decimal>,
    /// The largest notional fillable while the average stays within each band
    /// of the touch. Same order as [`SIZE_BANDS_BPS`].
    pub max_size_usd_within_bps: [Decimal; 3],
}

/// Walk `book` for an order of `sz` on the side a buy or a sell would take.
///
/// A buy lifts asks, a sell hits bids.
pub fn walk(book: &L2Book, is_buy: bool, sz: Decimal) -> BookWalk {
    let levels = if is_buy { book.asks() } else { book.bids() };
    let top_px = levels.first().map(|level| level.px);

    let (filled_sz, notional) = take(levels, sz);
    let avg_px = if filled_sz > Decimal::ZERO {
        notional.checked_div(filled_sz)
    } else {
        None
    };

    let slip_bps = match (top_px, avg_px) {
        // Away from the touch is the only direction that costs: a buy fills at
        // or above the best ask, a sell at or below the best bid.
        (Some(top), Some(avg)) if top > Decimal::ZERO => {
            let adverse = if is_buy { avg - top } else { top - avg };
            adverse.max(Decimal::ZERO).checked_div(top).map(|r| r * BPS)
        }
        _ => None,
    };

    BookWalk {
        top_px,
        avg_px,
        filled_sz,
        exhausts_book: filled_sz < sz,
        slip_bps,
        max_size_usd_within_bps: SIZE_BANDS_BPS.map(|bps| max_notional_within(levels, is_buy, bps)),
    }
}

/// Consume up to `want` from the levels, returning what was taken and its
/// notional. Partial levels count for exactly the part taken.
fn take(levels: &[Level], want: Decimal) -> (Decimal, Decimal) {
    let mut remaining = want;
    let mut filled = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    for level in levels {
        if remaining <= Decimal::ZERO {
            break;
        }
        let taken = level.sz.min(remaining);
        filled += taken;
        notional += taken * level.px;
        remaining -= taken;
    }
    (filled, notional)
}

/// The largest notional whose *average* fill stays within `bps` of the touch.
///
/// The average, not the last level: an order does not pay the worst price it
/// reaches, it pays the mean of what it crossed. Measuring the limit at the
/// last level would understate the size by roughly half a band, which is the
/// difference between a usable number and a conservative-looking wrong one.
fn max_notional_within(levels: &[Level], is_buy: bool, bps: u32) -> Decimal {
    let Some(top) = levels.first().map(|level| level.px) else {
        return Decimal::ZERO;
    };
    if top <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    // The worst average this band allows, on the side that costs.
    let band = top * Decimal::from(bps) / BPS;
    let limit = if is_buy { top + band } else { top - band };

    let mut filled = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    for level in levels {
        // Taking this level whole: does the average still sit inside the band?
        let next_filled = filled + level.sz;
        let next_notional = notional + level.sz * level.px;
        if next_filled <= Decimal::ZERO {
            continue;
        }
        let within = match next_notional.checked_div(next_filled) {
            Some(avg) if is_buy => avg <= limit,
            Some(avg) => avg >= limit,
            None => false,
        };
        if within {
            filled = next_filled;
            notional = next_notional;
            continue;
        }
        // Part of this level fits. The average is monotone in how much of it
        // is taken, so solve for the size that lands exactly on the limit:
        //   (notional + x·px) / (filled + x) = limit
        //   x = (limit·filled - notional) / (px - limit)
        let denominator = level.px - limit;
        if denominator.is_zero() {
            break;
        }
        let x = (limit * filled - notional) / denominator;
        if x > Decimal::ZERO {
            let taken = x.min(level.sz);
            notional += taken * level.px;
        }
        break;
    }
    notional
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    /// `levels[0]` bids, `levels[1]` asks, best first.
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

    #[test]
    fn a_buy_inside_the_touch_pays_the_touch_and_slips_nothing() {
        let b = book(&[("99", "10")], &[("100", "10")]);
        let walk = walk(&b, true, d("4"));
        assert_eq!(walk.top_px, Some(d("100")));
        assert_eq!(walk.avg_px, Some(d("100")));
        assert_eq!(walk.filled_sz, d("4"));
        assert!(!walk.exhausts_book);
        assert_eq!(walk.slip_bps, Some(Decimal::ZERO));
    }

    /// The average of what was crossed, not the last price reached.
    #[test]
    fn walking_two_levels_averages_them_by_size() {
        let b = book(&[], &[("100", "1"), ("102", "3")]);
        let walk = walk(&b, true, d("4"));
        // (1×100 + 3×102) / 4 = 101.5
        assert_eq!(walk.avg_px, Some(d("101.5")));
        // 1.5 / 100 = 150 bps
        assert_eq!(walk.slip_bps, Some(d("150")));
    }

    #[test]
    fn a_sell_walks_the_bids_downward_and_slippage_stays_positive() {
        let b = book(&[("100", "1"), ("98", "3")], &[]);
        let walk = walk(&b, false, d("4"));
        assert_eq!(walk.avg_px, Some(d("98.5")));
        assert_eq!(walk.slip_bps, Some(d("150")), "slippage is never negative");
    }

    /// A book that cannot cover the order says so rather than reporting the
    /// average of the part it could — which would look like a good fill.
    #[test]
    fn a_size_past_the_book_is_flagged_and_reports_only_what_rests() {
        let b = book(&[], &[("100", "1"), ("101", "1")]);
        let walk = walk(&b, true, d("10"));
        assert!(walk.exhausts_book);
        assert_eq!(walk.filled_sz, d("2"));
        assert_eq!(walk.avg_px, Some(d("100.5")));
    }

    #[test]
    fn an_empty_side_has_no_price_and_no_slippage() {
        let b = book(&[("99", "10")], &[]);
        let walk = walk(&b, true, d("1"));
        assert_eq!(walk.top_px, None);
        assert_eq!(walk.avg_px, None);
        assert_eq!(walk.slip_bps, None);
        assert!(walk.exhausts_book);
        assert_eq!(walk.max_size_usd_within_bps, [Decimal::ZERO; 3]);
    }

    /// A level exactly on the band boundary is inside it.
    #[test]
    fn depth_within_a_band_includes_the_level_that_lands_on_it() {
        // 100 then 100.05: 100.05 is exactly 5 bps above the touch, so a full
        // take of both averages 100.025 — inside 5 bps.
        let b = book(&[], &[("100", "2"), ("100.05", "2")]);
        let walk = walk(&b, true, d("1"));
        assert_eq!(walk.max_size_usd_within_bps[0], d("400.1"));
    }

    /// The bands are cumulative: a wider one can never admit less notional.
    #[test]
    fn wider_bands_never_admit_less_than_narrower_ones() {
        let b = book(
            &[],
            &[("100", "1"), ("100.5", "2"), ("101", "5"), ("110", "50")],
        );
        let walk = walk(&b, true, d("1"));
        let [five, ten, twenty_five] = walk.max_size_usd_within_bps;
        assert!(five <= ten, "{five} > {ten}");
        assert!(ten <= twenty_five, "{ten} > {twenty_five}");
        assert!(five > Decimal::ZERO, "the touch itself is always within");
    }

    /// The band limit is on the *average*, so a partial take of a level past
    /// the band still counts — stopping at the whole level would understate
    /// the size by roughly half a band.
    #[test]
    fn a_level_past_the_band_still_contributes_the_part_that_fits() {
        // Touch 100 with 1 lot; next level 101 (100 bps away) with 100 lots.
        // At 25 bps the average may reach 100.25, so x solves
        // (100 + 101x)/(1 + x) = 100.25  →  x = 1/3.
        let b = book(&[], &[("100", "1"), ("101", "100")]);
        let walk = walk(&b, true, d("1"));
        let within_25 = walk.max_size_usd_within_bps[2];
        assert!(
            within_25 > d("100") && within_25 < d("135"),
            "expected the touch plus a third of the next level, got {within_25}"
        );
    }

    /// Both sides measure cost away from their own touch.
    #[test]
    fn a_sell_band_walks_downward() {
        let b = book(&[("100", "1"), ("99.9", "10")], &[]);
        let walk = walk(&b, false, d("1"));
        // 99.9 is 10 bps below 100, so it is outside 5 bps as a whole take but
        // contributes partially.
        assert!(walk.max_size_usd_within_bps[0] > d("100"));
        assert!(walk.max_size_usd_within_bps[1] > walk.max_size_usd_within_bps[0]);
    }

    #[test]
    fn a_zero_size_walk_reports_no_fill_rather_than_dividing_by_zero() {
        let b = book(&[("99", "10")], &[("100", "10")]);
        let walk = walk(&b, true, Decimal::ZERO);
        assert_eq!(walk.filled_sz, Decimal::ZERO);
        assert_eq!(walk.avg_px, None);
        assert_eq!(walk.slip_bps, None);
        assert!(!walk.exhausts_book, "zero size is covered by any book");
    }
}
