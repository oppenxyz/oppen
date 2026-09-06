//! Condition alerts (`docs/spec.md` item 22).
//!
//! "Agents do not experience time; wakeups replace polling." An agent that
//! wants to act when BTC crosses 70,000 has two options without this: hold a
//! session open and poll `get_state` in a loop, or miss the cross. Both are
//! bad, and the first is the one that costs the venue's rate budget all day.
//!
//! So the agent says the condition once and stops. oppen watches the feed it
//! is already running, and when the condition holds it writes a chained
//! [`crate::ledger::EventKind::Alert`] row carrying the agent id — which
//! `docs/decisions.md` C6 delivers to that agent's `get_events` and to nobody
//! else's. The agent reads it on its next turn. The OS notification item 22
//! also names is the operator's half and lands with the console (P5).
//!
//! **Not the ledger's job to store the arming.** An alert is mutable state —
//! it is armed, then it is fired — and the ledger is append-only by
//! construction (`AGENTS.md` invariant 7). So the arming lives in its own
//! per-network file next to the journal's, and only the *firing* is chained.
//! Per-network for the reason everything is (`docs/decisions.md` R4): a level
//! that means something on testnet prices does not on mainnet.
//!
//! **One-shot.** A crossing that fired once does not fire again on the next
//! tick that is still above the level, which is what "wakeup" means and what
//! a repeating alert would drown. Re-arming is the agent's to do, and it costs
//! one call.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;

use oppen_hl::types::Fill;

/// How many alerts one agent may keep armed.
///
/// Bounded for the journal's reason: an agent that arms on a loop otherwise
/// fills the disk, and each armed alert is also a row the evaluator reads on
/// every tick. Fired alerts do not count — they are history, and a firing
/// gives the capacity back, so an agent working through conditions one at a
/// time never meets this.
pub const MAX_ARMED_PER_AGENT: usize = 100;

/// Basis points per unit, for the funding threshold's stated units.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

#[derive(Debug, thiserror::Error)]
pub enum AlertError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the condition could not be stored: {0}")]
    Encode(#[from] serde_json::Error),
    #[error(
        "this agent already keeps {MAX_ARMED_PER_AGENT} armed alerts; cancel one \
         or wait for one to fire"
    )]
    Full,
    #[error("the symbol is empty")]
    EmptySymbol,
    #[error("a price to cross must be greater than zero, got {0}")]
    NonPositivePrice(Decimal),
}

/// Which side of a level the alert is watching for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Fires when the observed value is at or above the level.
    Above,
    /// Fires when the observed value is at or below the level.
    Below,
}

impl Direction {
    /// Whether `observed` has reached `level` on this side.
    ///
    /// Inclusive at the level itself: an agent that says "at or above 70,000"
    /// and watches the mark print exactly 70,000 has had its condition met,
    /// and the alternative is an alert that silently needs one more tick.
    fn reached(self, observed: Decimal, level: Decimal) -> bool {
        match self {
            Direction::Above => observed >= level,
            Direction::Below => observed <= level,
        }
    }
}

/// What an agent asked to be woken for.
///
/// Three of the five item 22 names. Liquidation distance needs the account's
/// positions, which the feed pump does not hold, and feature thresholds need
/// `get_features`, which is not built — both are named on the tool rather than
/// half-answered, the way `docs/decisions.md` C8 handles fees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    /// The venue's mark for `symbol` reached `px`.
    PriceCross {
        symbol: String,
        direction: Direction,
        px: Decimal,
    },
    /// A fill printed on this account — for `symbol`, or any symbol when it is
    /// `None`.
    ///
    /// The cheapest useful alert and the one an agent wants most: it placed a
    /// resting order and wants its turn back when the order works, not on a
    /// timer.
    Fill { symbol: Option<String> },
    /// The hour-to-date funding rate for `symbol` reached
    /// `hour_to_date_bps`.
    ///
    /// The units are in the name because the mistake is otherwise free to
    /// make: [`oppen_hl::types::HourToDateRate1h`] is a rate per hour and this
    /// is that rate in basis points, never an annualised APR and never a
    /// premium.
    FundingRate {
        symbol: String,
        direction: Direction,
        hour_to_date_bps: Decimal,
    },
}

impl Condition {
    /// The symbol this condition needs a market feed for, if any.
    ///
    /// A `Fill` needs none: the account channels are subscribed for every
    /// account oppen runs, so a fill alert is answered by a feed that is
    /// already flowing.
    pub fn watched_symbol(&self) -> Option<&str> {
        match self {
            Condition::PriceCross { symbol, .. } | Condition::FundingRate { symbol, .. } => {
                Some(symbol)
            }
            Condition::Fill { .. } => None,
        }
    }

    /// Refuse a condition that could never hold, at the moment it is armed.
    ///
    /// An alert that cannot fire is worse than a refused one: the agent stops
    /// watching and waits for a wakeup nothing will send.
    fn validate(&self) -> Result<(), AlertError> {
        if let Some(symbol) = self.watched_symbol()
            && symbol.is_empty()
        {
            return Err(AlertError::EmptySymbol);
        }
        if let Condition::PriceCross { px, .. } = self
            && *px <= Decimal::ZERO
        {
            return Err(AlertError::NonPositivePrice(*px));
        }
        Ok(())
    }
}

/// What the venue's latest context says about one symbol.
///
/// The two readings a market condition can be answered from, taken off the
/// same `activeAssetCtx` frame so they describe one instant.
#[derive(Debug, Clone, Copy)]
pub struct MarketTick<'a> {
    pub symbol: &'a str,
    /// The venue's mark. `None` when the venue has stopped quoting the asset —
    /// see [`oppen_hl::types::ReferencePrices`] for why a published `markPx`
    /// is not on its own evidence of a live market.
    pub mark_px: Option<Decimal>,
    /// Hour-to-date funding, in basis points.
    pub funding_hour_to_date_bps: Decimal,
}

/// One armed or fired alert.
///
/// Field order is the wire order (`AGENTS.md` invariant 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alert {
    pub alert_id: i64,
    pub condition: Condition,
    pub armed_at_ms: i64,
    /// When it fired. `None` while it is still watching.
    pub fired_at_ms: Option<i64>,
}

/// The per-network alert file.
#[derive(Debug)]
pub struct AlertStore {
    conn: Mutex<Connection>,
    /// Raised when the armed set changes.
    ///
    /// A price alert needs its symbol subscribed before it can fire, and the
    /// pump is the only thing that can subscribe it. Waiting for the pump's
    /// next event would mean an alert on a symbol nothing is watching waits
    /// for a feed that nothing is watching either — so arming says so
    /// immediately.
    armed_changed: tokio::sync::Notify,
}

impl AlertStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AlertError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// The same schema and the same SQL without a file. Test-only; the app
    /// opens the per-network database.
    #[cfg(test)]
    fn in_memory() -> Result<Self, AlertError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, AlertError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS alerts (
                 alert_id     INTEGER PRIMARY KEY,
                 agent        TEXT NOT NULL,
                 condition    TEXT NOT NULL,
                 armed_at_ms  INTEGER NOT NULL,
                 fired_at_ms  INTEGER
             );
             CREATE INDEX IF NOT EXISTS alerts_armed ON alerts (fired_at_ms, agent);",
        )?;
        Ok(AlertStore {
            conn: Mutex::new(conn),
            armed_changed: tokio::sync::Notify::new(),
        })
    }

    /// Arm one alert for `agent`.
    pub fn arm(
        &self,
        agent: &str,
        condition: &Condition,
        now_ms: i64,
    ) -> Result<Alert, AlertError> {
        condition.validate()?;
        let encoded = serde_json::to_string(condition)?;
        let guard = self.lock();
        let armed: usize = guard.query_row(
            "SELECT COUNT(*) FROM alerts WHERE agent = ?1 AND fired_at_ms IS NULL",
            params![agent],
            |row| row.get(0),
        )?;
        if armed >= MAX_ARMED_PER_AGENT {
            return Err(AlertError::Full);
        }
        guard.execute(
            "INSERT INTO alerts (agent, condition, armed_at_ms) VALUES (?1, ?2, ?3)",
            params![agent, encoded, now_ms],
        )?;
        let alert_id = guard.last_insert_rowid();
        drop(guard);
        self.armed_changed.notify_one();
        Ok(Alert {
            alert_id,
            condition: condition.clone(),
            armed_at_ms: now_ms,
            fired_at_ms: None,
        })
    }

    /// Everything this agent has armed or fired, newest first.
    pub fn for_agent(&self, agent: &str) -> Result<Vec<Alert>, AlertError> {
        let guard = self.lock();
        let mut statement = guard.prepare(
            "SELECT alert_id, condition, armed_at_ms, fired_at_ms FROM alerts \
             WHERE agent = ?1 ORDER BY alert_id DESC",
        )?;
        let rows = statement.query_map(params![agent], row_to_alert)?;
        collect(rows)
    }

    /// Every symbol an armed alert needs a market feed for, in a stable order.
    ///
    /// The feed pump subscribes exactly this set: an alert whose symbol nobody
    /// subscribed is an alert that silently never fires, which is the failure
    /// this whole module exists to avoid.
    pub fn watched_symbols(&self) -> Result<Vec<String>, AlertError> {
        let mut symbols: Vec<String> = self
            .armed()?
            .into_iter()
            .filter_map(|(_, _, condition)| {
                condition.watched_symbol().map(|symbol| symbol.to_owned())
            })
            .collect();
        symbols.sort();
        symbols.dedup();
        Ok(symbols)
    }

    /// `(alert_id, agent, condition)` for everything still watching.
    fn armed(&self) -> Result<Vec<(i64, String, Condition)>, AlertError> {
        let guard = self.lock();
        let mut statement = guard.prepare(
            "SELECT alert_id, agent, condition FROM alerts \
             WHERE fired_at_ms IS NULL ORDER BY alert_id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (alert_id, agent, encoded) = row?;
            // A row that will not decode is skipped rather than failing the
            // sweep: one unreadable alert must not stop every other agent's
            // from being evaluated, and it can only get there by editing the
            // file by hand.
            match serde_json::from_str(&encoded) {
                Ok(condition) => out.push((alert_id, agent, condition)),
                Err(error) => {
                    tracing::error!(%error, alert_id, "skipping an unreadable alert");
                }
            }
        }
        Ok(out)
    }

    /// Mark one alert fired. Returns false if something already did.
    ///
    /// The `fired_at_ms IS NULL` in the statement is what makes one-shot true
    /// under concurrency: two ticks racing on the same crossing produce one
    /// update and one `false`, so exactly one of them writes the event.
    fn mark_fired(&self, alert_id: i64, now_ms: i64) -> Result<bool, AlertError> {
        let changed = self.lock().execute(
            "UPDATE alerts SET fired_at_ms = ?2 WHERE alert_id = ?1 AND fired_at_ms IS NULL",
            params![alert_id, now_ms],
        )?;
        Ok(changed == 1)
    }

    /// Wait until the armed set changes.
    ///
    /// Cancel-safe, so the pump can select on it alongside the event stream.
    pub async fn armed_changed(&self) {
        self.armed_changed.notified().await;
    }

    /// A poisoned lock means a previous caller panicked. The store holds no
    /// half-applied state — every write is one statement — so the guard is
    /// taken back rather than turning an unrelated panic into an alert system
    /// that never fires again.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn row_to_alert(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, i64, Option<i64>)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn collect<'a, I>(rows: I) -> Result<Vec<Alert>, AlertError>
where
    I: Iterator<Item = rusqlite::Result<(i64, String, i64, Option<i64>)>> + 'a,
{
    let mut out = Vec::new();
    for row in rows {
        let (alert_id, encoded, armed_at_ms, fired_at_ms) = row?;
        out.push(Alert {
            alert_id,
            condition: serde_json::from_str(&encoded)?,
            armed_at_ms,
            fired_at_ms,
        });
    }
    Ok(out)
}

/// What a condition matched on, for the chained event's payload.
///
/// The observed value travels with the firing so the agent can see what it was
/// woken for without re-reading a feed that has since moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "matched", rename_all = "snake_case")]
enum Matched {
    PriceCross { symbol: String, mark_px: Decimal },
    Fill { symbol: String, tid: u64 },
    FundingRate { symbol: String, bps: Decimal },
}

/// Evaluate every armed alert against one market tick, firing what matched.
///
/// Returns the alerts that fired. Writing the ledger row is the caller's, so
/// this stays a function over the store and the reading — the same split
/// [`crate::feed::FeedSession`] makes between deciding and doing.
pub fn on_market_tick(
    store: &AlertStore,
    tick: &MarketTick<'_>,
    now_ms: i64,
) -> Result<Vec<Fired>, AlertError> {
    let mut fired = Vec::new();
    for (alert_id, agent, condition) in store.armed()? {
        let matched = match &condition {
            Condition::PriceCross {
                symbol,
                direction,
                px,
            } if symbol == tick.symbol => {
                // No mark means the venue is not quoting this asset. A
                // crossing cannot be evaluated against a price that is not
                // being published, and firing on the last one oppen saw would
                // wake an agent for a market that has gone away.
                match tick.mark_px {
                    Some(mark) if direction.reached(mark, *px) => Some(Matched::PriceCross {
                        symbol: symbol.clone(),
                        mark_px: mark,
                    }),
                    _ => None,
                }
            }
            Condition::FundingRate {
                symbol,
                direction,
                hour_to_date_bps,
            } if symbol == tick.symbol => direction
                .reached(tick.funding_hour_to_date_bps, *hour_to_date_bps)
                .then(|| Matched::FundingRate {
                    symbol: symbol.clone(),
                    bps: tick.funding_hour_to_date_bps,
                }),
            _ => None,
        };
        if let Some(matched) = matched
            && store.mark_fired(alert_id, now_ms)?
        {
            fired.push(Fired {
                alert_id,
                agent,
                condition,
                matched,
            });
        }
    }
    Ok(fired)
}

/// Evaluate every armed fill alert against the fills one event carried.
pub fn on_fills(store: &AlertStore, fills: &[Fill], now_ms: i64) -> Result<Vec<Fired>, AlertError> {
    let mut fired = Vec::new();
    for (alert_id, agent, condition) in store.armed()? {
        let Condition::Fill { symbol } = &condition else {
            continue;
        };
        // The first fill this alert wanted. One wakeup per alert, however many
        // fills arrived in the frame.
        let hit = fills
            .iter()
            .find(|fill| symbol.as_deref().is_none_or(|want| want == fill.coin));
        if let Some(fill) = hit
            && store.mark_fired(alert_id, now_ms)?
        {
            fired.push(Fired {
                alert_id,
                agent,
                condition: condition.clone(),
                matched: Matched::Fill {
                    symbol: fill.coin.clone(),
                    tid: fill.tid,
                },
            });
        }
    }
    Ok(fired)
}

/// One alert that just fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub alert_id: i64,
    pub agent: String,
    pub condition: Condition,
    matched: Matched,
}

impl Fired {
    /// The chained event's payload: what was asked for, and what was seen.
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "alert_id": self.alert_id,
            "condition": self.condition,
            "observed": self.matched,
        })
    }
}

/// Read the hour-to-date funding rate as the basis points a condition states.
pub fn funding_bps(rate: oppen_hl::types::HourToDateRate1h) -> Decimal {
    rate.hour_to_date_1h() * BPS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const NOW: i64 = 1_756_000_000_000;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    fn store() -> AlertStore {
        AlertStore::in_memory().expect("store")
    }

    fn cross(symbol: &str, direction: Direction, px: &str) -> Condition {
        Condition::PriceCross {
            symbol: symbol.into(),
            direction,
            px: d(px),
        }
    }

    fn tick<'a>(symbol: &'a str, mark: Option<&str>) -> MarketTick<'a> {
        MarketTick {
            symbol,
            mark_px: mark.map(d),
            funding_hour_to_date_bps: Decimal::ZERO,
        }
    }

    fn fill(coin: &str, tid: u64) -> Fill {
        Fill {
            coin: coin.into(),
            px: d("64000"),
            sz: d("0.01"),
            side: oppen_hl::types::Side::B,
            time: NOW as u64,
            start_position: Decimal::ZERO,
            dir: "Open Long".into(),
            closed_pnl: Decimal::ZERO,
            hash: "0x00".into(),
            oid: 1,
            crossed: true,
            fee: d("0.02"),
            fee_token: "USDC".into(),
            builder_fee: None,
            tid,
            cloid: None,
        }
    }

    #[test]
    fn a_mark_reaching_the_level_fires_once_and_not_again() {
        let store = store();
        store
            .arm("agent-a", &cross("BTC", Direction::Above, "70000"), NOW)
            .expect("arm");

        assert!(
            on_market_tick(&store, &tick("BTC", Some("69999.9")), NOW)
                .expect("tick")
                .is_empty(),
            "below the level is not a crossing"
        );

        let fired = on_market_tick(&store, &tick("BTC", Some("70000")), NOW + 1).expect("tick");
        assert_eq!(fired.len(), 1, "at the level counts as reaching it");
        assert_eq!(fired[0].agent, "agent-a");

        assert!(
            on_market_tick(&store, &tick("BTC", Some("70500")), NOW + 2)
                .expect("tick")
                .is_empty(),
            "one-shot: still above is not a second wakeup"
        );
    }

    #[test]
    fn a_below_alert_watches_the_other_side() {
        let store = store();
        store
            .arm("agent-a", &cross("ETH", Direction::Below, "3000"), NOW)
            .expect("arm");
        assert!(
            on_market_tick(&store, &tick("ETH", Some("3001")), NOW)
                .expect("tick")
                .is_empty()
        );
        assert_eq!(
            on_market_tick(&store, &tick("ETH", Some("2999")), NOW + 1)
                .expect("tick")
                .len(),
            1
        );
    }

    /// An alert on one symbol is not woken by another's tick — the sweep reads
    /// every armed row, so the symbol test is load-bearing.
    #[test]
    fn another_symbols_tick_does_not_fire_it() {
        let store = store();
        store
            .arm("agent-a", &cross("BTC", Direction::Above, "1"), NOW)
            .expect("arm");
        assert!(
            on_market_tick(&store, &tick("ETH", Some("9999999")), NOW)
                .expect("tick")
                .is_empty()
        );
    }

    /// The venue publishes a `markPx` for an asset it has stopped quoting —
    /// a frozen last print. Waking an agent for a market that has gone away is
    /// the same fault `ReferencePrices` refuses on the order path.
    #[test]
    fn an_unquoted_symbol_does_not_fire_on_its_frozen_mark() {
        let store = store();
        store
            .arm("agent-a", &cross("FRIEND", Direction::Above, "1"), NOW)
            .expect("arm");
        assert!(
            on_market_tick(&store, &tick("FRIEND", None), NOW)
                .expect("tick")
                .is_empty(),
            "no live mark, no crossing"
        );
    }

    #[test]
    fn a_fill_alert_can_name_a_symbol_or_take_any() {
        let store = store();
        store
            .arm(
                "agent-a",
                &Condition::Fill {
                    symbol: Some("BTC".into()),
                },
                NOW,
            )
            .expect("arm");
        store
            .arm("agent-b", &Condition::Fill { symbol: None }, NOW)
            .expect("arm");

        let fired = on_fills(&store, &[fill("ETH", 1)], NOW).expect("fills");
        assert_eq!(fired.len(), 1, "only the any-symbol alert");
        assert_eq!(fired[0].agent, "agent-b");

        let fired = on_fills(&store, &[fill("BTC", 2)], NOW + 1).expect("fills");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].agent, "agent-a");
    }

    /// Several fills in one frame are one wakeup, not one per fill.
    #[test]
    fn many_fills_in_a_frame_wake_an_alert_once() {
        let store = store();
        store
            .arm("agent-a", &Condition::Fill { symbol: None }, NOW)
            .expect("arm");
        let fired = on_fills(
            &store,
            &[fill("BTC", 1), fill("BTC", 2), fill("ETH", 3)],
            NOW,
        )
        .expect("fills");
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn funding_fires_on_the_hour_to_date_rate_in_bps() {
        let store = store();
        store
            .arm(
                "agent-a",
                &Condition::FundingRate {
                    symbol: "BTC".into(),
                    direction: Direction::Above,
                    hour_to_date_bps: d("1.5"),
                },
                NOW,
            )
            .expect("arm");
        let mut reading = tick("BTC", Some("64000"));
        reading.funding_hour_to_date_bps = d("1.4");
        assert!(
            on_market_tick(&store, &reading, NOW)
                .expect("tick")
                .is_empty()
        );
        reading.funding_hour_to_date_bps = d("1.6");
        assert_eq!(
            on_market_tick(&store, &reading, NOW + 1)
                .expect("tick")
                .len(),
            1
        );
    }

    /// The venue's rate is a fraction per hour; the condition states bps.
    #[test]
    fn the_funding_conversion_is_bps_not_a_fraction() {
        let rate = oppen_hl::types::HourToDateRate1h::from_hour_to_date_1h(d("0.000125"));
        assert_eq!(funding_bps(rate), d("1.250"));
    }

    /// The pump subscribes exactly this set. A fill alert needs no market
    /// feed; the account channels already carry it.
    #[test]
    fn watched_symbols_are_the_market_feeds_the_pump_must_subscribe() {
        let store = store();
        store
            .arm("agent-a", &cross("BTC", Direction::Above, "70000"), NOW)
            .expect("arm");
        store
            .arm("agent-a", &cross("BTC", Direction::Below, "60000"), NOW)
            .expect("arm");
        store
            .arm("agent-b", &cross("ETH", Direction::Above, "4000"), NOW)
            .expect("arm");
        store
            .arm("agent-b", &Condition::Fill { symbol: None }, NOW)
            .expect("arm");

        assert_eq!(store.watched_symbols().expect("symbols"), ["BTC", "ETH"]);
    }

    /// A fired alert stops being watched, so the pump can drop its feed.
    #[test]
    fn a_fired_alert_leaves_the_watched_set() {
        let store = store();
        store
            .arm("agent-a", &cross("BTC", Direction::Above, "1"), NOW)
            .expect("arm");
        assert_eq!(store.watched_symbols().expect("symbols"), ["BTC"]);
        on_market_tick(&store, &tick("BTC", Some("2")), NOW).expect("tick");
        assert!(store.watched_symbols().expect("symbols").is_empty());
    }

    /// An alert that cannot fire is worse than a refused one: the agent stops
    /// watching and waits for a wakeup nothing will send.
    #[test]
    fn a_condition_that_could_never_hold_is_refused_when_it_is_armed() {
        let store = store();
        assert!(matches!(
            store.arm("agent-a", &cross("BTC", Direction::Above, "0"), NOW),
            Err(AlertError::NonPositivePrice(_))
        ));
        assert!(matches!(
            store.arm("agent-a", &cross("", Direction::Above, "1"), NOW),
            Err(AlertError::EmptySymbol)
        ));
    }

    /// The ceiling counts what is watching, not what has ever been armed, so
    /// an agent working through conditions one at a time never meets it.
    #[test]
    fn the_cap_counts_armed_alerts_and_a_firing_gives_the_room_back() {
        let store = store();
        for i in 0..MAX_ARMED_PER_AGENT {
            store
                .arm(
                    "agent-a",
                    &cross("BTC", Direction::Above, &format!("{}", i + 1)),
                    NOW,
                )
                .expect("arm");
        }
        assert!(matches!(
            store.arm("agent-a", &cross("BTC", Direction::Above, "1"), NOW),
            Err(AlertError::Full)
        ));
        // Another agent is unaffected: the cap is per agent.
        store
            .arm("agent-b", &cross("BTC", Direction::Above, "1"), NOW)
            .expect("agent-b has its own room");

        on_market_tick(&store, &tick("BTC", Some("1")), NOW).expect("tick");
        store
            .arm("agent-a", &cross("BTC", Direction::Below, "1"), NOW)
            .expect("a firing gave the room back");
    }

    #[test]
    fn an_agent_reads_its_own_alerts_and_their_state() {
        let store = store();
        store
            .arm("agent-a", &cross("BTC", Direction::Above, "1"), NOW)
            .expect("arm");
        store
            .arm("agent-b", &cross("ETH", Direction::Above, "1"), NOW)
            .expect("arm");
        on_market_tick(&store, &tick("BTC", Some("2")), NOW + 5).expect("tick");

        let mine = store.for_agent("agent-a").expect("mine");
        assert_eq!(mine.len(), 1, "another agent's alerts are not mine");
        assert_eq!(mine[0].fired_at_ms, Some(NOW + 5));
    }

    /// The payload carries what was asked for and what was seen, so an agent
    /// woken later does not have to re-read a feed that has moved.
    #[test]
    fn the_event_payload_names_the_observed_value() {
        let store = store();
        store
            .arm("agent-a", &cross("BTC", Direction::Above, "70000"), NOW)
            .expect("arm");
        let fired = on_market_tick(&store, &tick("BTC", Some("70123.5")), NOW).expect("tick");
        let payload = fired[0].payload();
        assert_eq!(payload["observed"]["matched"], "price_cross");
        assert_eq!(payload["observed"]["mark_px"], "70123.5");
        assert_eq!(payload["condition"]["type"], "price_cross");
    }
}
