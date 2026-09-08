//! Bounded display observations, independent of execution/feed authority.
use std::{collections::BTreeMap, sync::Arc};

use oppen_hl::{types::Candle, ws::WsTrade};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::candles::{Bar, Interval, Trade, bars_from_candles};

const HISTORY_BARS: i64 = 600;
const MAX_IDENTITIES: usize = 65_536;
// Beyond this bounded clock disagreement, refuse recoverably without evicting history.
const MAX_CLOCK_SKEW_MS: u64 = 5_000;
const CLOCK_SKEW: &str = "venue clock is ahead of host receipt time; trade marker is unverified";

#[derive(Debug, thiserror::Error)]
pub enum ChartError {
    #[error("invalid chart observation: {0}")]
    Invalid(String),
    #[error("history request belongs to a different chart or is superseded")]
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeStatus {
    Observing,
    Interrupted,
    CapacityExceeded,
    InvalidObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BarSource {
    Venue,
    ObservedTrades,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedBar {
    pub time_ms: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    pub source: BarSource,
    pub partial: bool,
    pub open_close_ambiguous: bool,
    pub received_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TradeMarker {
    pub time_ms: u64,
    pub price: String,
    pub price_ambiguous: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChartProjection {
    pub symbol: String,
    pub interval: String,
    pub interval_ms: i64,
    pub revision: String,
    pub price_decimals: Option<u32>,
    pub closed: Vec<ObservedBar>,
    pub forming: Option<ObservedBar>,
    pub latest_trade: Option<TradeMarker>,
    pub history_error: Option<String>,
    pub observation_error: Option<String>,
    pub last_observation_received_at_ms: Option<u64>,
    pub tape_status: TapeStatus,
}

pub struct HistorySnapshot {
    pub candles: Vec<Candle>,
    pub price_decimals: u32,
}

/// Single-use local capability; never reconstructed from IPC fields.
pub struct HistoryTicket {
    owner: Arc<()>,
    request: Arc<()>,
}

struct VenueBar {
    bar: Bar,
    received: u64,
    ws: bool,
}
struct TapeBar {
    bar: Bar,
    first: (u64, u64),
    last: (u64, u64),
    first_ambiguous: bool,
    last_ambiguous: bool,
    received: u64,
}

pub struct LiveChart {
    symbol: String,
    interval: Interval,
    precision: Option<u32>,
    owner: Arc<()>,
    request: Arc<()>,
    revision: u128,
    classification_ms: Option<u64>,
    clock_rollback: bool,
    floor: i64,
    venue: BTreeMap<i64, VenueBar>,
    tape: BTreeMap<i64, TapeBar>,
    identities: BTreeMap<(u64, u64), WsTrade>,
    marker: Option<((u64, u64), TradeMarker)>,
    status: TapeStatus,
    history_error: Option<String>,
    observation_error: Option<String>,
    transport_error: Option<String>,
    received: Option<u64>,
}

impl LiveChart {
    pub fn new(
        symbol: String,
        interval: Interval,
        price_decimals: Option<u32>,
    ) -> Result<Self, ChartError> {
        if symbol.is_empty() || symbol.len() > 256 {
            return Err(ChartError::Invalid("empty or oversized symbol".into()));
        }
        Ok(Self {
            symbol,
            interval,
            precision: price_decimals,
            owner: Arc::new(()),
            request: Arc::new(()),
            revision: 0,
            classification_ms: None,
            clock_rollback: false,
            floor: 0,
            venue: BTreeMap::new(),
            tape: BTreeMap::new(),
            identities: BTreeMap::new(),
            marker: None,
            status: TapeStatus::Observing,
            history_error: None,
            observation_error: None,
            transport_error: None,
            received: None,
        })
    }

    fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    fn frozen(&self) -> bool {
        self.transport_error.is_some()
            || matches!(
                self.status,
                TapeStatus::CapacityExceeded | TapeStatus::InvalidObservation
            )
    }
    fn freeze(&mut self, status: TapeStatus, detail: &str) {
        self.status = status;
        self.observation_error = Some(detail.into());
        if let Some((_, marker)) = &mut self.marker {
            marker.price_ambiguous = true;
        }
        self.changed();
    }
    fn received(&mut self, at: u64) {
        self.received = Some(self.received.unwrap_or(0).max(at));
    }
    fn advance_floor(&mut self, bucket: i64) {
        let next = self
            .floor
            .max(bucket.saturating_sub((HISTORY_BARS - 1) * self.interval.millis()));
        if next == self.floor {
            return;
        }
        self.floor = next;
        self.venue.retain(|time, _| *time >= self.floor);
        self.tape.retain(|time, _| *time >= self.floor);
        let floor = self.floor as u64;
        self.identities.retain(|(time, _), _| *time >= floor);
    }

    pub fn observe_trades(&mut self, trades: &[WsTrade], received_at_ms: u64) {
        if self.frozen() {
            return;
        }
        if trades.len() > MAX_IDENTITIES {
            self.freeze(
                TapeStatus::CapacityExceeded,
                "trade batch exceeds identity capacity",
            );
            return;
        }
        // Sort borrowed rows: batch order must not change which bucket gets a print.
        let mut rows: Vec<_> = trades.iter().collect();
        rows.sort_by_key(|trade| (trade.time, trade.tid));
        for trade in rows {
            let valid_time = i64::try_from(trade.time)
                .ok()
                .and_then(|time| Trade::checked(time, trade.px, trade.sz).ok());
            let Some(checked) = valid_time else {
                self.freeze(
                    TapeStatus::InvalidObservation,
                    "trade timestamp outside supported range",
                );
                return;
            };
            if trade.coin != self.symbol
                || trade.px <= Decimal::ZERO
                || trade.sz <= Decimal::ZERO
                || trade.hash.len() > 128
                || trade.users.len() > 2
            {
                self.freeze(
                    TapeStatus::InvalidObservation,
                    "invalid trade symbol, price or size",
                );
                return;
            }
            if trade.time > received_at_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
                self.observation_error =
                    Some("trade exceeds supported forward clock skew; observation ignored".into());
                self.changed();
                continue;
            }
            let clock_uncertain = trade.time > received_at_ms;
            let width = self.interval.millis();
            let bucket = checked.time_ms / width * width;
            if bucket < self.floor {
                continue;
            }
            let key = (trade.time, trade.tid);
            if let Some(prior) = self.identities.get(&key) {
                if prior != trade {
                    self.freeze(TapeStatus::InvalidObservation, "conflicting trade identity");
                    return;
                }
                self.received(received_at_ms);
                self.changed();
                continue;
            }
            self.advance_floor(bucket);
            if self.identities.len() >= MAX_IDENTITIES {
                self.freeze(
                    TapeStatus::CapacityExceeded,
                    "trade identity capacity exceeded",
                );
                return;
            }
            let old_volume = self
                .tape
                .get(&bucket)
                .map_or(Decimal::ZERO, |bar| bar.bar.volume);
            let Some(volume) = old_volume.checked_add(trade.sz) else {
                self.freeze(TapeStatus::InvalidObservation, "trade volume overflow");
                return;
            };
            let entry = self.tape.entry(bucket).or_insert_with(|| TapeBar {
                bar: Bar {
                    open_time_ms: bucket,
                    close_time_ms: bucket + width - 1,
                    open: trade.px,
                    high: trade.px,
                    low: trade.px,
                    close: trade.px,
                    volume: Decimal::ZERO,
                    trades: 0,
                },
                first: key,
                last: key,
                first_ambiguous: false,
                last_ambiguous: false,
                received: received_at_ms,
            });
            if key.0 < entry.first.0 {
                entry.first_ambiguous = false;
            }
            if key.0 == entry.first.0 && trade.px != entry.bar.open {
                entry.first_ambiguous = true;
            }
            if key < entry.first {
                entry.first = key;
                entry.bar.open = trade.px;
            }
            if key.0 > entry.last.0 {
                entry.last_ambiguous = false;
            }
            if key.0 == entry.last.0 && trade.px != entry.bar.close {
                entry.last_ambiguous = true;
            }
            if key > entry.last {
                entry.last = key;
                entry.bar.close = trade.px;
            }
            entry.bar.high = entry.bar.high.max(trade.px);
            entry.bar.low = entry.bar.low.min(trade.px);
            entry.bar.volume = volume;
            entry.bar.trades += 1;
            entry.received = entry.received.max(received_at_ms);
            match &mut self.marker {
                Some((last, marker)) if key.0 == last.0 => {
                    marker.price_ambiguous |= marker.price != trade.px.normalize().to_string();
                    if key > *last {
                        *last = key;
                        marker.price = trade.px.normalize().to_string();
                    }
                }
                Some((last, _)) if key < *last => {}
                _ => {
                    self.marker = Some((
                        key,
                        TradeMarker {
                            time_ms: trade.time,
                            price: trade.px.normalize().to_string(),
                            price_ambiguous: false,
                        },
                    ))
                }
            }
            if clock_uncertain {
                if let Some((_, marker)) = &mut self.marker {
                    marker.price_ambiguous = true;
                }
                self.observation_error = Some(CLOCK_SKEW.into());
            } else if self.observation_error.as_deref() == Some(CLOCK_SKEW) {
                self.observation_error = None;
            }
            self.identities.insert(key, trade.clone());
            self.status = TapeStatus::Observing;
            self.received(received_at_ms);
            self.changed();
        }
    }

    fn validate_candles(&self, candles: &[Candle]) -> Result<Vec<Bar>, ChartError> {
        // The REST query spans 600 widths and may include both endpoints.
        if candles.len() > HISTORY_BARS as usize + 1 {
            return Err(ChartError::Invalid("history exceeds chart window".into()));
        }
        let interval = self.interval.to_string();
        if candles.iter().any(|c| {
            c.s != self.symbol
                || c.i != interval
                || c.o <= Decimal::ZERO
                || c.c <= Decimal::ZERO
                || c.l <= Decimal::ZERO
                || c.h < c.l
                || c.h < c.o
                || c.h < c.c
                || c.l > c.o
                || c.l > c.c
                || c.v < Decimal::ZERO
        }) {
            return Err(ChartError::Invalid("invalid candle symbol or OHLCV".into()));
        }
        let bars = bars_from_candles(candles, self.interval)
            .map_err(|e| ChartError::Invalid(e.to_string()))?;
        if bars
            .windows(2)
            .any(|pair| pair[0].open_time_ms >= pair[1].open_time_ms)
        {
            return Err(ChartError::Invalid(
                "history buckets are not strictly ascending".into(),
            ));
        }
        Ok(bars)
    }

    pub fn observe_candle(&mut self, candle: &Candle, received_at_ms: u64) {
        if candle.t > received_at_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            self.observation_error =
                Some("candle bucket exceeds supported forward clock skew".into());
            self.changed();
            return;
        }
        match self.validate_candles(std::slice::from_ref(candle)) {
            Ok(bars) => {
                for bar in bars {
                    if bar.open_time_ms < self.floor {
                        continue;
                    }
                    self.advance_floor(bar.open_time_ms);
                    self.venue.insert(
                        bar.open_time_ms,
                        VenueBar {
                            bar,
                            received: received_at_ms,
                            ws: true,
                        },
                    );
                    self.received(received_at_ms);
                    self.changed();
                }
            }
            Err(error) => {
                self.observation_error = Some(error.to_string());
                self.changed();
            }
        }
    }

    pub fn interrupt(&mut self, _received_at_ms: u64) {
        if !self.frozen() {
            self.status = TapeStatus::Interrupted;
            self.changed();
        }
    }

    /// Record the owner's first terminal transport/application failure. Repeated
    /// projection polling cannot clear it, change freshness, or churn revisions.
    pub fn observation_failed(&mut self, detail: &str) {
        if self.transport_error.is_some() {
            return;
        }
        if !self.frozen() {
            self.status = TapeStatus::Interrupted;
        }
        self.transport_error = Some(detail.chars().take(512).collect());
        if let Some((_, marker)) = &mut self.marker {
            marker.price_ambiguous = true;
        }
        self.changed();
    }

    pub fn begin_history(&mut self) -> HistoryTicket {
        self.request = Arc::new(());
        HistoryTicket {
            owner: self.owner.clone(),
            request: self.request.clone(),
        }
    }

    pub fn finish_history(
        &mut self,
        ticket: HistoryTicket,
        outcome: Result<HistorySnapshot, String>,
        received_at_ms: u64,
    ) -> Result<(), ChartError> {
        if !Arc::ptr_eq(&ticket.owner, &self.owner) || !Arc::ptr_eq(&ticket.request, &self.request)
        {
            return Err(ChartError::Superseded);
        }
        let snapshot = match outcome {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.history_error = Some(error.chars().take(512).collect());
                self.changed();
                return Ok(());
            }
        };
        let bars = match self.validate_candles(&snapshot.candles) {
            Ok(bars) => bars,
            Err(error) => {
                self.history_error = Some(error.to_string());
                self.changed();
                return Err(error);
            }
        };
        if bars.last().is_some_and(|bar| {
            bar.open_time_ms as u64 > received_at_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        }) {
            let error = ChartError::Invalid("history bucket starts after receipt time".into());
            self.history_error = Some(error.to_string());
            self.changed();
            return Err(error);
        }
        if let Some(last) = bars.last() {
            self.advance_floor(last.open_time_ms);
        }
        for bar in bars {
            if bar.open_time_ms < self.floor
                || self
                    .venue
                    .get(&bar.open_time_ms)
                    .is_some_and(|prior| prior.ws)
            {
                continue;
            }
            self.venue.insert(
                bar.open_time_ms,
                VenueBar {
                    bar,
                    received: received_at_ms,
                    ws: false,
                },
            );
        }
        self.precision = Some(snapshot.price_decimals);
        self.history_error = None;
        if !snapshot.candles.is_empty() {
            self.received(received_at_ms);
        }
        self.changed();
        Ok(())
    }

    pub fn projection(&mut self, now_ms: u64) -> ChartProjection {
        // Elapsed buckets cannot become future/forming again after a host clock
        // correction. Keep the prior classification until wall time catches up.
        let classification_ms = self.classification_ms.unwrap_or(now_ms).max(now_ms);
        let clock_rollback = now_ms < classification_ms;
        let width = self.interval.millis() as u64;
        if self.classification_ms.map(|time| time / width) != Some(classification_ms / width)
            || self.clock_rollback != clock_rollback
        {
            self.changed();
        }
        self.classification_ms = Some(classification_ms);
        self.clock_rollback = clock_rollback;
        let mut bars = BTreeMap::new();
        for (time, tape) in &self.tape {
            bars.insert(
                *time,
                project_bar(
                    &tape.bar,
                    BarSource::ObservedTrades,
                    true,
                    tape.first_ambiguous || tape.last_ambiguous,
                    tape.received,
                ),
            );
        }
        for (time, venue) in &self.venue {
            bars.insert(
                *time,
                project_bar(&venue.bar, BarSource::Venue, false, false, venue.received),
            );
        }
        let mut closed = Vec::new();
        let mut forming = None;
        for (time, bar) in bars {
            if (time as u64).saturating_add(width) <= classification_ms {
                closed.push(bar);
            } else if time as u64 <= classification_ms {
                forming = Some(bar);
            }
        }
        let mut observation_error = self.observation_error.clone();
        if let Some(transport_error) = &self.transport_error {
            let detail = format!("chart transport failed: {transport_error}");
            observation_error = Some(match observation_error {
                Some(error) => format!("{error}; {detail}"),
                None => detail,
            });
        }
        let observation_error = if clock_rollback {
            let detail = "host clock moved backward; retaining prior bar classification until clock recovery";
            Some(match &observation_error {
                Some(error) => format!("{error}; {detail}"),
                None => detail.to_owned(),
            })
        } else {
            observation_error
        };
        ChartProjection {
            symbol: self.symbol.clone(),
            interval: self.interval.to_string(),
            interval_ms: self.interval.millis(),
            revision: self.revision.to_string(),
            price_decimals: self.precision,
            closed,
            forming,
            latest_trade: self.marker.as_ref().map(|(_, marker)| marker.clone()),
            history_error: self.history_error.clone(),
            observation_error,
            last_observation_received_at_ms: self.received,
            tape_status: self.status,
        }
    }
}

fn project_bar(
    bar: &Bar,
    source: BarSource,
    partial: bool,
    ambiguous: bool,
    received_at_ms: u64,
) -> ObservedBar {
    ObservedBar {
        time_ms: bar.open_time_ms,
        open: bar.open.to_string(),
        high: bar.high.to_string(),
        low: bar.low.to_string(),
        close: bar.close.to_string(),
        volume: bar.volume.to_string(),
        source,
        partial,
        open_close_ambiguous: ambiguous,
        received_at_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::Side;

    fn d(value: i64) -> Decimal {
        Decimal::from(value)
    }
    fn chart() -> LiveChart {
        LiveChart::new("TEST".into(), Interval::parse("1m").unwrap(), None).unwrap()
    }
    fn trade(time: u64, tid: u64, price: i64) -> WsTrade {
        WsTrade {
            coin: "TEST".into(),
            side: Side::B,
            px: d(price),
            sz: d(1),
            time,
            tid,
            hash: format!("{tid}"),
            users: vec![],
        }
    }
    fn candle(time: u64, price: i64, volume: i64) -> Candle {
        Candle {
            t: time,
            t_close: time + 59_999,
            s: "TEST".into(),
            i: "1m".into(),
            o: d(price),
            h: d(price),
            l: d(price),
            c: d(price),
            v: d(volume),
            n: 1,
        }
    }
    fn history(candles: Vec<Candle>) -> Result<HistorySnapshot, String> {
        Ok(HistorySnapshot {
            candles,
            price_decimals: 2,
        })
    }

    #[test]
    fn cross_bucket_shuffled_duplicates_and_late_endpoints() {
        let mut c = chart();
        let rows = [trade(60_002, 3, 30), trade(59_999, 2, 20), trade(10, 1, 10)];
        c.observe_trades(&rows, 60_003);
        c.observe_trades(&rows, 60_004);
        c.observe_trades(&[trade(5, 4, 5), trade(60_001, 5, 25)], 60_005);
        let p = c.projection(60_010);
        assert_eq!(p.closed.len(), 1);
        let old = &p.closed[0];
        assert_eq!((&*old.open, &*old.close, &*old.volume), ("5", "20", "3"));
        assert!(old.partial);
        let forming = p.forming.unwrap();
        assert_eq!(
            (&*forming.open, &*forming.close, &*forming.volume),
            ("25", "30", "2")
        );
        assert_eq!(p.latest_trade.unwrap().price, "30");
    }

    #[test]
    fn equal_time_is_deterministic_and_marker_ambiguity_survives_replays() {
        let rows = [trade(10, 2, 20), trade(10, 1, 10), trade(10, 3, 30)];
        let mut a = chart();
        let mut b = chart();
        a.observe_trades(&rows, 11);
        for row in rows.iter().rev() {
            b.observe_trades(std::slice::from_ref(row), 11);
        }
        let pa = a.projection(12);
        let pb = b.projection(12);
        assert_eq!(pa.forming, pb.forming);
        assert_eq!(pa.latest_trade, pb.latest_trade);
        assert!(pa.forming.unwrap().open_close_ambiguous);
        a.observe_trades(&[rows[2].clone()], 13);
        assert!(a.projection(14).latest_trade.unwrap().price_ambiguous);
    }

    #[test]
    fn venue_volume_never_blends_and_late_candle_never_rolls_back() {
        let mut c = chart();
        c.observe_candle(&candle(0, 100, 50), 5);
        c.observe_trades(&[trade(10, 1, 101), trade(60_001, 2, 102)], 60_002);
        c.observe_candle(&candle(0, 99, 49), 60_003);
        let p = c.projection(60_004);
        assert_eq!(p.closed[0].volume, "49");
        assert_eq!(p.closed[0].source, BarSource::Venue);
        assert_eq!(p.forming.unwrap().time_ms, 60_000);
        assert_eq!(p.latest_trade.unwrap().price, "102");
    }

    #[test]
    fn history_fences_foreign_superseded_ws_and_failure_bootstrap() {
        let mut c = chart();
        let old = c.begin_history();
        let current = c.begin_history();
        assert!(matches!(
            c.finish_history(old, Err("old".into()), 1),
            Err(ChartError::Superseded)
        ));
        c.observe_trades(&[trade(1, 1, 10)], 2);
        c.finish_history(current, Err("offline".into()), 3).unwrap();
        assert!(c.projection(4).forming.is_some());
        let ticket = c.begin_history();
        c.observe_candle(&candle(0, 12, 20), 5);
        c.finish_history(ticket, history(vec![candle(0, 9, 5)]), 6)
            .unwrap();
        let p = c.projection(7);
        assert_eq!(p.forming.unwrap().volume, "20");
        assert!(p.history_error.is_none());
        let foreign = chart().begin_history();
        assert!(matches!(
            c.finish_history(foreign, history(vec![]), 8),
            Err(ChartError::Superseded)
        ));
    }

    #[test]
    fn all_sources_obey_permanent_floor_and_no_empty_bars() {
        let mut c = chart();
        c.observe_trades(&[trade(1, 1, 10), trade(60_000 * 700, 2, 20)], 60_000 * 700);
        assert!(c.identities.len() <= 1);
        c.observe_trades(&[trade(1, 1, 10)], 60_000 * 700 + 1);
        c.observe_candle(&candle(0, 10, 10), 60_000 * 700 + 2);
        let ticket = c.begin_history();
        c.finish_history(ticket, history(vec![candle(0, 10, 10)]), 60_000 * 700 + 3)
            .unwrap();
        let p = c.projection(60_000 * 700 + 4);
        assert!(p.closed.is_empty());
        assert_eq!(p.forming.unwrap().volume, "1");
        assert!(c.venue.is_empty());
    }

    #[test]
    fn capacity_freezes_marker_and_tape_but_not_venue() {
        let mut c = chart();
        let rows: Vec<_> = (0..MAX_IDENTITIES as u64)
            .map(|id| trade(1, id, 10))
            .collect();
        c.observe_trades(&rows, 2);
        c.observe_trades(&[trade(2, MAX_IDENTITIES as u64, 20)], 3);
        assert_eq!(c.status, TapeStatus::CapacityExceeded);
        c.interrupt(4);
        c.observe_trades(&[trade(3, 999_999, 30)], 5);
        c.observe_candle(&candle(0, 15, 25), 6);
        let p = c.projection(7);
        let marker = p.latest_trade.unwrap();
        assert_eq!(marker.price, "10");
        assert!(marker.price_ambiguous);
        assert_eq!(p.forming.unwrap().volume, "25");
        assert_eq!(p.tape_status, TapeStatus::CapacityExceeded);
        assert_eq!(c.identities.len(), MAX_IDENTITIES);
    }

    #[test]
    fn conflict_and_checked_overflow_are_terminal_for_tape() {
        for overflow in [false, true] {
            let mut c = chart();
            let mut first = trade(1, 1, 10);
            if overflow {
                first.sz = Decimal::MAX;
            }
            c.observe_trades(&[first], 2);
            c.observe_trades(
                &[trade(
                    if overflow { 2 } else { 1 },
                    if overflow { 2 } else { 1 },
                    11,
                )],
                3,
            );
            assert_eq!(c.status, TapeStatus::InvalidObservation);
            c.observe_trades(&[trade(10, 10, 20)], 11);
            let p = c.projection(12);
            assert_eq!(p.latest_trade.as_ref().unwrap().price, "10");
            assert!(p.latest_trade.unwrap().price_ambiguous);
        }
    }

    #[test]
    fn malformed_candles_and_trades_never_panic_or_replace_good_observations() {
        let mut c = chart();
        c.observe_candle(&candle(0, 10, 2), 1);
        for bad in [
            Candle {
                s: "OTHER".into(),
                ..candle(0, 20, 3)
            },
            Candle {
                h: d(1),
                ..candle(0, 20, 3)
            },
            Candle {
                v: d(-1),
                ..candle(0, 20, 3)
            },
            Candle {
                t_close: 1,
                ..candle(0, 20, 3)
            },
        ] {
            c.observe_candle(&bad, 2);
            assert_eq!(c.projection(3).forming.unwrap().volume, "2");
        }
        let ticket = c.begin_history();
        assert!(
            c.finish_history(
                ticket,
                history(vec![candle(60_000, 10, 1), candle(0, 10, 1)]),
                4
            )
            .is_err()
        );
        c.observe_trades(&[trade(u64::MAX, 1, 10)], 5);
        assert_eq!(c.status, TapeStatus::InvalidObservation);
    }

    #[test]
    fn clock_rollover_is_monotonic_and_rollback_is_explicit_until_recovery() {
        let mut c = chart();
        c.observe_candle(&candle(60_000, 10, 2), 60_001);
        let early = c.projection(10);
        assert!(early.closed.is_empty() && early.forming.is_none());
        let current = c.projection(60_010);
        assert!(current.forming.is_some());
        let forming_rollback = c.projection(10);
        assert_eq!(forming_rollback.forming, current.forming);
        assert!(forming_rollback.observation_error.is_some());
        let later = c.projection(120_000);
        assert_ne!(current.revision, later.revision);
        assert_eq!(later.closed.len(), 1);
        assert!(later.observation_error.is_none());
        let rollback = c.projection(10);
        assert_eq!(rollback.closed, later.closed);
        assert!(rollback.forming.is_none());
        assert_ne!(rollback.revision, later.revision);
        assert!(rollback.observation_error.is_some());
        let still_behind = c.projection(119_999);
        assert_eq!(still_behind.closed, later.closed);
        assert!(still_behind.observation_error.is_some());
        assert_eq!(still_behind.revision, rollback.revision);
        let recovered = c.projection(120_000);
        assert_eq!(recovered.closed, later.closed);
        assert!(recovered.observation_error.is_none());
        assert_ne!(recovered.revision, rollback.revision);
        assert_eq!(recovered.last_observation_received_at_ms, Some(60_001));
    }

    #[test]
    fn rollback_warning_preserves_latched_observation_error() {
        let mut c = chart();
        c.observe_trades(&[trade(1, 1, 10), trade(1, 1, 11)], 2);
        let before = c.projection(100);
        let rollback = c.projection(99);
        assert_eq!(rollback.tape_status, TapeStatus::InvalidObservation);
        assert!(
            rollback
                .observation_error
                .as_ref()
                .unwrap()
                .contains(before.observation_error.as_ref().unwrap())
        );
        assert_ne!(before.revision, rollback.revision);
        let recovered = c.projection(100);
        assert_eq!(recovered.observation_error, before.observation_error);
        assert_eq!(recovered.latest_trade, before.latest_trade);
    }

    #[test]
    fn terminal_transport_failure_is_bounded_sticky_and_does_not_refresh_observations() {
        for conflict in [false, true] {
            let mut c = chart();
            c.observe_trades(&[trade(1, 1, 10)], 2);
            if conflict {
                c.observe_trades(&[trade(1, 1, 11)], 3);
            }
            let before = c.projection(100);
            c.observation_failed(&"x".repeat(1_024));
            let failed = c.projection(99);
            assert_ne!(failed.revision, before.revision);
            assert_eq!(
                failed.last_observation_received_at_ms,
                before.last_observation_received_at_ms
            );
            assert_eq!(c.transport_error.as_ref().unwrap().len(), 512);
            assert_eq!(
                failed.tape_status,
                if conflict {
                    TapeStatus::InvalidObservation
                } else {
                    TapeStatus::Interrupted
                }
            );
            assert!(failed.latest_trade.as_ref().unwrap().price_ambiguous);
            assert!(
                failed
                    .observation_error
                    .as_ref()
                    .unwrap()
                    .contains("host clock moved backward")
            );
            if let Some(prior) = &before.observation_error {
                assert!(failed.observation_error.as_ref().unwrap().contains(prior));
            }
            c.observation_failed("different later failure");
            c.observe_trades(&[trade(4, 2, 20)], 5);
            let repeated = c.projection(99);
            assert_eq!(repeated.revision, failed.revision);
            assert_eq!(repeated.latest_trade, failed.latest_trade);
            assert_eq!(
                repeated.last_observation_received_at_ms,
                before.last_observation_received_at_ms
            );
            let ticket = c.begin_history();
            c.finish_history(ticket, history(vec![candle(0, 12, 20)]), 101)
                .unwrap();
            c.observe_candle(&candle(0, 13, 21), 102);
            let recovered_clock = c.projection(103);
            assert!(
                recovered_clock
                    .observation_error
                    .unwrap()
                    .contains("chart transport failed")
            );
            assert_eq!(recovered_clock.tape_status, failed.tape_status);
            assert_eq!(recovered_clock.latest_trade, failed.latest_trade);
        }
    }

    #[test]
    fn interruption_does_not_invent_volume_or_clear_freeze() {
        let mut c = chart();
        c.observe_trades(&[trade(1, 1, 10)], 2);
        c.interrupt(3);
        let p = c.projection(600_000);
        assert_eq!(p.closed.len(), 1);
        assert!(p.closed[0].partial);
        assert_eq!(p.last_observation_received_at_ms, Some(2));
        assert_eq!(p.tape_status, TapeStatus::Interrupted);
        c.observe_trades(&[trade(600_001, 2, 11)], 600_002);
        assert_eq!(c.projection(600_003).closed.len(), 1);
    }

    #[test]
    fn future_observations_do_not_advance_floor_or_destroy_history() {
        let mut c = chart();
        c.observe_candle(&candle(0, 10, 1), 1);
        c.observe_candle(&candle(60_000 * 700, 20, 1), 2);
        let ticket = c.begin_history();
        assert!(
            c.finish_history(ticket, history(vec![candle(60_000 * 700, 20, 1)]), 3)
                .is_err()
        );
        c.observe_trades(&[trade(60_000 * 700, 1, 10)], 4);
        assert_eq!(c.floor, 0);
        assert_eq!(c.projection(5).forming.unwrap().close, "10");
        assert_eq!(c.status, TapeStatus::Observing);
        c.observe_trades(&[trade(5, 2, 11)], 6);
        assert_eq!(c.projection(7).latest_trade.unwrap().price, "11");
    }

    #[test]
    fn small_clock_skew_is_partial_and_recovers_without_reset() {
        let mut c = chart();
        c.observe_trades(&[trade(60_001, 1, 10)], 59_999);
        let early = c.projection(59_999);
        assert!(early.forming.is_none());
        assert!(early.latest_trade.unwrap().price_ambiguous);
        assert_eq!(early.tape_status, TapeStatus::Observing);
        assert_eq!(early.last_observation_received_at_ms, Some(59_999));
        assert!(c.projection(60_002).forming.is_some());
        c.observe_trades(&[trade(60_003, 2, 11)], 60_004);
        let recovered = c.projection(60_005);
        assert!(!recovered.latest_trade.unwrap().price_ambiguous);
        assert!(recovered.observation_error.is_none());
    }

    #[test]
    fn equal_numeric_prices_with_different_scales_are_not_ambiguous() {
        let mut c = chart();
        let mut scaled = trade(1, 1, 10);
        scaled.px = "10.00".parse().unwrap();
        c.observe_trades(&[scaled, trade(1, 2, 10)], 2);
        let p = c.projection(3);
        assert!(!p.latest_trade.unwrap().price_ambiguous);
        assert!(!p.forming.unwrap().open_close_ambiguous);
    }
}
