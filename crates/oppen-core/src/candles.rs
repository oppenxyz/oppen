//! Candle store and the arbitrary-interval engine.
//!
//! Implements `docs/specs/charts.md` §3. The operator types any interval and
//! gets a chart; every requested interval resolves to exactly one of three
//! cases and the UI is told which one, because silently degrading a chart is
//! how a reader ends up trusting a line that is not there. [`Resolution`] is
//! those three cases and what each one promises.
//!
//! Nothing here reads a local clock or a calendar: buckets are epoch-aligned
//! integer arithmetic ([`Interval::bucket_start_ms`]), so the same interval
//! produces the same buckets on every machine and across a DST transition.
//!
//! Aggregation lives here rather than in the view layer because the fair-value
//! engine and the renderer must see the same bars (`charts.md` §2.4), and
//! because `docs/decisions.md` R1 keeps this crate headless. Bars land in the
//! unchained side tables described on [`CandleStore`] (`docs/decisions.md` R5).
//!
//! Owed to `docs/specs/charts.md`: §3.2's History column reads "Full" for the
//! resampled row and needs amending to "full, or the source's rolling floor" —
//! see the per-request cap in the audit below.
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

use std::cmp::Ordering;
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

/// Earliest timestamp this module accepts. The venue serves no pre-epoch bar.
pub const MIN_TIME_MS: i64 = 0;

/// Latest timestamp this module accepts: 9999-12-31T23:59:59.999Z.
///
/// Bounding both ends at the two doors — [`bars_from_candles`] and
/// [`Trade::checked`] — is what makes every millisecond addition downstream
/// provably in range, rather than scattering `checked_add` across the module.
/// `MAX_TIME_MS + MAX_INTERVAL_MS` is four orders of magnitude below
/// `i64::MAX`, so a bucket close can never wrap.
pub const MAX_TIME_MS: i64 = 253_402_300_799_999;

/// How many intervals `candleSnapshot` serves, whatever the interval. A rolling
/// floor, not a page size — see the audit note in the module docs.
pub const VENUE_HISTORY_INTERVALS: u32 = 5_000;

/// The time unit of an interval string. Case matters and is not a typo:
/// `m` is a minute and `M` is the venue's thirty-day bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// `s`. Only ever produces a locally aggregated series: the venue's finest
    /// bar is `1m`.
    Second,
    /// `m`, lowercase.
    Minute,
    Hour,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    count: u32,
    unit: Unit,
}

impl Ord for Interval {
    /// By width, never by field order. Written out because the derived version
    /// compares `count` first, which puts `1M` below `3m`, leaves
    /// [`NATIVE_INTERVALS`] unsorted under its own ordering, and would make a
    /// `BTreeMap<Interval, _>` serialise in that order — a determinism hazard
    /// on the MCP surface (`AGENTS.md` 6). Agrees with the derived [`Eq`]
    /// because canonicalisation gives exactly one `Interval` per width.
    fn cmp(&self, other: &Self) -> Ordering {
        self.millis().cmp(&other.millis())
    }
}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// How far back a source serves, for an interval that has a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Horizon {
    /// Everything the venue has ever had for the asset. True where the venue's
    /// own age is fewer than [`VENUE_HISTORY_INTERVALS`] bars: BTC `1d` returns
    /// all 2,208 rows back to 2019.
    Full,
    /// Only the most recent `intervals` bars exist, no matter how the request
    /// is paged.
    Rolling {
        /// How many bars back the source reaches.
        intervals: u32,
    },
}

impl Horizon {
    /// The horizon as a duration, for an interval of this width. `None` when
    /// there is no limit to express.
    pub const fn span_ms(self, of: Interval) -> Option<i64> {
        match self {
            Horizon::Full => None,
            Horizon::Rolling { intervals } => Some((intervals as i64).saturating_mul(of.millis())),
        }
    }
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

/// The venue's fixed menu, ascending by width and therefore sorted under
/// [`Interval`]'s own [`Ord`] — which [`Interval::resolve`] relies on when it
/// walks the menu backwards looking for the largest divisor. `charts.md` §3.1
/// lists it; the audit in the module docs confirms these fourteen and only
/// these fourteen are served.
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

    /// Width in milliseconds. Never zero, so it is always a safe divisor.
    pub const fn millis(self) -> i64 {
        self.count as i64 * self.unit.millis()
    }

    /// Whether the venue serves this interval directly.
    pub fn is_native(self) -> bool {
        NATIVE_INTERVALS.contains(&self)
    }

    /// How far back `candleSnapshot` serves this interval.
    ///
    /// The floor only bites when the asset is older than [`VENUE_HISTORY_INTERVALS`]
    /// bars. Five thousand days is thirteen years, more than Hyperliquid has
    /// existed, which is why BTC `1d` returns its whole history while BTC `1m`
    /// reaches only about three and a half days back.
    pub const fn venue_history(self) -> Horizon {
        if self.millis() >= Unit::Day.millis() {
            Horizon::Full
        } else {
            Horizon::Rolling {
                intervals: VENUE_HISTORY_INTERVALS,
            }
        }
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
                return Resolution::Resampled {
                    from: *native,
                    source_history: native.venue_history(),
                };
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
    ///
    /// Saturating rather than wrapping because a `pub const fn` must not panic
    /// for any `i64` (`AGENTS.md` conventions). [`MIN_TIME_MS`] and
    /// [`MAX_TIME_MS`] bound every timestamp that enters the module, so
    /// saturation is only reachable from a direct call with an absurd argument.
    pub const fn bucket_start_ms(self, t_ms: i64) -> i64 {
        let width = self.millis();
        t_ms.div_euclid(width).saturating_mul(width)
    }

    /// Inclusive close of the bucket containing `t_ms`, in the venue's own
    /// convention: `T = t + width - 1`, verified on every bar in the audit.
    /// Saturating for the same reason as [`Interval::bucket_start_ms`].
    pub const fn bucket_close_ms(self, t_ms: i64) -> i64 {
        self.bucket_start_ms(t_ms).saturating_add(self.millis() - 1)
    }
}

impl fmt::Display for Interval {
    /// The canonical wire string. For a native interval this is exactly what
    /// `candleSnapshot`'s `interval` field wants.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.count, self.unit.suffix())
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
    ///
    /// Lossless is not the same as unlimited. A resampled interval inherits its
    /// source's horizon, and a `1m` source reaches roughly three and a half
    /// days back, so `7m` and `13m` are three-and-a-half-day charts while a
    /// `2h`-derived `6h` goes back years. `source_history` is what stops the
    /// chip from claiming otherwise.
    Resampled {
        /// The largest native interval that divides this one.
        from: Interval,
        /// How far back `from` is served — [`Interval::venue_history`].
        source_history: Horizon,
    },
    /// Built here from the `trades` feed. **Forward only** — there is no
    /// history and the UI must say so rather than drawing a flat line.
    Local {
        /// Which condition in `charts.md` §3.2 put it in this case.
        reason: LocalReason,
    },
}

impl Resolution {
    /// The chip text from `charts.md` §3.3, extended with the source's reach
    /// when that reach is finite. Kept in core rather than the view so the
    /// requirement "the UI must say which one it is" is testable.
    ///
    /// `RESAMPLED FROM 1m · 3d` rather than a bare `RESAMPLED FROM 1m`: the
    /// bare form says the same thing for a chart that goes back three days and
    /// one that goes back a decade, which is the silent degrade this whole type
    /// exists to prevent.
    pub fn label(self) -> String {
        match self {
            Resolution::Native => "NATIVE".to_owned(),
            Resolution::Resampled {
                from,
                source_history,
            } => match source_history.span_ms(from) {
                None => format!("RESAMPLED FROM {from}"),
                Some(span) => format!("RESAMPLED FROM {from} · {}", compact_span(span)),
            },
            Resolution::Local { .. } => "LOCAL · FORWARD ONLY".to_owned(),
        }
    }

    /// Whether this case has *any* history behind it. `false` means the chart
    /// starts at the moment oppen first listened and the empty region before it
    /// must be labelled, never drawn.
    ///
    /// It does not mean "full history": ask [`Resolution::source_history`] for
    /// how far back the case actually reaches.
    pub fn has_history(self) -> bool {
        !matches!(self, Resolution::Local { .. })
    }

    /// How far back the *source* reaches, for the one case where the source is
    /// not the requested interval. `None` for the native case — ask
    /// [`Interval::venue_history`] on the interval itself — and `None` for the
    /// local case, which has no history at all and reports it through
    /// [`LocalAggregator::no_history_before_ms`] instead.
    pub const fn source_history(self) -> Option<Horizon> {
        match self {
            Resolution::Resampled { source_history, .. } => Some(source_history),
            Resolution::Native | Resolution::Local { .. } => None,
        }
    }
}

/// A millisecond span as the coarsest whole unit that is at least one, for chip
/// text. Deliberately truncating: `3d` under-promises a 3.47-day horizon, and a
/// history marker that rounds up is a claim.
fn compact_span(ms: i64) -> String {
    const MINUTE_MS: i64 = 60 * MS_PER_SECOND;
    const HOUR_MS: i64 = 60 * MINUTE_MS;
    const DAY_MS: i64 = 24 * HOUR_MS;
    if ms >= DAY_MS {
        format!("{}d", ms / DAY_MS)
    } else if ms >= HOUR_MS {
        format!("{}h", ms / HOUR_MS)
    } else {
        format!("{}m", ms / MINUTE_MS)
    }
}

/// Where a stored bar came from. Persisted so retention can distinguish the
/// re-derivable from the unrecoverable: `docs/decisions.md` D-e prunes local
/// bars on a budget and leaves venue bars alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    const fn as_str(self) -> &'static str {
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
    pub high: Decimal,
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
    ///
    /// This is one of the two doors timestamps enter through — [`Trade::checked`]
    /// is the other — and bounding them here is what makes the millisecond
    /// arithmetic in [`resample`] and [`LocalAggregator`] provably in range.
    /// A row with `t = 2^63 - 2^16` passes every other check this module makes
    /// (right interval, aligned open, exactly one width wide) and then overflows
    /// `i64` inside the resampler.
    ///
    /// Private: a venue row reaches the outside world through
    /// [`bars_from_candles`], which also checks the partition. This alone does
    /// not, and a bar that skipped those checks corrupts every aggregate above
    /// it.
    fn from_candle(candle: &Candle) -> Result<Self, BarError> {
        Ok(Self {
            open_time_ms: checked_time_ms(candle.t)?,
            close_time_ms: checked_time_ms(candle.t_close)?,
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
            close_time_ms: open_time_ms.saturating_add(width_ms - 1),
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
        expected: String,
        /// What the row carries in its `i` field.
        found: String,
    },
    /// A candle timestamp outside `MIN_TIME_MS..=MAX_TIME_MS`.
    #[error("candle timestamp {0} ms is out of range")]
    Timestamp(u64),
    /// A trade timestamp outside `MIN_TIME_MS..=MAX_TIME_MS`. Signed, because
    /// the WS feed's own field is signed and a negative print is as wrong as an
    /// absurdly large one.
    #[error("trade timestamp {time_ms} ms is out of range")]
    TradeTime { time_ms: i64 },
    /// The row's open is not aligned to its own interval, which would break
    /// every downstream bucket.
    #[error("candle open {open_time_ms} ms is not aligned to {interval}")]
    Misaligned { open_time_ms: i64, interval: String },
    /// `T` is not `t + width - 1`. The venue has never done this; if it starts,
    /// the resampler's partition assumption is void and we stop rather than
    /// aggregate.
    #[error("candle at {open_time_ms} ms spans {actual_ms} ms, expected {expected_ms} ms")]
    WrongWidth {
        open_time_ms: i64,
        /// `T - t + 1` as served.
        actual_ms: i64,
        expected_ms: i64,
    },
}

/// A venue timestamp, bounded. The venue's field is `u64`, so only the upper
/// bound can bite, but the range is written out so the invariant is one thing
/// and not two.
fn checked_time_ms(raw: u64) -> Result<i64, BarError> {
    let ms = i64::try_from(raw).map_err(|_| BarError::Timestamp(raw))?;
    if !(MIN_TIME_MS..=MAX_TIME_MS).contains(&ms) {
        return Err(BarError::Timestamp(raw));
    }
    Ok(ms)
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
    /// Throughout this enum `from` is the interval the bars are in and `to` the
    /// one that was requested, rather than `source`/`target`, because
    /// `thiserror` reads a field named `source` as a nested error.
    #[error("{to} is not an integer multiple of {from}")]
    NotAMultiple { from: String, to: String },
    /// The target is the same width as the source or narrower. Resampling only
    /// ever coarsens; going finer would require inventing intra-bar structure.
    #[error("{to} is not coarser than {from}")]
    NotCoarser { from: String, to: String },
    /// A source bar's open is not aligned to the source interval.
    #[error("source bar at {open_time_ms} ms is not aligned to {from}")]
    Misaligned { from: String, open_time_ms: i64 },
    /// Source bars are not strictly ascending. Deduplicating or sorting here
    /// would hide a feed bug behind a plausible chart.
    #[error("source bars are not strictly ascending at {open_time_ms} ms")]
    OutOfOrder {
        /// The open that did not exceed the one before it.
        open_time_ms: i64,
    },
    /// A target bucket in the interior of the window did not receive all of its
    /// source bars.
    ///
    /// The venue emits a bar for **every** bucket including empty ones (282 of
    /// PURR's 1,441 `1m` bars in the audit window), so a hole is a feed bug.
    /// Aggregating around it would produce a bar that is byte-indistinguishable
    /// from a correct one, which is the one thing this module will not do
    /// (`charts.md` §3.2, "resampling is exact, not approximate").
    #[error("target bucket at {open_time_ms} ms got {got} of {want} source bars")]
    IncompleteBucket {
        open_time_ms: i64,
        /// How many source bars landed in it.
        got: i64,
        /// How many it needed: `target / source`.
        want: i64,
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
/// ## Incomplete target buckets
///
/// A target bucket needs exactly `target / source` source bars. What happens
/// when it does not get them depends on *where* it is, because the three cases
/// mean three different things:
///
/// * **Leading.** The window began part-way through the first target bucket, so
///   its true open, high and low were never served. This is the normal case,
///   not an edge one: `candleSnapshot` serves a rolling
///   [`VENUE_HISTORY_INTERVALS`]-bar window whose oldest bar sits at an
///   arbitrary offset into a resampled bucket — measured 2026-09-04, BTC `1m`'s
///   oldest row was 13 minutes into a `45m` bucket. The bucket is **dropped**;
///   the series simply starts at the next whole one. Emitting it would produce
///   a bar byte-indistinguishable from a correct one at the left edge of every
///   resampled chart.
/// * **Interior.** A hole. The venue emits a bar for every bucket, so this is a
///   feed bug and it is refused with [`ResampleError::IncompleteBucket`] rather
///   than absorbed.
/// * **Trailing.** The last bucket is still forming. It is returned like any
///   other and the caller decides with [`Bar::is_closed`] — but only if what
///   arrived is an unbroken run from the bucket's own start. A hole inside the
///   last bucket is a hole like any other and is refused; "still forming" is
///   the only reason a returned bucket may be short.
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
    let want = target_ms / source_ms;

    let mut out: Vec<Bar> = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut previous_open: Option<i64> = None;
    // Set once the first target bucket has been decided, kept or dropped, so a
    // dropped leading bucket cannot make the second one look like the first.
    let mut leading_resolved = false;

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
        match &mut pending {
            Some(current) if current.bar.open_time_ms == bucket => {
                current.bar.close = bar.close;
                current.bar.high = current.bar.high.max(bar.high);
                current.bar.low = current.bar.low.min(bar.low);
                current.bar.volume += bar.volume;
                current.bar.trades = current.bar.trades.saturating_add(bar.trades);
                current.got += 1;
                current.last_open_ms = bar.open_time_ms;
            }
            _ => {
                if let Some(done) = pending.take() {
                    if let Some(kept) = done.seal(want, leading_resolved)? {
                        out.push(kept);
                    }
                    leading_resolved = true;
                }
                pending = Some(Pending {
                    bar: Bar {
                        open_time_ms: bucket,
                        close_time_ms: target.bucket_close_ms(bucket),
                        ..bar.clone()
                    },
                    got: 1,
                    opened_on_boundary: bar.open_time_ms == bucket,
                    last_open_ms: bar.open_time_ms,
                });
            }
        }
    }

    if let Some(last) = pending {
        // The trailing bucket is allowed to be short: it is the one still
        // forming. Unless it is also the leading one and we never saw its open,
        // in which case it is short for the other reason and has to go.
        if leading_resolved || last.opened_on_boundary {
            // Short only earns the benefit of the doubt if what arrived is an
            // unbroken run from the bucket's own start. A hole inside the last
            // bucket produces a bar byte-indistinguishable from one that is
            // genuinely still filling, which is the same defect as an interior
            // hole and gets the same answer.
            if !last.is_contiguous_prefix(source_ms) {
                return Err(ResampleError::IncompleteBucket {
                    open_time_ms: last.bar.open_time_ms,
                    got: last.got,
                    want,
                });
            }
            out.push(last.bar);
        }
    }
    Ok(out)
}

/// A target bucket under construction, with enough context to judge whether it
/// is complete and, if not, which kind of incomplete.
struct Pending {
    bar: Bar,
    got: i64,
    opened_on_boundary: bool,
    last_open_ms: i64,
}

impl Pending {
    /// Decide a bucket that has been left behind, i.e. one that is not the
    /// trailing bucket. `Ok(None)` means "drop it silently"; see [`resample`].
    fn seal(self, want: i64, leading_resolved: bool) -> Result<Option<Bar>, ResampleError> {
        if self.got == want {
            return Ok(Some(self.bar));
        }
        if !leading_resolved && !self.opened_on_boundary {
            return Ok(None);
        }
        Err(ResampleError::IncompleteBucket {
            open_time_ms: self.bar.open_time_ms,
            got: self.got,
            want,
        })
    }

    /// Whether the source bars seen so far are the bucket's first `got` slots
    /// with nothing missing between them. Source bars are aligned and strictly
    /// ascending, so that is true exactly when the newest one sits `got - 1`
    /// source widths above the bucket start.
    ///
    /// Saturating because [`Bar`]'s fields are public and `resample` takes any
    /// slice; a caller-built bar near `i64::MAX` must not panic here.
    fn is_contiguous_prefix(&self, source_ms: i64) -> bool {
        let expected = self
            .bar
            .open_time_ms
            .saturating_add(self.got.saturating_sub(1).saturating_mul(source_ms));
        self.last_open_ms == expected
    }
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

impl Trade {
    /// Build a print, refusing a timestamp this module cannot do arithmetic on.
    ///
    /// The WS converter calls this instead of writing the struct literally, so
    /// an out-of-range time is a typed rejection at the boundary rather than an
    /// overflow four calls later. [`LocalAggregator::push`] re-checks anyway,
    /// because the fields are public and a literal is still constructible.
    pub fn checked(time_ms: i64, price: Decimal, size: Decimal) -> Result<Self, BarError> {
        if !(MIN_TIME_MS..=MAX_TIME_MS).contains(&time_ms) {
            return Err(BarError::TradeTime { time_ms });
        }
        Ok(Self {
            time_ms,
            price,
            size,
        })
    }
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
/// history: it says "no trades".
///
/// # The caller owes it an interruption signal
///
/// A flat bar means "the feed was watching and nothing traded". This type
/// cannot tell that apart from "the socket was dead" on its own — a trade ten
/// minutes after the previous one looks identical either way — so the WS pool
/// **must** call [`LocalAggregator::feed_interrupted`] on every disconnect and
/// every staleness trip, and must pass a truthful `feed_live_through_ms` to
/// [`LocalAggregator::close_through`]. Without that, a ten-minute outage on a
/// `30s` chart comes back as nineteen fabricated flat bars at the pre-outage
/// price, which is precisely the "flat line that reads as a quiet market"
/// `charts.md` §3.2 forbids and the failure [`Resolution::Local`] exists to
/// prevent.
#[derive(Debug, Clone)]
pub struct LocalAggregator {
    interval: Interval,
    no_history_before_ms: i64,
    accepts_from_ms: i64,
    bucket: Option<i64>,
    newest_bucket_ms: Option<i64>,
    forming: Option<Bar>,
    last_close: Option<Decimal>,
    late: u64,
    before_history: u64,
    skipped_fills: u64,
    out_of_range: u64,
    gaps: u64,
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
            newest_bucket_ms: None,
            forming: None,
            last_close: None,
            late: 0,
            before_history: 0,
            skipped_fills: 0,
            out_of_range: 0,
            gaps: 0,
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
            no_history_before_ms,
            last_close,
            ..Self::new(interval, listening_since_ms)
        }
    }

    /// Clamped into `MIN_TIME_MS..=MAX_TIME_MS`, because the caller's
    /// `listening_since_ms` is an unchecked clock reading and every bucket
    /// derived from it is then provably in range.
    fn first_whole_bucket(interval: Interval, from_ms: i64) -> i64 {
        let from_ms = from_ms.clamp(MIN_TIME_MS, MAX_TIME_MS);
        let start = interval.bucket_start_ms(from_ms);
        if start == from_ms {
            start
        } else {
            start.saturating_add(interval.millis())
        }
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

    /// Prints dropped for carrying a timestamp outside
    /// `MIN_TIME_MS..=MAX_TIME_MS`. Non-zero means the feed or the converter is
    /// producing garbage, not that the market did something.
    pub const fn out_of_range_trades(&self) -> u64 {
        self.out_of_range
    }

    /// How many times the feed has been declared interrupted while this
    /// aggregator held state. Each one is a hole in the stored series that was
    /// deliberately left as absent rows rather than flat-filled.
    pub const fn feed_gaps(&self) -> u64 {
        self.gaps
    }

    /// Tell the aggregator the trades feed stopped being trustworthy.
    ///
    /// Called by the WS pool on every disconnect and on every staleness trip.
    /// The in-progress bucket is discarded rather than emitted — it is partial,
    /// and a partial bucket published as a whole bar is a wrong number, not a
    /// rough one — and the next trade opens a fresh bucket with nothing filled
    /// in behind it, exactly as [`LocalAggregator::resume`] does across a
    /// process restart. The gap becomes absent rows, which the renderer labels.
    ///
    /// The last close is kept, so a symbol that is genuinely quiet after the
    /// reconnect still continues from a price rather than from nothing.
    ///
    /// The late-trade watermark is **not** cleared. `bucket` is where the
    /// aggregator is writing; `newest_bucket_ms` is how far it has ever been,
    /// and only the first is forgotten. Clearing both would let a print
    /// replayed from before the outage — reconnects deliver those — re-open an
    /// already-emitted bucket, and the next live print would then flat-fill the
    /// whole outage from it, which is the fabrication this method exists to
    /// prevent.
    pub fn feed_interrupted(&mut self) {
        if self.bucket.is_some() || self.forming.is_some() {
            self.gaps += 1;
        }
        self.bucket = None;
        self.forming = None;
    }

    /// The in-progress bar, if any: either the bucket's trades so far, or a
    /// flat continuation when the current bucket has had none.
    pub fn forming(&self) -> Option<Bar> {
        if let Some(bar) = &self.forming {
            return Some(bar.clone());
        }
        let (open, close) = (self.bucket?, self.last_close?);
        Some(Bar::empty_at(open, self.interval.millis(), close))
    }

    /// Fold one trade in, returning any bars that closed as a result, ascending.
    ///
    /// A print asserts the feed was live at its own instant, so the buckets
    /// between the last one and this one are flat-filled. That is only sound
    /// because the pool calls [`LocalAggregator::feed_interrupted`] when it is
    /// not — see this type's documentation.
    pub fn push(&mut self, trade: Trade) -> Vec<Bar> {
        if !(MIN_TIME_MS..=MAX_TIME_MS).contains(&trade.time_ms) {
            self.out_of_range += 1;
            return Vec::new();
        }
        if trade.time_ms < self.accepts_from_ms {
            self.before_history += 1;
            return Vec::new();
        }
        let bucket = self.interval.bucket_start_ms(trade.time_ms);
        // Against the high-water mark rather than the current bucket: after
        // `feed_interrupted` there is no current bucket, and a print replayed
        // from before the outage must still be refused rather than re-opening a
        // bucket that has already been emitted.
        if self.newest_bucket_ms.is_some_and(|newest| bucket < newest) {
            self.late += 1;
            return Vec::new();
        }
        let width = self.interval.millis();
        let closed = self.advance_to(bucket, bucket);
        // An absent bucket starts as an empty one at this print's price, so the
        // fold below is the same arithmetic for the first trade and the tenth.
        let bar = self
            .forming
            .get_or_insert_with(|| Bar::empty_at(bucket, width, trade.price));
        bar.high = bar.high.max(trade.price);
        bar.low = bar.low.min(trade.price);
        bar.close = trade.price;
        bar.volume += trade.size;
        bar.trades = bar.trades.saturating_add(1);
        closed
    }

    /// Close every bucket that has finished as of `now_ms`, returning them
    /// ascending. Driven by the caller's clock so a quiet symbol still produces
    /// bars; without it a bucket with no trades would never close.
    ///
    /// `feed_live_through_ms` is the caller's assertion of how far the trades
    /// subscription is actually known to have been alive — the pool's last
    /// received message, not its wall clock. Flat continuation bars are only
    /// synthesised for buckets at or before it, so a stalled socket produces
    /// absent rows instead of a plausible flat market. In the healthy case the
    /// two arguments are the same value and nothing changes.
    ///
    /// A bucket that received real prints is emitted with what was observed
    /// either way; the clamp governs invention, not observation.
    pub fn close_through(&mut self, now_ms: i64, feed_live_through_ms: i64) -> Vec<Bar> {
        if now_ms < self.accepts_from_ms || now_ms > MAX_TIME_MS {
            return Vec::new();
        }
        let target = self.interval.bucket_start_ms(now_ms);
        let fill_through = self
            .interval
            .bucket_start_ms(feed_live_through_ms.clamp(MIN_TIME_MS, MAX_TIME_MS));
        self.advance_to(target, fill_through)
    }

    /// Move the current bucket to `target`, emitting everything in between.
    ///
    /// `fill_through` is the newest bucket start for which a flat continuation
    /// may be synthesised. Buckets past it are left absent.
    fn advance_to(&mut self, target: i64, fill_through: i64) -> Vec<Bar> {
        let width = self.interval.millis();
        let Some(current) = self.bucket else {
            self.enter_bucket(target);
            return Vec::new();
        };
        if target <= current {
            return Vec::new();
        }
        self.enter_bucket(target);

        let mut out = Vec::new();
        if let Some(bar) = self.forming.take() {
            self.last_close = Some(bar.close);
            out.push(bar);
        } else if let Some(close) = self.last_close
            && current <= fill_through
        {
            // No prints, and the caller vouches for the feed inside this
            // bucket, so it really was empty: flat continuation.
            out.push(Bar::empty_at(current, width, close));
        }
        if let Some(close) = self.last_close {
            let newest_fill = (target - width).min(fill_through);
            let gap = (newest_fill - current) / width;
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

    /// Start writing into `target`, keeping the high-water mark that outlives
    /// [`LocalAggregator::feed_interrupted`].
    fn enter_bucket(&mut self, target: i64) {
        self.bucket = Some(target);
        self.newest_bucket_ms = Some(self.newest_bucket_ms.map_or(target, |n| n.max(target)));
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
/// Opening a second `Connection` to the same file instead would skip the
/// ledger's own `configure()` and so run with no `busy_timeout`, no WAL check
/// and `synchronous` at the default — a concurrent ledger write would come back
/// `SQLITE_BUSY` immediately rather than waiting.
///
/// Nothing here is hash-chained: candles are re-derivable, and a chart is not a
/// record of record.
///
/// Every write nests in a `SAVEPOINT`, so these methods are safe to call from
/// inside a ledger transaction. `Ledger` still owes an accessor that hands out a
/// `CandleStore` under its mutex — `Connection` is not `Sync`, so the borrow has
/// to come from behind that lock.
pub struct CandleStore<'a> {
    conn: &'a Connection,
}

impl<'a> CandleStore<'a> {
    /// Attach to an open per-network connection. See [`crate::db_file_name`].
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Run `f` inside a savepoint.
    ///
    /// A savepoint rather than `BEGIN`: `Connection::unchecked_transaction`
    /// issues a bare `BEGIN DEFERRED`, which fails outright with "cannot start
    /// a transaction within a transaction" when the ledger already has one
    /// open on this connection. `SAVEPOINT` nests, and outside a transaction it
    /// starts one, so this is atomic in both situations.
    fn in_savepoint<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.conn.execute_batch("SAVEPOINT oppen_candles")?;
        match f(self.conn) {
            Ok(value) => {
                self.conn.execute_batch("RELEASE oppen_candles")?;
                Ok(value)
            }
            Err(err) => {
                // Undo the partial write and drop the savepoint, leaving any
                // enclosing ledger transaction intact. The unwind must not mask
                // the failure that caused it, so its own result is discarded.
                let _ = self
                    .conn
                    .execute_batch("ROLLBACK TO oppen_candles; RELEASE oppen_candles");
                Err(err)
            }
        }
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

    /// Insert or replace bars for one series, atomically.
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
        self.in_savepoint(|conn| {
            let mut stmt = conn.prepare(
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
            Ok(())
        })
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
    /// The floor of each series that actually loses rows is raised to the
    /// cutoff in the same savepoint, derived from the rows about to go rather
    /// than written across the whole table. Two failures fall out of that:
    ///
    /// * A series that never had a floor row gets one, so the UI can tell
    ///   "thirty days were just deleted" from "nothing has been aggregated
    ///   yet". Pruning without that leaves the marker absent at exactly the
    ///   moment it is needed, which is the one failure it exists to prevent.
    /// * A series the prune did not touch keeps its floor. A blanket `UPDATE`
    ///   moves every row, including series with no stored bars at all, which
    ///   makes the marker claim a deletion that never happened.
    pub fn prune_local(&self, now_ms: i64, retention_ms: i64) -> Result<usize, StoreError> {
        let cutoff = now_ms.saturating_sub(retention_ms);
        self.in_savepoint(|conn| {
            // Before the DELETE: the rows are still there to be read from.
            conn.execute(
                "INSERT INTO candle_local_history (coin, interval, no_history_before_ms)
                 SELECT DISTINCT coin, interval, ?2
                   FROM candle_bars
                  WHERE source = ?1 AND open_time_ms < ?2
                 ON CONFLICT (coin, interval) DO UPDATE SET
                     no_history_before_ms =
                         MAX(no_history_before_ms, excluded.no_history_before_ms)",
                params![Source::Local.as_str(), cutoff],
            )?;
            Ok(conn.execute(
                "DELETE FROM candle_bars WHERE source = ?1 AND open_time_ms < ?2",
                params![Source::Local.as_str(), cutoff],
            )?)
        })
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

    /// The resampled case for a source spelt as a string, with the horizon the
    /// module derives for it. Pinned independently by
    /// `the_resampled_chip_carries_the_source_horizon`.
    fn resampled(from: &str) -> Resolution {
        let from = iv(from);
        Resolution::Resampled {
            from,
            source_history: from.venue_history(),
        }
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
        assert_eq!(iv("7m").resolve(), resampled("1m"), "7m is 7 x 1m");
        assert_eq!(
            iv("45m").resolve(),
            resampled("15m"),
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
        assert_eq!(iv("6h").resolve(), resampled("2h"));
        assert_eq!(iv("2d").resolve(), resampled("1d"));
        assert_eq!(iv("4w").resolve(), resampled("1w"));
        assert_eq!(iv("2m").resolve(), resampled("1m"));
    }

    #[test]
    fn labels_say_which_case_it_is() {
        assert_eq!(iv("7m").resolve().label(), "RESAMPLED FROM 1m · 3d");
        assert_eq!(iv("45m").resolve().label(), "RESAMPLED FROM 15m · 52d");
        assert_eq!(iv("90s").resolve().label(), "LOCAL · FORWARD ONLY");
        assert!(!iv("90s").resolve().has_history());
        assert!(iv("7m").resolve().has_history());
    }

    /// `charts.md` §3.2 promises History = Full for the resampled case. It is
    /// not: a `1m`-derived interval reaches about three and a half days, because
    /// `candleSnapshot` serves a rolling ~5,000-bar window (module audit note).
    /// The chip has to carry that or a 3-day chart and a 15-year one say the
    /// same thing.
    #[test]
    fn the_resampled_chip_carries_the_source_horizon() {
        // Below 1d the rolling floor bites; the horizon is the source's, not
        // the requested interval's.
        assert_eq!(
            iv("1m").venue_history(),
            Horizon::Rolling { intervals: 5_000 }
        );
        assert_eq!(
            iv("2h").venue_history(),
            Horizon::Rolling { intervals: 5_000 }
        );
        // 5,000 days is thirteen years, longer than the venue has existed, so
        // 1d and coarser really are full: BTC 1d serves all 2,208 rows.
        assert_eq!(iv("1d").venue_history(), Horizon::Full);
        assert_eq!(iv("1w").venue_history(), Horizon::Full);
        assert_eq!(iv("1M").venue_history(), Horizon::Full);

        // 5,000 x 1m = 3.47 days, truncated to 3d: a history marker never
        // rounds up.
        assert_eq!(iv("7m").resolve().label(), "RESAMPLED FROM 1m · 3d");
        assert_eq!(iv("13m").resolve().label(), "RESAMPLED FROM 1m · 3d");
        // 5,000 x 2h = 416 days.
        assert_eq!(iv("6h").resolve().label(), "RESAMPLED FROM 2h · 416d");
        // Derived from 1d and coarser: no floor to state.
        assert_eq!(iv("2d").resolve().label(), "RESAMPLED FROM 1d");
        assert_eq!(iv("4w").resolve().label(), "RESAMPLED FROM 1w");

        assert_eq!(
            iv("7m").resolve().source_history(),
            Some(Horizon::Rolling { intervals: 5_000 }),
            "the warmup gate reads this, not the chip string"
        );
        assert_eq!(iv("2d").resolve().source_history(), Some(Horizon::Full));
        assert_eq!(iv("1h").resolve().source_history(), None, "native");
        assert_eq!(iv("90s").resolve().source_history(), None, "local");

        assert_eq!(
            Horizon::Rolling { intervals: 5_000 }.span_ms(iv("1m")),
            Some(300_000_000)
        );
        assert_eq!(Horizon::Full.span_ms(iv("1m")), None);
    }

    /// The MCP surface carries this enum, so the extra field has to serialise
    /// deterministically and survive a round trip (`AGENTS.md` 6).
    #[test]
    fn the_resolution_wire_shape_is_stable() {
        let cases = [
            (iv("1h"), r#"{"case":"native"}"#),
            (
                iv("7m"),
                r#"{"case":"resampled","from":"1m","source_history":{"rolling":{"intervals":5000}}}"#,
            ),
            (
                iv("2d"),
                r#"{"case":"resampled","from":"1d","source_history":"full"}"#,
            ),
            (
                iv("90s"),
                r#"{"case":"local","reason":"not_a_multiple_of_any_native"}"#,
            ),
        ];
        for (interval, expected) in cases {
            let resolution = interval.resolve();
            let json = serde_json::to_string(&resolution).expect("serialise");
            assert_eq!(json, expected, "{interval} wire shape");
            assert_eq!(
                serde_json::from_str::<Resolution>(&json).expect("deserialise"),
                resolution
            );
        }
    }

    /// Interval's ordering must be by width. The derived one compares `count`
    /// first, which puts `1M` below `3m`, leaves NATIVE_INTERVALS unsorted under
    /// its own Ord, and would make a `BTreeMap<Interval, _>` serialise in that
    /// order.
    #[test]
    fn interval_ordering_is_by_width() {
        assert!(iv("1M") > iv("30m"));
        assert!(iv("1M") > iv("3m"));
        assert!(iv("1h") > iv("3m"));
        assert!(iv("1d") > iv("12h"));
        assert!(iv("90s") > iv("1m"));
        assert_eq!(iv("60s").cmp(&iv("1m")), Ordering::Equal, "same width");

        assert!(
            NATIVE_INTERVALS.is_sorted(),
            "the menu is documented ascending and resolve() walks it backwards"
        );
        assert_eq!(NATIVE_INTERVALS.iter().max(), Some(&iv("1M")));
        assert_eq!(NATIVE_INTERVALS.iter().min(), Some(&iv("1m")));

        let mut shuffled = NATIVE_INTERVALS;
        shuffled.reverse();
        shuffled.sort_unstable();
        assert_eq!(shuffled, NATIVE_INTERVALS, "sort() must rebuild the menu");
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

    /// Bar opens observed on mainnet BTC, 2026-09-04, driven through the real
    /// conversion path rather than through `bucket_start_ms` alone.
    ///
    /// `assert_eq!(bucket_start_ms(open), open)` is a tautology for any open
    /// that is a multiple of the width, so it proved the seven constants were
    /// round numbers and nothing else. What matters is that a row shaped the way
    /// the venue shapes them survives `bars_from_candles`' three partition
    /// checks — and that a row that is off by one millisecond does not.
    ///
    /// The opens are the observed ones. The OHLCV is synthetic: the audit
    /// recorded timestamps, and inventing prices and calling them live data
    /// would be worse than saying so.
    #[test]
    fn audited_venue_rows_pass_the_partition_checks() {
        for (spelling, open) in [
            ("1M", 1_783_296_000_000_u64),
            ("1M", 1_788_480_000_000),
            ("1w", 1_788_393_600_000),
            ("3d", 1_788_480_000_000),
            ("1d", 1_785_024_000_000),
            ("12h", 1_785_024_000_000),
            ("1m", 1_788_490_020_000),
        ] {
            let interval = iv(spelling);
            let width = u64::try_from(interval.millis()).expect("positive width");
            let row = candle(open, spelling, width);
            let bars = bars_from_candles(std::slice::from_ref(&row), interval)
                .unwrap_or_else(|err| panic!("{spelling} row at {open} refused: {err}"));
            let expected = i64::try_from(open).expect("in range");
            assert_eq!(
                bars,
                vec![Bar {
                    open_time_ms: expected,
                    close_time_ms: expected + interval.millis() - 1,
                    open: d("1"),
                    high: d("3"),
                    low: d("0.5"),
                    close: d("2"),
                    volume: d("7"),
                    trades: 4,
                }]
            );

            // One millisecond off the grid, and one millisecond too wide, are
            // both refused: those checks are what make resampling lossless.
            assert!(matches!(
                bars_from_candles(&[candle(open + 1, spelling, width)], interval),
                Err(BarError::Misaligned { .. })
            ));
            assert!(matches!(
                bars_from_candles(&[candle(open, spelling, width + 1)], interval),
                Err(BarError::WrongWidth { .. })
            ));
        }
    }

    /// Bucketing is integer arithmetic on the epoch and reads no calendar.
    ///
    /// Renamed from `dst_transitions_do_not_move_buckets`, which claimed more
    /// than it tested: it carried no timezone data, linked no calendar library,
    /// and every assertion in it was self-referential — `start - previous ==
    /// 3_600_000` holds for any stub that floors to a multiple of the width.
    /// `charts.md` §6's gate is "identical across a DST change **in the display
    /// timezone**", and the display timezone lives in the renderer, so that gate
    /// belongs to the renderer's snapshot test. (`docs/specs/charts.md` §6 is
    /// owed an edit saying where it lives; this file cannot meet it.)
    ///
    /// What this file can prove is the premise the gate rests on: the instants
    /// below are real DST transitions, and the buckets containing them are
    /// hard-coded integers computed once by hand, not recomputed from the
    /// function under test.
    #[test]
    fn bucketing_is_pure_epoch_arithmetic() {
        // 2026-03-08T07:00:00Z — 01:59:59 EST becomes 03:00 EDT in New York,
        // so that local day is 23 hours long.
        const NY_SPRING_FORWARD: i64 = 1_772_953_200_000;
        // 2026-11-01T06:00:00Z — 01:30 local happens twice; the day is 25 hours.
        const NY_FALL_BACK: i64 = 1_793_512_800_000;
        // 2026-03-29T01:00:00Z — GMT becomes BST in London.
        const LONDON_SPRING_FORWARD: i64 = 1_774_746_000_000;

        let hour = iv("1h");
        let day = iv("1d");

        // Hand-computed: floor(t / 86_400_000) * 86_400_000.
        assert_eq!(day.bucket_start_ms(NY_SPRING_FORWARD), 1_772_928_000_000);
        assert_eq!(day.bucket_start_ms(NY_FALL_BACK), 1_793_491_200_000);
        assert_eq!(
            day.bucket_start_ms(LONDON_SPRING_FORWARD),
            1_774_742_400_000
        );
        assert_eq!(day.bucket_close_ms(NY_FALL_BACK), 1_793_577_599_999);

        // The transition instants are themselves whole hours on the grid.
        assert_eq!(hour.bucket_start_ms(NY_SPRING_FORWARD), 1_772_953_200_000);
        assert_eq!(hour.bucket_start_ms(NY_FALL_BACK), 1_793_512_800_000);
        assert_eq!(
            hour.bucket_start_ms(LONDON_SPRING_FORWARD),
            1_774_746_000_000
        );

        // The doubled local hour, 01:30 EDT and 01:30 EST on 2026-11-01, is two
        // instants half an hour either side of the transition, and they land in
        // two different, consecutive hourly buckets.
        assert_eq!(
            hour.bucket_start_ms(NY_FALL_BACK - 30 * 60_000),
            1_793_509_200_000
        );
        assert_eq!(
            hour.bucket_start_ms(NY_FALL_BACK + 30 * 60_000),
            1_793_512_800_000
        );

        // The span between the two American transitions is 5,711 whole hours:
        // no 23- or 25-hour day anywhere in it, because there are no days in it,
        // only milliseconds.
        assert_eq!((NY_FALL_BACK - NY_SPRING_FORWARD) / 3_600_000, 5_711);
        assert_eq!((NY_FALL_BACK - NY_SPRING_FORWARD) % 3_600_000, 0);
    }

    /// Every timestamp door refuses what it cannot do arithmetic on, and the two
    /// `pub const fn` on the time axis do not panic for any `i64`.
    ///
    /// Before the bounds existed, `bucket_close_ms(i64::MAX)` panicked with
    /// "attempt to add with overflow" and `bucket_start_ms(i64::MIN)` with
    /// "attempt to multiply with overflow" — and a `candleSnapshot` row at
    /// t = 9_223_372_036_854_660_000 passed every check `bars_from_candles`
    /// makes before overflowing inside the resampler.
    #[test]
    fn absurd_timestamps_are_refused_rather_than_overflowing() {
        for interval in NATIVE_INTERVALS {
            let _ = interval.bucket_close_ms(i64::MAX);
            let _ = interval.bucket_start_ms(i64::MIN);
            let _ = interval.bucket_close_ms(i64::MIN);
        }

        // A row that is well-formed by every other rule in the module.
        let mut hostile = candle(0, "1m", 60_000);
        hostile.t = 9_223_372_036_854_660_000;
        hostile.t_close = 9_223_372_036_854_719_999;
        assert_eq!(
            bars_from_candles(&[hostile], iv("1m")),
            Err(BarError::Timestamp(9_223_372_036_854_660_000))
        );
        let mut too_far = candle(0, "1m", 60_000);
        too_far.t = u64::try_from(MAX_TIME_MS).expect("positive") + 60_000;
        assert!(matches!(
            bars_from_candles(&[too_far], iv("1m")),
            Err(BarError::Timestamp(_))
        ));

        // Trade::checked is the other door.
        assert_eq!(
            Trade::checked(i64::MAX, d("1"), d("1")),
            Err(BarError::TradeTime { time_ms: i64::MAX })
        );
        assert_eq!(
            Trade::checked(-1, d("1"), d("1")),
            Err(BarError::TradeTime { time_ms: -1 })
        );
        assert!(Trade::checked(1_788_490_020_000, d("1"), d("1")).is_ok());

        // Trade's fields are public, so push re-checks rather than trusting it.
        let mut agg = LocalAggregator::new(iv("1m"), 0);
        for time_ms in [i64::MAX, i64::MIN, -1, MAX_TIME_MS + 1] {
            assert!(
                agg.push(Trade {
                    time_ms,
                    price: d("1"),
                    size: d("1"),
                })
                .is_empty()
            );
        }
        assert_eq!(agg.out_of_range_trades(), 4);
        assert_eq!(agg.late_trades(), 0);
        assert_eq!(agg.discarded_partial_trades(), 0);
        assert!(agg.close_through(i64::MAX, i64::MAX).is_empty());

        // An aggregator handed a nonsense start clamps rather than wrapping.
        let clamped = LocalAggregator::new(iv("1m"), i64::MIN);
        assert_eq!(clamped.no_history_before_ms(), MIN_TIME_MS);
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

    /// A window that begins part-way through a target bucket has that bucket
    /// **dropped**, not emitted.
    ///
    /// This test previously asserted the opposite, and asserted a defect as
    /// correct behaviour. Feeding only the 00:15 and 00:30 bars of
    /// [00:00, 00:45) produced `open=10 high=15 low=9 volume=3 trades=3` with
    /// the same `open_time_ms` and `close_time_ms` as the true bar
    /// (`open=100 high=100 low=1 volume=12 trades=12` below) — byte-
    /// indistinguishable from a correct one, at the left edge of every chart,
    /// because `candleSnapshot` only ever serves a rolling window.
    #[test]
    fn resample_drops_the_incomplete_leading_bucket() {
        let m = 15 * 60_000_i64;
        // The truth: three 15m bars filling [00:00, 00:45), plus a fourth
        // opening the next bucket.
        let whole = vec![
            bar(0, m, ["100", "100", "1", "50"], "9", 9),
            bar(m, m, ["10", "12", "9", "11"], "1", 1),
            bar(2 * m, m, ["11", "15", "10", "14"], "2", 2),
            bar(3 * m, m, ["14", "14", "13", "13"], "3", 3),
        ];
        let full = resample(&whole, iv("15m"), iv("45m")).expect("45m is 3 x 15m");
        assert_eq!(
            full[0],
            bar(0, 3 * m, ["100", "100", "1", "14"], "12", 12),
            "the true first bucket"
        );

        // What the venue actually serves: the same window starting at 00:15.
        let truncated = whole[1..].to_vec();
        let out = resample(&truncated, iv("15m"), iv("45m")).expect("45m is 3 x 15m");
        assert_eq!(
            out,
            vec![bar(3 * m, 3 * m, ["14", "14", "13", "13"], "3", 3)],
            "the partial leading bucket is dropped, not published as a whole bar"
        );

        // A leading bucket that *is* complete survives, and a leading bucket
        // that starts on the boundary but is still filling is the trailing case.
        assert_eq!(
            resample(&whole[..3], iv("15m"), iv("45m")).expect("resample"),
            vec![bar(0, 3 * m, ["100", "100", "1", "14"], "12", 12)],
            "a complete leading bucket must not be dropped too"
        );
        assert_eq!(
            resample(&whole[..1], iv("15m"), iv("45m")).expect("resample"),
            vec![bar(0, 3 * m, ["100", "100", "1", "50"], "9", 9)],
            "on the boundary and short means forming, not partial"
        );

        // Off the boundary and also the only bucket: nothing can be said.
        assert_eq!(
            resample(&whole[1..2], iv("15m"), iv("45m")).expect("resample"),
            Vec::new()
        );
    }

    /// A hole in the middle of the window is a feed bug, and the module's stated
    /// policy is refuse-not-approximate. The venue emits a bar for every bucket
    /// including empty ones, so a missing bar is not a quiet market.
    #[test]
    fn resample_refuses_an_interior_hole() {
        let m = 15 * 60_000_i64;
        // 00:00, 00:15, [00:30 MISSING], 00:45, 01:00, 01:15.
        let holed: Vec<Bar> = [0, 1, 3, 4, 5]
            .iter()
            .map(|k| bar(k * m, m, ["10", "10", "10", "10"], "1", 1))
            .collect();
        assert_eq!(
            resample(&holed, iv("15m"), iv("45m")),
            Err(ResampleError::IncompleteBucket {
                open_time_ms: 0,
                got: 2,
                want: 3
            })
        );

        // Even when the leading bucket was already dropped, the next hole is
        // still an error rather than the "leading" case a second time.
        let dropped_then_holed: Vec<Bar> = [1, 3, 5, 6, 7, 8]
            .iter()
            .map(|k| bar(k * m, m, ["10", "10", "10", "10"], "1", 1))
            .collect();
        assert_eq!(
            resample(&dropped_then_holed, iv("15m"), iv("45m")),
            Err(ResampleError::IncompleteBucket {
                open_time_ms: 3 * m,
                got: 2,
                want: 3
            })
        );
    }

    /// "Still forming" is the only reason a returned bucket may be short.
    ///
    /// Counting contributions is not enough on its own: a hole in the **last**
    /// target bucket leaves it short too, and the bar it produces is byte-for-
    /// byte the bar an honestly-forming bucket produces. The last bucket of a
    /// `candleSnapshot` window is always the partial one, so that is exactly
    /// where a dropped bar would hide.
    #[test]
    fn resample_refuses_a_hole_in_the_trailing_bucket() {
        let m = 15 * 60_000_i64;
        let rows = |slots: &[i64]| -> Vec<Bar> {
            slots
                .iter()
                .map(|k| bar(k * m, m, ["10", "10", "10", "10"], "1", 1))
                .collect()
        };

        // 00:00,00:15,00:30 | 00:45, [01:00 MISSING], 01:15.
        assert_eq!(
            resample(&rows(&[0, 1, 2, 3, 5]), iv("15m"), iv("45m")),
            Err(ResampleError::IncompleteBucket {
                open_time_ms: 3 * m,
                got: 2,
                want: 3
            })
        );
        // The trailing bucket's own open is the missing one: its `open` would
        // come from a bar 15 minutes into the bucket.
        assert_eq!(
            resample(&rows(&[0, 1, 2, 4]), iv("15m"), iv("45m")),
            Err(ResampleError::IncompleteBucket {
                open_time_ms: 3 * m,
                got: 1,
                want: 3
            })
        );

        // The honest forming bucket — the one the holed result was
        // indistinguishable from — still comes back short and unrefused.
        assert_eq!(
            resample(&rows(&[0, 1, 2, 3, 4]), iv("15m"), iv("45m")).expect("forming is not a hole"),
            vec![
                bar(0, 3 * m, ["10", "10", "10", "10"], "3", 3),
                bar(3 * m, 3 * m, ["10", "10", "10", "10"], "2", 2),
            ]
        );
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

    /// Deterministic pseudo-random source, so a counterexample is reproducible
    /// from the seed alone. No proptest dependency is available to this crate.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    /// Over random aligned inputs: volume and trades are conserved, every
    /// emitted bucket got exactly `target / source` source bars, the buckets
    /// partition the axis with no overlap and no gap, and the high and low
    /// bound every source bar in the bucket.
    #[test]
    fn resample_partitions_every_target_bucket_exactly() {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        for (source, target) in [
            ("1m", "7m"),
            ("1m", "13m"),
            ("15m", "45m"),
            ("1m", "1h"),
            ("2h", "6h"),
            ("1d", "2d"),
        ] {
            let (source, target) = (iv(source), iv(target));
            let per_bucket = target.millis() / source.millis();
            // Start on a target boundary and leave no holes, so nothing is
            // dropped and nothing is refused; then drop the final partial
            // bucket so every survivor must be complete.
            let count = per_bucket * 9;
            let bars: Vec<Bar> = (0..count)
                .map(|k| {
                    let base = 100 + (lcg(&mut seed) % 500) as i64;
                    let high = base + (lcg(&mut seed) % 40) as i64;
                    let low = base - (lcg(&mut seed) % 40) as i64;
                    bar(
                        k * source.millis(),
                        source.millis(),
                        [
                            base.to_string().as_str(),
                            high.to_string().as_str(),
                            low.to_string().as_str(),
                            base.to_string().as_str(),
                        ],
                        "0.125",
                        2,
                    )
                })
                .collect();

            let out = resample(&bars, source, target).expect("aligned, hole-free, coarser");
            assert_eq!(out.len() as i64, 9, "{source} -> {target} bucket count");

            let source_volume: Decimal = bars.iter().map(|b| b.volume).sum();
            let target_volume: Decimal = out.iter().map(|b| b.volume).sum();
            assert_eq!(source_volume, target_volume, "{source} -> {target} volume");
            assert_eq!(
                bars.iter().map(|b| b.trades).sum::<u32>(),
                out.iter().map(|b| b.trades).sum::<u32>(),
                "{source} -> {target} trades"
            );

            for (index, produced) in out.iter().enumerate() {
                let start = index as i64 * target.millis();
                assert_eq!(produced.open_time_ms, start);
                assert_eq!(produced.close_time_ms, start + target.millis() - 1);
                let members: Vec<&Bar> = bars
                    .iter()
                    .filter(|b| target.bucket_start_ms(b.open_time_ms) == start)
                    .collect();
                assert_eq!(members.len() as i64, per_bucket, "bucket at {start}");
                assert_eq!(produced.open, members[0].open);
                assert_eq!(produced.close, members[members.len() - 1].close);
                assert_eq!(
                    produced.high,
                    members.iter().map(|b| b.high).max().expect("non-empty")
                );
                assert_eq!(
                    produced.low,
                    members.iter().map(|b| b.low).min().expect("non-empty")
                );
            }
        }
    }

    /// `parse(x.to_string()) == x` for every canonical interval, and every
    /// spelling of a width canonicalises to the same value. Covers the whole
    /// ladder rather than the nine literals `parses_and_canonicalises` names.
    #[test]
    fn every_canonical_interval_round_trips_through_parse() {
        let mut checked = 0_u32;
        for unit in [
            Unit::Second,
            Unit::Minute,
            Unit::Hour,
            Unit::Day,
            Unit::Week,
            Unit::Month,
        ] {
            let max_count = MAX_INTERVAL_MS / unit.millis();
            // Every count for the coarse units; a stride through the seconds so
            // the test stays under a second of wall clock.
            let stride = if unit == Unit::Second { 997 } else { 1 };
            let mut count = 1;
            while count <= max_count {
                let spelt = format!("{count}{}", unit.suffix());
                let parsed = Interval::parse(&spelt).expect("in range by construction");
                assert_eq!(
                    parsed.millis(),
                    count * unit.millis(),
                    "{spelt} changed width"
                );
                assert_eq!(
                    Interval::parse(&parsed.to_string()),
                    Ok(parsed),
                    "{spelt} did not survive its own Display"
                );
                checked += 1;
                count += stride;
            }
        }
        assert!(checked > 3_000, "only {checked} intervals covered");
        // Anything past the top of the menu is refused rather than wrapped.
        assert!(Interval::parse(&format!("{}s", MAX_INTERVAL_MS / 1_000 + 1)).is_err());
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

        // Nothing trades for two whole buckets; the clock closes them. The
        // feed is healthy, so `now` and `live through` are the same instant.
        let closed = agg.close_through(120_000, 120_000);
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
        let closed = agg.close_through(20_000 * 1_000, 20_000 * 1_000);
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

    /// A websocket outage must not come back as a quiet market.
    ///
    /// Before `feed_interrupted` existed, this sequence returned twenty bars:
    /// the one real bucket plus nineteen flat continuations at the pre-outage
    /// price with `volume = 0` and `trades = 0`, byte-identical to nineteen
    /// genuinely empty buckets. `charts.md` §3.2 forbids exactly that, and
    /// `Resolution::Local` exists to prevent it.
    #[test]
    fn a_feed_interruption_leaves_a_gap_not_a_fabricated_flat_market() {
        let mut agg = LocalAggregator::new(iv("30s"), 0);
        assert!(agg.push(trade(1_000, "50", "2")).is_empty());

        // The pool notices the socket died and says so.
        agg.feed_interrupted();
        assert_eq!(agg.feed_gaps(), 1);
        assert!(
            agg.forming().is_none(),
            "the partial bucket is discarded, not published"
        );

        // Ten minutes later the feed is back and the first print arrives.
        let closed = agg.push(trade(601_000, "51", "1"));
        assert!(
            closed.is_empty(),
            "nothing is invented across the outage, exactly as resume() does \
             across a restart; got {closed:#?}"
        );
        assert_eq!(
            agg.forming().expect("post-reconnect bucket"),
            bar(600_000, 30_000, ["51", "51", "51", "51"], "1", 1)
        );

        // A second interruption with nothing held is not a second gap.
        agg.feed_interrupted();
        agg.feed_interrupted();
        assert_eq!(agg.feed_gaps(), 2);
    }

    /// A reconnect that replays a print from before the outage must not re-arm
    /// the fabrication `feed_interrupted` exists to stop.
    ///
    /// `feed_interrupted` clears the current bucket, and the current bucket was
    /// also the late-trade watermark, so a single replayed print re-opened an
    /// already-emitted bucket without being counted late — and the next live
    /// print then flat-filled the entire outage from it: twenty bars, nineteen
    /// of them invented, at the replayed price rather than the real one.
    #[test]
    fn a_replayed_print_after_an_interruption_cannot_reopen_a_closed_bucket() {
        let mut agg = LocalAggregator::new(iv("30s"), 0);
        agg.push(trade(1_000, "50", "2"));
        assert_eq!(
            agg.push(trade(61_000, "51", "1")).len(),
            2,
            "buckets 0 and 30_000 close"
        );

        agg.feed_interrupted();
        assert_eq!(agg.feed_gaps(), 1);

        // The socket comes back and the venue replays a print from bucket 0,
        // which was emitted two lines ago.
        assert!(agg.push(trade(1_000, "40", "1")).is_empty());
        assert_eq!(
            agg.late_trades(),
            1,
            "a print for an already-emitted bucket is late whether or not the \
             feed was interrupted since"
        );
        assert!(
            agg.forming().is_none(),
            "the replayed print must not re-open the closed bucket"
        );

        // Ten minutes later the feed produces a real print.
        let closed = agg.push(trade(601_000, "52", "1"));
        assert!(
            closed.is_empty(),
            "nothing is invented across the outage; got {closed:#?}"
        );
        assert_eq!(
            agg.forming().expect("post-reconnect bucket"),
            bar(600_000, 30_000, ["52", "52", "52", "52"], "1", 1)
        );
    }

    /// The clock alone is not evidence the feed was alive.
    ///
    /// `close_through` used to fill from `now_ms` with nothing to check it
    /// against, so a stalled socket produced a full run of flat bars on the
    /// caller's timer. The second argument is the caller's assertion of how far
    /// the subscription is known to have been live.
    #[test]
    fn close_through_will_not_fill_past_the_live_feed() {
        let mut agg = LocalAggregator::new(iv("30s"), 0);
        assert!(agg.push(trade(1_000, "50", "2")).is_empty());

        // Ten minutes on the clock; the feed's last message was at t = 1_000.
        let closed = agg.close_through(601_000, 1_000);
        assert_eq!(
            closed,
            vec![bar(0, 30_000, ["50", "50", "50", "50"], "2", 1)],
            "only the bucket that actually received prints"
        );
        assert_eq!(agg.skipped_fills(), 0, "this is not a MAX_FILL_BARS skip");

        // A healthy feed over the same span fills normally: the clamp costs
        // nothing when the caller can vouch for the socket.
        let mut healthy = LocalAggregator::new(iv("30s"), 0);
        assert!(healthy.push(trade(1_000, "50", "2")).is_empty());
        assert_eq!(healthy.close_through(601_000, 601_000).len(), 20);
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

    /// The store must nest inside the ledger's transaction, not fight it.
    ///
    /// `Connection::unchecked_transaction()` issues a bare `BEGIN DEFERRED`,
    /// which failed with "cannot start a transaction within a transaction" the
    /// moment a caller held one open on the same connection — and R4 gives the
    /// whole network one file and one connection, so that caller is the ledger.
    #[test]
    fn writes_nest_inside_an_open_ledger_transaction() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let interval = iv("1m");
        let bars = vec![bar(0, 60_000, ["1", "2", "0.5", "1.5"], "3", 2)];

        conn.execute_batch("BEGIN")
            .expect("the ledger's transaction");
        store
            .put_bars("BTC", interval, Source::Local, &bars)
            .expect("put_bars nests");
        store
            .prune_local(0, DEFAULT_LOCAL_RETENTION_MS)
            .expect("prune_local nests");
        conn.execute_batch("COMMIT").expect("commit");
        assert_eq!(store.bars("BTC", interval, 0, 60_000).expect("read"), bars);

        // And the savepoint really is nested: the enclosing rollback takes the
        // candle rows with it rather than leaving them independently committed.
        conn.execute_batch("BEGIN").expect("second transaction");
        store
            .put_bars(
                "ETH",
                interval,
                Source::Local,
                &[bar(0, 60_000, ["9", "9", "9", "9"], "1", 1)],
            )
            .expect("put_bars nests");
        conn.execute_batch("ROLLBACK").expect("rollback");
        assert_eq!(
            store.bars("ETH", interval, 0, 60_000).expect("read"),
            Vec::new(),
            "a nested write must not survive the outer rollback"
        );
    }

    /// Pruning a series that never had a floor row must create one.
    ///
    /// Otherwise the marker is absent at exactly the moment it is needed: the
    /// UI cannot tell "thirty days were just deleted" from "nothing has been
    /// aggregated yet", which is the failure `prune_local` exists to prevent.
    #[test]
    fn prune_creates_a_history_floor_for_a_series_that_never_had_one() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let interval = iv("90s");
        let day = 24 * 60 * 60 * 1_000_i64;
        let now = 40 * day;
        let cutoff = now - DEFAULT_LOCAL_RETENTION_MS;

        store
            .put_bars(
                "BTC",
                interval,
                Source::Local,
                &[
                    bar(day, 90_000, ["1", "1", "1", "1"], "1", 1),
                    bar(35 * day, 90_000, ["2", "2", "2", "2"], "2", 1),
                ],
            )
            .expect("insert");
        assert_eq!(
            store.no_history_before("BTC", interval).expect("none"),
            None
        );

        assert_eq!(
            store
                .prune_local(now, DEFAULT_LOCAL_RETENTION_MS)
                .expect("prune"),
            1
        );
        assert_eq!(
            store.no_history_before("BTC", interval).expect("floor"),
            Some(cutoff),
            "history was deleted and nothing said so"
        );
    }

    /// A series the prune did not touch keeps its floor.
    ///
    /// The blanket `UPDATE candle_local_history SET no_history_before_ms =
    /// MAX(...)` moved every row in the table, including series with no stored
    /// bars at all, so the marker claimed a deletion that never happened.
    #[test]
    fn prune_leaves_an_untouched_series_floor_alone() {
        let conn = store_conn();
        let store = CandleStore::new(&conn);
        let day = 24 * 60 * 60 * 1_000_i64;

        // DOGE/1s has a floor and no stored bars at all.
        store
            .set_no_history_before("DOGE", iv("1s"), 1_000)
            .expect("floor");
        // BTC/90s has bars, none of them old enough to prune.
        store
            .put_bars(
                "BTC",
                iv("90s"),
                Source::Local,
                &[bar(35 * day, 90_000, ["2", "2", "2", "2"], "2", 1)],
            )
            .expect("insert");
        store
            .set_no_history_before("BTC", iv("90s"), 2_000)
            .expect("floor");

        assert_eq!(
            store
                .prune_local(40 * day, DEFAULT_LOCAL_RETENTION_MS)
                .expect("prune"),
            0
        );
        assert_eq!(
            store.no_history_before("DOGE", iv("1s")).expect("floor"),
            Some(1_000),
            "a series with nothing deleted must not claim a deletion"
        );
        assert_eq!(
            store.no_history_before("BTC", iv("90s")).expect("floor"),
            Some(2_000)
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
        let Resolution::Resampled { from, .. } = iv("7m").resolve() else {
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
