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
//! | `l2Book` | 5451 ms | Depth-on-demand. Budgeted at 15 s, not §5.2's 2 s. |
//!
//! So: `bbo` is the microprice source and `l2Book` is depth-on-demand
//! (fair-value.md §14.4 correction 4). §5.2's 2 s figure is the budget for the
//! book *component*, which correction 4 moved to `bbo`; charging it to a 5.4 s
//! depth channel would mark over half of all samples stale by arithmetic, so
//! depth has its own budget ([`StalenessThresholds::depth`]).
//!
//! # The pre-sign gate is per-feed and fails closed
//!
//! Spec item 34 requires execution tools to fail closed during a disconnect.
//! Two rules follow, and both are load-bearing:
//!
//! - **Fail closed on every channel.** A feed whose socket is down, whose
//!   subscription the venue has not acknowledged, or that has been quarantined
//!   reads stale *whether or not its channel carries a silence budget*. The
//!   user channels (`userFills`, `orderUpdates`) carry none, and they are
//!   exactly the ones spec item 9 says must never silently drop a fill.
//! - **Name what you need.** [`WsPool::stale_feeds`] takes the feeds a decision
//!   actually depended on, and a feed the pool has never been asked to carry
//!   blocks too. [`WsPool::any_stale`] is a console summary, not the signing
//!   gate: an unrelated depth ladder must not block an order priced off `bbo`.
//!
//! `activeAssetCtx` and `fastAssetCtxs` carry no venue timestamp while
//! `l2Book`, `trades` and `bbo` all do (§14.1). Locally-stamped samples cannot
//! be aligned against venue-stamped ones, so the distinction is carried in the
//! field names: [`WsEvent::ActiveAssetCtx`] stamps its own `received_at_ms`,
//! while [`WsEvent::Bbo`] carries the venue's `venue_time_ms`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::types::{AssetCtx, Candle, Fill, L2Book, Level, OrderStatusEntry, Side};
use crate::{Address, Network};

/// The venue's own keepalive. The server drops a connection that has been idle
/// for roughly 60 s (`docs/spec.md` item 9), and it answers this frame with
/// `{"channel":"pong"}` (verified live).
const PING_FRAME: &str = r#"{"method":"ping"}"#;

/// Depth of the consumer's event channel. Bounded on purpose: see [`WsEvent`].
const EVENT_BUFFER: usize = 4096;

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
    /// per IP, so this is a real ceiling, not a self-imposed one. `cap` is the
    /// effective one, i.e. already clamped to
    /// `MAX_SUBSCRIPTIONS_PER_IP`.
    #[error(
        "subscription capacity exhausted: {used} of {cap} slots across {connections} connections"
    )]
    CapacityExhausted {
        used: usize,
        cap: usize,
        connections: usize,
    },
    /// There is room in the pool, but not for *this* subscription: every open
    /// connection already owns one of a kind that may not share a socket
    /// (`orderUpdates`, see `Subscription::exclusive_per_connection`), and no
    /// further connection may be opened. Distinct from
    /// [`PoolError::CapacityExhausted`] because the operator's fix is
    /// different: raise `max_connections`, not the per-connection cap. D1 gives
    /// every agent its own sub-account, so this is the ceiling on the agent
    /// roster.
    #[error(
        "no connection may hold a second {kind} subscription and all {connections} connections already own one"
    )]
    ExclusiveSlotExhausted {
        kind: &'static str,
        connections: usize,
    },
    /// [`WsPool::new`] was called outside a tokio runtime. The pool spawns one
    /// task per connection, so it needs a runtime handle; failing here is
    /// typed and cheap, whereas failing at the first `subscribe` would be a
    /// panic on a synchronous caller's thread (`AGENTS.md`: no panic on any
    /// input path).
    #[error("websocket pool needs a tokio runtime handle; construct it from inside a runtime")]
    NoRuntime,
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
    /// A connection task panicked. Drain still waits for every other task.
    #[error("websocket connection task {connection} failed: {detail}")]
    ConnectionTask {
        connection: ConnectionId,
        detail: String,
    },
    /// A frame arrived on a known channel but did not deserialize. Surfaced
    /// rather than swallowed, because a silent drop on a data plane is
    /// indistinguishable from a quiet market.
    #[error("malformed `{channel}` payload: {detail}")]
    Parse { channel: String, detail: String },
    /// `orderUpdates` payloads carry no `user` field, so a connection can only
    /// attribute them by owning exactly one such subscription. See
    /// `Subscription::exclusive_per_connection`.
    #[error("orderUpdates arrived on a connection that owns no orderUpdates subscription")]
    UnattributableOrderUpdates,
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

    /// The venue's channel name, for error messages that have to name a kind
    /// rather than an instance.
    fn kind(&self) -> &'static str {
        match self {
            Subscription::ActiveAssetCtx { .. } => "activeAssetCtx",
            Subscription::Bbo { .. } => "bbo",
            Subscription::Trades { .. } => "trades",
            Subscription::Candle { .. } => "candle",
            Subscription::L2Book { .. } => "l2Book",
            Subscription::UserFills { .. } => "userFills",
            Subscription::OrderUpdates { .. } => "orderUpdates",
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
    fn exclusive_per_connection(&self) -> bool {
        matches!(self, Subscription::OrderUpdates { .. })
    }

    /// How long this feed may go silent before the console dims it and the
    /// guardrails fail closed (`docs/spec.md` item 34).
    ///
    /// Thresholds are `docs/specs/fair-value.md` §5.2's: book 2 s, mark 5 s,
    /// oracle 10 s. `activeAssetCtx` carries both a mark leg and an oracle leg,
    /// so the tighter of the two binds it. `l2Book` gets
    /// [`StalenessThresholds::depth`] rather than `book`, for the measured
    /// reason recorded there.
    ///
    /// `None` means silence is not a fault. Trades, candles, fills and order
    /// updates are event-driven: a quiet tape is information, not a broken
    /// socket, and a staleness alarm on it would be a false positive every time
    /// the market is calm. Their age is still reported so a caller can judge.
    /// It does **not** mean such a feed is never stale: a down, unacked or
    /// quarantined socket is stale on every channel — see
    /// `SubscriptionRegistry::health`.
    fn staleness_threshold(&self, thresholds: &StalenessThresholds) -> Option<Duration> {
        match self {
            Subscription::Bbo { .. } => Some(thresholds.book),
            Subscription::L2Book { .. } => Some(thresholds.depth),
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

/// Per-component silence budgets from `docs/specs/fair-value.md` §5.2, plus one
/// §5.2 does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StalenessThresholds {
    /// The book component, which §14.4 correction 4 sources from `bbo`
    /// (0.11 s median).
    pub book: Duration,
    /// Mark leg of `activeAssetCtx`.
    pub mark: Duration,
    /// Oracle leg of `activeAssetCtx`.
    pub oracle: Duration,
    /// `l2Book`, which is depth-on-demand rather than a book component.
    ///
    /// Not from §5.2. Applying §5.2's 2 s book budget to this channel is a
    /// category error: default `l2Book` pushes at a **5.4 s median gap**
    /// (measured 2026-09-03, and §14.4 correction 4 is why the book component
    /// moved off it), so a 2 s budget marks more than half of all samples
    /// stale by arithmetic — and, before [`WsPool::stale_feeds`] narrowed the
    /// gate, blocked every order the terminal would ever place. 15 s is the
    /// measured median with room for two consecutive missed pushes.
    pub depth: Duration,
}

impl Default for StalenessThresholds {
    fn default() -> Self {
        StalenessThresholds {
            book: Duration::from_secs(2),
            mark: Duration::from_secs(5),
            oracle: Duration::from_secs(10),
            depth: Duration::from_secs(15),
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Identifies one socket in the pool. Present on every control event so the
/// console can show which shard degraded (`docs/spec.md` item 34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ConnectionId(usize);

impl ConnectionId {
    /// Name a connection by index.
    ///
    /// The event structs are public data with public fields, so a consumer can
    /// build one — to replay a recorded stream, or to drive its own state
    /// machine in a test — and this is the one field it could not otherwise
    /// fill in.
    pub fn new(index: usize) -> Self {
        ConnectionId(index)
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ws{}", self.0)
    }
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
    /// Attempts since the last session that proved itself: failed connects,
    /// failed re-subscribes, and sessions that died inside the subscribe round
    /// trip. Zero for a clean reconnect after a healthy socket dropped. The
    /// same counter drives the backoff curve, so a flapping connection reports
    /// a climbing number rather than resetting to one each time.
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
    /// Live bar. `t`/`T` are bar boundaries, not a sample instant.
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
    /// `SubscriptionRegistry::record_failed_session`.
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
    fn subscription_key(&self) -> Option<String> {
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
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// What one inbound frame turned out to be.
#[derive(Debug, Clone, PartialEq)]
enum Incoming {
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
struct ParseContext {
    /// Arrival clock, ms. Passed in rather than read inside so the parser is
    /// pure and its `activeAssetCtx` stamping is testable.
    now_ms: u64,
    /// The single address whose `orderUpdates` this connection owns, if any.
    /// Required because the payload carries no `user`.
    order_updates_user: Option<Address>,
}

#[derive(Deserialize)]
struct Envelope {
    channel: String,
    #[serde(default)]
    data: serde_json::Value,
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
fn parse_message(raw: &str, ctx: &ParseContext) -> Result<Incoming, PoolError> {
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
            // `crate::types::Bbo`, not a second declaration of the same wire
            // shape: one decoder per venue payload, and this one already owns
            // the one-sided-book cases.
            let d: crate::types::Bbo = parse_field(channel, envelope.data)?;
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
    fn nominal(&self, attempt: u32) -> Duration {
        let base_ms = u64::try_from(self.base.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(base_ms.saturating_mul(1u64 << attempt.min(32))).min(self.max)
    }

    /// `Backoff::nominal` spread uniformly over `±jitter_pct%`, never
    /// negative. A zero band needs no special case: it draws from `0..1`.
    fn delay(&self, attempt: u32, jitter: &mut Jitter) -> Duration {
        let nominal_ms = u64::try_from(self.nominal(attempt).as_millis()).unwrap_or(u64::MAX);
        let spread = nominal_ms.saturating_mul(u64::from(self.jitter_pct.min(100))) / 100;
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
struct Jitter(u64);

impl Jitter {
    /// Seeded explicitly. Only zero is replaced — xorshift cannot escape it —
    /// so distinct seeds stay distinct streams.
    fn from_seed(seed: u64) -> Self {
        Jitter(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// Wall clock plus a per-connection salt, so sibling connections in one
    /// pool do not share a sequence.
    fn from_entropy(salt: u64) -> Self {
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
    fn next_below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// The venue's documented subscription budget, which it counts **per IP**
/// (`docs/spec.md` item 9). Every connection in the pool spends from the same
/// one, so it caps the pool rather than a socket — and
/// `SubscriptionRegistry::capacity` clamps to it rather than trusting
/// `max_connections × max_subs_per_connection`, which can multiply past it.
const MAX_SUBSCRIPTIONS_PER_IP: usize = 1000;

/// Retry schedule for a quarantined subscription: a minute, doubling per
/// quarantine served, capped at an hour.
///
/// Un-jittered on purpose. Quarantine expiries are minutes apart and are
/// re-sent on a ping tick, so they cannot synchronize into the request burst
/// the reconnect jitter exists to prevent.
const QUARANTINE_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(60),
    max: Duration::from_secs(3600),
    jitter_pct: 0,
};

/// Where a `SubscriptionRegistry::place` call put a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
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
    /// Budget from `Subscription::staleness_threshold`; `None` for
    /// event-driven feeds.
    pub threshold_ms: Option<u64>,
    /// Fails closed. True whenever this feed is not delivering, which is any
    /// of: the socket is down, the venue has not acknowledged the subscription
    /// on the current socket, the subscription is quarantined, or it has a
    /// silence budget it has blown (including never having delivered at all).
    ///
    /// The first three do **not** depend on `threshold_ms`: a `userFills` feed
    /// on a dead socket is as blind as a `bbo` one, and `docs/spec.md` item 9
    /// says a dropped fill is the failure the ledger exists to prevent.
    pub stale: bool,
    /// The pool has stopped re-subscribing this one because the venue kept
    /// killing the socket rather than acknowledging it. Expires; see
    /// `SubscriptionRegistry::record_failed_session`.
    pub quarantined: bool,
}

#[derive(Debug, Clone)]
struct SubEntry {
    sub: Subscription,
    last_message_ms: Option<u64>,
    acked: bool,
    strikes: u32,
    /// When the current quarantine lifts, ms. `None` means never quarantined
    /// or already lifted by `SubscriptionRegistry::clear_quarantine`.
    quarantined_until_ms: Option<u64>,
    /// Quarantines served. Widens the next interval so a genuinely poisoned
    /// subscription converges instead of retrying on a fixed schedule.
    quarantines: u32,
}

impl SubEntry {
    fn new(sub: Subscription) -> Self {
        SubEntry {
            sub,
            last_message_ms: None,
            acked: false,
            strikes: 0,
            quarantined_until_ms: None,
            quarantines: 0,
        }
    }

    /// Whether the quarantine is in force *now*. A lapsed one is not.
    fn quarantined_at(&self, now_ms: u64) -> bool {
        self.quarantined_until_ms
            .is_some_and(|until| now_ms < until)
    }
}

/// Why one named feed is not fit to be signed against.
///
/// Typed rather than a bool so the console and the MCP error taxonomy can say
/// which feed and why (`AGENTS.md` invariant 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedBlock {
    /// The caller named a feed the pool was never asked to carry. Fails closed:
    /// an absent subscription is the most complete kind of blindness there is.
    NotSubscribed(Subscription),
    /// Subscribed, and not delivering. See [`FeedHealth::stale`].
    Stale(Box<FeedHealth>),
}

impl fmt::Display for FeedBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeedBlock::NotSubscribed(sub) => write!(f, "{sub}: not subscribed"),
            FeedBlock::Stale(h) if h.quarantined => write!(f, "{}: quarantined", h.subscription),
            FeedBlock::Stale(h) if !h.connected => write!(f, "{}: socket down", h.subscription),
            FeedBlock::Stale(h) if !h.acked => write!(f, "{}: unacknowledged", h.subscription),
            FeedBlock::Stale(h) => match (h.age_ms, h.threshold_ms) {
                (Some(age), Some(limit)) => {
                    write!(f, "{}: silent {age} ms of {limit} ms", h.subscription)
                }
                _ => write!(f, "{}: no message yet", h.subscription),
            },
        }
    }
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
struct SubscriptionRegistry {
    max_connections: usize,
    max_subs_per_connection: usize,
    slots: Vec<ConnectionSlot>,
}

impl SubscriptionRegistry {
    /// `max_subs_per_connection` defaults to the venue's documented 1000
    /// (`docs/spec.md` item 9). Note the venue documents that ceiling **per
    /// IP**, so a pool of several connections shares one budget; size
    /// `max_connections` accordingly.
    fn new(max_connections: usize, max_subs_per_connection: usize) -> Self {
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
    /// `Subscription::exclusive_per_connection`.
    fn place(&mut self, sub: Subscription) -> Result<Placement, PoolError> {
        let key = sub.key();
        if let Some(slot) = self.slots.iter().find(|s| s.subs.contains_key(&key)) {
            return Ok(Placement::AlreadyPresent(slot.id));
        }
        if self.len() >= self.capacity() {
            return Err(self.capacity_exhausted());
        }
        let cap = self.max_subs_per_connection;
        let exclusive = sub.exclusive_per_connection();
        let target = self.slots.iter_mut().find(|s| {
            s.subs.len() < cap
                && !(exclusive && s.subs.values().any(|e| e.sub.exclusive_per_connection()))
        });
        if let Some(slot) = target {
            slot.subs.insert(key, SubEntry::new(sub));
            return Ok(Placement::Existing(slot.id));
        }
        if self.slots.len() >= self.max_connections {
            // Distinguish "the pool is full" from "every socket already owns
            // one of these": an operator told the pool is out of slots while
            // thousands sit free reads it as a lie, and the fix for the two is
            // different.
            if exclusive && self.slots.iter().any(|s| s.subs.len() < cap) {
                return Err(PoolError::ExclusiveSlotExhausted {
                    kind: sub.kind(),
                    connections: self.slots.len(),
                });
            }
            return Err(self.capacity_exhausted());
        }
        let id = ConnectionId(self.slots.len());
        let mut subs = BTreeMap::new();
        subs.insert(key, SubEntry::new(sub));
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
    fn remove(&mut self, sub: &Subscription) -> Result<ConnectionId, PoolError> {
        let key = sub.key();
        for slot in self.slots.iter_mut() {
            if slot.subs.remove(&key).is_some() {
                return Ok(slot.id);
            }
        }
        Err(PoolError::NotSubscribed(key))
    }

    /// One connection's subscriptions matching `keep`, in key order. Empty for
    /// a connection that does not exist.
    fn subs_where(&self, id: ConnectionId, keep: impl Fn(&SubEntry) -> bool) -> Vec<Subscription> {
        self.slot(id)
            .map(|s| {
                s.subs
                    .values()
                    .filter(|e| keep(e))
                    .map(|e| e.sub.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Everything one connection owns, in key order, quarantined included.
    fn subscriptions(&self, id: ConnectionId) -> Vec<Subscription> {
        self.subs_where(id, |_| true)
    }

    /// What a reconnect should actually re-send, in key order: everything the
    /// connection owns except what `SubscriptionRegistry::record_failed_session`
    /// has quarantined *and whose quarantine has not yet lapsed*.
    fn resubscribe_set(&self, id: ConnectionId, now_ms: u64) -> Vec<Subscription> {
        self.subs_where(id, |e| !e.quarantined_at(now_ms))
    }

    /// Quarantines that have lapsed on a live connection: cleared here and
    /// returned in key order so the caller re-sends the subscribe frames.
    ///
    /// Without this the expiry would be unreachable on a healthy shard —
    /// nothing else re-reads `SubscriptionRegistry::resubscribe_set` until
    /// the next reconnect, which a converged pool never performs.
    fn take_expired_quarantines(&mut self, id: ConnectionId, now_ms: u64) -> Vec<Subscription> {
        let Some(slot) = self.slot_mut(id) else {
            return Vec::new();
        };
        let mut revived = Vec::new();
        for entry in slot.subs.values_mut() {
            if entry
                .quarantined_until_ms
                .is_some_and(|until| now_ms >= until)
            {
                entry.quarantined_until_ms = None;
                revived.push(entry.sub.clone());
            }
        }
        revived
    }

    /// Lift a quarantine now, forgetting the strikes behind it. The operator's
    /// documented way back after fixing whatever the
    /// [`WsEvent::SubscriptionQuarantined`] event named.
    ///
    /// Returns the owning connection so the caller can re-send the subscribe.
    fn clear_quarantine(&mut self, sub: &Subscription) -> Result<ConnectionId, PoolError> {
        let key = sub.key();
        for slot in self.slots.iter_mut() {
            if let Some(entry) = slot.subs.get_mut(&key) {
                entry.quarantined_until_ms = None;
                entry.quarantines = 0;
                entry.strikes = 0;
                return Ok(slot.id);
            }
        }
        Err(PoolError::NotSubscribed(key))
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
    /// The caller must charge a strike only when the dead session is *evidence*
    /// about a subscription — see [`WsPoolConfig::session_grace`]. Three
    /// ordinary drops that happened to land inside the subscribe window would
    /// otherwise quarantine a perfectly healthy feed.
    ///
    /// The quarantine **expires**, at [`QUARANTINE_BACKOFF`] widened by the
    /// number already served, because a permanent one is unrecoverable by
    /// construction: a quarantined subscription is never sent, so it is never
    /// acked, so the "cleared by any ack" escape can never fire. Strikes are
    /// deliberately *not* reset, so a subscription that poisons the socket
    /// again after its quarantine lapses is re-quarantined on the first
    /// failure, at double the interval.
    ///
    /// Returns the subscription that just became quarantined, if any.
    fn record_failed_session(
        &mut self,
        id: ConnectionId,
        quarantine_after: u32,
        now_ms: u64,
    ) -> Option<(Subscription, u32)> {
        let entry = self
            .slot_mut(id)?
            .subs
            .values_mut()
            .find(|e| !e.acked && !e.quarantined_at(now_ms))?;
        entry.strikes = entry.strikes.saturating_add(1);
        if entry.strikes >= quarantine_after.max(1) {
            let wait = QUARANTINE_BACKOFF.nominal(entry.quarantines);
            entry.quarantines = entry.quarantines.saturating_add(1);
            let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
            entry.quarantined_until_ms = Some(now_ms.saturating_add(wait_ms));
            return Some((entry.sub.clone(), entry.strikes));
        }
        None
    }

    /// Subscriptions the venue has not acknowledged on the current socket.
    fn unacked(&self, id: ConnectionId) -> Vec<Subscription> {
        self.subs_where(id, |e| !e.acked)
    }

    /// The address whose `orderUpdates` this connection owns, used to attribute
    /// a payload that carries no user.
    fn order_updates_user(&self, id: ConnectionId) -> Option<Address> {
        self.slot(id)?.subs.values().find_map(|e| match &e.sub {
            Subscription::OrderUpdates { user } => Some(*user),
            _ => None,
        })
    }

    /// Last message on the connection, whatever channel it came on. This is the
    /// gap-window anchor; see [`GapWindow`].
    fn connection_last_message(&self, id: ConnectionId) -> Option<u64> {
        self.slot(id)?.last_message_ms
    }

    /// Record traffic on the connection as a whole. Called for every frame,
    /// including pongs, because a pong proves the socket is alive.
    fn touch_connection(&mut self, id: ConnectionId, now_ms: u64) {
        if let Some(slot) = self.slot_mut(id) {
            slot.last_message_ms = Some(now_ms);
        }
    }

    /// Record traffic on one feed.
    fn touch(&mut self, id: ConnectionId, key: &str, now_ms: u64) {
        if let Some(entry) = self.slot_mut(id).and_then(|s| s.subs.get_mut(key)) {
            entry.last_message_ms = Some(now_ms);
        }
    }

    /// Record the venue's `subscriptionResponse`. Clears the strike count: a
    /// subscription the venue has just accepted is not a suspect.
    fn ack(&mut self, id: ConnectionId, key: &str) {
        if let Some(entry) = self.slot_mut(id).and_then(|s| s.subs.get_mut(key)) {
            entry.acked = true;
            entry.strikes = 0;
        }
    }

    /// Flip a connection up or down.
    ///
    /// Either transition clears every ack: a dead socket confirms nothing, and
    /// a fresh one has confirmed nothing yet. Callers that need the pre-drop
    /// ack state — `SubscriptionRegistry::unacked`, which names the likely
    /// poison in a [`Disconnected`] — must read it before flipping.
    ///
    /// Coming up seeds the connection clock so the idle detector has a
    /// baseline. Neither transition touches per-feed timestamps, so each feed's
    /// age keeps growing through an outage, which is what makes them read
    /// stale.
    fn set_connected(&mut self, id: ConnectionId, connected: bool, now_ms: u64) {
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
    fn health(&self, now_ms: u64, thresholds: &StalenessThresholds) -> Vec<FeedHealth> {
        let mut out = Vec::with_capacity(self.len());
        for slot in &self.slots {
            for entry in slot.subs.values() {
                let threshold = entry.sub.staleness_threshold(thresholds);
                let threshold_ms =
                    threshold.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
                let age_ms = entry.last_message_ms.map(|t| now_ms.saturating_sub(t));
                // A blown silence budget. Only feeds that *have* a budget can
                // blow one, and one that has never delivered has already.
                let blown = match (threshold_ms, age_ms) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(limit), Some(age)) => age > limit,
                };
                let quarantined = entry.quarantined_at(now_ms);
                // The three fail-closed conditions are read BEFORE the budget,
                // not inside it: they hold on every channel, including the
                // event-driven ones the budget does not cover. A `userFills`
                // feed on a dead socket is blind, and `docs/spec.md` item 34
                // requires execution to fail closed there.
                let stale = quarantined || !slot.connected || !entry.acked || blown;
                out.push(FeedHealth {
                    connection: slot.id,
                    subscription: entry.sub.clone(),
                    connected: slot.connected,
                    acked: entry.acked,
                    last_message_ms: entry.last_message_ms,
                    age_ms,
                    threshold_ms,
                    stale,
                    quarantined,
                });
            }
        }
        out
    }

    /// Which of the `required` feeds are not fit to be signed against, in key
    /// order. Empty means every named feed is live.
    ///
    /// This is the pre-sign gate (`docs/spec.md` item 34), and it is per-feed
    /// on purpose: a caller about to sign names the feeds its decision actually
    /// depended on, so an unrelated depth ladder does not block an order priced
    /// off `bbo`. A required feed that is not subscribed at all blocks — the
    /// pool cannot report an age for something it was never asked to watch.
    fn stale_feeds(
        &self,
        now_ms: u64,
        thresholds: &StalenessThresholds,
        required: &[Subscription],
    ) -> Vec<FeedBlock> {
        let health = self.health(now_ms, thresholds);
        let mut wanted: Vec<&Subscription> = required.iter().collect();
        wanted.sort();
        wanted.dedup();
        wanted
            .into_iter()
            .filter_map(|sub| match health.iter().find(|h| &h.subscription == sub) {
                None => Some(FeedBlock::NotSubscribed(sub.clone())),
                Some(h) if h.stale => Some(FeedBlock::Stale(Box::new(h.clone()))),
                Some(_) => None,
            })
            .collect()
    }

    /// Subscriptions held across the whole pool, for the item 9 cap.
    fn len(&self) -> usize {
        self.slots.iter().map(|s| s.subs.len()).sum()
    }

    /// Total slots this pool may ever hold, clamped to the venue's per-IP
    /// budget. `max_connections × max_subs_per_connection` can multiply past
    /// `MAX_SUBSCRIPTIONS_PER_IP`; the venue's counter does not care how the
    /// pool sharded them.
    fn capacity(&self) -> usize {
        self.max_connections
            .saturating_mul(self.max_subs_per_connection)
            .min(MAX_SUBSCRIPTIONS_PER_IP)
    }

    fn capacity_exhausted(&self) -> PoolError {
        PoolError::CapacityExhausted {
            used: self.len(),
            cap: self.capacity(),
            connections: self.slots.len(),
        }
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
    /// Sockets the pool may open.
    ///
    /// This is the ceiling on the agent roster, not just on sharding: D1 gives
    /// every agent its own sub-account, `orderUpdates` may not share a socket
    /// (`Subscription::exclusive_per_connection`), so watching N agents needs
    /// N connections. Sixteen against the venue's documented 100-per-IP
    /// connection ceiling. Raise it for a larger roster — the subscription
    /// budget stays safe either way, because
    /// `SubscriptionRegistry::capacity` clamps to
    /// `MAX_SUBSCRIPTIONS_PER_IP`.
    pub max_connections: usize,
    /// Per-socket subscription cap. The pool-wide ceiling is
    /// `MAX_SUBSCRIPTIONS_PER_IP`, which binds first whenever this one times
    /// `max_connections` exceeds it.
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
    /// (`SubscriptionRegistry::record_failed_session`). Three, because one
    /// drop inside a subscribe window is ordinary bad luck and three in a row
    /// is not.
    pub quarantine_after: u32,
    /// How long a session must live before it counts as having proven itself.
    ///
    /// Two things hang on this, and both are about telling a *refusal* apart
    /// from a *drop*. A session shorter than this ends inside the subscribe
    /// round trip, which is where the venue kills a socket carrying a
    /// subscription it will not accept (measured 2026-09-03: an unknown coin on
    /// `bbo` produces a bare TCP close, code 1006, within a round trip). So:
    ///
    /// - shorter: the close is evidence about whatever was unacknowledged, and
    ///   a strike is charged (`SubscriptionRegistry::record_failed_session`);
    /// - shorter: the reconnect backoff does **not** reset, so a
    ///   connect-then-immediately-drop loop actually backs off instead of
    ///   hammering the venue's per-IP budget every `base` ms;
    /// - longer: the socket worked, so no strike and the backoff resets.
    ///
    /// Five seconds, an order of magnitude above the measured round trip.
    pub session_grace: Duration,
}

impl Default for WsPoolConfig {
    fn default() -> Self {
        WsPoolConfig {
            network: Network::default(),
            max_connections: 16,
            max_subs_per_connection: 1000,
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(75),
            backoff: Backoff::default(),
            thresholds: StalenessThresholds::default(),
            quarantine_after: 3,
            session_grace: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
enum ConnCommand {
    Subscribe(Subscription),
    Unsubscribe(Subscription),
    Shutdown,
}

#[derive(Debug)]
struct PoolInner {
    registry: SubscriptionRegistry,
    conns: Vec<mpsc::UnboundedSender<ConnCommand>>,
    shutdown: bool,
    tasks: Vec<ConnectionTask>,
}

struct ConnectionTask {
    abort: tokio::task::AbortHandle,
    completion: Shared<BoxFuture<'static, Result<(), PoolError>>>,
}

impl fmt::Debug for ConnectionTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionTask")
            .field("abort", &self.abort)
            .finish_non_exhaustive()
    }
}

/// A pool of independently-reconnecting Hyperliquid websockets.
///
/// `docs/spec.md` item 9: all agents multiplex over a shared socket pool.
/// Subscriptions are distributed across connections, each connection owns its
/// own reconnect loop, and a drop on one degrades exactly the feeds it carried
/// while emitting the gap window the ledger needs to backfill.
///
/// Dropping the pool signals cancellation, as does [`WsPool::shutdown`]. Await
/// [`WsPool::shutdown_and_drain`] to prove every connection task has stopped.
#[derive(Debug)]
pub struct WsPool {
    cfg: WsPoolConfig,
    inner: Arc<Mutex<PoolInner>>,
    events: mpsc::Sender<WsEvent>,
    /// Captured at construction so [`WsPool::subscribe`] can stay synchronous
    /// and callable from a plain thread — a Tauri command thread, say — without
    /// `tokio::spawn`'s "no reactor running" panic.
    handle: tokio::runtime::Handle,
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
    /// No socket is opened until the first [`WsPool::subscribe`], but a runtime
    /// **handle** is required now: the pool spawns one task per connection, and
    /// discovering that at the first subscribe would mean panicking on the
    /// caller's thread. Returns [`PoolError::NoRuntime`] when called from
    /// outside a runtime.
    pub fn new(cfg: WsPoolConfig) -> Result<(Self, mpsc::Receiver<WsEvent>), PoolError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| PoolError::NoRuntime)?;
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let inner = PoolInner {
            registry: SubscriptionRegistry::new(cfg.max_connections, cfg.max_subs_per_connection),
            conns: Vec::new(),
            shutdown: false,
            tasks: Vec::new(),
        };
        Ok((
            WsPool {
                cfg,
                inner: Arc::new(Mutex::new(inner)),
                events: tx,
                handle,
            },
            rx,
        ))
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
        self.subscribe_to(sub, self.cfg.network.ws_url())
    }

    // The explicit URL is private so tests can exercise real loopback sockets.
    fn subscribe_to(&self, sub: Subscription, url: &str) -> Result<(), PoolError> {
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
                self.spawn_connection(
                    &mut guard,
                    id,
                    run_connection(
                        id,
                        url.to_owned(),
                        self.cfg.clone(),
                        Arc::clone(&self.inner),
                        self.events.clone(),
                        rx,
                    ),
                );
                Ok(())
            }
        }
    }

    fn spawn_connection(
        &self,
        guard: &mut PoolInner,
        id: ConnectionId,
        run: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let task = self.handle.spawn(run);
        let abort = task.abort_handle();
        let completion = async move {
            match task.await {
                Ok(()) => Ok(()),
                Err(error) if error.is_cancelled() => Ok(()),
                Err(error) => Err(PoolError::ConnectionTask {
                    connection: id,
                    detail: error.to_string(),
                }),
            }
        }
        .boxed()
        .shared();
        // Publication shares the admission lock with shutdown. The retained
        // shared join is never taken by a caller that could abandon its wait.
        guard.tasks.push(ConnectionTask { abort, completion });
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

    /// Per-feed health for the console's stale overlay (`docs/spec.md`
    /// item 34).
    pub fn health(&self) -> Vec<FeedHealth> {
        lock(&self.inner)
            .registry
            .health(now_ms(), &self.cfg.thresholds)
    }

    /// **The pre-sign gate.** The feeds among `required` a caller must not sign
    /// against, in key order; empty means go ahead.
    ///
    /// Execution paths call this, not [`WsPool::any_stale`], and they name the
    /// feeds the decision depended on: the price source, and the user channels
    /// for the account being traded. Naming them is what keeps the gate honest
    /// in both directions — a feed that is down, unacknowledged, quarantined or
    /// silent past its budget blocks, and so does one the pool was never asked
    /// to carry, while an unrelated `l2Book` ladder pushing at 5.4 s does not.
    pub fn stale_feeds(&self, required: &[Subscription]) -> Vec<FeedBlock> {
        lock(&self.inner)
            .registry
            .stale_feeds(now_ms(), &self.cfg.thresholds, required)
    }

    /// Console-level summary: is anything at all degraded?
    ///
    /// Not the signing gate — use [`WsPool::stale_feeds`] for that, which names
    /// the feeds the decision needs instead of blocking on an unrelated one. A
    /// pool with nothing subscribed reads degraded rather than healthy: it is
    /// not watching anything.
    pub fn any_stale(&self) -> bool {
        let health = self.health();
        health.is_empty() || health.iter().any(|h| h.stale)
    }

    /// Lift a quarantine and re-send the subscription, after the operator has
    /// fixed whatever [`WsEvent::SubscriptionQuarantined`] named.
    pub fn clear_quarantine(&self, sub: &Subscription) -> Result<(), PoolError> {
        let mut guard = lock(&self.inner);
        if guard.shutdown {
            return Err(PoolError::Shutdown);
        }
        let id = guard.registry.clear_quarantine(sub)?;
        if let Some(tx) = guard.conns.get(id.0) {
            let _ = tx.send(ConnCommand::Subscribe(sub.clone()));
        }
        Ok(())
    }

    /// Signal cancellation of every connection. Idempotent; further subscribes
    /// return [`PoolError::Shutdown`]. Use [`Self::shutdown_and_drain`] to wait
    /// for actual completion rather than treating this signal as proof.
    pub fn shutdown(&self) {
        let mut guard = lock(&self.inner);
        guard.shutdown = true;
        for task in &guard.tasks {
            task.abort.abort();
        }
        for tx in &guard.conns {
            let _ = tx.send(ConnCommand::Shutdown);
        }
        guard.conns.clear();
        for index in 0..guard.tasks.len() {
            guard
                .registry
                .set_connected(ConnectionId(index), false, now_ms());
        }
    }

    /// Stop admission and await actual termination of every connection task.
    /// Concurrent callers and retries after a dropped wait observe the same
    /// retained completions, including task panic errors. All tasks are joined
    /// before an error is returned. No pool lock is held across an await.
    ///
    /// Socket I/O and blocked event sends are canceled; this does not require
    /// the event consumer to make progress. Already queued events remain
    /// readable, but an in-flight frame may not be delivered. This is an
    /// ownership boundary, not a lossless flush: reconcile on the next start.
    /// The receiver reaches EOF only after the pool itself is also dropped.
    pub async fn shutdown_and_drain(&self) -> Result<(), PoolError> {
        self.shutdown();
        let completions: Vec<_> = lock(&self.inner)
            .tasks
            .iter()
            .map(|task| task.completion.clone())
            .collect();
        let mut failure = None;
        for completion in completions {
            if let Err(error) = completion.await {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for WsPool {
    fn drop(&mut self) {
        self.shutdown();
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

/// What the I/O half of a connection just observed. One per turn of the
/// reconnect loop.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Turn {
    /// `connect_async` failed.
    ConnectFailed { at_ms: u64, reason: String },
    /// Connected, but a subscribe frame could not be sent. Ours, not the
    /// venue's, so it is not evidence about any subscription.
    ResubscribeFailed {
        at_ms: u64,
        last_seen_ms: Option<u64>,
        reason: String,
    },
    /// Connected and the whole subscribe burst went out.
    Subscribed { at_ms: u64 },
    /// A live session ended. `lived` against `grace` is what separates a venue
    /// refusal from an ordinary drop — see [`WsPoolConfig::session_grace`].
    SessionDropped {
        at_ms: u64,
        last_seen_ms: Option<u64>,
        reason: String,
        lived: Duration,
        grace: Duration,
    },
}

/// What the I/O half must do about a [`Turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// Nothing to announce: the same outage, still outstanding.
    Idle,
    /// Emit a [`WsEvent::Disconnected`].
    ReportDown { reason: String, strike: bool },
    /// Emit a [`WsEvent::Reconnected`] over this gap.
    ReportUp { gap: GapWindow, attempts: u32 },
}

/// One connection's reconnect state.
///
/// Split out so the ordering rules the loop depends on are unit-testable
/// without a socket: that an outage is announced once and answered once, that
/// the gap anchors on the pre-drop message time, and that the backoff resets
/// only on a session that proved itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnState {
    /// Consecutive attempts that have not produced a proven session.
    attempt: u32,
    /// A `Disconnected` has been emitted and no `Reconnected` has answered it.
    ///
    /// This, and **not** "did a session ever succeed", is what gates the
    /// `Reconnected`: an outage before the first successful session — oppen
    /// launched before the wifi is up, a laptop resumed into a dead network —
    /// still emits a `Disconnected`, so it still needs the matching
    /// `Reconnected` and its gap window, or the item 34 fail-closed latch never
    /// releases and item 9's `userFillsByTime` backfill for that window never
    /// runs.
    announced_down: bool,
    /// Last message before the outage began: the gap anchor ([`GapWindow`]).
    last_seen_ms: Option<u64>,
    /// When the outage was noticed, used when there is no last message.
    disconnected_at_ms: u64,
}

impl ConnState {
    fn new(now_ms: u64) -> Self {
        ConnState {
            attempt: 0,
            announced_down: false,
            last_seen_ms: None,
            disconnected_at_ms: now_ms,
        }
    }

    /// Index into the backoff curve for the next retry: the first retry of an
    /// outage waits `base`, and each further failure doubles it.
    fn backoff_index(&self) -> u32 {
        self.attempt.saturating_sub(1)
    }

    /// Announce an outage, once. Retries of the same outage are `Idle`: the
    /// activity stream is for the operator, not for the retry loop.
    fn announce_down(
        &mut self,
        at_ms: u64,
        last_seen_ms: Option<u64>,
        reason: String,
        strike: bool,
    ) -> Step {
        if self.announced_down {
            return Step::Idle;
        }
        self.announced_down = true;
        self.last_seen_ms = last_seen_ms;
        self.disconnected_at_ms = at_ms;
        Step::ReportDown { reason, strike }
    }
}

/// The whole reconnect decision, as a pure function of the state and one turn.
fn next_step(state: &mut ConnState, turn: Turn) -> Step {
    match turn {
        Turn::ConnectFailed { at_ms, reason } => {
            state.attempt = state.attempt.saturating_add(1);
            state.announce_down(at_ms, None, reason, false)
        }
        Turn::ResubscribeFailed {
            at_ms,
            last_seen_ms,
            reason,
        } => {
            state.attempt = state.attempt.saturating_add(1);
            state.announce_down(at_ms, last_seen_ms, reason, false)
        }
        Turn::SessionDropped {
            at_ms,
            last_seen_ms,
            reason,
            lived,
            grace,
        } => {
            // A session that survived the subscribe round trip proved the
            // socket and the subscribe set: reset the curve, charge nobody. One
            // that died inside it is the measured shape of a venue refusal, so
            // it keeps climbing and the unacked suspect takes a strike.
            let proved = lived >= grace;
            state.attempt = if proved {
                0
            } else {
                state.attempt.saturating_add(1)
            };
            state.announce_down(at_ms, last_seen_ms, reason, !proved)
        }
        Turn::Subscribed { at_ms } => {
            if !state.announced_down {
                return Step::Idle;
            }
            state.announced_down = false;
            let start = state
                .last_seen_ms
                .unwrap_or(state.disconnected_at_ms)
                .min(at_ms);
            Step::ReportUp {
                gap: GapWindow {
                    start_ms: start,
                    end_ms: at_ms,
                },
                attempts: state.attempt,
            }
        }
    }
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
    let mut state = ConnState::new(now_ms());

    loop {
        match connect_async(url.as_str()).await {
            Ok((mut stream, _response)) => {
                let subs = {
                    let mut guard = lock(&inner);
                    if guard.shutdown {
                        return;
                    }
                    let now = now_ms();
                    guard.registry.set_connected(id, true, now);
                    guard.registry.resubscribe_set(id, now)
                };
                let mut resubscribe_error = None;
                for sub in &subs {
                    if let Err(e) = send_frame(&mut stream, "subscribe", sub).await {
                        resubscribe_error = Some(e);
                        break;
                    }
                }
                if let Some(reason) = resubscribe_error {
                    let turn = Turn::ResubscribeFailed {
                        at_ms: now_ms(),
                        last_seen_ms: lock(&inner).registry.connection_last_message(id),
                        reason,
                    };
                    if !apply(&mut state, turn, id, &cfg, &inner, &events, &subs).await {
                        return;
                    }
                } else {
                    let turn = Turn::Subscribed { at_ms: now_ms() };
                    if !apply(&mut state, turn, id, &cfg, &inner, &events, &subs).await {
                        return;
                    }
                    let started = tokio::time::Instant::now();
                    match session(id, &mut stream, &mut cmds, &events, &inner, &cfg).await {
                        SessionEnd::Shutdown => {
                            lock(&inner).registry.set_connected(id, false, now_ms());
                            let _ = stream.close(None).await;
                            return;
                        }
                        SessionEnd::Dropped(reason) => {
                            let turn = Turn::SessionDropped {
                                at_ms: now_ms(),
                                last_seen_ms: lock(&inner).registry.connection_last_message(id),
                                reason,
                                lived: started.elapsed(),
                                grace: cfg.session_grace,
                            };
                            if !apply(&mut state, turn, id, &cfg, &inner, &events, &subs).await {
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let turn = Turn::ConnectFailed {
                    at_ms: now_ms(),
                    reason: format!("connect failed: {e}"),
                };
                if !apply(&mut state, turn, id, &cfg, &inner, &events, &[]).await {
                    return;
                }
            }
        }

        if lock(&inner).shutdown {
            return;
        }
        let wait = cfg.backoff.delay(state.backoff_index(), &mut jitter);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            cmd = cmds.recv() => {
                // A shutdown during backoff should not wait out the sleep.
                if matches!(cmd, None | Some(ConnCommand::Shutdown)) {
                    return;
                }
            }
        }
    }
}

/// Decide what a [`Turn`] means ([`next_step`]) and carry it out. Returns false
/// when the consumer's receiver is gone, which ends the connection task.
async fn apply(
    state: &mut ConnState,
    turn: Turn,
    id: ConnectionId,
    cfg: &WsPoolConfig,
    inner: &Arc<Mutex<PoolInner>>,
    events: &mpsc::Sender<WsEvent>,
    resubscribed: &[Subscription],
) -> bool {
    match next_step(state, turn) {
        Step::Idle => true,
        Step::ReportDown { reason, strike } => {
            report_disconnect(id, cfg, inner, events, reason, strike).await
        }
        Step::ReportUp { gap, attempts } => {
            let event = WsEvent::Reconnected(Box::new(Reconnected {
                connection: id,
                at_ms: gap.end_ms,
                gap,
                resubscribed: resubscribed.to_vec(),
                attempts,
            }));
            events.send(event).await.is_ok()
        }
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
        // Ordering rule: `unacked` and `record_failed_session` both read the
        // ack state, and `set_connected(false)` clears it. Read first, flip
        // last, or every feed reads unacked and the strike lands on whichever
        // healthy subscription sorts first.
        let unacked = guard.registry.unacked(id);
        let quarantined = if strike {
            guard
                .registry
                .record_failed_session(id, cfg.quarantine_after, at)
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
                let now = now_ms();
                let (last, revived) = {
                    let mut guard = lock(inner);
                    (
                        guard.registry.connection_last_message(id),
                        guard.registry.take_expired_quarantines(id, now),
                    )
                };
                if let Some(last) = last
                    && now.saturating_sub(last) > idle_ms
                {
                    return SessionEnd::Dropped(format!("idle for more than {idle_ms} ms"));
                }
                // A lapsed quarantine is retried here rather than waiting for a
                // reconnect a converged shard will never perform.
                for sub in &revived {
                    tracing::info!(connection = %id, subscription = %sub, "retrying a lapsed quarantine");
                    if let Err(e) = send_frame(stream, "subscribe", sub).await {
                        return SessionEnd::Dropped(e);
                    }
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
        /// Bid only. The venue's array order for a one-sided book is
        /// `[bid, null]`; this frame exists so `parse_message` is pinned
        /// against the same shape `crate::types::Bbo`'s own tests cover.
        pub const BBO_BID_ONLY: &str = r#"{"channel":"bbo","data":{"coin":"FRIEND","time":1788490146994,"bbo":[{"px":"1.0","sz":"2.0","n":1},null]}}"#;
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
        // Depth is not the book component: §14.4 correction 4 moved that to
        // `bbo`, and `l2Book` pushes at a 5.4 s median.
        assert_eq!(
            Subscription::L2Book { coin: "BTC".into() }.staleness_threshold(&t),
            Some(Duration::from_secs(15))
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

    /// D1 gives every agent its own sub-account, so the roster size is the
    /// number of `orderUpdates` subscriptions, and the exclusivity rule pins
    /// one per connection. The agent past the last connection must be told
    /// *that*, not handed a capacity error reporting thousands of free slots.
    #[test]
    fn an_exclusive_subscription_past_the_last_connection_is_named_as_such() {
        let mut r = SubscriptionRegistry::new(2, 100);
        for i in 1..=2u8 {
            let user = addr(&format!("0x00000000000000000000000000000000000000{i:02}"));
            assert!(r.place(Subscription::OrderUpdates { user }).is_ok());
        }
        let fifth = addr("0x0000000000000000000000000000000000000003");
        assert_eq!(
            r.place(Subscription::OrderUpdates { user: fifth }),
            Err(PoolError::ExclusiveSlotExhausted {
                kind: "orderUpdates",
                connections: 2
            })
        );
        // The message must not read as "the pool is full": it is not.
        let message = r
            .place(Subscription::OrderUpdates { user: fifth })
            .expect_err("still refused")
            .to_string();
        assert_eq!(
            message,
            "no connection may hold a second orderUpdates subscription and all 2 connections already own one"
        );
        // Everything else still places, which is what makes the old message a
        // lie: 198 of 200 slots are free.
        assert_eq!(
            r.place(Subscription::Bbo { coin: "BTC".into() }),
            Ok(Placement::Existing(ConnectionId(0)))
        );
    }

    /// The venue counts subscriptions per IP, not per socket, so the pool's own
    /// ceiling must clamp to that however `max_connections` is configured. The
    /// default 16 × 1000 would otherwise advertise 16,000.
    #[test]
    fn capacity_is_clamped_to_the_per_ip_budget() {
        let cfg = WsPoolConfig::default();
        let r = SubscriptionRegistry::new(cfg.max_connections, cfg.max_subs_per_connection);
        assert!(cfg.max_connections * cfg.max_subs_per_connection > MAX_SUBSCRIPTIONS_PER_IP);
        assert_eq!(r.capacity(), MAX_SUBSCRIPTIONS_PER_IP);

        // And the refusal reports the effective cap, not the product.
        let mut r = SubscriptionRegistry::new(2, MAX_SUBSCRIPTIONS_PER_IP);
        for i in 0..MAX_SUBSCRIPTIONS_PER_IP {
            r.place(Subscription::Bbo {
                coin: format!("C{i}"),
            })
            .expect("inside the budget");
        }
        assert_eq!(
            r.place(Subscription::Bbo {
                coin: "OVER".into()
            }),
            Err(PoolError::CapacityExhausted {
                used: MAX_SUBSCRIPTIONS_PER_IP,
                cap: MAX_SUBSCRIPTIONS_PER_IP,
                connections: 1
            })
        );
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

        // Never connected, never a message: stale on both, budget or no budget.
        let h = r.health(10_000, &t);
        let bbo_h = h.iter().find(|x| x.subscription == bbo).expect("bbo");
        assert!(bbo_h.stale && !bbo_h.connected && bbo_h.age_ms.is_none());
        let trades_h = h.iter().find(|x| x.subscription == trades).expect("trades");
        assert!(
            trades_h.stale,
            "a feed that has never connected is blind, whatever its channel"
        );
        assert_eq!(trades_h.threshold_ms, None);

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

    /// The fail-open the reviewer found: every channel without a silence budget
    /// — `userFills`, `orderUpdates`, `trades`, `candle` — used to report
    /// `stale: false` with its socket dead, never connected, or quarantined,
    /// because the `None` threshold was matched first. A pool carrying only the
    /// ledger's user channels (the D1 case) then read healthy while blind, and
    /// signing proceeded (`docs/spec.md` items 9 and 34).
    #[test]
    fn a_feed_without_a_silence_budget_still_fails_closed() {
        let t = StalenessThresholds::default();
        let user = addr("0x0000000000000000000000000000000000000009");
        let budgetless = [
            Subscription::UserFills { user },
            Subscription::OrderUpdates { user },
            Subscription::Trades { coin: "BTC".into() },
            Subscription::Candle {
                coin: "BTC".into(),
                interval: "1m".into(),
            },
        ];

        for sub in &budgetless {
            assert_eq!(sub.staleness_threshold(&t), None, "{sub} has no budget");
            let mut r = SubscriptionRegistry::new(1, 10);
            r.place(sub.clone()).expect("place");
            let health = |r: &SubscriptionRegistry, at: u64| r.health(at, &t)[0].clone();

            // 1. Never connected.
            assert!(health(&r, 1_000).stale, "{sub}: never connected");

            // 2. Connected but not yet acknowledged by the venue.
            r.set_connected(ConnectionId(0), true, 1_000);
            assert!(health(&r, 1_000).stale, "{sub}: unacknowledged");

            // Acked and delivering: live, and silence alone is not a fault.
            r.ack(ConnectionId(0), &sub.key());
            r.touch(ConnectionId(0), &sub.key(), 1_000);
            let live = health(&r, 600_000);
            assert!(
                !live.stale,
                "{sub}: a quiet tape on a live socket is information, not a fault"
            );
            assert_eq!(live.age_ms, Some(599_000));

            // 3. Socket down.
            r.set_connected(ConnectionId(0), false, 2_000);
            assert!(health(&r, 2_000).stale, "{sub}: socket down");

            // 4. Quarantined — stale even with a fresh ack and a fresh message,
            // because the pool has stopped re-sending the subscription.
            r.set_connected(ConnectionId(0), true, 3_000);
            assert_eq!(
                r.record_failed_session(ConnectionId(0), 1, 3_000)
                    .map(|(s, _)| s),
                Some(sub.clone())
            );
            r.ack(ConnectionId(0), &sub.key());
            r.touch(ConnectionId(0), &sub.key(), 3_000);
            let quarantined = health(&r, 3_000);
            assert!(
                quarantined.quarantined && quarantined.stale,
                "{sub}: quarantined"
            );
        }
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
                r.record_failed_session(ConnectionId(0), 3, strike * 1_000),
                None,
                "strike {strike} must not quarantine yet"
            );
            r.set_connected(ConnectionId(0), false, strike * 1_000);
            assert_eq!(r.resubscribe_set(ConnectionId(0), strike * 1_000).len(), 2);
        }

        r.set_connected(ConnectionId(0), true, 3_000);
        r.ack(ConnectionId(0), &good.key());
        assert_eq!(
            r.record_failed_session(ConnectionId(0), 3, 3_000),
            Some((poison.clone(), 3))
        );
        // The poison is dropped from the resubscribe set; the healthy feed
        // beside it is untouched, so the shard converges instead of looping.
        assert_eq!(
            r.resubscribe_set(ConnectionId(0), 3_000),
            vec![good.clone()]
        );
        assert_eq!(r.subscriptions(ConnectionId(0)), vec![good, poison.clone()]);
        let health = r.health(3_000, &StalenessThresholds::default());
        let poisoned = health
            .iter()
            .find(|h| h.subscription == poison)
            .expect("poison health");
        assert!(poisoned.quarantined && poisoned.stale);
    }

    /// A permanent quarantine is unrecoverable by construction: the feed is
    /// never re-sent, so it is never acked, so the documented "cleared by any
    /// ack" escape can never fire. It must lapse on its own — and lapse later
    /// each time, so a genuinely poisoned subscription still converges.
    #[test]
    fn a_quarantine_lapses_and_widens() {
        let mut r = SubscriptionRegistry::new(1, 10);
        let poison = Subscription::Bbo {
            coin: "NOTACOIN".into(),
        };
        r.place(poison.clone()).expect("place");
        r.set_connected(ConnectionId(0), true, 0);
        assert_eq!(r.record_failed_session(ConnectionId(0), 2, 0), None);
        assert_eq!(
            r.record_failed_session(ConnectionId(0), 2, 0),
            Some((poison.clone(), 2))
        );

        // First quarantine: one minute.
        assert_eq!(r.resubscribe_set(ConnectionId(0), 59_999), vec![]);
        assert_eq!(r.take_expired_quarantines(ConnectionId(0), 59_999), vec![]);
        assert_eq!(
            r.resubscribe_set(ConnectionId(0), 60_000),
            vec![poison.clone()],
            "a lapsed quarantine is sent again"
        );
        assert_eq!(
            r.take_expired_quarantines(ConnectionId(0), 60_000),
            vec![poison.clone()],
            "and is handed to the live session to re-send"
        );
        assert!(!r.health(60_000, &StalenessThresholds::default())[0].quarantined);
        assert_eq!(
            r.take_expired_quarantines(ConnectionId(0), 60_000),
            vec![],
            "cleared once, not once per tick"
        );

        // Poisons the socket again: re-quarantined on the *first* failure this
        // round, because the strikes behind it were never forgiven, and for
        // twice as long.
        assert_eq!(
            r.record_failed_session(ConnectionId(0), 2, 60_000),
            Some((poison.clone(), 3))
        );
        assert_eq!(r.resubscribe_set(ConnectionId(0), 179_999), vec![]);
        assert_eq!(
            r.resubscribe_set(ConnectionId(0), 180_000),
            vec![poison.clone()],
            "second quarantine lasts 120 s, not 60 s"
        );
    }

    /// The operator's documented way back: `SubscriptionQuarantined` tells them
    /// to fix the subscription, so there has to be something to do afterwards.
    #[test]
    fn an_operator_can_clear_a_quarantine() {
        let mut r = SubscriptionRegistry::new(1, 10);
        let sub = Subscription::Bbo {
            coin: "NOTACOIN".into(),
        };
        r.place(sub.clone()).expect("place");
        r.set_connected(ConnectionId(0), true, 0);
        assert!(r.record_failed_session(ConnectionId(0), 1, 0).is_some());
        assert_eq!(r.resubscribe_set(ConnectionId(0), 1_000), vec![]);

        assert_eq!(r.clear_quarantine(&sub), Ok(ConnectionId(0)));
        assert_eq!(r.resubscribe_set(ConnectionId(0), 1_000), vec![sub.clone()]);
        assert!(!r.health(1_000, &StalenessThresholds::default())[0].quarantined);
        // The strikes go with it: the next failure starts a fresh count.
        assert_eq!(r.record_failed_session(ConnectionId(0), 3, 1_000), None);

        assert_eq!(
            r.clear_quarantine(&Subscription::Bbo { coin: "ETH".into() }),
            Err(PoolError::NotSubscribed("bbo:ETH".to_owned()))
        );
    }

    #[test]
    fn an_acknowledged_subscription_never_accumulates_strikes() {
        let mut r = SubscriptionRegistry::new(1, 10);
        let sub = Subscription::Bbo { coin: "BTC".into() };
        r.place(sub.clone()).expect("place");
        for round in 0..10 {
            r.set_connected(ConnectionId(0), true, round * 1_000);
            r.ack(ConnectionId(0), &sub.key());
            assert_eq!(
                r.record_failed_session(ConnectionId(0), 3, round * 1_000),
                None
            );
            r.set_connected(ConnectionId(0), false, round * 1_000);
        }
        assert_eq!(r.resubscribe_set(ConnectionId(0), 10_000), vec![sub]);
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
        // The venue sends no timestamp on this channel (fair-value.md §14.1),
        // so the event carries oppen's own arrival stamp instead.
        assert_eq!(*received_at_ms, 1_700_000_000_000);
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

    /// `bbo` is decoded by `crate::types::Bbo`, not by a second declaration of
    /// the same wire shape living in this module. The one-sided book is the
    /// case that proves it: those assertions live on the shared type, and the
    /// duplicate never had them.
    #[test]
    fn a_one_sided_bbo_frame_keeps_the_side_that_is_there() {
        let Ok(Incoming::Event(WsEvent::Bbo {
            coin,
            venue_time_ms,
            bid,
            ask,
        })) = parse_message(fixtures::BBO_BID_ONLY, &ctx())
        else {
            panic!("expected a bbo event");
        };
        assert_eq!(coin, "FRIEND");
        assert_eq!(venue_time_ms, 1_788_490_146_994);
        assert_eq!(bid.expect("bid").px, dec("1.0"));
        assert!(ask.is_none(), "an empty side is None, never zero");

        // The shared type defaults a missing `bbo` rather than failing the
        // whole frame — §14.4 correction 2's standing lesson, and the tolerance
        // the deleted duplicate did not carry.
        let Ok(Incoming::Event(WsEvent::Bbo { bid, ask, .. })) = parse_message(
            r#"{"channel":"bbo","data":{"coin":"FRIEND","time":1788490146994}}"#,
            &ctx(),
        ) else {
            panic!("one over-strict field must not kill the whole payload");
        };
        assert!(bid.is_none() && ask.is_none());
    }

    // -- the pre-sign gate ----------------------------------------------------

    /// `l2Book` pushes at a 5.4 s median (§14.4 correction 4), so §5.2's 2 s
    /// book budget — which correction 4 moved to `bbo` — marks more than half
    /// of all depth samples stale by arithmetic.
    #[test]
    fn depth_has_its_own_budget_and_bbo_keeps_the_two_second_one() {
        let t = StalenessThresholds::default();
        let depth = Subscription::L2Book { coin: "BTC".into() };
        let micro = Subscription::Bbo { coin: "BTC".into() };
        assert_eq!(depth.staleness_threshold(&t), Some(Duration::from_secs(15)));
        assert_eq!(micro.staleness_threshold(&t), Some(Duration::from_secs(2)));

        let mut r = SubscriptionRegistry::new(1, 10);
        r.place(depth.clone()).expect("place");
        r.set_connected(ConnectionId(0), true, 0);
        r.ack(ConnectionId(0), &depth.key());
        r.touch(ConnectionId(0), &depth.key(), 0);
        // The measured median gap, and then some.
        assert!(
            !r.health(5_451, &t)[0].stale,
            "the measured median push gap"
        );
        assert!(!r.health(15_000, &t)[0].stale);
        assert!(r.health(15_001, &t)[0].stale, "silent past its own budget");
    }

    /// The gate names the feeds a decision depended on. An unrelated one — the
    /// depth ladder that item 20's `preflight` book walk needs, say — must not
    /// block an order priced off `bbo`, and a feed the pool was never asked to
    /// carry must block, because absence is the most complete blindness there
    /// is.
    #[test]
    fn the_pre_sign_gate_blocks_on_named_feeds_only() {
        let t = StalenessThresholds::default();
        let user = addr("0x0000000000000000000000000000000000000009");
        let micro = Subscription::Bbo { coin: "BTC".into() };
        let depth = Subscription::L2Book { coin: "BTC".into() };
        let fills = Subscription::UserFills { user };

        let mut r = SubscriptionRegistry::new(1, 10);
        for sub in [&micro, &depth, &fills] {
            r.place(sub.clone()).expect("place");
            r.set_connected(ConnectionId(0), true, 0);
        }
        for sub in [&micro, &depth, &fills] {
            r.ack(ConnectionId(0), &sub.key());
            r.touch(ConnectionId(0), &sub.key(), 0);
        }
        assert_eq!(
            r.stale_feeds(0, &t, &[micro.clone(), fills.clone()]),
            vec![]
        );

        // Depth goes silent past its 15 s budget. It blocks a caller that named
        // it and nobody else.
        let late = 20_000;
        r.touch(ConnectionId(0), &micro.key(), late);
        r.touch(ConnectionId(0), &fills.key(), late);
        assert_eq!(
            r.stale_feeds(late, &t, &[micro.clone(), fills.clone()]),
            vec![],
            "an unrelated stale depth ladder must not block a signature"
        );
        let blocked = r.stale_feeds(late, &t, &[micro.clone(), depth.clone()]);
        assert_eq!(blocked.len(), 1);
        let FeedBlock::Stale(depth_block) = &blocked[0] else {
            panic!("a subscribed feed past its budget is Stale, not NotSubscribed");
        };
        assert_eq!(depth_block.subscription, depth);
        assert_eq!(
            blocked[0].to_string(),
            "l2Book:BTC: silent 20000 ms of 15000 ms"
        );

        // A feed nobody subscribed blocks: the pool cannot report an age for
        // something it was never asked to watch.
        let unwatched = Subscription::UserFills {
            user: addr("0x0000000000000000000000000000000000000001"),
        };
        assert_eq!(
            r.stale_feeds(late, &t, std::slice::from_ref(&unwatched)),
            vec![FeedBlock::NotSubscribed(unwatched.clone())]
        );
        assert_eq!(
            r.stale_feeds(late, &t, &[unwatched])[0].to_string(),
            "userFills:0x0000000000000000000000000000000000000001: not subscribed"
        );

        // And the socket dying blocks everything on it, user channels included.
        r.set_connected(ConnectionId(0), false, late);
        let down = r.stale_feeds(late, &t, &[fills.clone(), micro.clone()]);
        assert_eq!(down.len(), 2);
        let FeedBlock::Stale(first) = &down[0] else {
            panic!("a subscribed feed on a dead socket is Stale, not NotSubscribed");
        };
        assert_eq!(first.subscription, micro, "blocks are in key order");
        assert_eq!(down[1].to_string(), format!("{fills}: socket down"));
    }

    // -- the reconnect state machine ------------------------------------------

    fn dropped(at_ms: u64, last_seen_ms: Option<u64>, lived_ms: u64) -> Turn {
        Turn::SessionDropped {
            at_ms,
            last_seen_ms,
            reason: "stream ended".to_owned(),
            lived: Duration::from_millis(lived_ms),
            grace: Duration::from_secs(5),
        }
    }

    /// The app-launch and laptop-resume case: an outage *before* the first
    /// successful session emitted a `Disconnected` and never a matching
    /// `Reconnected`, because the emission was keyed on "did a session ever
    /// succeed". The item 34 latch then never released and item 9's
    /// `userFillsByTime` backfill for that window never ran.
    #[test]
    fn an_outage_before_the_first_session_still_reports_a_reconnect() {
        let mut s = ConnState::new(0);
        assert_eq!(
            next_step(
                &mut s,
                Turn::ConnectFailed {
                    at_ms: 1_000,
                    reason: "dns".to_owned()
                }
            ),
            Step::ReportDown {
                reason: "dns".to_owned(),
                strike: false
            }
        );
        // Retries of the same outage are silent.
        assert_eq!(
            next_step(
                &mut s,
                Turn::ConnectFailed {
                    at_ms: 1_500,
                    reason: "dns".to_owned()
                }
            ),
            Step::Idle
        );
        // The network comes back.
        assert_eq!(
            next_step(&mut s, Turn::Subscribed { at_ms: 9_000 }),
            Step::ReportUp {
                gap: GapWindow {
                    start_ms: 1_000,
                    end_ms: 9_000
                },
                attempts: 2
            },
            "a never-connected pool anchors the gap at the first Disconnected"
        );
        // And the next outage is announced again, exactly once.
        assert_eq!(
            next_step(&mut s, dropped(10_000, Some(9_500), 1_000)),
            Step::ReportDown {
                reason: "stream ended".to_owned(),
                strike: true
            }
        );
        assert_eq!(
            next_step(&mut s, Turn::Subscribed { at_ms: 11_000 }),
            Step::ReportUp {
                gap: GapWindow {
                    start_ms: 9_500,
                    end_ms: 11_000
                },
                // Still counting up: no session has proven itself yet.
                attempts: 3
            },
            "the gap anchors on the last message before the drop, not on the drop"
        );
    }

    /// The backoff used to reset on TCP connect, so a connect-then-drop loop
    /// reconnected every ~500 ms forever across every shard — a self-inflicted
    /// burst against the address budget item 10 says must keep headroom for
    /// risk-reducing actions.
    #[test]
    fn a_connect_then_drop_loop_keeps_backing_off() {
        let mut s = ConnState::new(0);
        for round in 1..=4u32 {
            let at_ms = u64::from(round) * 100;
            assert_eq!(
                next_step(&mut s, Turn::Subscribed { at_ms }),
                if round == 1 {
                    Step::Idle
                } else {
                    Step::ReportUp {
                        gap: GapWindow {
                            start_ms: at_ms - 100,
                            end_ms: at_ms,
                        },
                        attempts: round - 1,
                    }
                }
            );
            // Dies inside the subscribe round trip: unproven.
            next_step(&mut s, dropped(at_ms, None, 40));
            assert_eq!(s.attempt, round, "an unproven session must not reset");
            assert_eq!(s.backoff_index(), round - 1);
        }
        let curve = Backoff::default();
        assert_eq!(
            curve.nominal(s.backoff_index()),
            Duration::from_millis(4_000)
        );

        // A session that proves itself resets the curve.
        next_step(&mut s, Turn::Subscribed { at_ms: 500 });
        next_step(&mut s, dropped(60_000, Some(59_000), 59_500));
        assert_eq!(s.attempt, 0);
        assert_eq!(curve.nominal(s.backoff_index()), curve.base);
    }

    /// A strike is evidence, not bookkeeping: only a session that died inside
    /// the subscribe round trip says anything about a subscription. Three
    /// ordinary drops that happened to land there used to quarantine a healthy
    /// feed permanently.
    #[test]
    fn only_a_session_that_died_in_the_subscribe_window_charges_a_strike() {
        let mut s = ConnState::new(0);
        next_step(&mut s, Turn::Subscribed { at_ms: 0 });
        assert_eq!(
            next_step(&mut s, dropped(1_000, None, 4_999)),
            Step::ReportDown {
                reason: "stream ended".to_owned(),
                strike: true
            }
        );
        next_step(&mut s, Turn::Subscribed { at_ms: 2_000 });
        assert_eq!(
            next_step(&mut s, dropped(3_000, None, 5_000)),
            Step::ReportDown {
                reason: "stream ended".to_owned(),
                strike: false
            },
            "a session that outlived the subscribe window is not evidence"
        );
        // Neither is a failure of ours rather than the venue's.
        next_step(&mut s, Turn::Subscribed { at_ms: 4_000 });
        assert_eq!(
            next_step(
                &mut s,
                Turn::ResubscribeFailed {
                    at_ms: 5_000,
                    last_seen_ms: None,
                    reason: "send failed".to_owned()
                }
            ),
            Step::ReportDown {
                reason: "send failed".to_owned(),
                strike: false
            }
        );
    }

    // -- pool wiring ----------------------------------------------------------

    #[tokio::test]
    async fn pool_tracks_capacity_and_refuses_over_it() {
        let cfg = WsPoolConfig {
            // No socket is opened for a subscription the registry refuses, and
            // these coins never reach one: the placement decision is local.
            max_connections: 1,
            max_subs_per_connection: 2,
            ..WsPoolConfig::default()
        };
        let (pool, _rx) = WsPool::new(cfg).expect("inside a runtime");
        pool.subscribe(Subscription::Bbo { coin: "BTC".into() })
            .expect("first");
        pool.subscribe(Subscription::Bbo { coin: "BTC".into() })
            .expect("repeat is a no-op");
        // The repeat took no slot: a second distinct feed still fits the cap
        // of two, and a third does not.
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
        let (pool, _rx) = WsPool::new(WsPoolConfig::default()).expect("runtime");
        assert_eq!(
            pool.unsubscribe(&Subscription::Bbo { coin: "BTC".into() }),
            Err(PoolError::NotSubscribed("bbo:BTC".to_owned()))
        );
        assert_eq!(
            pool.clear_quarantine(&Subscription::Bbo { coin: "BTC".into() }),
            Err(PoolError::NotSubscribed("bbo:BTC".to_owned()))
        );
        pool.shutdown();
    }

    /// `WsPool::subscribe` used to abort with tokio's "there is no reactor
    /// running" on the first subscription when the pool had been built on a
    /// plain thread — a synchronous Tauri command thread being exactly the
    /// caller its own doc invited. A panic on an input path is a blocking
    /// defect (`AGENTS.md` conventions), so the runtime is now required where
    /// failing is typed and cheap.
    #[test]
    fn a_pool_built_outside_a_runtime_is_refused_not_a_panic() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let refused = WsPool::new(WsPoolConfig::default())
            .map(|_| ())
            .expect_err("no runtime here");
        assert_eq!(refused, PoolError::NoRuntime);
        assert_eq!(
            refused.to_string(),
            "websocket pool needs a tokio runtime handle; construct it from inside a runtime"
        );
    }

    /// A pool watching nothing is not a healthy pool. `any_stale` is the
    /// console summary, so it reads degraded rather than green.
    #[tokio::test]
    async fn an_empty_pool_reads_degraded() {
        let (pool, _rx) = WsPool::new(WsPoolConfig::default()).expect("runtime");
        assert!(pool.health().is_empty());
        assert!(pool.any_stale(), "a pool watching nothing is not healthy");
        // And the pre-sign gate blocks on anything it is asked about.
        let sub = Subscription::Bbo { coin: "BTC".into() };
        assert_eq!(
            pool.stale_feeds(std::slice::from_ref(&sub)),
            vec![FeedBlock::NotSubscribed(sub.clone())]
        );
        // A subscribed but not-yet-connected feed also blocks.
        pool.subscribe(sub.clone()).expect("subscribe");
        let blocked = pool.stale_feeds(std::slice::from_ref(&sub));
        assert_eq!(blocked.len(), 1);
        assert!(matches!(blocked[0], FeedBlock::Stale(_)));
        assert!(pool.any_stale());
        pool.shutdown();
    }

    // -- connection task internals --------------------------------------------

    #[tokio::test]
    async fn drain_stops_a_backpressured_connection_without_consuming_queued_events() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(fixtures::BBO.to_owned()))
                .await
                .unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let (pool, mut rx) = WsPool::new(WsPoolConfig::default()).unwrap();
        for _ in 0..EVENT_BUFFER {
            pool.events
                .try_send(WsEvent::VenueError {
                    connection: ConnectionId(0),
                    message: "queued fixture".into(),
                })
                .unwrap();
        }
        pool.subscribe_to(
            Subscription::Bbo { coin: "BTC".into() },
            &format!("ws://{addr}"),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while pool.health()[0].last_message_ms.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection did not reach blocked event send");
        tokio::time::timeout(Duration::from_secs(1), pool.shutdown_and_drain())
            .await
            .unwrap()
            .unwrap();
        assert!(pool.health().iter().all(|feed| !feed.connected));
        for _ in 0..EVENT_BUFFER {
            assert!(matches!(rx.try_recv(), Ok(WsEvent::VenueError { .. })));
        }
        assert!(rx.try_recv().is_err(), "a writer survived drain");
        pool.shutdown_and_drain().await.unwrap();
        drop(pool);
        assert!(rx.recv().await.is_none());
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn dropping_pool_cancels_a_stalled_handshake_and_breaks_the_inner_cycle() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted, ready) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            accepted.send(()).unwrap();
            let _ = tcp.read_to_end(&mut Vec::new()).await;
        });
        let (pool, mut rx) = WsPool::new(WsPoolConfig::default()).unwrap();
        let inner = Arc::downgrade(&pool.inner);
        pool.subscribe_to(
            Subscription::Bbo { coin: "BTC".into() },
            &format!("ws://{addr}"),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), ready)
            .await
            .unwrap()
            .unwrap();
        drop(pool);
        tokio::time::timeout(Duration::from_secs(1), async {
            while inner.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
            assert!(rx.recv().await.is_none());
        })
        .await
        .expect("connection retained its own shutdown sender");
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_publication_and_shutdown_are_one_admission_boundary() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        for _ in 0..32 {
            let (pool, _rx) = WsPool::new(WsPoolConfig::default()).unwrap();
            let pool = Arc::new(pool);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let subscribing = pool.clone();
            let start = barrier.clone();
            let thread_url = url.clone();
            let thread = std::thread::spawn(move || {
                start.wait();
                subscribing.subscribe_to(Subscription::Bbo { coin: "BTC".into() }, &thread_url)
            });
            barrier.wait();
            pool.shutdown_and_drain().await.unwrap();
            let result = thread.join().unwrap();
            assert!(result.is_ok() || result == Err(PoolError::Shutdown));
            let guard = lock(&pool.inner);
            assert!(guard.shutdown);
            assert!(guard.tasks.iter().all(|task| task.abort.is_finished()));
            drop(guard);
            assert_eq!(
                pool.subscribe_to(Subscription::Bbo { coin: "ETH".into() }, &url),
                Err(PoolError::Shutdown)
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropped_and_concurrent_drain_waiters_retain_actual_completion_and_panic_result() {
        struct HeldDrop {
            entered: Arc<tokio::sync::Notify>,
            release: std::sync::mpsc::Receiver<()>,
            completed: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Drop for HeldDrop {
            fn drop(&mut self) {
                self.entered.notify_one();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                self.completed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let (pool, _rx) = WsPool::new(WsPoolConfig::default()).unwrap();
        let pool = Arc::new(pool);
        let started = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, wait) = std::sync::mpsc::channel();
        let panicked = Arc::new(tokio::sync::Notify::new());
        {
            let mut guard = lock(&pool.inner);
            let panicked = panicked.clone();
            pool.spawn_connection(&mut guard, ConnectionId(0), async move {
                panicked.notify_one();
                panic!("synthetic connection failure");
            });
            let started = started.clone();
            let held = HeldDrop {
                entered: entered.clone(),
                release: wait,
                completed: completed.clone(),
            };
            pool.spawn_connection(&mut guard, ConnectionId(1), async move {
                let _held = held;
                started.notify_one();
                std::future::pending::<()>().await;
            });
        }
        started.notified().await;
        panicked.notified().await;
        let draining = pool.clone();
        let first = tokio::spawn(async move { draining.shutdown_and_drain().await });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let second_pool = pool.clone();
        let mut second = tokio::spawn(async move { second_pool.shutdown_and_drain().await });
        let third_pool = pool.clone();
        let mut third = tokio::spawn(async move { third_pool.shutdown_and_drain().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut second)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut third)
                .await
                .is_err()
        );
        assert!(!completed.load(std::sync::atomic::Ordering::SeqCst));
        release.send(()).unwrap();
        let error = second.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            PoolError::ConnectionTask {
                connection: ConnectionId(0),
                ..
            }
        ));
        assert_eq!(third.await.unwrap(), Err(error.clone()));
        assert_eq!(pool.shutdown_and_drain().await, Err(error));
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
    }

    fn test_inner(registry: SubscriptionRegistry) -> Arc<Mutex<PoolInner>> {
        Arc::new(Mutex::new(PoolInner {
            registry,
            conns: Vec::new(),
            shutdown: false,
            tasks: Vec::new(),
        }))
    }

    /// The one ordering rule the disconnect path depends on: `unacked` and the
    /// strike both read the ack state, and `set_connected(false)` clears it.
    /// Read first, flip last — otherwise every feed reads unacked and the
    /// strike lands on whichever healthy subscription sorts first.
    #[tokio::test]
    async fn report_disconnect_reads_unacked_before_clearing_acks() {
        let mut registry = SubscriptionRegistry::new(1, 10);
        let good = Subscription::Bbo { coin: "BTC".into() };
        let poison = Subscription::Bbo {
            coin: "NOTACOIN".into(),
        };
        registry.place(good.clone()).expect("place");
        registry.place(poison.clone()).expect("place");
        registry.set_connected(ConnectionId(0), true, 0);
        registry.ack(ConnectionId(0), &good.key());
        let inner = test_inner(registry);
        let (tx, mut rx) = mpsc::channel(8);
        let cfg = WsPoolConfig {
            quarantine_after: 1,
            ..WsPoolConfig::default()
        };

        assert!(
            report_disconnect(
                ConnectionId(0),
                &cfg,
                &inner,
                &tx,
                "closed".to_owned(),
                true
            )
            .await
        );

        let Some(WsEvent::Disconnected(d)) = rx.recv().await else {
            panic!("expected a Disconnected");
        };
        assert_eq!(
            d.unacked,
            vec![poison.clone()],
            "the acked feed is not a suspect"
        );
        assert_eq!(d.subscriptions, vec![good.clone(), poison.clone()]);
        assert_eq!(d.reason, "closed");
        let Some(WsEvent::SubscriptionQuarantined { subscription, .. }) = rx.recv().await else {
            panic!("expected a SubscriptionQuarantined");
        };
        assert_eq!(
            subscription, poison,
            "the strike must not land on the healthy feed"
        );
        // And the flip happened: the socket is down and no ack survives it.
        let guard = lock(&inner);
        assert!(
            guard
                .registry
                .health(0, &cfg.thresholds)
                .iter()
                .all(|h| !h.connected && !h.acked)
        );
    }

    /// A local send failure is ours, not the venue's, so it charges nobody.
    #[tokio::test]
    async fn a_disconnect_without_a_strike_quarantines_nothing() {
        let mut registry = SubscriptionRegistry::new(1, 10);
        let sub = Subscription::Bbo { coin: "BTC".into() };
        registry.place(sub.clone()).expect("place");
        registry.set_connected(ConnectionId(0), true, 0);
        let inner = test_inner(registry);
        let (tx, mut rx) = mpsc::channel(8);
        let cfg = WsPoolConfig {
            quarantine_after: 1,
            ..WsPoolConfig::default()
        };

        assert!(
            report_disconnect(
                ConnectionId(0),
                &cfg,
                &inner,
                &tx,
                "send failed".to_owned(),
                false
            )
            .await
        );
        assert!(matches!(rx.recv().await, Some(WsEvent::Disconnected(_))));
        assert!(rx.try_recv().is_err(), "no quarantine without evidence");
        assert_eq!(
            lock(&inner).registry.resubscribe_set(ConnectionId(0), 0),
            vec![sub]
        );
    }

    /// Frame handling drives the registry: an ack marks the feed acknowledged,
    /// data touches both the feed and the connection clock (the gap anchor),
    /// and an unparseable frame is reported rather than dropped.
    #[tokio::test]
    async fn handle_text_acks_touches_and_reports() {
        let mut registry = SubscriptionRegistry::new(1, 10);
        let bbo = Subscription::Bbo { coin: "BTC".into() };
        registry.place(bbo.clone()).expect("place");
        registry.set_connected(ConnectionId(0), true, 0);
        let inner = test_inner(registry);
        let (tx, mut rx) = mpsc::channel(8);
        let thresholds = StalenessThresholds::default();

        assert!(handle_text(ConnectionId(0), fixtures::ACK_BBO, &inner, &tx).await);
        assert!(rx.try_recv().is_err(), "an ack is not a consumer event");
        assert!(lock(&inner).registry.health(0, &thresholds)[0].acked);

        assert!(handle_text(ConnectionId(0), fixtures::BBO, &inner, &tx).await);
        assert!(matches!(rx.recv().await, Some(WsEvent::Bbo { .. })));
        let anchor = lock(&inner)
            .registry
            .connection_last_message(ConnectionId(0));
        assert!(anchor.is_some(), "every frame moves the gap anchor");
        assert!(
            lock(&inner).registry.health(0, &thresholds)[0]
                .last_message_ms
                .is_some()
        );

        // A pong proves the socket is alive without being an event.
        assert!(handle_text(ConnectionId(0), fixtures::PONG, &inner, &tx).await);
        assert!(rx.try_recv().is_err());

        assert!(
            handle_text(
                ConnectionId(0),
                r#"{"channel":"bbo","data":{}}"#,
                &inner,
                &tx
            )
            .await
        );
        let Some(WsEvent::MessageDropped { channel, .. }) = rx.recv().await else {
            panic!("a frame that cannot be understood must be reported, not swallowed");
        };
        assert_eq!(channel, "bbo");
    }

    /// A websocket server on loopback, so `session` gets real frames without
    /// leaving the machine. Sends `script`, records every text frame the client
    /// sends, and answers `PING_FRAME` with a pong when `answer_pings`.
    async fn scripted_server(
        script: Vec<Message>,
        record: Arc<Mutex<Vec<String>>>,
        answer_pings: bool,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let mut server = tokio_tungstenite::accept_async(tcp)
                .await
                .expect("server handshake");
            for frame in script {
                if server.send(frame).await.is_err() {
                    return;
                }
            }
            while let Some(Ok(message)) = server.next().await {
                if let Message::Text(text) = message {
                    record
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(text.clone());
                    if text == PING_FRAME
                        && answer_pings
                        && server
                            .send(Message::Text(fixtures::PONG.to_owned()))
                            .await
                            .is_err()
                    {
                        return;
                    }
                }
            }
        });
        addr
    }

    fn connected_registry(sub: &Subscription) -> Arc<Mutex<PoolInner>> {
        let mut registry = SubscriptionRegistry::new(1, 10);
        registry.place(sub.clone()).expect("place");
        registry.set_connected(ConnectionId(0), true, now_ms());
        test_inner(registry)
    }

    /// Frames that arrived before a close still reach the consumer, and the
    /// close itself ends the session as a drop rather than a shutdown — the
    /// difference between reconnecting and going dark.
    #[tokio::test]
    async fn session_pumps_frames_then_reports_a_server_close_as_a_drop() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let addr = scripted_server(
            vec![
                Message::Text(fixtures::BBO.to_owned()),
                Message::Close(None),
            ],
            Arc::clone(&record),
            true,
        )
        .await;
        let (mut stream, _) = connect_async(format!("ws://{addr}"))
            .await
            .expect("client connect");

        let bbo = Subscription::Bbo { coin: "BTC".into() };
        let inner = connected_registry(&bbo);
        let (tx, mut rx) = mpsc::channel(8);
        let (_cmd_tx, mut cmds) = mpsc::unbounded_channel();
        let cfg = WsPoolConfig::default();

        let end = session(ConnectionId(0), &mut stream, &mut cmds, &tx, &inner, &cfg).await;
        let SessionEnd::Dropped(reason) = end else {
            panic!("a server close is a drop, not a shutdown");
        };
        assert!(reason.starts_with("server closed"), "{reason}");
        assert!(matches!(rx.recv().await, Some(WsEvent::Bbo { .. })));
        assert!(
            lock(&inner)
                .registry
                .connection_last_message(ConnectionId(0))
                .is_some(),
            "the frame moved the gap anchor"
        );
    }

    /// A TCP connection can black-hole without erroring. The client ping is
    /// what makes that visible, and the idle detector is what ends it — the one
    /// failure the reconnect loop would otherwise never see.
    #[tokio::test]
    async fn session_pings_and_drops_a_black_holed_socket() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let addr = scripted_server(vec![], Arc::clone(&record), false).await;
        let (mut stream, _) = connect_async(format!("ws://{addr}"))
            .await
            .expect("client connect");

        let bbo = Subscription::Bbo { coin: "BTC".into() };
        let inner = connected_registry(&bbo);
        let (tx, _rx) = mpsc::channel(8);
        let (_cmd_tx, mut cmds) = mpsc::unbounded_channel();
        let cfg = WsPoolConfig {
            ping_interval: Duration::from_millis(40),
            idle_timeout: Duration::from_millis(100),
            ..WsPoolConfig::default()
        };

        let end = session(ConnectionId(0), &mut stream, &mut cmds, &tx, &inner, &cfg).await;
        let SessionEnd::Dropped(reason) = end else {
            panic!("a silent socket must end the session");
        };
        assert_eq!(reason, "idle for more than 100 ms");
        let sent = record.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert!(
            sent.iter().any(|f| f == PING_FRAME),
            "the client must ping before giving up: {sent:?}"
        );
    }

    /// The expiry from `a_quarantine_lapses_and_widens` has to be reachable on
    /// a shard that has converged and will therefore never reconnect. The ping
    /// tick is where it is retried.
    #[tokio::test]
    async fn session_retries_a_lapsed_quarantine_on_the_ping_tick() {
        let record = Arc::new(Mutex::new(Vec::new()));
        let addr = scripted_server(vec![], Arc::clone(&record), true).await;
        let (mut stream, _) = connect_async(format!("ws://{addr}"))
            .await
            .expect("client connect");

        let poison = Subscription::Bbo {
            coin: "NOTACOIN".into(),
        };
        let inner = connected_registry(&poison);
        // Quarantined against an epoch-zero clock, so it lapsed decades ago.
        assert!(
            lock(&inner)
                .registry
                .record_failed_session(ConnectionId(0), 1, 0)
                .is_some()
        );
        let (tx, _rx) = mpsc::channel(8);
        let (cmd_tx, mut cmds) = mpsc::unbounded_channel();
        let cfg = WsPoolConfig {
            ping_interval: Duration::from_millis(40),
            idle_timeout: Duration::from_secs(60),
            ..WsPoolConfig::default()
        };
        let stop = cmd_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = stop.send(ConnCommand::Shutdown);
        });

        let end = session(ConnectionId(0), &mut stream, &mut cmds, &tx, &inner, &cfg).await;
        assert!(matches!(end, SessionEnd::Shutdown));
        let sent = record.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert!(
            sent.iter()
                .any(|f| f.contains("subscribe") && f.contains("NOTACOIN")),
            "the lapsed quarantine must be re-sent: {sent:?}"
        );
        assert!(
            !lock(&inner).registry.health(now_ms(), &cfg.thresholds)[0].quarantined,
            "and cleared once retried"
        );
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
        let cfg = WsPoolConfig {
            network: Network::Mainnet,
            ..Default::default()
        };
        let (pool, mut rx) = WsPool::new(cfg).expect("runtime");
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
            network: Network::Mainnet,
            ..WsPoolConfig::default()
        };
        let (pool, mut rx) = WsPool::new(cfg).expect("runtime");
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
                        r.gap.end_ms.saturating_sub(r.gap.start_ms),
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
        assert!(
            reconnect.gap.end_ms > reconnect.gap.start_ms,
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
