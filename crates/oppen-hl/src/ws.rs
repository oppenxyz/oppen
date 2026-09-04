//! WebSocket pool: oppen's market and user data plane (`docs/spec.md` items 9
//! and 34).
//!
//! # Why a pool and not a socket
//!
//! Spec item 9 asks for "a WS pool with reconnect and reconcile" so that one
//! socket failing degrades the terminal rather than blinding it. That is not
//! theoretical on Hyperliquid: a subscription naming a coin the venue does not
//! know **closes the entire connection** with no close frame (measured
//! 2026-09-03 against `wss://api.hyperliquid.xyz/ws`: subscribing `bbo` on
//! `NOTACOIN` produced a bare TCP close, code 1006, taking every other
//! subscription on that socket with it). Independent connection lifecycles turn
//! that from a blackout into the loss of one shard, and
//! [`Disconnected::unacked`] names the likely poison.
//!
//! # Channel choices are measured, not guessed
//!
//! `docs/specs/fair-value.md` §14 is a live API audit and it wins over anything
//! earlier. The findings that shape this module, each re-measured while writing
//! it (45 s, mainnet, BTC):
//!
//! | Channel | Median gap | Note |
//! |---|---|---|
//! | `bbo` | 111 ms | `{px, sz, n}` per side, venue-timestamped. The micro source. |
//! | `activeAssetCtx` | 1009 ms | Complete ctx, nested `{coin, ctx:{…}}`, **no timestamp**. |
//! | `candle` | 821 ms | Bar boundaries only, not a sample instant. |
//! | `trades` | 648 ms | Venue-timestamped, event-driven. |
//! | `l2Book` | 5451 ms | Fails §5.2's own 2 s book threshold on every sample. |
//!
//! So: `bbo` is the microprice source and `l2Book` is depth-on-demand
//! (fair-value.md §14.4 correction 4). A subscribed `l2Book` feed reads *stale*
//! through [`WsPool::health`] by construction — that is the honest reading of a
//! 5.4 s feed against a 2 s threshold, not a bug in the tracker.
//!
//! `activeAssetCtx` and `fastAssetCtxs` carry no venue timestamp while
//! `l2Book`, `trades` and `bbo` all do (§14.1). Locally-stamped samples cannot
//! be aligned against venue-stamped ones, so the distinction is carried in the
//! event type: see [`EventTime`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::types::{AssetCtx, Candle, Fill, L2Book, Level, OrderStatusEntry, Side};
use crate::{Address, Error, Network};

/// The venue's own keepalive. The server drops a connection that has been idle
/// for roughly 60 s (`docs/spec.md` item 9), and it answers this frame with
/// `{"channel":"pong"}` (verified live).
const PING_FRAME: &str = r#"{"method":"ping"}"#;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed pool failures. `AGENTS.md` invariant 8 forbids bare strings on a
/// rejection path, and the console renders these directly in the feed-health
/// panel (`docs/spec.md` item 34).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    /// Every connection is at its subscription cap and the pool may not open
    /// another. Hyperliquid documents 1000 subscriptions and 100 connections
    /// per IP, so this is a real ceiling, not a self-imposed one.
    #[error(
        "subscription capacity exhausted: {used} of {cap} slots across {connections} connections"
    )]
    CapacityExhausted {
        used: usize,
        cap: usize,
        connections: usize,
    },
    /// Unsubscribe for something the registry never placed. The venue answers
    /// a redundant unsubscribe with `{"channel":"error","data":"Already
    /// unsubscribed: …"}` (measured), so catching it locally saves a round
    /// trip and an error event.
    #[error("not subscribed: {0}")]
    NotSubscribed(String),
    /// The pool was shut down; connections are closing and no new
    /// subscriptions are accepted.
    #[error("websocket pool is shut down")]
    Shutdown,
    /// A frame arrived on a known channel but did not deserialize. Surfaced
    /// rather than swallowed, because a silent drop on a data plane is
    /// indistinguishable from a quiet market.
    #[error("malformed `{channel}` payload: {detail}")]
    Parse { channel: String, detail: String },
    /// `orderUpdates` payloads carry no `user` field, so a connection can only
    /// attribute them by owning exactly one such subscription. See
    /// [`Subscription::exclusive_per_connection`].
    #[error("orderUpdates arrived on a connection that owns no orderUpdates subscription")]
    UnattributableOrderUpdates,
}

impl From<PoolError> for Error {
    fn from(e: PoolError) -> Self {
        Error::Ws(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// A Hyperliquid websocket subscription.
///
/// The set is exactly what `docs/spec.md` item 11 and `docs/specs/fair-value.md`
/// §14.4 correction 5 require: `bbo` for micro, `activeAssetCtx` for carry and
/// mark, `trades` for component (2)'s last-trade leg, `candle` for charts,
/// `l2Book` for on-demand depth, and the two user channels the ledger
/// reconciles against. The user channels need only an address — no key, no
/// signature.
///
/// Serializes to the venue's `subscription` object verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Subscription {
    /// Complete per-asset context at a ~1 s cadence. Carries `markPx`,
    /// `oraclePx`, `premium`, `funding`, `openInterest`, `midPx` and
    /// `impactPxs`, nested as `{coin, ctx:{…}}` — unlike the flat REST array
    /// elements (fair-value.md §14.4 correction 13).
    #[serde(rename = "activeAssetCtx")]
    ActiveAssetCtx { coin: String },
    /// Best bid and offer with size and order count per side, ~0.11 s.
    /// The microprice source (fair-value.md §14.4 correction 4).
    #[serde(rename = "bbo")]
    Bbo { coin: String },
    /// Public tape. Supplies the last-trade leg of `c2` in the mark
    /// containment monitor (fair-value.md §14.2).
    #[serde(rename = "trades")]
    Trades { coin: String },
    /// Live bar for one interval. Feeds the chart (`docs/spec.md` item 31).
    #[serde(rename = "candle")]
    Candle { coin: String, interval: String },
    /// Full depth ladder. Pushed at a 5.4 s median, so it is depth-on-demand
    /// for `preflight` book walks (`docs/spec.md` item 20), never the micro
    /// source.
    #[serde(rename = "l2Book")]
    L2Book { coin: String },
    /// Fills for one address. The ledger's live source; the reconnect gap is
    /// backfilled with `userFillsByTime` (`docs/spec.md` item 9).
    #[serde(rename = "userFills")]
    UserFills { user: Address },
    /// Order state transitions for one address. Reconciled after a gap with
    /// `frontendOpenOrders` + `orderStatus` by cloid.
    #[serde(rename = "orderUpdates")]
    OrderUpdates { user: Address },
}

impl Subscription {
    /// Stable identity for the registry and for feed-health reporting.
    ///
    /// A string key rather than the value itself keeps every collection in this
    /// module a [`BTreeMap`], which the deterministic-ordering rule requires of
    /// anything that reaches a serialized surface (`AGENTS.md` invariant 6).
    pub fn key(&self) -> String {
        match self {
            Subscription::ActiveAssetCtx { coin } => format!("activeAssetCtx:{coin}"),
            Subscription::Bbo { coin } => format!("bbo:{coin}"),
            Subscription::Trades { coin } => format!("trades:{coin}"),
            Subscription::Candle { coin, interval } => format!("candle:{coin}:{interval}"),
            Subscription::L2Book { coin } => format!("l2Book:{coin}"),
            Subscription::UserFills { user } => format!("userFills:{user}"),
            Subscription::OrderUpdates { user } => format!("orderUpdates:{user}"),
        }
    }

    /// Whether a connection may hold at most one of these.
    ///
    /// `orderUpdates` frames are a bare array with **no `user` field**, so two
    /// addresses multiplexed onto one socket produce updates that cannot be
    /// attributed to a sub-account — and D1 gives every agent its own
    /// sub-account, so that is the normal case, not an edge one. Pinning one
    /// per connection makes the owning address recoverable from the socket that
    /// delivered the frame. `userFills` needs no such rule: its payload does
    /// carry `user` (verified live).
    pub fn exclusive_per_connection(&self) -> bool {
        matches!(self, Subscription::OrderUpdates { .. })
    }

    /// How long this feed may go silent before the console dims it and the
    /// guardrails fail closed (`docs/spec.md` item 34).
    ///
    /// Thresholds are `docs/specs/fair-value.md` §5.2's: book 2 s, mark 5 s,
    /// oracle 10 s. `activeAssetCtx` carries both a mark leg and an oracle leg,
    /// so the tighter of the two binds it.
    ///
    /// `None` means silence is not a fault. Trades, candles, fills and order
    /// updates are event-driven: a quiet tape is information, not a broken
    /// socket, and a staleness alarm on it would be a false positive every time
    /// the market is calm. Their age is still reported so a caller can judge.
    pub fn staleness_threshold(&self, thresholds: &StalenessThresholds) -> Option<Duration> {
        match self {
            Subscription::Bbo { .. } | Subscription::L2Book { .. } => Some(thresholds.book),
            Subscription::ActiveAssetCtx { .. } => Some(thresholds.mark.min(thresholds.oracle)),
            Subscription::Trades { .. }
            | Subscription::Candle { .. }
            | Subscription::UserFills { .. }
            | Subscription::OrderUpdates { .. } => None,
        }
    }
}

impl PartialOrd for Subscription {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Ordered by [`Subscription::key`] so any listing of subscriptions is stable
/// across runs (`AGENTS.md` invariant 6).
impl Ord for Subscription {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

impl fmt::Display for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// Per-component silence budgets from `docs/specs/fair-value.md` §5.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StalenessThresholds {
    /// Book components (`bbo`, `l2Book`).
    pub book: Duration,
    /// Mark leg of `activeAssetCtx`.
    pub mark: Duration,
    /// Oracle leg of `activeAssetCtx`.
    pub oracle: Duration,
}

impl Default for StalenessThresholds {
    fn default() -> Self {
        StalenessThresholds {
            book: Duration::from_secs(2),
            mark: Duration::from_secs(5),
            oracle: Duration::from_secs(10),
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Identifies one socket in the pool. Present on every control event so the
/// console can show which shard degraded (`docs/spec.md` item 34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ConnectionId(pub usize);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ws{}", self.0)
    }
}

/// Whose clock stamped an event.
///
/// `activeAssetCtx` carries no timestamp while `bbo`, `trades` and `l2Book` all
/// do (`docs/specs/fair-value.md` §14.1). A 250 ms misalignment alone injects
/// 0.3–0.9 bp at p99, so the fair value sampler must never treat a local
/// arrival stamp as a venue instant. Making the distinction a type rather than
/// a convention is the only way that rule survives contact with a `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTime {
    /// The venue stamped it. Alignable against other venue-stamped feeds.
    Venue(u64),
    /// oppen stamped it on arrival. Carries this machine's clock plus an
    /// unobservable network delay; not alignable.
    LocalArrival(u64),
    /// No meaningful sample instant (control events; candle bar boundaries).
    None,
}

/// One public trade off the `trades` channel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WsTrade {
    pub coin: String,
    pub side: Side,
    pub px: Decimal,
    pub sz: Decimal,
    /// Venue timestamp, ms.
    pub time: u64,
    pub hash: String,
    pub tid: u64,
    /// `[maker, taker]` when the venue sends it. Defaulted rather than
    /// required: fair-value.md §14.4 correction 2 is the standing lesson that
    /// one over-strict field kills the whole payload, not one row.
    #[serde(default)]
    pub users: Vec<Address>,
}

/// The gap a reconnect left in the record.
///
/// `docs/spec.md` item 9: "HL has no server-side cursor — the ledger is the
/// app's own and must never silently drop a fill across a laptop sleep." This
/// window is exactly what the ledger replays through `userFillsByTime` and
/// `candleSnapshot`.
///
/// `start_ms` is the last message seen on that connection before the drop, not
/// the moment the socket errored. If the socket was alive at time *t*, every
/// fill up to *t* was delivered; anything after it may not have been. That
/// makes the connection's last-message time the tightest provably-safe start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapWindow {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl GapWindow {
    /// Width of the window. Saturating: a backwards clock yields zero rather
    /// than a panic.
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// A connection lost its socket. Emitted once per outage, not once per retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnected {
    pub connection: ConnectionId,
    /// When the pool noticed, ms.
    pub at_ms: u64,
    /// Last message on this connection, ms. `None` means it never connected.
    pub last_message_ms: Option<u64>,
    /// Everything this connection owned, in key order.
    pub subscriptions: Vec<Subscription>,
    /// Subscriptions the venue had not acknowledged when the socket died.
    ///
    /// A subscription naming an unknown coin closes the whole connection with
    /// no close frame (measured 2026-09-03). This list is the diagnostic: a
    /// connection that dies with exactly one unacked subscription is almost
    /// certainly dying *of* it.
    pub unacked: Vec<Subscription>,
    pub reason: String,
}

/// A connection came back and re-established everything it owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconnected {
    pub connection: ConnectionId,
    pub at_ms: u64,
    /// The window the ledger must backfill.
    pub gap: GapWindow,
    /// Re-sent subscriptions, in key order.
    pub resubscribed: Vec<Subscription>,
    /// How many failed attempts preceded this one.
    pub attempts: u32,
}

/// Everything a consumer sees. Delivered over one `tokio::sync::mpsc` channel
/// so backpressure is real: a slow consumer slows the reader rather than
/// silently dropping ticks, which a broadcast channel would do.
#[derive(Debug, Clone, PartialEq)]
pub enum WsEvent {
    /// Complete asset context. **Locally stamped** — the venue sends no time.
    ActiveAssetCtx {
        coin: String,
        ctx: Box<AssetCtx>,
        received_at_ms: u64,
    },
    /// Best bid / offer. Either side is `None` when that side of the book is
    /// empty — `docs/specs/fair-value.md` §5.2 requires that to mark `micro`
    /// stale, never zero.
    Bbo {
        coin: String,
        venue_time_ms: u64,
        bid: Option<Level>,
        ask: Option<Level>,
    },
    /// Public tape, never emitted empty.
    Trades { coin: String, trades: Vec<WsTrade> },
    /// Live bar. `t`/`T` are bar boundaries, not a sample instant, so this
    /// event's [`EventTime`] is `None`.
    Candle(Box<Candle>),
    /// Depth ladder for a `l2Book` subscription.
    L2Book(Box<L2Book>),
    /// Fills for one address. `is_snapshot` marks the backlog the venue sends
    /// on subscribe, which the ledger must dedupe by `tid` rather than append.
    UserFills {
        user: Address,
        is_snapshot: bool,
        fills: Vec<Fill>,
    },
    /// Order state transitions. `user` is supplied by the owning connection,
    /// because the payload does not carry it.
    OrderUpdates {
        user: Address,
        updates: Vec<OrderStatusEntry>,
    },
    /// Socket lost. Execution tools fail closed from here (`docs/spec.md`
    /// item 34).
    Disconnected(Box<Disconnected>),
    /// Socket back, subscriptions restored, gap window attached.
    Reconnected(Box<Reconnected>),
    /// The pool stopped re-subscribing one feed because it kept taking the
    /// socket down instead of being acknowledged. The shard converges instead
    /// of reconnect-looping, at the cost of that feed; the operator has to fix
    /// the subscription. See
    /// [`SubscriptionRegistry::record_failed_session`].
    SubscriptionQuarantined {
        connection: ConnectionId,
        subscription: Subscription,
        strikes: u32,
    },
    /// `{"channel":"error", …}` from the venue, verbatim and inert.
    VenueError {
        connection: ConnectionId,
        message: String,
    },
    /// A frame arrived and could not be understood. Reported rather than
    /// dropped: on a data plane, silence and a parse failure look identical
    /// from the outside, and only one of them is safe.
    MessageDropped {
        connection: ConnectionId,
        channel: String,
        reason: String,
    },
}

impl WsEvent {
    /// Which subscription this event belongs to, for staleness bookkeeping.
    /// `None` for control events, which belong to a connection, not a feed.
    pub fn subscription_key(&self) -> Option<String> {
        let sub = match self {
            WsEvent::ActiveAssetCtx { coin, .. } => {
                Subscription::ActiveAssetCtx { coin: coin.clone() }
            }
            WsEvent::Bbo { coin, .. } => Subscription::Bbo { coin: coin.clone() },
            WsEvent::Trades { coin, .. } => Subscription::Trades { coin: coin.clone() },
            WsEvent::Candle(c) => Subscription::Candle {
                coin: c.s.clone(),
                interval: c.i.clone(),
            },
            WsEvent::L2Book(b) => Subscription::L2Book {
                coin: b.coin.clone(),
            },
            WsEvent::UserFills { user, .. } => Subscription::UserFills { user: *user },
            WsEvent::OrderUpdates { user, .. } => Subscription::OrderUpdates { user: *user },
            WsEvent::Disconnected(_)
            | WsEvent::Reconnected(_)
            | WsEvent::SubscriptionQuarantined { .. }
            | WsEvent::VenueError { .. }
            | WsEvent::MessageDropped { .. } => return None,
        };
        Some(sub.key())
    }

    /// Whose clock stamped this event. See [`EventTime`] for why the caller is
    /// not allowed to forget the difference.
    pub fn event_time(&self) -> EventTime {
        match self {
            WsEvent::ActiveAssetCtx { received_at_ms, .. } => {
                EventTime::LocalArrival(*received_at_ms)
            }
            WsEvent::Bbo { venue_time_ms, .. } => EventTime::Venue(*venue_time_ms),
            WsEvent::Trades { trades, .. } => trades
                .last()
                .map(|t| EventTime::Venue(t.time))
                .unwrap_or(EventTime::None),
            WsEvent::L2Book(b) => EventTime::Venue(b.time),
            WsEvent::UserFills { fills, .. } => fills
                .last()
                .map(|f| EventTime::Venue(f.time))
                .unwrap_or(EventTime::None),
            WsEvent::OrderUpdates { updates, .. } => updates
                .last()
                .map(|u| EventTime::Venue(u.status_timestamp))
                .unwrap_or(EventTime::None),
            WsEvent::Candle(_)
            | WsEvent::Disconnected(_)
            | WsEvent::Reconnected(_)
            | WsEvent::SubscriptionQuarantined { .. }
            | WsEvent::VenueError { .. }
            | WsEvent::MessageDropped { .. } => EventTime::None,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// What one inbound frame turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Data for a consumer.
    Event(WsEvent),
    /// `subscriptionResponse`. `subscribed` distinguishes subscribe from
    /// unsubscribe; the key is `None` when the echo names a subscription shape
    /// this build does not model.
    Ack {
        key: Option<String>,
        subscribed: bool,
    },
    /// Answer to [`PING_FRAME`].
    Pong,
    /// `{"channel":"error","data":"…"}`.
    VenueError(String),
    /// A channel this build does not consume.
    Ignored { channel: String },
}

/// Everything [`parse_message`] needs that is not in the frame itself.
#[derive(Debug, Clone, Copy)]
pub struct ParseContext {
    /// Arrival clock, ms. Passed in rather than read inside so the parser is
    /// pure and its `activeAssetCtx` stamping is testable.
    pub now_ms: u64,
    /// The single address whose `orderUpdates` this connection owns, if any.
    /// Required because the payload carries no `user`.
    pub order_updates_user: Option<Address>,
}

#[derive(Deserialize)]
struct Envelope {
    channel: String,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct BboData {
    coin: String,
    time: u64,
    bbo: [Option<Level>; 2],
}

#[derive(Deserialize)]
struct ActiveAssetCtxData {
    coin: String,
    ctx: AssetCtx,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserFillsData {
    #[serde(default)]
    is_snapshot: bool,
    user: Address,
    fills: Vec<Fill>,
}

#[derive(Deserialize)]
struct AckData {
    method: String,
    subscription: serde_json::Value,
}

fn parse_field<T: serde::de::DeserializeOwned>(
    channel: &str,
    data: serde_json::Value,
) -> Result<T, PoolError> {
    serde_json::from_value(data).map_err(|e| PoolError::Parse {
        channel: channel.to_owned(),
        detail: e.to_string(),
    })
}

/// Turn one text frame into an [`Incoming`].
///
/// Pure, so the shapes below are pinned by fixtures captured from live mainnet
/// rather than by a running socket (`AGENTS.md`: tests before "done").
pub fn parse_message(raw: &str, ctx: &ParseContext) -> Result<Incoming, PoolError> {
    let envelope: Envelope = serde_json::from_str(raw).map_err(|e| PoolError::Parse {
        channel: "<envelope>".to_owned(),
        detail: e.to_string(),
    })?;
    let channel = envelope.channel.as_str();
    let event = match channel {
        "activeAssetCtx" => {
            let d: ActiveAssetCtxData = parse_field(channel, envelope.data)?;
            WsEvent::ActiveAssetCtx {
                coin: d.coin,
                ctx: Box::new(d.ctx),
                received_at_ms: ctx.now_ms,
            }
        }
        "bbo" => {
            let d: BboData = parse_field(channel, envelope.data)?;
            let [bid, ask] = d.bbo;
            WsEvent::Bbo {
                coin: d.coin,
                venue_time_ms: d.time,
                bid,
                ask,
            }
        }
        "trades" => {
            let trades: Vec<WsTrade> = parse_field(channel, envelope.data)?;
            match trades.first() {
                // An empty tape frame names no coin and carries no data.
                None => {
                    return Ok(Incoming::Ignored {
                        channel: channel.to_owned(),
                    });
                }
                Some(first) => WsEvent::Trades {
                    coin: first.coin.clone(),
                    trades,
                },
            }
        }
        "candle" => WsEvent::Candle(Box::new(parse_field(channel, envelope.data)?)),
        "l2Book" => WsEvent::L2Book(Box::new(parse_field(channel, envelope.data)?)),
        "userFills" => {
            let d: UserFillsData = parse_field(channel, envelope.data)?;
            WsEvent::UserFills {
                user: d.user,
                is_snapshot: d.is_snapshot,
                fills: d.fills,
            }
        }
        "orderUpdates" => {
            let user = ctx
                .order_updates_user
                .ok_or(PoolError::UnattributableOrderUpdates)?;
            WsEvent::OrderUpdates {
                user,
                updates: parse_field(channel, envelope.data)?,
            }
        }
        "subscriptionResponse" => {
            let d: AckData = parse_field(channel, envelope.data)?;
            // The venue echoes the subscription with its own defaults filled in
            // (`userFills` comes back carrying `aggregateByTime`), so this is a
            // lenient read: an unmodelled shape is an ack with no key, not an
            // error.
            let key = serde_json::from_value::<Subscription>(d.subscription)
                .ok()
                .map(|s| s.key());
            return Ok(Incoming::Ack {
                key,
                subscribed: d.method == "subscribe",
            });
        }
        "pong" => return Ok(Incoming::Pong),
        "error" => {
            let message: String = parse_field(channel, envelope.data)?;
            return Ok(Incoming::VenueError(message));
        }
        other => {
            return Ok(Incoming::Ignored {
                channel: other.to_owned(),
            });
        }
    };
    Ok(Incoming::Event(event))
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Exponential backoff with bounded jitter, per `docs/spec.md` item 9.
///
/// Jitter matters here because oppen opens several connections to one venue: a
/// venue-side blip drops them together, and without jitter they would all
/// reconnect on the same schedule forever, converting one outage into a
/// self-inflicted request burst against an address rate budget that item 10
/// says must always keep headroom for risk-reducing actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay for attempt 0.
    pub base: Duration,
    /// Ceiling on the nominal delay, before jitter.
    pub max: Duration,
    /// Half-width of the jitter band, as a percentage of the nominal delay.
    /// Clamped to 100.
    pub jitter_pct: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            jitter_pct: 20,
        }
    }
}

impl Backoff {
    /// Un-jittered delay for `attempt`, doubling from `base` and capped at
    /// `max`. Saturating throughout: no attempt count can overflow it.
    pub fn nominal(&self, attempt: u32) -> Duration {
        let base_ms = u64::try_from(self.base.as_millis()).unwrap_or(u64::MAX);
        let factor = 1u64.checked_shl(attempt.min(32)).unwrap_or(u64::MAX);
        Duration::from_millis(base_ms.saturating_mul(factor)).min(self.max)
    }

    /// [`Backoff::nominal`] spread uniformly over
    /// `±jitter_pct%`, never negative.
    pub fn delay(&self, attempt: u32, jitter: &mut Jitter) -> Duration {
        let nominal_ms = u64::try_from(self.nominal(attempt).as_millis()).unwrap_or(u64::MAX);
        let spread = nominal_ms.saturating_mul(u64::from(self.jitter_pct.min(100))) / 100;
        if spread == 0 {
            return Duration::from_millis(nominal_ms);
        }
        let low = nominal_ms.saturating_sub(spread);
        Duration::from_millis(low.saturating_add(jitter.next_below(2 * spread + 1)))
    }
}

/// xorshift64\* jitter source.
///
/// A three-line PRNG rather than a dependency: this seeds a sleep duration, not
/// a key, and `oppen-hl` is the crate where every added dependency is one more
/// thing sharing an address space with a private key (`AGENTS.md` invariant 2).
/// Seedable so the backoff band is a deterministic test rather than a
/// statistical one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jitter(u64);

impl Jitter {
    /// Seeded explicitly. Only zero is replaced — xorshift cannot escape it —
    /// so distinct seeds stay distinct streams.
    pub fn from_seed(seed: u64) -> Self {
        Jitter(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// Wall clock plus a per-connection salt, so sibling connections in one
    /// pool do not share a sequence.
    pub fn from_entropy(salt: u64) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Jitter::from_seed(nanos ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish draw in `0..n`; zero when `n` is zero.
    pub fn next_below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Where a [`SubscriptionRegistry::place`] call put a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Added to a connection that is already running.
    Existing(ConnectionId),
    /// Added to a connection the caller must now spawn.
    NewConnection(ConnectionId),
    /// Already subscribed; nothing to send.
    AlreadyPresent(ConnectionId),
}

/// Live health of one feed. What `docs/spec.md` item 34 renders as the stale
/// overlay and what the guardrails read to fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedHealth {
    pub connection: ConnectionId,
    pub subscription: Subscription,
    /// Whether the owning socket is up.
    pub connected: bool,
    /// Whether the venue acknowledged the subscription on the current socket.
    pub acked: bool,
    pub last_message_ms: Option<u64>,
    /// Silence so far, ms. `None` before the first message.
    pub age_ms: Option<u64>,
    /// Budget from [`Subscription::staleness_threshold`]; `None` for
    /// event-driven feeds.
    pub threshold_ms: Option<u64>,
    /// Fails closed: a down socket, or a feed with a threshold that has never
    /// delivered, both count as stale.
    pub stale: bool,
    /// The pool stopped re-subscribing this one because the venue kept killing
    /// the socket rather than acknowledging it. See
    /// [`SubscriptionRegistry::record_failed_session`].
    pub quarantined: bool,
}

#[derive(Debug, Clone)]
struct SubEntry {
    sub: Subscription,
    last_message_ms: Option<u64>,
    acked: bool,
    strikes: u32,
    quarantined: bool,
}

#[derive(Debug)]
struct ConnectionSlot {
    id: ConnectionId,
    subs: BTreeMap<String, SubEntry>,
    connected: bool,
    last_message_ms: Option<u64>,
}

/// Which subscriptions live on which connection, and when each last spoke.
///
/// Pure bookkeeping with no I/O, so the placement rules, the per-connection cap
/// and the staleness arithmetic are unit-testable without a socket.
#[derive(Debug)]
pub struct SubscriptionRegistry {
    max_connections: usize,
    max_subs_per_connection: usize,
    slots: Vec<ConnectionSlot>,
}

impl SubscriptionRegistry {
    /// `max_subs_per_connection` defaults to the venue's documented 1000
    /// (`docs/spec.md` item 9). Note the venue documents that ceiling **per
    /// IP**, so a pool of several connections shares one budget; size
    /// `max_connections` accordingly.
    pub fn new(max_connections: usize, max_subs_per_connection: usize) -> Self {
        SubscriptionRegistry {
            max_connections: max_connections.max(1),
            max_subs_per_connection: max_subs_per_connection.max(1),
            slots: Vec::new(),
        }
    }

    /// Assign a subscription to a connection.
    ///
    /// First connection with room wins, so a pool stays as small as the load
    /// requires; `orderUpdates` is placed alone per
    /// [`Subscription::exclusive_per_connection`].
    pub fn place(&mut self, sub: Subscription) -> Result<Placement, PoolError> {
        let key = sub.key();
        if let Some(slot) = self.slots.iter().find(|s| s.subs.contains_key(&key)) {
            return Ok(Placement::AlreadyPresent(slot.id));
        }
        let cap = self.max_subs_per_connection;
        let exclusive = sub.exclusive_per_connection();
        let target = self.slots.iter_mut().find(|s| {
            s.subs.len() < cap
                && !(exclusive && s.subs.values().any(|e| e.sub.exclusive_per_connection()))
        });
        let entry = SubEntry {
            sub,
            last_message_ms: None,
            acked: false,
            strikes: 0,
            quarantined: false,
        };
        if let Some(slot) = target {
            slot.subs.insert(key, entry);
            return Ok(Placement::Existing(slot.id));
        }
        if self.slots.len() >= self.max_connections {
            return Err(PoolError::CapacityExhausted {
                used: self.len(),
                cap: self.max_connections.saturating_mul(cap),
                connections: self.slots.len(),
            });
        }
        let id = ConnectionId(self.slots.len());
        let mut subs = BTreeMap::new();
        subs.insert(key, entry);
        self.slots.push(ConnectionSlot {
            id,
            subs,
            connected: false,
            last_message_ms: None,
        });
        Ok(Placement::NewConnection(id))
    }

    /// Drop a subscription, returning the connection that must send the
    /// unsubscribe frame. The now-possibly-empty connection is kept open and
    /// reused: reconnecting costs a handshake, idling costs a ping every 30 s.
    pub fn remove(&mut self, sub: &Subscription) -> Result<ConnectionId, PoolError> {
        let key = sub.key();
        for slot in self.slots.iter_mut() {
            if slot.subs.remove(&key).is_some() {
                return Ok(slot.id);
            }
        }
        Err(PoolError::NotSubscribed(key))
    }

    /// Everything one connection owns, in key order, quarantined included.
    pub fn subscriptions(&self, id: ConnectionId) -> Vec<Subscription> {
        self.slot(id)
            .map(|s| s.subs.values().map(|e| e.sub.clone()).collect())
            .unwrap_or_default()
    }

    /// What a reconnect should actually re-send, in key order: everything the
    /// connection owns except what [`SubscriptionRegistry::record_failed_session`]
    /// has quarantined.
    pub fn resubscribe_set(&self, id: ConnectionId) -> Vec<Subscription> {
        self.slot(id)
            .map(|s| {
                s.subs
                    .values()
                    .filter(|e| !e.quarantined)
                    .map(|e| e.sub.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Charge one strike for a session that died with unacknowledged
    /// subscriptions, and quarantine the suspect once it reaches
    /// `quarantine_after`.
    ///
    /// Without this the pool does not converge, and that is measured, not
    /// hypothetical: subscribing `bbo` on an unknown coin closes the socket, the
    /// reconnect re-sends the same poison, and the shard reconnect-loops
    /// forever — blinding exactly the feeds the pool exists to keep alive.
    ///
    /// Only the **first** unacknowledged subscription in send order is charged.
    /// The venue processes a subscribe burst in order and closes on the one it
    /// refuses, so everything after the first unacked one is collateral, not
    /// cause. Charging one per outage means a healthy feed behind a poison is
    /// never quarantined with it, and an ack clears the count, so a strike only
    /// accumulates across consecutive failures to acknowledge.
    ///
    /// Returns the subscription that just became quarantined, if any.
    pub fn record_failed_session(
        &mut self,
        id: ConnectionId,
        quarantine_after: u32,
    ) -> Option<(Subscription, u32)> {
        let entry = self
            .slot_mut(id)?
            .subs
            .values_mut()
            .find(|e| !e.acked && !e.quarantined)?;
        entry.strikes = entry.strikes.saturating_add(1);
        if entry.strikes >= quarantine_after.max(1) {
            entry.quarantined = true;
            return Some((entry.sub.clone(), entry.strikes));
        }
        None
    }

    /// Subscriptions the venue has not acknowledged on the current socket.
    pub fn unacked(&self, id: ConnectionId) -> Vec<Subscription> {
        self.slot(id)
            .map(|s| {
                s.subs
                    .values()
                    .filter(|e| !e.acked)
                    .map(|e| e.sub.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The address whose `orderUpdates` this connection owns, used to attribute
    /// a payload that carries no user.
    pub fn order_updates_user(&self, id: ConnectionId) -> Option<Address> {
        self.slot(id)?.subs.values().find_map(|e| match &e.sub {
            Subscription::OrderUpdates { user } => Some(*user),
            _ => None,
        })
    }

    /// Last message on the connection, whatever channel it came on. This is the
    /// gap-window anchor; see [`GapWindow`].
    pub fn connection_last_message(&self, id: ConnectionId) -> Option<u64> {
        self.slot(id)?.last_message_ms
    }

    /// Record traffic on the connection as a whole. Called for every frame,
    /// including pongs, because a pong proves the socket is alive.
    pub fn touch_connection(&mut self, id: ConnectionId, now_ms: u64) {
        if let Some(slot) = self.slot_mut(id) {
            slot.last_message_ms = Some(now_ms);
        }
    }

    /// Record traffic on one feed.
    pub fn touch(&mut self, id: ConnectionId, key: &str, now_ms: u64) {
        if let Some(entry) = self.slot_mut(id).and_then(|s| s.subs.get_mut(key)) {
            entry.last_message_ms = Some(now_ms);
        }
    }

    /// Record the venue's `subscriptionResponse`. Clears the strike count: a
    /// subscription the venue has just accepted is not a suspect.
    pub fn ack(&mut self, id: ConnectionId, key: &str) {
        if let Some(entry) = self.slot_mut(id).and_then(|s| s.subs.get_mut(key)) {
            entry.acked = true;
            entry.strikes = 0;
        }
    }

    /// Flip a connection up or down.
    ///
    /// Either transition clears every ack: a dead socket confirms nothing, and
    /// a fresh one has confirmed nothing yet. Callers that need the pre-drop
    /// ack state — [`SubscriptionRegistry::unacked`], which names the likely
    /// poison in a [`Disconnected`] — must read it before flipping.
    ///
    /// Coming up seeds the connection clock so the idle detector has a
    /// baseline. Neither transition touches per-feed timestamps, so each feed's
    /// age keeps growing through an outage, which is what makes them read
    /// stale.
    pub fn set_connected(&mut self, id: ConnectionId, connected: bool, now_ms: u64) {
        if let Some(slot) = self.slot_mut(id) {
            slot.connected = connected;
            for entry in slot.subs.values_mut() {
                entry.acked = false;
            }
            if connected {
                slot.last_message_ms = Some(now_ms);
            }
        }
    }

    /// Health of every feed, ordered by connection then key.
    pub fn health(&self, now_ms: u64, thresholds: &StalenessThresholds) -> Vec<FeedHealth> {
        let mut out = Vec::with_capacity(self.len());
        for slot in &self.slots {
            for entry in slot.subs.values() {
                let threshold = entry.sub.staleness_threshold(thresholds);
                let threshold_ms =
                    threshold.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
                let age_ms = entry.last_message_ms.map(|t| now_ms.saturating_sub(t));
                let stale = match (threshold_ms, age_ms) {
                    (None, _) => false,
                    // A quarantined feed is not coming back on its own.
                    (Some(_), _) if entry.quarantined => true,
                    // A socket that is down is stale regardless of history, and
                    // a feed with a budget that has never delivered has already
                    // blown it. Both fail closed (`docs/spec.md` item 34).
                    (Some(_), _) if !slot.connected => true,
                    (Some(_), None) => true,
                    (Some(limit), Some(age)) => age > limit,
                };
                out.push(FeedHealth {
                    connection: slot.id,
                    subscription: entry.sub.clone(),
                    connected: slot.connected,
                    acked: entry.acked,
                    last_message_ms: entry.last_message_ms,
                    age_ms,
                    threshold_ms,
                    stale,
                    quarantined: entry.quarantined,
                });
            }
        }
        out
    }

    /// Subscriptions held across the whole pool, for the item 9 cap.
    pub fn len(&self) -> usize {
        self.slots.iter().map(|s| s.subs.len()).sum()
    }

    /// True when nothing is subscribed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total slots this pool may ever hold.
    pub fn capacity(&self) -> usize {
        self.max_connections
            .saturating_mul(self.max_subs_per_connection)
    }

    /// Connections opened so far.
    pub fn connection_count(&self) -> usize {
        self.slots.len()
    }

    fn slot(&self, id: ConnectionId) -> Option<&ConnectionSlot> {
        self.slots.get(id.0)
    }

    fn slot_mut(&mut self, id: ConnectionId) -> Option<&mut ConnectionSlot> {
        self.slots.get_mut(id.0)
    }
}

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

/// Pool tuning. Defaults target testnet-first operation (`AGENTS.md`
/// invariant 5, D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsPoolConfig {
    pub network: Network,
    /// Sockets the pool may open. Four shards the failure domain without
    /// approaching the venue's per-IP connection ceiling.
    pub max_connections: usize,
    /// Venue-documented ceiling (`docs/spec.md` item 9).
    pub max_subs_per_connection: usize,
    /// Client ping cadence. The server drops idle connections at roughly 60 s,
    /// so 30 s keeps two pings inside every window.
    pub ping_interval: Duration,
    /// Force a reconnect when nothing at all has arrived for this long. A TCP
    /// connection can black-hole without erroring, and a socket that is up but
    /// silent is the one failure the reconnect loop would otherwise never see.
    pub idle_timeout: Duration,
    pub backoff: Backoff,
    pub thresholds: StalenessThresholds,
    /// Consecutive sessions that may die with the same subscription
    /// unacknowledged before the pool stops re-sending it
    /// ([`SubscriptionRegistry::record_failed_session`]). Three, because one
    /// drop inside a subscribe window is ordinary bad luck and three in a row
    /// is not.
    pub quarantine_after: u32,
    /// Event channel depth. Bounded on purpose: see [`WsEvent`].
    pub event_buffer: usize,
}

impl Default for WsPoolConfig {
    fn default() -> Self {
        WsPoolConfig {
            network: Network::default(),
            max_connections: 4,
            max_subs_per_connection: 1000,
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(75),
            backoff: Backoff::default(),
            thresholds: StalenessThresholds::default(),
            quarantine_after: 3,
            event_buffer: 4096,
        }
    }
}

impl WsPoolConfig {
    /// Defaults on an explicit network.
    pub fn new(network: Network) -> Self {
        WsPoolConfig {
            network,
            ..Default::default()
        }
    }
}

#[derive(Debug)]
enum ConnCommand {
    Subscribe(Subscription),
    Unsubscribe(Subscription),
    Shutdown,
}

struct PoolInner {
    registry: SubscriptionRegistry,
    conns: Vec<mpsc::UnboundedSender<ConnCommand>>,
    shutdown: bool,
}

/// A pool of independently-reconnecting Hyperliquid websockets.
///
/// `docs/spec.md` item 9: all agents multiplex over a shared socket pool.
/// Subscriptions are distributed across connections, each connection owns its
/// own reconnect loop, and a drop on one degrades exactly the feeds it carried
/// while emitting the gap window the ledger needs to backfill.
///
/// Dropping the pool closes every command channel, which stops every connection
/// task; [`WsPool::shutdown`] does the same explicitly.
pub struct WsPool {
    cfg: WsPoolConfig,
    inner: Arc<Mutex<PoolInner>>,
    events: mpsc::Sender<WsEvent>,
}

fn lock(inner: &Mutex<PoolInner>) -> MutexGuard<'_, PoolInner> {
    // A panicking connection task must not take the pool down with it: the
    // point of independent lifecycles is that one shard failing degrades
    // rather than blinds.
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl WsPool {
    /// Build a pool and hand back the consumer's event stream.
    ///
    /// No socket is opened until the first [`WsPool::subscribe`], so
    /// constructing a pool costs nothing and needs no runtime.
    pub fn new(cfg: WsPoolConfig) -> (Self, mpsc::Receiver<WsEvent>) {
        let (tx, rx) = mpsc::channel(cfg.event_buffer.max(1));
        let inner = PoolInner {
            registry: SubscriptionRegistry::new(cfg.max_connections, cfg.max_subs_per_connection),
            conns: Vec::new(),
            shutdown: false,
        };
        (
            WsPool {
                cfg,
                inner: Arc::new(Mutex::new(inner)),
                events: tx,
            },
            rx,
        )
    }

    /// Add a subscription, opening a connection if every existing one is full.
    ///
    /// Synchronous and non-blocking: commands go to the owning connection task
    /// over an unbounded channel, so this never awaits while holding the
    /// registry lock. Re-subscribing something already held is a no-op.
    ///
    /// Callers must validate `coin` against the meta universe first. An unknown
    /// coin closes the whole connection (measured), and the pool cannot tell
    /// that apart from a network drop.
    pub fn subscribe(&self, sub: Subscription) -> Result<(), PoolError> {
        let mut guard = lock(&self.inner);
        if guard.shutdown {
            return Err(PoolError::Shutdown);
        }
        match guard.registry.place(sub.clone())? {
            Placement::AlreadyPresent(_) => Ok(()),
            Placement::Existing(id) => {
                if let Some(tx) = guard.conns.get(id.0) {
                    let _ = tx.send(ConnCommand::Subscribe(sub));
                }
                Ok(())
            }
            Placement::NewConnection(id) => {
                let (tx, rx) = mpsc::unbounded_channel();
                guard.conns.push(tx);
                drop(guard);
                tokio::spawn(run_connection(
                    id,
                    self.cfg.network.ws_url().to_owned(),
                    self.cfg.clone(),
                    Arc::clone(&self.inner),
                    self.events.clone(),
                    rx,
                ));
                Ok(())
            }
        }
    }

    /// Drop a subscription and tell its connection to unsubscribe.
    pub fn unsubscribe(&self, sub: &Subscription) -> Result<(), PoolError> {
        let mut guard = lock(&self.inner);
        if guard.shutdown {
            return Err(PoolError::Shutdown);
        }
        let id = guard.registry.remove(sub)?;
        if let Some(tx) = guard.conns.get(id.0) {
            let _ = tx.send(ConnCommand::Unsubscribe(sub.clone()));
        }
        Ok(())
    }

    /// Per-feed health for the console's stale overlay and the guardrails'
    /// fail-closed check (`docs/spec.md` item 34).
    pub fn health(&self) -> Vec<FeedHealth> {
        lock(&self.inner)
            .registry
            .health(now_ms(), &self.cfg.thresholds)
    }

    /// True when any feed with a staleness budget has blown it. This is the
    /// check execution paths read before signing.
    pub fn any_stale(&self) -> bool {
        self.health().iter().any(|h| h.stale)
    }

    /// Subscriptions held, against [`WsPool::capacity`] (`docs/spec.md`
    /// item 9's 1000-per-connection cap).
    pub fn subscription_count(&self) -> usize {
        lock(&self.inner).registry.len()
    }

    /// Total subscription slots this pool may ever hold.
    pub fn capacity(&self) -> usize {
        lock(&self.inner).registry.capacity()
    }

    /// Connections opened so far.
    pub fn connection_count(&self) -> usize {
        lock(&self.inner).registry.connection_count()
    }

    /// Stop every connection. Idempotent; further subscribes return
    /// [`PoolError::Shutdown`].
    pub fn shutdown(&self) {
        let mut guard = lock(&self.inner);
        guard.shutdown = true;
        for tx in &guard.conns {
            let _ = tx.send(ConnCommand::Shutdown);
        }
        guard.conns.clear();
    }
}

impl fmt::Debug for WsPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsPool")
            .field("network", &self.cfg.network)
            .field("subscriptions", &self.subscription_count())
            .field("connections", &self.connection_count())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Connection task
// ---------------------------------------------------------------------------

enum SessionEnd {
    /// The pool asked us to stop, or the consumer went away.
    Shutdown,
    /// The socket died; reconnect.
    Dropped(String),
}

async fn send_frame(stream: &mut WsStream, method: &str, sub: &Subscription) -> Result<(), String> {
    let frame = serde_json::json!({ "method": method, "subscription": sub });
    let text = serde_json::to_string(&frame).map_err(|e| e.to_string())?;
    stream
        .send(Message::Text(text))
        .await
        .map_err(|e| e.to_string())
}

/// One connection's whole life: connect, re-subscribe, pump, reconnect.
async fn run_connection(
    id: ConnectionId,
    url: String,
    cfg: WsPoolConfig,
    inner: Arc<Mutex<PoolInner>>,
    events: mpsc::Sender<WsEvent>,
    mut cmds: mpsc::UnboundedReceiver<ConnCommand>,
) {
    let mut jitter = Jitter::from_entropy(id.0 as u64);
    let mut attempt: u32 = 0;
    let mut ever_connected = false;
    // Anchor for the next gap window: the last moment this connection is known
    // to have been receiving.
    let mut last_seen_ms: Option<u64> = None;
    let mut disconnected_at_ms: u64 = now_ms();

    loop {
        match connect_async(url.as_str()).await {
            Ok((mut stream, _response)) => {
                let subs = {
                    let mut guard = lock(&inner);
                    guard.registry.set_connected(id, true, now_ms());
                    guard.registry.resubscribe_set(id)
                };
                let mut resubscribe_error = None;
                for sub in &subs {
                    if let Err(e) = send_frame(&mut stream, "subscribe", sub).await {
                        resubscribe_error = Some(e);
                        break;
                    }
                }
                if let Some(reason) = resubscribe_error {
                    // A local send failure is not the venue refusing anything,
                    // so nothing is charged a strike.
                    if !report_disconnect(id, &cfg, &inner, &events, reason, false).await {
                        return;
                    }
                    last_seen_ms = lock(&inner).registry.connection_last_message(id);
                    disconnected_at_ms = now_ms();
                } else {
                    if ever_connected {
                        let at = now_ms();
                        let start = last_seen_ms.unwrap_or(disconnected_at_ms);
                        let event = WsEvent::Reconnected(Box::new(Reconnected {
                            connection: id,
                            at_ms: at,
                            gap: GapWindow {
                                start_ms: start.min(at),
                                end_ms: at,
                            },
                            resubscribed: subs,
                            attempts: attempt,
                        }));
                        if events.send(event).await.is_err() {
                            return;
                        }
                    }
                    ever_connected = true;
                    attempt = 0;
                    match session(id, &mut stream, &mut cmds, &events, &inner, &cfg).await {
                        SessionEnd::Shutdown => {
                            lock(&inner).registry.set_connected(id, false, now_ms());
                            let _ = stream.close(None).await;
                            return;
                        }
                        SessionEnd::Dropped(reason) => {
                            last_seen_ms = lock(&inner).registry.connection_last_message(id);
                            disconnected_at_ms = now_ms();
                            if !report_disconnect(id, &cfg, &inner, &events, reason, true).await {
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // Only the first failure of an outage is an event; the retries
                // that follow are the same outage, and the activity stream is
                // for the operator, not for the retry loop.
                if attempt == 0 {
                    let reason = format!("connect failed: {e}");
                    if !report_disconnect(id, &cfg, &inner, &events, reason, false).await {
                        return;
                    }
                    disconnected_at_ms = now_ms();
                }
            }
        }

        if lock(&inner).shutdown {
            return;
        }
        let wait = cfg.backoff.delay(attempt, &mut jitter);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            cmd = cmds.recv() => {
                // A shutdown during backoff should not wait out the sleep.
                if matches!(cmd, None | Some(ConnCommand::Shutdown)) {
                    return;
                }
            }
        }
        attempt = attempt.saturating_add(1);
    }
}

/// Emits [`WsEvent::Disconnected`], and [`WsEvent::SubscriptionQuarantined`]
/// when a suspect ran out of strikes. Returns false when the consumer is gone.
///
/// `strike` is false for failures that are ours rather than the venue's — a
/// failed TCP connect, a local send error — because neither is evidence about
/// any particular subscription.
async fn report_disconnect(
    id: ConnectionId,
    cfg: &WsPoolConfig,
    inner: &Arc<Mutex<PoolInner>>,
    events: &mpsc::Sender<WsEvent>,
    reason: String,
    strike: bool,
) -> bool {
    let at = now_ms();
    let (last_message_ms, subscriptions, unacked, quarantined) = {
        let mut guard = lock(inner);
        let last = guard.registry.connection_last_message(id);
        let subs = guard.registry.subscriptions(id);
        let unacked = guard.registry.unacked(id);
        let quarantined = if strike {
            guard
                .registry
                .record_failed_session(id, cfg.quarantine_after)
        } else {
            None
        };
        guard.registry.set_connected(id, false, at);
        (last, subs, unacked, quarantined)
    };
    let event = WsEvent::Disconnected(Box::new(Disconnected {
        connection: id,
        at_ms: at,
        last_message_ms,
        subscriptions,
        unacked,
        reason,
    }));
    if events.send(event).await.is_err() {
        return false;
    }
    if let Some((subscription, strikes)) = quarantined {
        tracing::warn!(
            connection = %id,
            subscription = %subscription,
            strikes,
            "quarantined a subscription that kept killing the socket"
        );
        let event = WsEvent::SubscriptionQuarantined {
            connection: id,
            subscription,
            strikes,
        };
        return events.send(event).await.is_ok();
    }
    true
}

async fn session(
    id: ConnectionId,
    stream: &mut WsStream,
    cmds: &mut mpsc::UnboundedReceiver<ConnCommand>,
    events: &mpsc::Sender<WsEvent>,
    inner: &Arc<Mutex<PoolInner>>,
    cfg: &WsPoolConfig,
) -> SessionEnd {
    let mut ping = tokio::time::interval(cfg.ping_interval);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // `interval` fires immediately; the socket has just been used.
    ping.tick().await;
    let idle_ms = u64::try_from(cfg.idle_timeout.as_millis()).unwrap_or(u64::MAX);

    loop {
        tokio::select! {
            biased;
            cmd = cmds.recv() => match cmd {
                None | Some(ConnCommand::Shutdown) => return SessionEnd::Shutdown,
                Some(ConnCommand::Subscribe(sub)) => {
                    if let Err(e) = send_frame(stream, "subscribe", &sub).await {
                        return SessionEnd::Dropped(e);
                    }
                }
                Some(ConnCommand::Unsubscribe(sub)) => {
                    if let Err(e) = send_frame(stream, "unsubscribe", &sub).await {
                        return SessionEnd::Dropped(e);
                    }
                }
            },
            item = stream.next() => match item {
                None => return SessionEnd::Dropped("stream ended".to_owned()),
                Some(Err(e)) => return SessionEnd::Dropped(e.to_string()),
                Some(Ok(Message::Text(text))) => {
                    if !handle_text(id, &text, inner, events).await {
                        return SessionEnd::Shutdown;
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    if stream.send(Message::Pong(payload)).await.is_err() {
                        return SessionEnd::Dropped("pong send failed".to_owned());
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let reason = frame
                        .map(|f| format!("server closed: {} {}", f.code, f.reason))
                        .unwrap_or_else(|| "server closed".to_owned());
                    return SessionEnd::Dropped(reason);
                }
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                let last = lock(inner).registry.connection_last_message(id);
                if let Some(last) = last
                    && now_ms().saturating_sub(last) > idle_ms
                {
                    return SessionEnd::Dropped(format!("idle for more than {idle_ms} ms"));
                }
                if stream.send(Message::Text(PING_FRAME.to_owned())).await.is_err() {
                    return SessionEnd::Dropped("ping send failed".to_owned());
                }
            }
        }
    }
}

/// Returns false when the consumer's receiver is gone, which ends the task.
async fn handle_text(
    id: ConnectionId,
    text: &str,
    inner: &Arc<Mutex<PoolInner>>,
    events: &mpsc::Sender<WsEvent>,
) -> bool {
    let now = now_ms();
    let order_updates_user = {
        let mut guard = lock(inner);
        guard.registry.touch_connection(id, now);
        guard.registry.order_updates_user(id)
    };
    let ctx = ParseContext {
        now_ms: now,
        order_updates_user,
    };
    match parse_message(text, &ctx) {
        Ok(Incoming::Event(event)) => {
            if let Some(key) = event.subscription_key() {
                lock(inner).registry.touch(id, &key, now);
            }
            events.send(event).await.is_ok()
        }
        Ok(Incoming::Ack { key, subscribed }) => {
            if let (Some(key), true) = (key, subscribed) {
                lock(inner).registry.ack(id, &key);
            }
            true
        }
        Ok(Incoming::Pong) => true,
        Ok(Incoming::Ignored { channel }) => {
            tracing::trace!(connection = %id, channel, "ignored ws channel");
            true
        }
        Ok(Incoming::VenueError(message)) => {
            tracing::warn!(connection = %id, message, "venue ws error");
            events
                .send(WsEvent::VenueError {
                    connection: id,
                    message,
                })
                .await
                .is_ok()
        }
        Err(e) => {
            let channel = match &e {
                PoolError::Parse { channel, .. } => channel.clone(),
                _ => "orderUpdates".to_owned(),
            };
            tracing::warn!(connection = %id, error = %e, "dropped ws message");
            events
                .send(WsEvent::MessageDropped {
                    connection: id,
                    channel,
                    reason: e.to_string(),
                })
                .await
                .is_ok()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromStr;

    /// Frames captured verbatim from `wss://api.hyperliquid.xyz/ws` on
    /// 2026-09-03. Checked in so parsing is pinned without a socket.
    mod fixtures {
        pub const BBO: &str = r#"{"channel":"bbo","data":{"coin":"BTC","time":1788490065008,"bbo":[{"px":"80639.0","sz":"0.00186","n":2},{"px":"80640.0","sz":"13.74589","n":46}]}}"#;
        pub const BBO_EMPTY_SIDE: &str =
            r#"{"channel":"bbo","data":{"coin":"MATIC","time":1788490065008,"bbo":[null,null]}}"#;
        pub const ACTIVE_ASSET_CTX: &str = r#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"funding":"0.0000052974","openInterest":"36295.80812","prevDayPx":"77759.0","dayNtlVlm":"4490636699.3287734985","premium":"-0.0005540084","oraclePx":"80684.7","markPx":"80640.0","midPx":"80639.5","impactPxs":["80637.2","80640.0"],"dayBaseVlm":"56265.69068"}}}"#;
        /// A book-less asset: `premium`, `midPx` and `impactPxs` are co-null
        /// (fair-value.md §14.4 correction 2).
        pub const ACTIVE_ASSET_CTX_NULLS: &str = r#"{"channel":"activeAssetCtx","data":{"coin":"MATIC","ctx":{"funding":"0.0","openInterest":"0.0","prevDayPx":"0.37621","dayNtlVlm":"0.0","premium":null,"oraclePx":"0.3754","markPx":"0.37621","midPx":null,"impactPxs":null,"dayBaseVlm":"0.0"}}}"#;
        pub const TRADES: &str = r#"{"channel":"trades","data":[{"coin":"BTC","side":"A","px":"80639.0","sz":"0.00022","time":1788490042052,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000","tid":1095884267920331,"users":["0xa62b923a112d50d03e1e096bbd53422490dac104","0x8a544ab75107efd541c9072a0efebb87cc4292f3"]},{"coin":"BTC","side":"B","px":"80640.0","sz":"0.00404","time":1788490042052,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000","tid":159797238452866,"users":["0xe86351e0f69bd808def36e7bb2f9107443838bb8","0x339030695894d065719b8ee501459a467ef1388a"]}]}"#;
        pub const CANDLE: &str = r#"{"channel":"candle","data":{"t":1788490020000,"T":1788490079999,"s":"BTC","i":"1m","o":"80640.0","c":"80640.0","h":"80641.0","l":"80639.0","v":"2.20493","n":61}}"#;
        pub const L2BOOK_EMPTY: &str =
            r#"{"channel":"l2Book","data":{"coin":"MATIC","time":1788490374938,"levels":[[],[]]}}"#;
        pub const USER_FILLS_SNAPSHOT: &str = r#"{"channel":"userFills","data":{"isSnapshot":true,"user":"0x31ca8395cf837de08b24da3f660e77761dfb974b","fills":[{"coin":"USUAL","px":"0.01152","sz":"1052.9","side":"A","time":1788490193264,"startPosition":"-3695042.8999999999","dir":"Open Short","closedPnl":"0.0","hash":"0xf5be271fb859cdd4f7370443a320840104010105535ceca79986d272775da7bf","oid":535618784464,"crossed":true,"fee":"0.0","tid":691079299625582,"feeToken":"USDC","twapId":null}]}}"#;
        pub const ACK_BBO: &str = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#;
        /// The venue echoes `userFills` with its own default filled in.
        pub const ACK_USER_FILLS: &str = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"userFills","user":"0x31ca8395cf837de08b24da3f660e77761dfb974b","aggregateByTime":false}}}"#;
        pub const ACK_UNSUB: &str = r#"{"channel":"subscriptionResponse","data":{"method":"unsubscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#;
        pub const PONG: &str = r#"{"channel":"pong"}"#;
        pub const VENUE_ERROR: &str = r#"{"channel":"error","data":"Already unsubscribed: {\"type\":\"trades\",\"coin\":\"ETH\"}"}"#;
        /// Shape from the venue docs. Not observed live: reproducing one needs
        /// an address that places orders while the probe is attached.
        pub const ORDER_UPDATES: &str = r#"{"channel":"orderUpdates","data":[{"order":{"coin":"BTC","side":"B","limitPx":"80000.0","sz":"0.001","oid":123456789,"timestamp":1788490065008,"origSz":"0.001","cloid":"0x1234567890abcdef1234567890abcdef"},"status":"open","statusTimestamp":1788490065010}]}"#;
    }

    fn addr(s: &str) -> Address {
        Address::parse(s).expect("test address")
    }

    fn ctx() -> ParseContext {
        ParseContext {
            now_ms: 1_700_000_000_000,
            order_updates_user: None,
        }
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("test decimal")
    }

    // -- subscription identity ------------------------------------------------

    #[test]
    fn subscription_wire_form_matches_the_venue() {
        let cases = [
            (
                Subscription::ActiveAssetCtx { coin: "BTC".into() },
                r#"{"type":"activeAssetCtx","coin":"BTC"}"#,
            ),
            (
                Subscription::Bbo { coin: "BTC".into() },
                r#"{"type":"bbo","coin":"BTC"}"#,
            ),
            (
                Subscription::Trades { coin: "ETH".into() },
                r#"{"type":"trades","coin":"ETH"}"#,
            ),
            (
                Subscription::Candle {
                    coin: "BTC".into(),
                    interval: "1m".into(),
                },
                r#"{"type":"candle","coin":"BTC","interval":"1m"}"#,
            ),
            (
                Subscription::L2Book { coin: "BTC".into() },
                r#"{"type":"l2Book","coin":"BTC"}"#,
            ),
            (
                Subscription::UserFills {
                    user: addr("0x0000000000000000000000000000000000000001"),
                },
                r#"{"type":"userFills","user":"0x0000000000000000000000000000000000000001"}"#,
            ),
            (
                Subscription::OrderUpdates {
                    user: addr("0x0000000000000000000000000000000000000001"),
                },
                r#"{"type":"orderUpdates","user":"0x0000000000000000000000000000000000000001"}"#,
            ),
        ];
        for (sub, expected) in cases {
            assert_eq!(serde_json::to_string(&sub).expect("serialize"), expected);
        }
    }

    #[test]
    fn subscription_keys_are_distinct_and_ordered() {
        let mut subs = [
            Subscription::Trades { coin: "BTC".into() },
            Subscription::Bbo { coin: "BTC".into() },
            Subscription::Candle {
                coin: "BTC".into(),
                interval: "1m".into(),
            },
            Subscription::Candle {
                coin: "BTC".into(),
                interval: "5m".into(),
            },
        ];
        subs.sort();
        let keys: Vec<String> = subs.iter().map(|s| s.key()).collect();
        assert_eq!(
            keys,
            vec!["bbo:BTC", "candle:BTC:1m", "candle:BTC:5m", "trades:BTC"]
        );
    }

    #[test]
    fn staleness_thresholds_follow_fair_value_5_2() {
        let t = StalenessThresholds::default();
        assert_eq!(
            Subscription::Bbo { coin: "BTC".into() }.staleness_threshold(&t),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            Subscription::L2Book { coin: "BTC".into() }.staleness_threshold(&t),
            Some(Duration::from_secs(2))
        );
        // Carries both a mark leg (5 s) and an oracle leg (10 s); the tighter binds.
        assert_eq!(
            Subscription::ActiveAssetCtx { coin: "BTC".into() }.staleness_threshold(&t),
            Some(Duration::from_secs(5))
        );
        // Event-driven: silence is information, not a fault.
        assert_eq!(
            Subscription::Trades { coin: "BTC".into() }.staleness_threshold(&t),
            None
        );
        assert_eq!(
            Subscription::UserFills {
                user: Address::ZERO
            }
            .staleness_threshold(&t),
            None
        );
    }

    // -- backoff --------------------------------------------------------------

    #[test]
    fn backoff_doubles_then_caps() {
        let b = Backoff {
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            jitter_pct: 20,
        };
        let ms: Vec<u128> = (0..10).map(|n| b.nominal(n).as_millis()).collect();
        assert_eq!(
            ms,
            vec![
                500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000, 30000
            ]
        );
    }

    #[test]
    fn backoff_saturates_instead_of_overflowing() {
        let b = Backoff {
            base: Duration::from_millis(u64::MAX / 2),
            max: Duration::from_secs(30),
            jitter_pct: 20,
        };
        assert_eq!(b.nominal(u32::MAX), Duration::from_secs(30));
        assert_eq!(b.nominal(64), Duration::from_secs(30));
    }

    #[test]
    fn jittered_delay_stays_inside_the_band() {
        let b = Backoff::default();
        let mut j = Jitter::from_seed(0xC0FF_EE00_1234_5678);
        let mut saw_below = false;
        let mut saw_above = false;
        for attempt in 0..8u32 {
            let nominal = b.nominal(attempt).as_millis() as u64;
            let low = nominal * 80 / 100;
            let high = nominal * 120 / 100;
            for _ in 0..500 {
                let d = b.delay(attempt, &mut j).as_millis() as u64;
                assert!(d >= low, "{d} < {low} at attempt {attempt}");
                assert!(d <= high, "{d} > {high} at attempt {attempt}");
                saw_below |= d < nominal;
                saw_above |= d > nominal;
            }
        }
        // A jitter that never moves is not jitter.
        assert!(saw_below && saw_above);
    }

    #[test]
    fn zero_jitter_is_exactly_nominal() {
        let b = Backoff {
            jitter_pct: 0,
            ..Backoff::default()
        };
        let mut j = Jitter::from_seed(7);
        assert_eq!(b.delay(3, &mut j), b.nominal(3));
    }

    #[test]
    fn seeded_jitter_is_reproducible() {
        let b = Backoff::default();
        let mut a = Jitter::from_seed(42);
        let mut c = Jitter::from_seed(42);
        let one: Vec<Duration> = (0..16).map(|n| b.delay(n % 5, &mut a)).collect();
        let two: Vec<Duration> = (0..16).map(|n| b.delay(n % 5, &mut c)).collect();
        assert_eq!(one, two);
        assert_ne!(Jitter::from_seed(42), Jitter::from_seed(43));
    }

    // -- registry -------------------------------------------------------------

    #[test]
    fn registry_fills_a_connection_before_opening_another() {
        let mut r = SubscriptionRegistry::new(3, 2);
        assert_eq!(
            r.place(Subscription::Bbo { coin: "BTC".into() }),
            Ok(Placement::NewConnection(ConnectionId(0)))
        );
        assert_eq!(
            r.place(Subscription::Bbo { coin: "ETH".into() }),
            Ok(Placement::Existing(ConnectionId(0)))
        );
        assert_eq!(
            r.place(Subscription::Bbo { coin: "SOL".into() }),
            Ok(Placement::NewConnection(ConnectionId(1)))
        );
        assert_eq!(r.len(), 3);
        assert_eq!(r.connection_count(), 2);
        assert_eq!(r.capacity(), 6);
    }

    #[test]
    fn registry_is_idempotent_on_a_repeat_subscribe() {
        let mut r = SubscriptionRegistry::new(2, 10);
        let sub = Subscription::Trades { coin: "BTC".into() };
        assert_eq!(
            r.place(sub.clone()),
            Ok(Placement::NewConnection(ConnectionId(0)))
        );
        assert_eq!(
            r.place(sub.clone()),
            Ok(Placement::AlreadyPresent(ConnectionId(0)))
        );
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn registry_refuses_past_the_documented_cap() {
        let mut r = SubscriptionRegistry::new(1, 2);
        assert!(r.place(Subscription::Bbo { coin: "A".into() }).is_ok());
        assert!(r.place(Subscription::Bbo { coin: "B".into() }).is_ok());
        assert_eq!(
            r.place(Subscription::Bbo { coin: "C".into() }),
            Err(PoolError::CapacityExhausted {
                used: 2,
                cap: 2,
                connections: 1
            })
        );
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn order_updates_never_share_a_connection() {
        let mut r = SubscriptionRegistry::new(4, 100);
        let a = addr("0x0000000000000000000000000000000000000001");
        let b = addr("0x0000000000000000000000000000000000000002");
        assert_eq!(
            r.place(Subscription::OrderUpdates { user: a }),
            Ok(Placement::NewConnection(ConnectionId(0)))
        );
        assert_eq!(
            r.place(Subscription::OrderUpdates { user: b }),
            Ok(Placement::NewConnection(ConnectionId(1)))
        );
        // userFills carries `user` in its payload, so it multiplexes freely.
        assert_eq!(
            r.place(Subscription::UserFills { user: a }),
            Ok(Placement::Existing(ConnectionId(0)))
        );
        assert_eq!(
            r.place(Subscription::UserFills { user: b }),
            Ok(Placement::Existing(ConnectionId(0)))
        );
        assert_eq!(r.order_updates_user(ConnectionId(0)), Some(a));
        assert_eq!(r.order_updates_user(ConnectionId(1)), Some(b));
    }

    #[test]
    fn registry_remove_reports_the_owning_connection() {
        let mut r = SubscriptionRegistry::new(2, 1);
        let first = Subscription::Bbo { coin: "BTC".into() };
        let second = Subscription::Bbo { coin: "ETH".into() };
        r.place(first.clone()).expect("place");
        r.place(second.clone()).expect("place");
        assert_eq!(r.remove(&second), Ok(ConnectionId(1)));
        assert_eq!(
            r.remove(&second),
            Err(PoolError::NotSubscribed("bbo:ETH".to_owned()))
        );
        assert_eq!(r.len(), 1);
        // The emptied connection is kept and reused.
        assert_eq!(r.connection_count(), 2);
        assert_eq!(
            r.place(Subscription::Bbo { coin: "SOL".into() }),
            Ok(Placement::Existing(ConnectionId(1)))
        );
    }

    #[test]
    fn subscriptions_and_health_are_listed_in_key_order() {
        let mut r = SubscriptionRegistry::new(1, 100);
        for coin in ["SOL", "BTC", "ETH"] {
            r.place(Subscription::Bbo { coin: coin.into() })
                .expect("place");
            r.place(Subscription::Trades { coin: coin.into() })
                .expect("place");
        }
        let keys: Vec<String> = r
            .subscriptions(ConnectionId(0))
            .iter()
            .map(|s| s.key())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let health_keys: Vec<String> = r
            .health(0, &StalenessThresholds::default())
            .iter()
            .map(|h| h.subscription.key())
            .collect();
        assert_eq!(health_keys, sorted);
    }

    #[test]
    fn staleness_fails_closed() {
        let t = StalenessThresholds::default();
        let mut r = SubscriptionRegistry::new(1, 10);
        let bbo = Subscription::Bbo { coin: "BTC".into() };
        let trades = Subscription::Trades { coin: "BTC".into() };
        r.place(bbo.clone()).expect("place");
        r.place(trades.clone()).expect("place");

        // Never connected, never a message: stale on the budgeted feed only.
        let h = r.health(10_000, &t);
        let bbo_h = h.iter().find(|x| x.subscription == bbo).expect("bbo");
        assert!(bbo_h.stale && !bbo_h.connected && bbo_h.age_ms.is_none());
        let trades_h = h.iter().find(|x| x.subscription == trades).expect("trades");
        assert!(!trades_h.stale, "an event-driven feed is never stale");

        r.set_connected(ConnectionId(0), true, 10_000);
        r.touch(ConnectionId(0), &bbo.key(), 10_000);
        r.ack(ConnectionId(0), &bbo.key());

        // 1.5 s of silence is inside the 2 s book budget.
        let fresh = &r.health(11_500, &t)[0];
        assert_eq!(fresh.subscription, bbo);
        assert!(!fresh.stale);
        assert_eq!(fresh.age_ms, Some(1_500));
        assert_eq!(fresh.threshold_ms, Some(2_000));
        assert!(fresh.acked);

        // 2.001 s is not.
        assert!(r.health(12_001, &t)[0].stale);

        // A drop is stale immediately, whatever the age says, and clears acks.
        r.touch(ConnectionId(0), &bbo.key(), 12_000);
        r.set_connected(ConnectionId(0), false, 12_000);
        let dropped = &r.health(12_100, &t)[0];
        assert!(dropped.stale && !dropped.acked);
        assert_eq!(dropped.age_ms, Some(100));
    }

    #[test]
    fn a_reconnect_clears_acks_and_seeds_the_connection_clock() {
        let mut r = SubscriptionRegistry::new(1, 10);
        let bbo = Subscription::Bbo { coin: "BTC".into() };
        r.place(bbo.clone()).expect("place");
        r.set_connected(ConnectionId(0), true, 1_000);
        r.ack(ConnectionId(0), &bbo.key());
        r.touch_connection(ConnectionId(0), 5_000);
        assert_eq!(r.unacked(ConnectionId(0)), vec![]);
        assert_eq!(r.connection_last_message(ConnectionId(0)), Some(5_000));

        r.set_connected(ConnectionId(0), false, 5_100);
        // The pre-drop last-message time is what the gap window anchors on.
        assert_eq!(r.connection_last_message(ConnectionId(0)), Some(5_000));
        assert_eq!(r.unacked(ConnectionId(0)), vec![bbo]);

        r.set_connected(ConnectionId(0), true, 9_000);
        assert_eq!(r.connection_last_message(ConnectionId(0)), Some(9_000));
    }

    #[test]
    fn a_repeatedly_unacked_subscription_is_quarantined_and_stops_being_resent() {
        let mut r = SubscriptionRegistry::new(1, 10);
        let good = Subscription::Bbo { coin: "BTC".into() };
        // Sorts after "bbo:BTC", i.e. it is sent second and is the first
        // unacked one when the venue closes on it.
        let poison = Subscription::Bbo {
            coin: "NOTACOIN".into(),
        };
        r.place(good.clone()).expect("place");
        r.place(poison.clone()).expect("place");

        for strike in 1..3 {
            r.set_connected(ConnectionId(0), true, strike * 1_000);
            r.ack(ConnectionId(0), &good.key());
            assert_eq!(
                r.record_failed_session(ConnectionId(0), 3),
                None,
                "strike {strike} must not quarantine yet"
            );
            r.set_connected(ConnectionId(0), false, strike * 1_000);
            assert_eq!(r.resubscribe_set(ConnectionId(0)).len(), 2);
        }

        r.set_connected(ConnectionId(0), true, 3_000);
        r.ack(ConnectionId(0), &good.key());
        assert_eq!(
            r.record_failed_session(ConnectionId(0), 3),
            Some((poison.clone(), 3))
        );
        // The poison is dropped from the resubscribe set; the healthy feed
        // beside it is untouched, so the shard converges instead of looping.
        assert_eq!(r.resubscribe_set(ConnectionId(0)), vec![good.clone()]);
        assert_eq!(r.subscriptions(ConnectionId(0)), vec![good, poison.clone()]);
        let health = r.health(3_000, &StalenessThresholds::default());
        let poisoned = health
            .iter()
            .find(|h| h.subscription == poison)
            .expect("poison health");
        assert!(poisoned.quarantined && poisoned.stale);
    }

    #[test]
    fn an_acknowledged_subscription_never_accumulates_strikes() {
        let mut r = SubscriptionRegistry::new(1, 10);
        let sub = Subscription::Bbo { coin: "BTC".into() };
        r.place(sub.clone()).expect("place");
        for round in 0..10 {
            r.set_connected(ConnectionId(0), true, round * 1_000);
            r.ack(ConnectionId(0), &sub.key());
            assert_eq!(r.record_failed_session(ConnectionId(0), 3), None);
            r.set_connected(ConnectionId(0), false, round * 1_000);
        }
        assert_eq!(r.resubscribe_set(ConnectionId(0)), vec![sub]);
    }

    #[test]
    fn gap_window_saturates_on_a_backwards_clock() {
        assert_eq!(
            GapWindow {
                start_ms: 100,
                end_ms: 400
            }
            .duration_ms(),
            300
        );
        assert_eq!(
            GapWindow {
                start_ms: 400,
                end_ms: 100
            }
            .duration_ms(),
            0
        );
    }

    // -- parsing --------------------------------------------------------------

    #[test]
    fn parses_bbo_with_both_sides() {
        let Ok(Incoming::Event(event)) = parse_message(fixtures::BBO, &ctx()) else {
            panic!("expected a bbo event");
        };
        let WsEvent::Bbo {
            coin,
            venue_time_ms,
            bid,
            ask,
        } = &event
        else {
            panic!("expected WsEvent::Bbo");
        };
        assert_eq!(coin, "BTC");
        assert_eq!(*venue_time_ms, 1_788_490_065_008);
        let bid = bid.as_ref().expect("bid");
        let ask = ask.as_ref().expect("ask");
        assert_eq!(bid.px, dec("80639.0"));
        assert_eq!(ask.px, dec("80640.0"));
        assert_eq!(ask.n, 46);
        assert!(bid.px < ask.px);
        assert_eq!(event.subscription_key().as_deref(), Some("bbo:BTC"));
        assert_eq!(
            event.event_time(),
            EventTime::Venue(1_788_490_065_008),
            "bbo is venue-stamped"
        );
    }

    #[test]
    fn an_empty_book_side_is_none_not_zero() {
        let Ok(Incoming::Event(WsEvent::Bbo { bid, ask, .. })) =
            parse_message(fixtures::BBO_EMPTY_SIDE, &ctx())
        else {
            panic!("expected a bbo event");
        };
        assert!(bid.is_none() && ask.is_none());
    }

    #[test]
    fn active_asset_ctx_is_nested_and_locally_stamped() {
        let Ok(Incoming::Event(event)) = parse_message(fixtures::ACTIVE_ASSET_CTX, &ctx()) else {
            panic!("expected an activeAssetCtx event");
        };
        let WsEvent::ActiveAssetCtx {
            coin,
            ctx: asset_ctx,
            received_at_ms,
        } = &event
        else {
            panic!("expected WsEvent::ActiveAssetCtx");
        };
        assert_eq!(coin, "BTC");
        assert_eq!(asset_ctx.mark_px, dec("80640.0"));
        assert_eq!(asset_ctx.oracle_px, dec("80684.7"));
        assert_eq!(asset_ctx.premium, Some(dec("-0.0005540084")));
        assert_eq!(*received_at_ms, 1_700_000_000_000);
        // The venue sends no timestamp on this channel (fair-value.md §14.1),
        // and the event type has to say so.
        assert_eq!(
            event.event_time(),
            EventTime::LocalArrival(1_700_000_000_000)
        );
    }

    #[test]
    fn a_bookless_asset_ctx_deserializes_with_co_null_fields() {
        let Ok(Incoming::Event(WsEvent::ActiveAssetCtx { ctx: c, .. })) =
            parse_message(fixtures::ACTIVE_ASSET_CTX_NULLS, &ctx())
        else {
            panic!("expected an activeAssetCtx event");
        };
        assert!(c.premium.is_none() && c.mid_px.is_none() && c.impact_pxs.is_none());
        assert_eq!(c.open_interest, Decimal::ZERO);
        assert_eq!(c.mark_px, dec("0.37621"));
    }

    #[test]
    fn parses_trades_and_names_the_coin() {
        let Ok(Incoming::Event(event)) = parse_message(fixtures::TRADES, &ctx()) else {
            panic!("expected a trades event");
        };
        let WsEvent::Trades { coin, trades } = &event else {
            panic!("expected WsEvent::Trades");
        };
        assert_eq!(coin, "BTC");
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].side, Side::A);
        assert_eq!(trades[1].px, dec("80640.0"));
        assert_eq!(trades[0].users.len(), 2);
        assert_eq!(event.subscription_key().as_deref(), Some("trades:BTC"));
        assert_eq!(event.event_time(), EventTime::Venue(1_788_490_042_052));
    }

    #[test]
    fn an_empty_trades_frame_is_ignored_not_emitted() {
        assert_eq!(
            parse_message(r#"{"channel":"trades","data":[]}"#, &ctx()),
            Ok(Incoming::Ignored {
                channel: "trades".to_owned()
            })
        );
    }

    #[test]
    fn parses_candle_and_keys_it_by_coin_and_interval() {
        let Ok(Incoming::Event(event)) = parse_message(fixtures::CANDLE, &ctx()) else {
            panic!("expected a candle event");
        };
        let WsEvent::Candle(c) = &event else {
            panic!("expected WsEvent::Candle");
        };
        assert_eq!(c.s, "BTC");
        assert_eq!(c.i, "1m");
        assert_eq!(c.h, dec("80641.0"));
        assert_eq!(event.subscription_key().as_deref(), Some("candle:BTC:1m"));
        // `t`/`T` are bar boundaries, not a sample instant.
        assert_eq!(event.event_time(), EventTime::None);
    }

    #[test]
    fn parses_an_empty_l2_book() {
        let Ok(Incoming::Event(event)) = parse_message(fixtures::L2BOOK_EMPTY, &ctx()) else {
            panic!("expected an l2Book event");
        };
        let WsEvent::L2Book(book) = &event else {
            panic!("expected WsEvent::L2Book");
        };
        assert!(book.bids().is_empty() && book.asks().is_empty());
        assert_eq!(event.subscription_key().as_deref(), Some("l2Book:MATIC"));
    }

    #[test]
    fn parses_a_user_fills_snapshot() {
        let Ok(Incoming::Event(event)) = parse_message(fixtures::USER_FILLS_SNAPSHOT, &ctx())
        else {
            panic!("expected a userFills event");
        };
        let WsEvent::UserFills {
            user,
            is_snapshot,
            fills,
        } = &event
        else {
            panic!("expected WsEvent::UserFills");
        };
        assert!(*is_snapshot, "the venue backfills on subscribe");
        assert_eq!(*user, addr("0x31ca8395cf837de08b24da3f660e77761dfb974b"));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].coin, "USUAL");
        assert_eq!(fills[0].tid, 691_079_299_625_582);
        assert_eq!(
            event.subscription_key().as_deref(),
            Some("userFills:0x31ca8395cf837de08b24da3f660e77761dfb974b")
        );
    }

    #[test]
    fn order_updates_are_attributed_by_the_owning_connection() {
        let user = addr("0x0000000000000000000000000000000000000009");
        let with_user = ParseContext {
            order_updates_user: Some(user),
            ..ctx()
        };
        let Ok(Incoming::Event(event)) = parse_message(fixtures::ORDER_UPDATES, &with_user) else {
            panic!("expected an orderUpdates event");
        };
        let WsEvent::OrderUpdates { user: got, updates } = &event else {
            panic!("expected WsEvent::OrderUpdates");
        };
        assert_eq!(*got, user);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, "open");
        assert_eq!(updates[0].order.oid, 123_456_789);
        assert_eq!(
            event.subscription_key().as_deref(),
            Some("orderUpdates:0x0000000000000000000000000000000000000009")
        );
    }

    #[test]
    fn order_updates_without_an_owner_are_refused_not_guessed() {
        assert_eq!(
            parse_message(fixtures::ORDER_UPDATES, &ctx()),
            Err(PoolError::UnattributableOrderUpdates)
        );
    }

    #[test]
    fn parses_acks_pongs_and_venue_errors() {
        assert_eq!(
            parse_message(fixtures::ACK_BBO, &ctx()),
            Ok(Incoming::Ack {
                key: Some("bbo:BTC".to_owned()),
                subscribed: true
            })
        );
        // The echo carries `aggregateByTime`, which must not defeat the match.
        assert_eq!(
            parse_message(fixtures::ACK_USER_FILLS, &ctx()),
            Ok(Incoming::Ack {
                key: Some("userFills:0x31ca8395cf837de08b24da3f660e77761dfb974b".to_owned()),
                subscribed: true
            })
        );
        assert_eq!(
            parse_message(fixtures::ACK_UNSUB, &ctx()),
            Ok(Incoming::Ack {
                key: Some("bbo:BTC".to_owned()),
                subscribed: false
            })
        );
        assert_eq!(parse_message(fixtures::PONG, &ctx()), Ok(Incoming::Pong));
        let Ok(Incoming::VenueError(message)) = parse_message(fixtures::VENUE_ERROR, &ctx()) else {
            panic!("expected a venue error");
        };
        assert!(message.starts_with("Already unsubscribed"));
    }

    #[test]
    fn an_unmodelled_ack_shape_is_still_an_ack() {
        let raw = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"webData2","user":"0x0000000000000000000000000000000000000001"}}}"#;
        assert_eq!(
            parse_message(raw, &ctx()),
            Ok(Incoming::Ack {
                key: None,
                subscribed: true
            })
        );
    }

    #[test]
    fn an_unknown_channel_is_ignored_not_an_error() {
        assert_eq!(
            parse_message(r#"{"channel":"webData2","data":{}}"#, &ctx()),
            Ok(Incoming::Ignored {
                channel: "webData2".to_owned()
            })
        );
    }

    #[test]
    fn a_malformed_payload_is_a_typed_parse_error() {
        let raw = r#"{"channel":"bbo","data":{"coin":"BTC"}}"#;
        let Err(PoolError::Parse { channel, .. }) = parse_message(raw, &ctx()) else {
            panic!("expected a parse error naming the channel");
        };
        assert_eq!(channel, "bbo");
        assert!(matches!(
            parse_message("not json", &ctx()),
            Err(PoolError::Parse { .. })
        ));
    }

    // -- pool wiring ----------------------------------------------------------

    #[tokio::test]
    async fn pool_tracks_capacity_and_refuses_over_it() {
        let cfg = WsPoolConfig {
            // No socket is opened for a subscription the registry refuses, and
            // these coins never reach one: the placement decision is local.
            max_connections: 1,
            max_subs_per_connection: 2,
            ..WsPoolConfig::new(Network::Testnet)
        };
        let (pool, _rx) = WsPool::new(cfg);
        assert_eq!(pool.capacity(), 2);
        assert_eq!(pool.subscription_count(), 0);
        pool.subscribe(Subscription::Bbo { coin: "BTC".into() })
            .expect("first");
        pool.subscribe(Subscription::Bbo { coin: "BTC".into() })
            .expect("repeat is a no-op");
        assert_eq!(pool.subscription_count(), 1);
        pool.subscribe(Subscription::Trades { coin: "BTC".into() })
            .expect("second");
        assert_eq!(
            pool.subscribe(Subscription::Trades { coin: "ETH".into() }),
            Err(PoolError::CapacityExhausted {
                used: 2,
                cap: 2,
                connections: 1
            })
        );
        pool.shutdown();
        assert_eq!(
            pool.subscribe(Subscription::Bbo { coin: "SOL".into() }),
            Err(PoolError::Shutdown)
        );
    }

    #[tokio::test]
    async fn unsubscribing_something_absent_is_typed() {
        let (pool, _rx) = WsPool::new(WsPoolConfig::new(Network::Testnet));
        assert_eq!(
            pool.unsubscribe(&Subscription::Bbo { coin: "BTC".into() }),
            Err(PoolError::NotSubscribed("bbo:BTC".to_owned()))
        );
        pool.shutdown();
    }

    // -- live -----------------------------------------------------------------

    /// Live mainnet check of the three channels the fair value engine depends
    /// on. Public info feeds: no key, no signature, read-only — which is what
    /// `docs/specs/fair-value.md` §14.2 explicitly allows against mainnet
    /// while trading stays testnet-default (D4).
    ///
    /// Ignored so CI stays hermetic:
    /// `cargo test -p oppen-hl ws::tests::live_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "hits the public mainnet websocket"]
    async fn live_mainnet_bbo_trades_and_ctx() {
        let cfg = WsPoolConfig::new(Network::Mainnet);
        let (pool, mut rx) = WsPool::new(cfg);
        for sub in [
            Subscription::Bbo { coin: "BTC".into() },
            Subscription::Trades { coin: "BTC".into() },
            Subscription::ActiveAssetCtx { coin: "BTC".into() },
            Subscription::L2Book { coin: "BTC".into() },
        ] {
            pool.subscribe(sub).expect("subscribe");
        }

        let mut arrivals: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(Some(event)) = tokio::time::timeout(remaining, rx.recv()).await else {
                break;
            };
            match &event {
                WsEvent::Bbo { bid, ask, .. } => {
                    if let (Some(b), Some(a)) = (bid, ask) {
                        assert!(b.px < a.px, "crossed bbo: {} >= {}", b.px, a.px);
                        assert!(b.sz > Decimal::ZERO && a.sz > Decimal::ZERO);
                    }
                }
                WsEvent::Trades { trades, .. } => {
                    assert!(!trades.is_empty());
                    assert!(trades.iter().all(|t| t.px > Decimal::ZERO && t.time > 0));
                }
                WsEvent::ActiveAssetCtx { ctx, .. } => {
                    assert!(ctx.mark_px > Decimal::ZERO && ctx.oracle_px > Decimal::ZERO);
                    assert!(
                        matches!(event.event_time(), EventTime::LocalArrival(_)),
                        "activeAssetCtx carries no venue timestamp and must be stamped locally"
                    );
                }
                WsEvent::L2Book(book) => {
                    assert!(!book.bids().is_empty() && !book.asks().is_empty());
                }
                WsEvent::Disconnected(d) => panic!("unexpected disconnect: {}", d.reason),
                WsEvent::MessageDropped { reason, .. } => panic!("unparsed frame: {reason}"),
                _ => {}
            }
            if let Some(key) = event.subscription_key() {
                arrivals.entry(key).or_default().push(now_ms());
            }
        }

        for channel in ["bbo:BTC", "trades:BTC", "activeAssetCtx:BTC", "l2Book:BTC"] {
            let stamps = arrivals.get(channel).map(Vec::as_slice).unwrap_or(&[]);
            let mut gaps: Vec<u64> = stamps.windows(2).map(|w| w[1] - w[0]).collect();
            gaps.sort_unstable();
            let median = gaps.get(gaps.len() / 2).copied().unwrap_or(0);
            println!("{channel}: n={} median_gap_ms={median}", stamps.len());
            assert!(!stamps.is_empty(), "{channel} delivered nothing in 45 s");
        }

        // The audit's central channel finding, re-measured: bbo is an order of
        // magnitude fresher than l2Book (fair-value.md §14.4 correction 4).
        let health = pool.health();
        println!("--- feed health ---");
        for h in &health {
            println!(
                "{} {} connected={} acked={} age_ms={:?} threshold_ms={:?} stale={}",
                h.connection,
                h.subscription,
                h.connected,
                h.acked,
                h.age_ms,
                h.threshold_ms,
                h.stale
            );
        }
        let bbo = health
            .iter()
            .find(|h| h.subscription == Subscription::Bbo { coin: "BTC".into() })
            .expect("bbo health");
        assert!(!bbo.stale, "bbo should sit well inside the 2 s book budget");
        assert!(bbo.acked);
        assert_eq!(pool.connection_count(), 1);
        assert_eq!(pool.subscription_count(), 4);

        pool.shutdown();
    }

    /// Reconnect against the live venue, driven by the venue itself: a
    /// subscription naming an unknown coin closes the socket with no close
    /// frame (measured 2026-09-03), which is the cleanest way to prove the
    /// re-subscribe, the gap window and the quarantine convergence without a
    /// proxy.
    #[tokio::test]
    #[ignore = "hits the public mainnet websocket"]
    async fn live_mainnet_reconnect_restores_subscriptions_and_reports_the_gap() {
        let cfg = WsPoolConfig {
            backoff: Backoff {
                base: Duration::from_millis(200),
                max: Duration::from_secs(2),
                jitter_pct: 20,
            },
            ..WsPoolConfig::new(Network::Mainnet)
        };
        let (pool, mut rx) = WsPool::new(cfg);
        pool.subscribe(Subscription::Bbo { coin: "BTC".into() })
            .expect("subscribe");

        // Wait for the first real tick so the gap window has an anchor.
        let deadline = Duration::from_secs(20);
        let mut ticked = false;
        while !ticked {
            let Ok(Some(event)) = tokio::time::timeout(deadline, rx.recv()).await else {
                panic!("no bbo within 20 s");
            };
            ticked = matches!(event, WsEvent::Bbo { .. });
        }

        // The poison. It rides the same connection and kills it.
        pool.subscribe(Subscription::Bbo {
            coin: "NOTACOIN".into(),
        })
        .expect("subscribe");

        let mut saw_disconnect = None;
        let mut saw_reconnect = None;
        let mut saw_quarantine = None;
        let mut ticks_after_quarantine = 0usize;
        let overall = tokio::time::Instant::now() + Duration::from_secs(90);
        while ticks_after_quarantine < 5 {
            let remaining = overall.saturating_duration_since(tokio::time::Instant::now());
            let Ok(Some(event)) = tokio::time::timeout(remaining, rx.recv()).await else {
                break;
            };
            match event {
                WsEvent::Disconnected(d) => {
                    println!(
                        "disconnected {}: {} unacked={:?}",
                        d.connection,
                        d.reason,
                        d.unacked.iter().map(|s| s.key()).collect::<Vec<_>>()
                    );
                    saw_disconnect.get_or_insert(d);
                }
                WsEvent::Reconnected(r) => {
                    println!(
                        "reconnected {} after {} attempts, gap {} ms, resubscribed {:?}",
                        r.connection,
                        r.attempts,
                        r.gap.duration_ms(),
                        r.resubscribed.iter().map(|s| s.key()).collect::<Vec<_>>()
                    );
                    saw_reconnect.get_or_insert(r);
                }
                WsEvent::SubscriptionQuarantined {
                    subscription,
                    strikes,
                    ..
                } => {
                    println!("quarantined {subscription} after {strikes} strikes");
                    saw_quarantine = Some(subscription);
                }
                WsEvent::Bbo { .. } if saw_quarantine.is_some() => ticks_after_quarantine += 1,
                _ => {}
            }
        }

        let disconnect = saw_disconnect.expect("a disconnect");
        assert!(
            disconnect.unacked.iter().any(|s| matches!(
                s,
                Subscription::Bbo { coin } if coin == "NOTACOIN"
            )),
            "the unacked list should name the poison"
        );
        let reconnect = saw_reconnect.expect("a reconnect");
        assert!(reconnect.gap.end_ms >= reconnect.gap.start_ms);
        assert!(
            reconnect.gap.duration_ms() > 0,
            "a real outage has a non-zero gap window"
        );
        assert!(
            reconnect
                .resubscribed
                .contains(&Subscription::Bbo { coin: "BTC".into() }),
            "the good subscription must be restored"
        );
        assert_eq!(
            saw_quarantine,
            Some(Subscription::Bbo {
                coin: "NOTACOIN".into()
            }),
            "the poison must be quarantined so the shard converges"
        );
        assert_eq!(
            ticks_after_quarantine, 5,
            "BTC must keep ticking once the poison is out"
        );
        pool.shutdown();
    }
}
