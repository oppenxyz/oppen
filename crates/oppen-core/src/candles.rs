//! Candle store and the arbitrary-interval engine.
//!
//! Implements `docs/specs/charts.md` §3. The operator types any interval and
//! gets a chart; every requested interval resolves to exactly one of three
//! cases and the UI is told which one, because silently degrading a chart is
//! how a reader ends up trusting a line that is not there:
//!
//! | Case | Source | History |
//! |---|---|---|
//! | [`Resolution::Native`] | `candleSnapshot` / the `candle` WS channel | full |
//! | [`Resolution::Resampled`] | aggregate the largest native divisor | full |
//! | [`Resolution::Local`] | aggregate the WS `trades` feed here | forward only |
//!
//! Bucket boundaries are aligned to the Unix epoch (`bucket_start_ms` is
//! `floor(t / interval) * interval`), so the same interval produces the same
//! buckets on every machine, in every timezone, across restarts, and across a
//! daylight-saving transition. Nothing in this module reads a local clock or a
//! calendar.
//!
//! Aggregation lives here rather than in the view layer because the fair-value
//! engine and the renderer must see the same bars (`charts.md` §2.4), and
//! because `docs/decisions.md` R1 keeps this crate headless.
//!
//! Bars are stored in **unchained side tables** (`docs/decisions.md` R5):
//! candles are re-derivable, so they are not part of the hash chain and they
//! carry a disk budget instead. Locally aggregated bars are the exception to
//! "re-derivable" — nobody serves them — and D-e still makes them prunable at
//! a 30-day default.
//!
//! ## Live API audit, 2026-09-04 (mainnet `candleSnapshot`, BTC and HYPE)
//!
//! Facts this module depends on, measured rather than assumed:
//!
//! - The native menu is exactly the fourteen strings in [`NATIVE_INTERVALS`].
//!   `1s`, `30s`, `6h`, `2d` and `7m` are rejected with HTTP 422.
//! - **`1M` is not a calendar month.** Every one of the 86 BTC and 23 HYPE
//!   `1M` bars is exactly 2,592,000,000 ms wide and satisfies
//!   `open_time % 30d == 0`. So every native interval is a fixed millisecond
//!   duration aligned to the epoch, and this module needs no calendar
//!   arithmetic anywhere. (`charts.md` §3 lists `1M` on the menu without
//!   saying which; this is the answer.)
//! - All 28 series probed (14 intervals × 2 coins, ~60,000 bars) had zero
//!   misaligned opens and zero wrong durations. `T` is always `t + width - 1`.
//! - The venue emits a bar for **every** bucket, including empty ones
//!   (`v = 0`, `n = 0`, `o = h = l = c`): PURR had 282 such 1m bars in 24 h
//!   and no missing buckets. [`LocalAggregator`] copies that convention.
//! - **The per-request bar cap is really a rolling history floor.**
//!   `candleSnapshot` serves only the most recent ≈5,000 intervals: a
//!   full-history request returned 5,001 rows on `1h`, 5,004 on `15m`, 5,064 on
//!   BTC `1m` and 5,161 on HYPE `1m`. Widening `startTime` past that adds
//!   nothing, and a window lying **entirely** before the floor returns `[]`
//!   with HTTP 200 rather than a truncated page. So `1d` and coarser serve full
//!   history (BTC `1d` gives all 2,208 rows back to 2019) while `1m` reaches
//!   only ~3.5 days back, no matter how the request is paged. Backfill for
//!   `docs/decisions.md` D-d must plan around that, and a resampled `1m`-based
//!   interval inherits the same short horizon.

use std::fmt;
use std::str::FromStr;

use oppen_hl::types::Candle;
use rusqlite::{Connection, OptionalExtension, params};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// One millisecond, as the smallest unit of the time axis. Every timestamp in
/// this module is Unix epoch milliseconds, matching the venue's `t`/`T`.
const MS_PER_SECOND: i64 = 1_000;

/// Width of the venue's `1M` bar. Measured, not assumed: see the audit note in
/// the module docs. Hyperliquid's "month" is thirty days.
const MONTH_MS: i64 = 30 * 24 * 60 * 60 * MS_PER_SECOND;

/// The longest interval oppen will accept, per `charts.md` §3.3 ("reject above
/// `1M`"). Equal to `1M` because that is the top of the venue menu.
pub const MAX_INTERVAL_MS: i64 = MONTH_MS;

/// Default retention for locally aggregated bars, `docs/decisions.md` D-e:
/// thirty days. Ledger events are kept forever; recomputable — or, for local
/// bars, unrecoverable-but-budgeted — inputs are pruned.
pub const DEFAULT_LOCAL_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * MS_PER_SECOND;

/// Ceiling on how many empty bars [`LocalAggregator`] will synthesise to span
/// one silence. A closed laptop on a `1s` chart would otherwise materialise a
/// bar per second of sleep; past this the gap is left as absent rows and
/// counted by [`LocalAggregator::skipped_fills`].
pub const MAX_FILL_BARS: i64 = 10_000;

/// The time unit of an interval string. Case matters and is not a typo:
/// `m` is a minute and `M` is the venue's thirty-day bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// `s`. Only ever produces a locally aggregated series: the venue's finest
    /// bar is `1m`.
    Second,
    /// `m`, lowercase.
    Minute,
    /// `h`.
    Hour,
    /// `d`.
    Day,
    /// `w`. Epoch-aligned, so a week starts Thursday UTC — verified against 367
    /// live `1w` bars.
    Week,
    /// `M`, uppercase. Thirty days, not a calendar month: see the audit note in
    /// the module docs.
    Month,
}

impl Unit {
    /// Width of one unit in milliseconds. Fixed for all six: the audit proved
    /// `Month` is thirty days on the wire, so no unit here is calendar-variable.
    pub const fn millis(self) -> i64 {
        match self {
            Unit::Second => MS_PER_SECOND,
            Unit::Minute => 60 * MS_PER_SECOND,
            Unit::Hour => 60 * 60 * MS_PER_SECOND,
            Unit::Day => 24 * 60 * 60 * MS_PER_SECOND,
            Unit::Week => 7 * 24 * 60 * 60 * MS_PER_SECOND,
            Unit::Month => MONTH_MS,
        }
    }

    /// The single character used on the wire and in the operator's input box.
    pub const fn suffix(self) -> char {
        match self {
            Unit::Second => 's',
            Unit::Minute => 'm',
            Unit::Hour => 'h',
            Unit::Day => 'd',
            Unit::Week => 'w',
            Unit::Month => 'M',
        }
    }

    fn from_suffix(c: char) -> Option<Self> {
        match c {
            's' => Some(Unit::Second),
            'm' => Some(Unit::Minute),
            'h' => Some(Unit::Hour),
            'd' => Some(Unit::Day),
            'w' => Some(Unit::Week),
            'M' => Some(Unit::Month),
            _ => None,
        }
    }
}

/// A chart interval, always held in canonical form.
///
/// Canonical means the coarsest unit that divides the width exactly, so `60s`,
/// `1m` and `1m` are one value and one storage key, and `720h` is `1M`. Two
/// intervals are equal exactly when their widths are equal, which is what makes
/// [`Resolution`] a total function and the storage key unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Interval {
    count: u32,
    unit: Unit,
}

impl Serialize for Interval {
    /// Serialised as the canonical wire string, not as its parts: one stable
    /// spelling per width keeps the MCP surface deterministic (`AGENTS.md` 6)
    /// and makes the JSON the same thing the venue and the operator both type.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Interval {
    /// Goes through [`Interval::parse`], so an out-of-range or non-canonical
    /// interval cannot enter the process through a deserialiser.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Interval::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The venue's fixed menu, ascending. `charts.md` §3.1 lists it; the audit in
/// the module docs confirms these fourteen and only these fourteen are served.
pub const NATIVE_INTERVALS: [Interval; 14] = [
    Interval::known(1, Unit::Minute),
    Interval::known(3, Unit::Minute),
    Interval::known(5, Unit::Minute),
    Interval::known(15, Unit::Minute),
    Interval::known(30, Unit::Minute),
    Interval::known(1, Unit::Hour),
    Interval::known(2, Unit::Hour),
    Interval::known(4, Unit::Hour),
    Interval::known(8, Unit::Hour),
    Interval::known(12, Unit::Hour),
    Interval::known(1, Unit::Day),
    Interval::known(3, Unit::Day),
    Interval::known(1, Unit::Week),
    Interval::known(1, Unit::Month),
];

impl Interval {
    /// Build an interval known at compile time to be canonical and in range.
    /// Private on purpose: every runtime value goes through [`Interval::parse`]
    /// so it cannot escape canonicalisation.
    const fn known(count: u32, unit: Unit) -> Self {
        Self { count, unit }
    }

    /// Parse `<n><unit>` with units `s m h d w M`, strictly, per `charts.md`
    /// §3.3: reject `0m`, reject non-integers, reject above `1M`. The result is
    /// canonical, so `120s` comes back as `2m` and resolves as native-derived
    /// rather than as a second interval nobody indexed.
    pub fn parse(input: &str) -> Result<Self, IntervalError> {
        if input.is_empty() {
            return Err(IntervalError::Empty);
        }
        let mut chars = input.chars();
        let Some(suffix) = chars.next_back() else {
            return Err(IntervalError::Empty);
        };
        let digits = chars.as_str();
        let Some(unit) = Unit::from_suffix(suffix) else {
            return Err(IntervalError::UnknownUnit(suffix));
        };
        if digits.is_empty() {
            return Err(IntervalError::MissingCount(input.to_owned()));
        }
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(IntervalError::NotAnInteger(digits.to_owned()));
        }
        let count: u32 = digits
            .parse()
            .map_err(|_| IntervalError::TooLarge(input.to_owned()))?;
        if count == 0 {
            return Err(IntervalError::Zero);
        }
        let millis = i64::from(count)
            .checked_mul(unit.millis())
            .ok_or_else(|| IntervalError::TooLarge(input.to_owned()))?;
        if millis > MAX_INTERVAL_MS {
            return Err(IntervalError::TooLarge(input.to_owned()));
        }
        Ok(Self::canonical(millis))
    }

    /// Coarsest exact representation of a width. Total because the finest unit
    /// is one second and every width built here is a whole number of seconds.
    fn canonical(millis: i64) -> Self {
        for unit in [Unit::Month, Unit::Week, Unit::Day, Unit::Hour, Unit::Minute] {
            let width = unit.millis();
            if millis % width == 0 {
                // In range by construction: MAX_INTERVAL_MS / 60_000 fits u32.
                return Self {
                    count: (millis / width) as u32,
                    unit,
                };
            }
        }
        Self {
            count: (millis / MS_PER_SECOND) as u32,
            unit: Unit::Second,
        }
    }

    /// How many of the unit. Always at least one.
    pub const fn count(self) -> u32 {
        self.count
    }

    /// The canonical unit.
    pub const fn unit(self) -> Unit {
        self.unit
    }

    /// Width in milliseconds. Never zero, so it is always a safe divisor.
    pub const fn millis(self) -> i64 {
        self.count as i64 * self.unit.millis()
    }

    /// Whether the venue serves this interval directly.
    pub fn is_native(self) -> bool {
        NATIVE_INTERVALS.contains(&self)
    }

    /// Which of `charts.md` §3.2's three cases this interval falls into, and
    /// for the resampled case which native interval to aggregate.
    pub fn resolve(self) -> Resolution {
        if self.is_native() {
            return Resolution::Native;
        }
        let millis = self.millis();
        if millis < Unit::Minute.millis() {
            return Resolution::Local {
                reason: LocalReason::SubMinute,
            };
        }
        // Largest native divisor: the menu is ascending, so walk it backwards.
        for native in NATIVE_INTERVALS.iter().rev() {
            let width = native.millis();
            if width < millis && millis % width == 0 {
                return Resolution::Resampled { from: *native };
            }
        }
        Resolution::Local {
            reason: LocalReason::NotAMultipleOfAnyNative,
        }
    }

    /// Start of the bucket containing `t_ms`, aligned to the Unix epoch.
    ///
    /// This one line is why a `4h` bar covers the same four hours in São Paulo
    /// and in Singapore, and why a daylight-saving transition cannot widen,
    /// narrow, duplicate or skip a bucket: the local calendar is never
    /// consulted. `div_euclid` floors rather than truncating, so pre-epoch
    /// timestamps land in the bucket below them rather than the one above.
    pub const fn bucket_start_ms(self, t_ms: i64) -> i64 {
        t_ms.div_euclid(self.millis()) * self.millis()
    }

    /// Inclusive close of the bucket containing `t_ms`, in the venue's own
    /// convention: `T = t + width - 1`, verified on every bar in the audit.
    pub const fn bucket_close_ms(self, t_ms: i64) -> i64 {
        self.bucket_start_ms(t_ms) + self.millis() - 1
    }
}

impl fmt::Display for Interval {
    /// The canonical wire string. For a native interval this is exactly what
    /// `candleSnapshot`'s `interval` field wants.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.count, self.unit.suffix())
    }
}

impl FromStr for Interval {
    type Err = IntervalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Interval::parse(s)
    }
}

/// Why an interval string was refused. Typed rather than a message so the UI
/// can name the rule that rejected the operator's input (`AGENTS.md` 8).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntervalError {
    /// Nothing was typed.
    #[error("interval is empty")]
    Empty,
    /// A unit with no number in front of it, such as `m`.
    #[error("interval `{0}` has no count")]
    MissingCount(String),
    /// A count that is not a whole number, such as `1.5m` or `-3m`.
    #[error("interval count `{0}` is not a whole number")]
    NotAnInteger(String),
    /// A unit letter outside `s m h d w M`. `M` is a month, `m` is a minute.
    #[error("unknown interval unit `{0}`; use one of s m h d w M")]
    UnknownUnit(char),
    /// `0m` and friends: a zero-width bucket has no meaning.
    #[error("interval count must be at least 1")]
    Zero,
    /// Longer than `1M`, the top of the venue menu.
    #[error("interval `{0}` is longer than the maximum 1M")]
    TooLarge(String),
}

/// Why an interval must be aggregated locally, and therefore has no history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalReason {
    /// Below `1m`, the finest bar the venue serves. `90s`'s sibling `30s`.
    SubMinute,
    /// At or above `1m` but not an integer multiple of any native interval,
    /// such as `90s`, which is a minute and a half.
    NotAMultipleOfAnyNative,
}

/// Which of the three cases an interval resolved to. The UI renders this as the
/// chip beside the interval box (`charts.md` §3.3) and must never omit it: a
/// forward-only chart that looks like a historical one is the failure this
/// whole type exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "case", rename_all = "snake_case")]
pub enum Resolution {
    /// On the venue menu. Backfilled by `candleSnapshot`, live from the
    /// `candle` WS channel.
    Native,
    /// An exact integer multiple of `from`. Lossless: the venue's bars
    /// partition the same time axis, so aggregating them loses nothing.
    Resampled {
        /// The largest native interval that divides this one.
        from: Interval,
    },
    /// Built here from the `trades` feed. **Forward only** — there is no
    /// history and the UI must say so rather than drawing a flat line.
    Local {
        /// Which condition in `charts.md` §3.2 put it in this case.
        reason: LocalReason,
    },
}

impl Resolution {
    /// The exact chip text from `charts.md` §3.3. Kept in core rather than the
    /// view so the requirement "the UI must say which one it is" is testable.
    pub fn label(self) -> String {
        match self {
            Resolution::Native => "NATIVE".to_owned(),
            Resolution::Resampled { from } => format!("RESAMPLED FROM {from}"),
            Resolution::Local { .. } => "LOCAL · FORWARD ONLY".to_owned(),
        }
    }

    /// Whether this case has history behind it. `false` means the chart starts
    /// at the moment oppen first listened and the empty region before it must
    /// be labelled, never drawn.
    pub fn has_history(self) -> bool {
        !matches!(self, Resolution::Local { .. })
    }
}

/// Where a stored bar came from. Persisted so retention can distinguish the
/// re-derivable from the unrecoverable: `docs/decisions.md` D-e prunes local
/// bars on a budget and leaves venue bars alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Straight from `candleSnapshot` or the `candle` WS channel.
    Venue,
    /// Aggregated from venue bars of a coarser-dividing native interval.
    Resampled,
    /// Aggregated here from the `trades` feed. Nobody else has it.
    Local,
}

impl Source {
    /// Stable database spelling. Deterministic and stable across releases;
    /// changing one of these strings is a migration, not a rename.
    pub const fn as_str(self) -> &'static str {
        match self {
            Source::Venue => "venue",
            Source::Resampled => "resampled",
            Source::Local => "local",
        }
    }

    fn from_db(s: &str) -> Option<Self> {
        match s {
            "venue" => Some(Source::Venue),
            "resampled" => Some(Source::Resampled),
            "local" => Some(Source::Local),
            _ => None,
        }
    }
}

/// One bar on the time axis, interval-agnostic.
///
/// Money is [`Decimal`] and never `f64`: summing a day of `1m` volumes in
/// binary floating point drifts, and a chart that disagrees with the ledger
/// about a number is worse than no chart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bar {
    /// Bucket start, Unix epoch ms. Always a multiple of the interval width.
    pub open_time_ms: i64,
    /// Inclusive bucket close, `open_time_ms + width - 1`, the venue's `T`.
    pub close_time_ms: i64,
    /// First trade price in the bucket, or the previous close if it was empty.
    pub open: Decimal,
    /// Highest trade price in the bucket.
    pub high: Decimal,
    /// Lowest trade price in the bucket.
    pub low: Decimal,
    /// Last trade price in the bucket, or the previous close if it was empty.
    pub close: Decimal,
    /// Base-asset volume, summed exactly.
    pub volume: Decimal,
    /// Trade count, the venue's `n`.
    pub trades: u32,
}

impl Bar {
    /// Whether the bucket has finished. The caller supplies the clock, because
    /// this crate must not assume one (`docs/decisions.md` R1) and because a
    /// backfilled bar and a live bar have to be judged the same way.
    pub const fn is_closed(&self, now_ms: i64) -> bool {
        self.close_time_ms < now_ms
    }

    /// Convert one `candleSnapshot` / `candle` row. Fails rather than saturates
    /// on an out-of-range timestamp so a corrupt row cannot become a plausible
    /// one.
    pub fn from_candle(candle: &Candle) -> Result<Self, BarError> {
        Ok(Self {
            open_time_ms: i64::try_from(candle.t).map_err(|_| BarError::Timestamp(candle.t))?,
            close_time_ms: i64::try_from(candle.t_close)
                .map_err(|_| BarError::Timestamp(candle.t_close))?,
            open: candle.o,
            high: candle.h,
            low: candle.l,
            close: candle.c,
            volume: candle.v,
            trades: candle.n,
        })
    }

    fn empty_at(open_time_ms: i64, width_ms: i64, price: Decimal) -> Self {
        Self {
            open_time_ms,
            close_time_ms: open_time_ms + width_ms - 1,
            open: price,
            high: price,
            low: price,
            close: price,
            volume: Decimal::ZERO,
            trades: 0,
        }
    }
}

/// Why a venue row could not become a [`Bar`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BarError {
    /// The row claims an interval other than the one requested. Mixing two
    /// intervals in one series would silently corrupt every aggregate above it.
    #[error("candle interval `{found}` does not match the requested `{expected}`")]
    IntervalMismatch {
        /// The interval the caller asked `candleSnapshot` for.
        expected: String,
        /// The interval the row actually carries in its `i` field.
        found: String,
    },
    /// A timestamp outside the representable range.
    #[error("candle timestamp {0} ms is out of range")]
    Timestamp(u64),
    /// The row's open is not aligned to its own interval, which would break
    /// every downstream bucket.
    #[error("candle open {open_time_ms} ms is not aligned to {interval}")]
    Misaligned {
        /// The offending bar's open, Unix epoch ms.
        open_time_ms: i64,
        /// The interval it should have been aligned to.
        interval: String,
    },
    /// `T` is not `t + width - 1`. The venue has never done this; if it starts,
    /// the resampler's partition assumption is void and we stop rather than
    /// aggregate.
    #[error("candle at {open_time_ms} ms spans {actual_ms} ms, expected {expected_ms} ms")]
    WrongWidth {
        /// The offending bar's open, Unix epoch ms.
        open_time_ms: i64,
        /// `T - t + 1` as served.
        actual_ms: i64,
        /// The interval width it should have been.
        expected_ms: i64,
    },
}

/// Convert a `candleSnapshot` response into bars, checking the three things
/// that make resampling lossless: the rows are the interval we asked for, each
/// open is epoch-aligned to that interval, and each row spans exactly one
/// width. A venue that violates any of these has stopped partitioning the time
/// axis, so we refuse rather than aggregate.
pub fn bars_from_candles(candles: &[Candle], interval: Interval) -> Result<Vec<Bar>, BarError> {
    let width = interval.millis();
    let expected = interval.to_string();
    let mut out = Vec::with_capacity(candles.len());
    for candle in candles {
        if candle.i != expected {
            return Err(BarError::IntervalMismatch {
                expected: expected.clone(),
                found: candle.i.clone(),
            });
        }
        let bar = Bar::from_candle(candle)?;
        if bar.open_time_ms % width != 0 {
            return Err(BarError::Misaligned {
                open_time_ms: bar.open_time_ms,
                interval: expected.clone(),
            });
        }
        let actual = bar.close_time_ms - bar.open_time_ms + 1;
        if actual != width {
            return Err(BarError::WrongWidth {
                open_time_ms: bar.open_time_ms,
                actual_ms: actual,
                expected_ms: width,
            });
        }
        out.push(bar);
    }
    Ok(out)
}

/// Why a resample was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResampleError {
    /// The target is not an exact integer multiple of the source, so the
    /// source's bars do not partition the target's buckets and aggregating
    /// them would be an approximation. `charts.md` §3.2 requires exactness.
    ///
    /// (The fields are `from`/`to` rather than `source`/`target` because
    /// `thiserror` reads a field named `source` as a nested error.)
    #[error("{to} is not an integer multiple of {from}")]
    NotAMultiple {
        /// The interval the bars are in.
        from: String,
        /// The interval that was requested.
        to: String,
    },
    /// The target is the same width as the source or narrower. Resampling only
    /// ever coarsens; going finer would require inventing intra-bar structure.
    #[error("{to} is not coarser than {from}")]
    NotCoarser {
        /// The interval the bars are in.
        from: String,
        /// The interval that was requested.
        to: String,
    },
    /// A source bar's open is not aligned to the source interval.
    #[error("source bar at {open_time_ms} ms is not aligned to {from}")]
    Misaligned {
        /// The interval the bars claim to be in.
        from: String,
        /// The offending bar's open, Unix epoch ms.
        open_time_ms: i64,
    },
    /// Source bars are not strictly ascending. Deduplicating or sorting here
    /// would hide a feed bug behind a plausible chart.
    #[error("source bars are not strictly ascending at {open_time_ms} ms")]
    OutOfOrder {
        /// The open that did not exceed the one before it.
        open_time_ms: i64,
    },
}

/// Aggregate native bars into a coarser interval, exactly.
///
/// `open` is the first bar's open, `close` the last bar's close, `high` the
/// maximum, `low` the minimum, `volume` and `trades` the sums — `charts.md`
/// §3.2. This is lossless because the venue's bars partition the same time
/// axis and both intervals are epoch-aligned, so every source bar lies wholly
/// inside exactly one target bucket.
///
/// Empty source buckets participate normally: the venue emits them with
/// `o = h = l = c` and zero volume (measured on PURR, 282 of 1,441 bars), so
/// they neither widen the range nor move the open.
///
/// A trailing partial bucket is returned like any other; the caller decides
/// whether it is forming with [`Bar::is_closed`].
pub fn resample(
    source_bars: &[Bar],
    source: Interval,
    target: Interval,
) -> Result<Vec<Bar>, ResampleError> {
    let source_ms = source.millis();
    let target_ms = target.millis();
    if target_ms <= source_ms {
        return Err(ResampleError::NotCoarser {
            from: source.to_string(),
            to: target.to_string(),
        });
    }
    if target_ms % source_ms != 0 {
        return Err(ResampleError::NotAMultiple {
            from: source.to_string(),
            to: target.to_string(),
        });
    }

    let mut out: Vec<Bar> = Vec::new();
    let mut previous_open: Option<i64> = None;
    for bar in source_bars {
        if bar.open_time_ms % source_ms != 0 {
            return Err(ResampleError::Misaligned {
                from: source.to_string(),
                open_time_ms: bar.open_time_ms,
            });
        }
        if previous_open.is_some_and(|previous| bar.open_time_ms <= previous) {
            return Err(ResampleError::OutOfOrder {
                open_time_ms: bar.open_time_ms,
            });
        }
        previous_open = Some(bar.open_time_ms);

        let bucket = target.bucket_start_ms(bar.open_time_ms);
        match out.last_mut() {
            Some(current) if current.open_time_ms == bucket => {
                current.close = bar.close;
                if bar.high > current.high {
                    current.high = bar.high;
                }
                if bar.low < current.low {
                    current.low = bar.low;
                }
                current.volume += bar.volume;
                current.trades = current.trades.saturating_add(bar.trades);
            }
            _ => out.push(Bar {
                open_time_ms: bucket,
                close_time_ms: bucket + target_ms - 1,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
                trades: bar.trades,
            }),
        }
    }
    Ok(out)
}

/// One print off the `trades` WS channel, reduced to what a bar needs.
///
/// Defined here rather than taken from `oppen_hl::ws` so this module compiles
/// against the feed's shape rather than its transport; the WS layer converts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trade {
    /// Venue timestamp, Unix epoch ms.
    pub time_ms: i64,
    /// Trade price.
    pub price: Decimal,
    /// Base-asset size. Side is irrelevant to a bar.
    pub size: Decimal,
}

/// Builds bars from the trades feed for an interval the venue does not serve.
///
/// **Forward only.** `charts.md` §3.2: a `90s` chart opened for the first time
/// is empty and fills forward. [`LocalAggregator::no_history_before_ms`] is the
/// instant before which this aggregator has nothing, and the renderer must
/// label that region rather than draw a flat line across it.
///
/// The first bucket of a session is discarded rather than emitted, because
/// oppen almost always starts listening part-way through one and a partial
/// bucket presented as a whole bar is a wrong number, not a rough one.
///
/// Empty buckets inside the covered range are emitted flat
/// (`o = h = l = c = previous close`, zero volume), which is exactly what the
/// venue does for its own empty buckets. That is a continuation, not invented
/// history: it says "no trades", and it is only ever emitted between two
/// buckets this aggregator actually observed.
#[derive(Debug, Clone)]
pub struct LocalAggregator {
    interval: Interval,
    no_history_before_ms: i64,
    accepts_from_ms: i64,
    bucket: Option<i64>,
    forming: Option<Bar>,
    last_close: Option<Decimal>,
    late: u64,
    before_history: u64,
    skipped_fills: u64,
}

impl LocalAggregator {
    /// Start with no prior local history. `listening_since_ms` is when the
    /// trades subscription became live; the first whole bucket at or after it
    /// is the first bar this aggregator will ever emit.
    pub fn new(interval: Interval, listening_since_ms: i64) -> Self {
        let first_whole = Self::first_whole_bucket(interval, listening_since_ms);
        Self {
            interval,
            no_history_before_ms: first_whole,
            accepts_from_ms: first_whole,
            bucket: None,
            forming: None,
            last_close: None,
            late: 0,
            before_history: 0,
            skipped_fills: 0,
        }
    }

    /// Resume with bars already on disk. `charts.md` §3.2: local bars are
    /// persisted so history accumulates across sessions.
    ///
    /// `no_history_before_ms` is the persisted floor and stays put; this
    /// session still refuses to emit its own partial first bucket, and it does
    /// not flat-fill the gap while oppen was closed — that gap is absent rows,
    /// because nothing was watching and a continuation there would be a claim.
    pub fn resume(
        interval: Interval,
        listening_since_ms: i64,
        no_history_before_ms: i64,
        last_close: Option<Decimal>,
    ) -> Self {
        Self {
            accepts_from_ms: Self::first_whole_bucket(interval, listening_since_ms),
            no_history_before_ms,
            last_close,
            ..Self::new(interval, listening_since_ms)
        }
    }

    fn first_whole_bucket(interval: Interval, from_ms: i64) -> i64 {
        let start = interval.bucket_start_ms(from_ms);
        if start == from_ms {
            start
        } else {
            start + interval.millis()
        }
    }

    /// The interval being built.
    pub const fn interval(&self) -> Interval {
        self.interval
    }

    /// The instant before which this series has nothing. The UI renders the
    /// region before it as "no local history before HH:MM" — never a flat line,
    /// never a gap that reads as downtime.
    pub const fn no_history_before_ms(&self) -> i64 {
        self.no_history_before_ms
    }

    /// Trades dropped because they landed in an already-closed bucket. The feed
    /// is not perfectly ordered; a closed bar is never mutated after the fact,
    /// so the count is surfaced instead of the data being silently folded back.
    pub const fn late_trades(&self) -> u64 {
        self.late
    }

    /// Trades dropped for landing before [`Self::no_history_before_ms`], i.e.
    /// in the partial bucket this session joined part-way through.
    pub const fn discarded_partial_trades(&self) -> u64 {
        self.before_history
    }

    /// Silences too long to flat-fill (see [`MAX_FILL_BARS`]). Each one is a
    /// real gap in the stored series.
    pub const fn skipped_fills(&self) -> u64 {
        self.skipped_fills
    }

    /// The in-progress bar, if any: either the bucket's trades so far, or a
    /// flat continuation when the current bucket has had none.
    pub fn forming(&self) -> Option<Bar> {
        if let Some(bar) = &self.forming {
            return Some(bar.clone());
        }
        match (self.bucket, self.last_close) {
            (Some(open), Some(close)) => Some(Bar::empty_at(open, self.interval.millis(), close)),
            _ => None,
        }
    }

    /// Fold one trade in, returning any bars that closed as a result, ascending.
    pub fn push(&mut self, trade: Trade) -> Vec<Bar> {
        if trade.time_ms < self.accepts_from_ms {
            self.before_history += 1;
            return Vec::new();
        }
        let bucket = self.interval.bucket_start_ms(trade.time_ms);
        if self.bucket.is_some_and(|current| bucket < current) {
            self.late += 1;
            return Vec::new();
        }
        let closed = self.advance_to(bucket);
        if let Some(bar) = &mut self.forming {
            if trade.price > bar.high {
                bar.high = trade.price;
            }
            if trade.price < bar.low {
                bar.low = trade.price;
            }
            bar.close = trade.price;
            bar.volume += trade.size;
            bar.trades = bar.trades.saturating_add(1);
        } else {
            self.forming = Some(Bar {
                open_time_ms: bucket,
                close_time_ms: bucket + self.interval.millis() - 1,
                open: trade.price,
                high: trade.price,
                low: trade.price,
                close: trade.price,
                volume: trade.size,
                trades: 1,
            });
        }
        closed
    }

    /// Close every bucket that has finished as of `now_ms`, returning them
    /// ascending. Driven by the caller's clock so a quiet symbol still produces
    /// bars; without it a bucket with no trades would never close.
    pub fn close_through(&mut self, now_ms: i64) -> Vec<Bar> {
        if now_ms < self.accepts_from_ms {
            return Vec::new();
        }
        self.advance_to(self.interval.bucket_start_ms(now_ms))
    }

    /// Move the current bucket to `target`, emitting everything in between.
    fn advance_to(&mut self, target: i64) -> Vec<Bar> {
        let width = self.interval.millis();
        let Some(current) = self.bucket else {
            self.bucket = Some(target);
            return Vec::new();
        };
        if target <= current {
            return Vec::new();
        }
        self.bucket = Some(target);

        let mut out = Vec::new();
        if let Some(bar) = self.forming.take() {
            self.last_close = Some(bar.close);
            out.push(bar);
        } else if let Some(close) = self.last_close {
            out.push(Bar::empty_at(current, width, close));
        }
        if let Some(close) = self.last_close {
            let gap = (target - current) / width - 1;
            if gap > MAX_FILL_BARS {
                self.skipped_fills += 1;
            } else {
                for step in 1..=gap {
                    out.push(Bar::empty_at(current + step * width, width, close));
                }
            }
        }
        out
    }
}

/// Failures of the candle side tables.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Anything SQLite itself refused.
    #[error("candle store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stored price or size that is no longer a decimal. Never silently
    /// coerced: money is exact or it is an error.
    #[error("candle store: `{0}` is not a decimal")]
    Decimal(String),
    /// A `source` column value written by a newer schema.
    #[error("candle store: unknown bar source `{0}`")]
    UnknownSource(String),
}

/// The unchained candle side tables (`docs/decisions.md` R5).
///
/// Borrows the caller's connection rather than owning one, because R4 puts one
/// database file and one hash chain per network and the ledger owns that file.
/// Nothing here is hash-chained: candles are re-derivable, and a chart is not a
/// record of record.
pub struct CandleStore<'a> {
    conn: &'a Connection,
}

impl<'a> CandleStore<'a> {
    /// Attach to an open per-network connection. See [`crate::db_file_name`].
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Create the side tables if they are absent. Idempotent, so it is safe to
    /// call on every start alongside the ledger's own migration.
    pub fn migrate(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS candle_bars (
                 coin           TEXT    NOT NULL,
                 interval       TEXT    NOT NULL,
                 open_time_ms   INTEGER NOT NULL,
                 close_time_ms  INTEGER NOT NULL,
                 source         TEXT    NOT NULL,
                 open           TEXT    NOT NULL,
                 high           TEXT    NOT NULL,
                 low            TEXT    NOT NULL,
                 close          TEXT    NOT NULL,
                 volume         TEXT    NOT NULL,
                 trades         INTEGER NOT NULL,
                 PRIMARY KEY (coin, interval, open_time_ms)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS candle_bars_by_source
                 ON candle_bars (source, open_time_ms);
             CREATE TABLE IF NOT EXISTS candle_local_history (
                 coin                  TEXT    NOT NULL,
                 interval              TEXT    NOT NULL,
                 no_history_before_ms  INTEGER NOT NULL,
                 PRIMARY KEY (coin, interval)
             ) WITHOUT ROWID;",
        )?;
        Ok(())
    }

    /// Insert or replace bars for one series, in one transaction.
    ///
    /// Replace rather than ignore: the newest bucket is re-written many times
    /// as it forms, and a backfill legitimately supersedes a bar assembled from
    /// a partial view.
    pub fn put_bars(
        &self,
        coin: &str,
        interval: Interval,
        source: Source,
        bars: &[Bar],
    ) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO candle_bars
                     (coin, interval, open_time_ms, close_time_ms, source,
                      open, high, low, close, volume, trades)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            let interval = interval.to_string();
            for bar in bars {
                stmt.execute(params![
                    coin,
                    interval,
                    bar.open_time_ms,
                    bar.close_time_ms,
                    source.as_str(),
                    bar.open.to_string(),
                    bar.high.to_string(),
                    bar.low.to_string(),
                    bar.close.to_string(),
                    bar.volume.to_string(),
                    bar.trades,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Bars for one series whose open lies in `[from_ms, to_ms]`, ascending.
    /// Ascending order is part of the contract: [`resample`] refuses unsorted
    /// input and the renderer walks left to right.
    pub fn bars(
        &self,
        coin: &str,
        interval: Interval,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<Bar>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT open_time_ms, close_time_ms, open, high, low, close, volume, trades
               FROM candle_bars
              WHERE coin = ?1 AND interval = ?2
                AND open_time_ms >= ?3 AND open_time_ms <= ?4
              ORDER BY open_time_ms ASC",
        )?;
        let rows = stmt.query_map(params![coin, interval.to_string(), from_ms, to_ms], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, u32>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (open_time_ms, close_time_ms, open, high, low, close, volume, trades) = row?;
            out.push(Bar {
                open_time_ms,
                close_time_ms,
                open: parse_decimal(&open)?,
                high: parse_decimal(&high)?,
                low: parse_decimal(&low)?,
                close: parse_decimal(&close)?,
                volume: parse_decimal(&volume)?,
                trades,
            });
        }
        Ok(out)
    }

    /// The newest stored bar of a series, if any. Used to decide where a
    /// backfill or a reconnect resumes (`spec.md` item 9).
    pub fn latest_bar(&self, coin: &str, interval: Interval) -> Result<Option<Bar>, StoreError> {
        let newest: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(open_time_ms) FROM candle_bars WHERE coin = ?1 AND interval = ?2",
                params![coin, interval.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        match newest {
            None => Ok(None),
            Some(open_time_ms) => Ok(self
                .bars(coin, interval, open_time_ms, open_time_ms)?
                .into_iter()
                .next()),
        }
    }

    /// Which source a stored bar carries, for a caller deciding whether a
    /// series is re-derivable.
    pub fn bar_source(
        &self,
        coin: &str,
        interval: Interval,
        open_time_ms: i64,
    ) -> Result<Option<Source>, StoreError> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT source FROM candle_bars
                  WHERE coin = ?1 AND interval = ?2 AND open_time_ms = ?3",
                params![coin, interval.to_string(), open_time_ms],
                |row| row.get(0),
            )
            .optional()?;
        match found {
            None => Ok(None),
            Some(raw) => Source::from_db(&raw)
                .map(Some)
                .ok_or(StoreError::UnknownSource(raw)),
        }
    }

    /// Record the instant before which a locally aggregated series has nothing.
    /// Monotonic: the floor only ever moves forward, because it is a claim
    /// about what was deleted or never seen and neither un-happens.
    pub fn set_no_history_before(
        &self,
        coin: &str,
        interval: Interval,
        ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO candle_local_history (coin, interval, no_history_before_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (coin, interval) DO UPDATE SET
                 no_history_before_ms = MAX(no_history_before_ms, excluded.no_history_before_ms)",
            params![coin, interval.to_string(), ms],
        )?;
        Ok(())
    }

    /// The stored floor for a locally aggregated series. `None` means nothing
    /// has been aggregated yet, which the UI must render as "no local history"
    /// rather than as an empty chart.
    pub fn no_history_before(
        &self,
        coin: &str,
        interval: Interval,
    ) -> Result<Option<i64>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT no_history_before_ms FROM candle_local_history
                  WHERE coin = ?1 AND interval = ?2",
                params![coin, interval.to_string()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Drop locally aggregated bars older than `retention_ms`, returning how
    /// many rows went. `docs/decisions.md` D-e: ledger records are kept
    /// forever, sub-minute local bars default to thirty days
    /// ([`DEFAULT_LOCAL_RETENTION_MS`]).
    ///
    /// Every local series' floor is advanced to the cutoff in the same
    /// transaction. Pruning without moving the floor would leave the UI
    /// claiming history that has been deleted, which is the one failure this
    /// marker exists to prevent.
    pub fn prune_local(&self, now_ms: i64, retention_ms: i64) -> Result<usize, StoreError> {
        let cutoff = now_ms.saturating_sub(retention_ms);
        let tx = self.conn.unchecked_transaction()?;
        let removed = tx.execute(
            "DELETE FROM candle_bars WHERE source = ?1 AND open_time_ms < ?2",
            params![Source::Local.as_str(), cutoff],
        )?;
        tx.execute(
            "UPDATE candle_local_history
                SET no_history_before_ms = MAX(no_history_before_ms, ?1)",
            params![cutoff],
        )?;
        tx.commit()?;
        Ok(removed)
    }
}

fn parse_decimal(raw: &str) -> Result<Decimal, StoreError> {
    Decimal::from_str(raw).map_err(|_| StoreError::Decimal(raw.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("test decimal literal")
    }

    fn iv(s: &str) -> Interval {
        Interval::parse(s).expect("test interval literal")
    }

    /// `[open, high, low, close]` as decimal literals, then volume and trade
    /// count. Grouped rather than passed flat so the helper stays inside
    /// clippy's argument budget.
    fn bar(open_time_ms: i64, width: i64, ohlc: [&str; 4], v: &str, n: u32) -> Bar {
        Bar {
            open_time_ms,
            close_time_ms: open_time_ms + width - 1,
            open: d(ohlc[0]),
            high: d(ohlc[1]),
            low: d(ohlc[2]),
            close: d(ohlc[3]),
            volume: d(v),
            trades: n,
        }
    }

    // ---- interval parsing -------------------------------------------------

    #[test]
    fn parses_and_canonicalises() {
        assert_eq!(iv("1m").to_string(), "1m");
        assert_eq!(iv("60s").to_string(), "1m", "60s is the same bucket as 1m");
        assert_eq!(iv("120s").to_string(), "2m");
        assert_eq!(
            iv("90s").to_string(),
            "90s",
            "90s has no coarser exact unit"
        );
        assert_eq!(iv("7d").to_string(), "1w");
        assert_eq!(iv("720h").to_string(), "1M");
        assert_eq!(iv("30d").to_string(), "1M", "the venue's 1M is exactly 30d");
        assert_eq!(iv("4w").to_string(), "4w");
        assert_eq!(iv("01m"), iv("1m"));
    }

    #[test]
    fn rejects_bad_intervals() {
        use IntervalError::*;
        assert_eq!(Interval::parse(""), Err(Empty));
        assert_eq!(Interval::parse("0m"), Err(Zero));
        assert_eq!(Interval::parse("0s"), Err(Zero));
        assert_eq!(Interval::parse("m"), Err(MissingCount("m".to_owned())));
        assert_eq!(
            Interval::parse("1.5m"),
            Err(NotAnInteger("1.5".to_owned())),
            "non-integers are refused, not rounded"
        );
        assert_eq!(Interval::parse("-3m"), Err(NotAnInteger("-3".to_owned())));
        assert_eq!(Interval::parse("5y"), Err(UnknownUnit('y')));
        assert_eq!(Interval::parse("15"), Err(UnknownUnit('5')));
        assert_eq!(Interval::parse("31d"), Err(TooLarge("31d".to_owned())));
        assert_eq!(Interval::parse("2M"), Err(TooLarge("2M".to_owned())));
        assert_eq!(Interval::parse("5w"), Err(TooLarge("5w".to_owned())));
        // u32 parses, i64 multiply would overflow: still refused, not wrapped.
        assert_eq!(
            Interval::parse("4294967295M"),
            Err(TooLarge("4294967295M".to_owned()))
        );
        assert_eq!(
            Interval::parse("99999999999m"),
            Err(TooLarge("99999999999m".to_owned()))
        );
    }

    #[test]
    fn unit_case_is_load_bearing() {
        assert_eq!(iv("1m").millis(), 60_000);
        assert_eq!(iv("1M").millis(), 2_592_000_000);
    }

    // ---- resolution -------------------------------------------------------

    /// The fourteen strings the live mainnet API accepted on 2026-09-04.
    /// `1s`, `30s`, `6h`, `2d` and `7m` were rejected with HTTP 422.
    #[test]
    fn native_menu_matches_the_audited_live_api() {
        let menu: Vec<String> = NATIVE_INTERVALS.iter().map(|i| i.to_string()).collect();
        let menu: Vec<&str> = menu.iter().map(String::as_str).collect();
        assert_eq!(
            menu,
            vec![
                "1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "8h", "12h", "1d", "3d", "1w",
                "1M"
            ]
        );
        for interval in NATIVE_INTERVALS {
            assert!(interval.is_native());
            assert_eq!(interval.resolve(), Resolution::Native);
            assert_eq!(interval.resolve().label(), "NATIVE");
            assert!(interval.resolve().has_history());
        }
    }

    #[test]
    fn resolves_the_three_cases() {
        assert_eq!(iv("1h").resolve(), Resolution::Native);
        // charts.md §3.2's own worked examples.
        assert_eq!(
            iv("7m").resolve(),
            Resolution::Resampled { from: iv("1m") },
            "7m is 7 x 1m"
        );
        assert_eq!(
            iv("45m").resolve(),
            Resolution::Resampled { from: iv("15m") },
            "45m takes the largest divisor, not the smallest"
        );
        assert_eq!(
            iv("90s").resolve(),
            Resolution::Local {
                reason: LocalReason::NotAMultipleOfAnyNative
            }
        );
        assert_eq!(
            iv("30s").resolve(),
            Resolution::Local {
                reason: LocalReason::SubMinute
            }
        );
        assert_eq!(iv("6h").resolve(), Resolution::Resampled { from: iv("2h") });
        assert_eq!(iv("2d").resolve(), Resolution::Resampled { from: iv("1d") });
        assert_eq!(iv("4w").resolve(), Resolution::Resampled { from: iv("1w") });
        assert_eq!(iv("2m").resolve(), Resolution::Resampled { from: iv("1m") });
    }

    #[test]
    fn labels_say_which_case_it_is() {
        assert_eq!(iv("7m").resolve().label(), "RESAMPLED FROM 1m");
        assert_eq!(iv("45m").resolve().label(), "RESAMPLED FROM 15m");
        assert_eq!(iv("90s").resolve().label(), "LOCAL · FORWARD ONLY");
        assert!(!iv("90s").resolve().has_history());
        assert!(iv("7m").resolve().has_history());
    }

    // ---- epoch alignment --------------------------------------------------

    #[test]
    fn buckets_are_aligned_to_the_unix_epoch() {
        for interval in NATIVE_INTERVALS {
            let width = interval.millis();
            assert_eq!(interval.bucket_start_ms(0), 0);
            assert_eq!(interval.bucket_start_ms(width - 1), 0);
            assert_eq!(interval.bucket_start_ms(width), width);
            for t in [1_788_490_020_000_i64, 1_565_568_000_000, 999_999_999] {
                let start = interval.bucket_start_ms(t);
                assert_eq!(start % width, 0, "{interval} start not a multiple");
                assert!(start <= t && t < start + width);
                assert_eq!(interval.bucket_close_ms(t), start + width - 1);
            }
        }
        // Pre-epoch timestamps floor downwards rather than truncating towards
        // zero, so the bucket containing them is still exactly one width wide.
        let hour = iv("1h");
        assert_eq!(hour.bucket_start_ms(-1), -3_600_000);
        assert_eq!(hour.bucket_start_ms(-3_600_000), -3_600_000);
    }

    /// Real HL bar opens observed on 2026-09-04, mainnet BTC. If bucketing ever
    /// stops agreeing with the venue's own grid, resampling stops being lossless.
    #[test]
    fn bucket_grid_matches_observed_venue_bars() {
        for (interval, open) in [
            ("1M", 1_783_296_000_000_i64),
            ("1M", 1_788_480_000_000),
            ("1w", 1_788_393_600_000),
            ("3d", 1_788_480_000_000),
            ("1d", 1_785_024_000_000),
            ("12h", 1_785_024_000_000),
            ("1m", 1_788_490_020_000),
        ] {
            let interval = iv(interval);
            assert_eq!(
                interval.bucket_start_ms(open),
                open,
                "{interval} bar at {open} is not on our grid"
            );
        }
    }

    /// Epoch alignment is what makes a bucket timezone- and DST-independent.
    ///
    /// The instants below are the exact UTC moments of real daylight-saving
    /// transitions in named zones:
    ///
    /// * `America/New_York` 2026-03-08 07:00Z — 01:59:59 EST becomes 03:00 EDT,
    ///   so that local day is 23 hours long.
    /// * `America/New_York` 2026-11-01 06:00Z — 01:59:59 EDT becomes 01:00 EST,
    ///   so 01:30 local happens twice and that local day is 25 hours long.
    /// * `Europe/London` 2026-03-29 01:00Z — GMT becomes BST.
    ///
    /// A wall-clock implementation would duplicate or skip an hourly bucket at
    /// each of these, and would produce a 25-hour or 23-hour "day". Epoch
    /// bucketing cannot: the sequence stays a plain arithmetic progression.
    #[test]
    fn dst_transitions_do_not_move_buckets() {
        const NY_SPRING_FORWARD: i64 = 1_772_953_200_000; // 2026-03-08T07:00:00Z
        const NY_FALL_BACK: i64 = 1_793_512_800_000; // 2026-11-01T06:00:00Z
        const LONDON_SPRING_FORWARD: i64 = 1_774_746_000_000; // 2026-03-29T01:00:00Z

        let hour = iv("1h");
        let day = iv("1d");

        for transition in [NY_SPRING_FORWARD, NY_FALL_BACK, LONDON_SPRING_FORWARD] {
            // Twelve hourly buckets spanning the transition are consecutive
            // multiples of one hour: none repeated, none skipped.
            let mut previous: Option<i64> = None;
            for step in -6..=6_i64 {
                let start = hour.bucket_start_ms(transition + step * 3_600_000);
                if let Some(previous) = previous {
                    assert_eq!(
                        start - previous,
                        3_600_000,
                        "hourly bucket stepped by something other than an hour at {transition}"
                    );
                }
                previous = Some(start);
            }
            // The transition instant itself is a whole hour on the grid.
            assert_eq!(hour.bucket_start_ms(transition), transition);

            // The daily bucket containing the transition is exactly 24 h, even
            // though the local day is 23 or 25.
            let day_start = day.bucket_start_ms(transition);
            assert_eq!(day.bucket_close_ms(transition) - day_start + 1, 86_400_000);
            assert_eq!(day_start % 86_400_000, 0);
        }

        // The doubled local hour, 01:30 EDT and 01:30 EST on 2026-11-01, is two
        // distinct instants and lands in two distinct hourly buckets.
        let first_0130_local = NY_FALL_BACK - 30 * 60_000;
        let second_0130_local = NY_FALL_BACK + 30 * 60_000;
        assert_ne!(
            hour.bucket_start_ms(first_0130_local),
            hour.bucket_start_ms(second_0130_local)
        );

        // And the whole DST year is a fixed number of hourly buckets: no ±1.
        let span = NY_FALL_BACK - NY_SPRING_FORWARD;
        assert_eq!(span % 3_600_000, 0);
        assert_eq!(
            (hour.bucket_start_ms(NY_FALL_BACK) - hour.bucket_start_ms(NY_SPRING_FORWARD))
                / 3_600_000,
            span / 3_600_000
        );
    }

    // ---- resampling -------------------------------------------------------

    /// Fourteen 1m bars from 00:00 UTC, resampled to 7m. Both target buckets
    /// are hand-computed below, including the boundary bar at 00:07 which must
    /// open the second bucket and must not touch the first.
    #[test]
    fn resamples_exactly_against_hand_computed_aggregation() {
        let m = 60_000_i64;
        let source: Vec<Bar> = vec![
            //     open      o        h        l        c        v      n
            bar(0, m, ["100.0", "101.0", "99.5", "100.5"], "1.1", 3),
            bar(m, m, ["100.5", "100.9", "100.0", "100.2"], "0.4", 2),
            bar(2 * m, m, ["100.2", "103.0", "100.1", "102.7"], "2.0", 9),
            bar(3 * m, m, ["102.7", "102.8", "98.0", "98.4"], "5.5", 12),
            bar(4 * m, m, ["98.4", "99.0", "98.4", "98.9"], "0.0", 0),
            bar(5 * m, m, ["98.9", "99.2", "98.7", "99.1"], "0.25", 1),
            bar(6 * m, m, ["99.1", "99.6", "99.0", "99.4"], "0.75", 4),
            // second bucket starts here
            bar(7 * m, m, ["99.4", "99.4", "99.4", "99.4"], "0", 0),
            bar(8 * m, m, ["99.4", "105.0", "99.4", "104.1"], "3.3", 7),
            bar(9 * m, m, ["104.1", "104.5", "103.0", "103.2"], "1.2", 5),
            bar(10 * m, m, ["103.2", "103.9", "97.25", "97.75"], "8.05", 20),
            bar(11 * m, m, ["97.75", "98.0", "97.5", "97.9"], "0.5", 2),
            bar(12 * m, m, ["97.9", "98.6", "97.9", "98.5"], "0.15", 1),
            bar(13 * m, m, ["98.5", "98.55", "98.2", "98.25"], "0.05", 1),
        ];

        let out = resample(&source, iv("1m"), iv("7m")).expect("7m divides evenly by 1m");
        assert_eq!(out.len(), 2);

        // Bucket [00:00, 00:07): first open 100.0, last close 99.4,
        // high max(101.0,100.9,103.0,102.8,99.0,99.2,99.6) = 103.0,
        // low  min(99.5,100.0,100.1,98.0,98.4,98.7,99.0)   = 98.0,
        // volume 1.1+0.4+2.0+5.5+0.0+0.25+0.75 = 10.0, trades 3+2+9+12+0+1+4 = 31.
        assert_eq!(
            out[0],
            bar(0, 7 * m, ["100.0", "103.0", "98.0", "99.4"], "10.0", 31)
        );

        // Bucket [00:07, 00:14): first open 99.4, last close 98.25,
        // high 105.0, low 97.25,
        // volume 0+3.3+1.2+8.05+0.5+0.15+0.05 = 13.25, trades 0+7+5+20+2+1+1 = 36.
        assert_eq!(
            out[1],
            bar(
                7 * m,
                7 * m,
                ["99.4", "105.0", "97.25", "98.25"],
                "13.25",
                36
            )
        );

        // Volume is summed in decimal, so the total is exact, not 9.999999998.
        assert_eq!(out[0].volume + out[1].volume, d("23.25"));
        assert_eq!(out[0].close_time_ms, 7 * m - 1);
        assert_eq!(out[1].open_time_ms, out[0].close_time_ms + 1);
    }

    #[test]
    fn resample_respects_bucket_boundaries_not_input_offsets() {
        let m = 15 * 60_000_i64;
        // Three 15m bars starting at 00:15, i.e. the input does not begin on a
        // 45m boundary. The first 45m bucket is [00:00, 00:45), so 00:15 and
        // 00:30 belong to it and 00:45 opens the next one.
        let source = vec![
            bar(m, m, ["10", "12", "9", "11"], "1", 1),
            bar(2 * m, m, ["11", "15", "10", "14"], "2", 2),
            bar(3 * m, m, ["14", "14", "13", "13"], "3", 3),
        ];
        let out = resample(&source, iv("15m"), iv("45m")).expect("45m is 3 x 15m");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], bar(0, 3 * m, ["10", "15", "9", "14"], "3", 3));
        assert_eq!(out[1], bar(3 * m, 3 * m, ["14", "14", "13", "13"], "3", 3));
    }

    #[test]
    fn resample_is_lossless_for_the_whole_native_ladder() {
        // Every native interval that divides another one round-trips volume and
        // trade count exactly, and preserves the first open and the last close.
        let m = 60_000_i64;
        let source: Vec<Bar> = (0..240)
            .map(|k| {
                let px = format!("{}.5", 100 + k % 7);
                bar(
                    k * m,
                    m,
                    [px.as_str(), px.as_str(), px.as_str(), px.as_str()],
                    "0.125",
                    2,
                )
            })
            .collect();
        for target in ["3m", "5m", "15m", "30m", "1h", "2h", "4h"] {
            let out = resample(&source, iv("1m"), iv(target)).expect("native divisor");
            let volume: Decimal = out.iter().map(|b| b.volume).sum();
            let trades: u32 = out.iter().map(|b| b.trades).sum();
            assert_eq!(volume, d("30.000"), "{target} lost volume");
            assert_eq!(trades, 480, "{target} lost trades");
            assert_eq!(out[0].open, source[0].open);
            assert_eq!(
                out.last().map(|b| b.close),
                source.last().map(|b| b.close),
                "{target} lost the final close"
            );
        }
    }

    #[test]
    fn resample_refuses_what_it_cannot_do_exactly() {
        let bars = vec![bar(0, 60_000, ["1", "1", "1", "1"], "1", 1)];
        assert_eq!(
            resample(&bars, iv("1m"), iv("90s")),
            Err(ResampleError::NotAMultiple {
                from: "1m".to_owned(),
                to: "90s".to_owned()
            })
        );
        assert_eq!(
            resample(&bars, iv("1h"), iv("1m")),
            Err(ResampleError::NotCoarser {
                from: "1h".to_owned(),
                to: "1m".to_owned()
            })
        );
        assert_eq!(
            resample(&bars, iv("1m"), iv("1m")),
            Err(ResampleError::NotCoarser {
                from: "1m".to_owned(),
                to: "1m".to_owned()
            })
        );
        let misaligned = vec![bar(30_000, 60_000, ["1", "1", "1", "1"], "1", 1)];
        assert_eq!(
            resample(&misaligned, iv("1m"), iv("5m")),
            Err(ResampleError::Misaligned {
                from: "1m".to_owned(),
                open_time_ms: 30_000
            })
        );
        let unsorted = vec![
            bar(60_000, 60_000, ["1", "1", "1", "1"], "1", 1),
            bar(0, 60_000, ["1", "1", "1", "1"], "1", 1),
        ];
        assert_eq!(
            resample(&unsorted, iv("1m"), iv("5m")),
            Err(ResampleError::OutOfOrder { open_time_ms: 0 })
        );
        let duplicated = vec![
            bar(0, 60_000, ["1", "1", "1", "1"], "1", 1),
            bar(0, 60_000, ["1", "1", "1", "1"], "1", 1),
        ];
        assert_eq!(
            resample(&duplicated, iv("1m"), iv("5m")),
            Err(ResampleError::OutOfOrder { open_time_ms: 0 })
        );
    }

    #[test]
    fn resample_of_nothing_is_nothing() {
        assert_eq!(resample(&[], iv("1m"), iv("7m")), Ok(Vec::new()));
    }

    // ---- venue rows -------------------------------------------------------

    fn candle(t: u64, i: &str, width: u64) -> Candle {
        Candle {
            t,
            t_close: t + width - 1,
            s: "BTC".to_owned(),
            i: i.to_owned(),
            o: d("1"),
            c: d("2"),
            h: d("3"),
            l: d("0.5"),
            v: d("7"),
            n: 4,
        }
    }

    #[test]
    fn converts_venue_rows_and_checks_the_partition() {
        let rows = vec![candle(0, "1m", 60_000), candle(60_000, "1m", 60_000)];
        let bars = bars_from_candles(&rows, iv("1m")).expect("well-formed venue rows");
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].close_time_ms, 59_999);

        assert_eq!(
            bars_from_candles(&[candle(0, "5m", 300_000)], iv("1m")),
            Err(BarError::IntervalMismatch {
                expected: "1m".to_owned(),
                found: "5m".to_owned()
            })
        );
        assert_eq!(
            bars_from_candles(&[candle(30_000, "1m", 60_000)], iv("1m")),
            Err(BarError::Misaligned {
                open_time_ms: 30_000,
                interval: "1m".to_owned()
            })
        );
        assert_eq!(
            bars_from_candles(&[candle(0, "1m", 59_000)], iv("1m")),
            Err(BarError::WrongWidth {
                open_time_ms: 0,
                actual_ms: 59_000,
                expected_ms: 60_000
            })
        );
    }

    // ---- local aggregation ------------------------------------------------

    fn trade(time_ms: i64, price: &str, size: &str) -> Trade {
        Trade {
            time_ms,
            price: d(price),
            size: d(size),
        }
    }

    #[test]
    fn local_aggregation_is_forward_only() {
        let interval = iv("90s");
        // Joined at 00:00:40, which is 40 s into the bucket that began at 00:00.
        let mut agg = LocalAggregator::new(interval, 40_000);
        assert_eq!(
            agg.no_history_before_ms(),
            90_000,
            "the partial first bucket is refused, not published as a bar"
        );

        assert!(agg.push(trade(50_000, "100", "1")).is_empty());
        assert_eq!(agg.discarded_partial_trades(), 1);
        assert!(agg.forming().is_none(), "nothing forming before history");

        // First whole bucket: [90_000, 180_000).
        assert!(agg.push(trade(90_000, "100", "1")).is_empty());
        assert!(agg.push(trade(120_000, "104", "2")).is_empty());
        assert!(agg.push(trade(150_000, "98", "0.5")).is_empty());
        let forming = agg.forming().expect("bucket in progress");
        assert_eq!(
            forming,
            bar(90_000, 90_000, ["100", "104", "98", "98"], "3.5", 3)
        );

        // A trade in the next bucket closes the first.
        let closed = agg.push(trade(185_000, "99", "1"));
        assert_eq!(
            closed,
            vec![bar(90_000, 90_000, ["100", "104", "98", "98"], "3.5", 3)]
        );

        // A late print never mutates a closed bar.
        assert!(agg.push(trade(95_000, "1000", "999")).is_empty());
        assert_eq!(agg.late_trades(), 1);
        assert_eq!(
            agg.forming().expect("second bucket"),
            bar(180_000, 90_000, ["99", "99", "99", "99"], "1", 1)
        );
    }

    #[test]
    fn silence_produces_flat_continuation_bars_like_the_venue() {
        let interval = iv("30s");
        let mut agg = LocalAggregator::new(interval, 0);
        assert_eq!(agg.no_history_before_ms(), 0);
        assert!(agg.push(trade(1_000, "50", "2")).is_empty());

        // Nothing trades for two whole buckets; the clock closes them.
        let closed = agg.close_through(120_000);
        assert_eq!(
            closed,
            vec![
                bar(0, 30_000, ["50", "50", "50", "50"], "2", 1),
                bar(30_000, 30_000, ["50", "50", "50", "50"], "0", 0),
                bar(60_000, 30_000, ["50", "50", "50", "50"], "0", 0),
                bar(90_000, 30_000, ["50", "50", "50", "50"], "0", 0),
            ],
            "empty buckets are flat continuations, matching the venue's own convention"
        );
        assert_eq!(
            agg.forming().expect("current bucket"),
            bar(120_000, 30_000, ["50", "50", "50", "50"], "0", 0)
        );
        assert_eq!(agg.skipped_fills(), 0);
    }

    #[test]
    fn a_long_silence_is_left_as_a_gap_not_ten_million_bars() {
        let mut agg = LocalAggregator::new(iv("1s"), 0);
        assert!(agg.push(trade(0, "50", "1")).is_empty());
        // Twenty thousand seconds of sleep, past MAX_FILL_BARS.
        let closed = agg.close_through(20_000 * 1_000);
        assert_eq!(closed.len(), 1, "only the bar we actually observed");
        assert_eq!(agg.skipped_fills(), 1);
    }

    #[test]
    fn resume_does_not_fabricate_across_the_gap_it_was_closed_for() {
        let interval = iv("90s");
        let mut agg = LocalAggregator::resume(interval, 900_000, 90_000, Some(d("100")));
        assert_eq!(
            agg.no_history_before_ms(),
            90_000,
            "the persisted floor is preserved across sessions"
        );
        // 900_000 is on a 90s boundary, so this session accepts from there.
        let closed = agg.push(trade(900_000, "101", "1"));
        assert!(
            closed.is_empty(),
            "no flat bars invented for the hours oppen was shut"
        );
    }

    #[test]
    fn local_volume_sums_are_exact() {
        let mut agg = LocalAggregator::new(iv("1m"), 0);
        for _ in 0..10 {
            agg.push(trade(1_000, "1", "0.1"));
        }
        assert_eq!(
            agg.forming().expect("forming").volume,
            d("1.0"),
            "ten lots of 0.1 is one, not 0.9999999999999999"
        );
    }

    // ---- storage ----------------------------------------------------------

    fn store_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        CandleStore::new(&conn).migrate().expect("migrate");
        conn
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        store.migrate().expect("second migrate is a no-op");
        store.migrate().expect("third migrate is a no-op");
    }

    #[test]
    fn bars_round_trip_through_the_side_tables() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let interval = iv("1m");
        let bars = vec![
            bar(0, 60_000, ["100.0", "101.5", "99.25", "100.75"], "12.5", 8),
            bar(
                60_000,
                60_000,
                ["100.75", "100.75", "100.75", "100.75"],
                "0",
                0,
            ),
        ];
        store
            .put_bars("BTC", interval, Source::Venue, &bars)
            .expect("insert");

        assert_eq!(store.bars("BTC", interval, 0, 120_000).expect("read"), bars);
        assert_eq!(
            store.bars("BTC", interval, 0, 59_999).expect("read"),
            bars[..1].to_vec()
        );
        assert_eq!(
            store.bars("ETH", interval, 0, 120_000).expect("read"),
            Vec::new(),
            "series are keyed by coin"
        );
        assert_eq!(
            store.bars("BTC", iv("5m"), 0, 120_000).expect("read"),
            Vec::new(),
            "series are keyed by interval"
        );
        assert_eq!(
            store.latest_bar("BTC", interval).expect("latest").as_ref(),
            bars.last()
        );
        assert_eq!(
            store.bar_source("BTC", interval, 0).expect("source"),
            Some(Source::Venue)
        );
        assert_eq!(
            store.bar_source("BTC", interval, 999).expect("source"),
            None
        );
    }

    #[test]
    fn the_forming_bar_is_replaced_not_duplicated() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let interval = iv("1m");
        store
            .put_bars(
                "BTC",
                interval,
                Source::Local,
                &[bar(0, 60_000, ["1", "1", "1", "1"], "1", 1)],
            )
            .expect("insert");
        store
            .put_bars(
                "BTC",
                interval,
                Source::Local,
                &[bar(0, 60_000, ["1", "9", "1", "9"], "5", 4)],
            )
            .expect("replace");
        let stored = store.bars("BTC", interval, 0, 60_000).expect("read");
        assert_eq!(stored, vec![bar(0, 60_000, ["1", "9", "1", "9"], "5", 4)]);
    }

    #[test]
    fn prune_drops_old_local_bars_and_moves_the_history_floor() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let interval = iv("90s");
        let day = 24 * 60 * 60 * 1_000_i64;
        let now = 40 * day;

        let old = bar(day, 90_000, ["1", "1", "1", "1"], "1", 1);
        let recent = bar(35 * day, 90_000, ["2", "2", "2", "2"], "2", 1);
        store
            .put_bars("BTC", interval, Source::Local, &[old, recent.clone()])
            .expect("insert local");
        // A venue series in the same window must survive: it is re-derivable
        // and D-e only budgets the locally aggregated bars.
        store
            .put_bars(
                "BTC",
                iv("1m"),
                Source::Venue,
                &[bar(day, 60_000, ["3", "3", "3", "3"], "3", 1)],
            )
            .expect("insert venue");
        store
            .set_no_history_before("BTC", interval, day)
            .expect("floor");

        let removed = store
            .prune_local(now, DEFAULT_LOCAL_RETENTION_MS)
            .expect("prune");
        assert_eq!(removed, 1);
        assert_eq!(
            store.bars("BTC", interval, 0, now).expect("read"),
            vec![recent]
        );
        assert_eq!(
            store.bars("BTC", iv("1m"), 0, now).expect("read").len(),
            1,
            "venue bars are not pruned"
        );
        assert_eq!(
            store.no_history_before("BTC", interval).expect("floor"),
            Some(now - DEFAULT_LOCAL_RETENTION_MS),
            "the UI must not claim history that was just deleted"
        );
    }

    #[test]
    fn the_history_floor_only_moves_forward() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let interval = iv("90s");
        assert_eq!(
            store.no_history_before("BTC", interval).expect("empty"),
            None
        );
        store
            .set_no_history_before("BTC", interval, 5_000)
            .expect("set");
        store
            .set_no_history_before("BTC", interval, 9_000)
            .expect("raise");
        assert_eq!(
            store.no_history_before("BTC", interval).expect("floor"),
            Some(9_000)
        );
        store
            .set_no_history_before("BTC", interval, 1_000)
            .expect("lower");
        assert_eq!(
            store.no_history_before("BTC", interval).expect("floor"),
            Some(9_000),
            "a floor that moved backwards would claim deleted history exists"
        );
    }

    #[test]
    fn stored_decimals_keep_their_exact_value() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let interval = iv("1m");
        let precise = bar(
            0,
            60_000,
            ["0.000000000000000001", "123456789.987654321", "0.1", "0.3"],
            "0.30000000000000004",
            1,
        );
        store
            .put_bars(
                "BTC",
                interval,
                Source::Venue,
                std::slice::from_ref(&precise),
            )
            .expect("insert");
        assert_eq!(
            store.bars("BTC", interval, 0, 0).expect("read"),
            vec![precise]
        );
    }

    // ---- end to end -------------------------------------------------------

    #[test]
    fn a_resampled_series_survives_a_store_round_trip() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let m = 60_000_i64;
        let native: Vec<Bar> = (0..21)
            .map(|k| bar(k * m, m, ["100", "101", "99", "100"], "1", 1))
            .collect();
        store
            .put_bars("BTC", iv("1m"), Source::Venue, &native)
            .expect("insert native");

        let loaded = store.bars("BTC", iv("1m"), 0, 21 * m).expect("read native");
        let Resolution::Resampled { from } = iv("7m").resolve() else {
            panic!("7m must resample");
        };
        assert_eq!(from, iv("1m"));
        let out = resample(&loaded, from, iv("7m")).expect("resample");
        assert_eq!(out.len(), 3);
        store
            .put_bars("BTC", iv("7m"), Source::Resampled, &out)
            .expect("insert resampled");

        assert_eq!(store.bars("BTC", iv("7m"), 0, 21 * m).expect("read"), out);
        assert_eq!(
            store.bar_source("BTC", iv("7m"), 0).expect("source"),
            Some(Source::Resampled)
        );
        // Pruning local bars leaves a resampled series alone: it is derivable
        // from the native rows next to it.
        assert_eq!(store.prune_local(i64::MAX / 2, 0).expect("prune"), 0);
    }
}
