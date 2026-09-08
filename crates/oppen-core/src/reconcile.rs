//! Closing the gaps a dropped socket leaves in the record.
//!
//! `docs/spec.md` item 9: "HL has no server-side cursor — the ledger is the
//! app's own and must never silently drop a fill across a laptop sleep." The
//! websocket pool already records the window it missed
//! ([`oppen_hl::ws::GapWindow`], written to `feed_gaps` by
//! [`Ledger::open_gap`]), and the ledger already holds the chain. Nothing
//! joined the two. This module is that join, and it is the P2 gate: **zero
//! fills lost across a 30 second disconnect**.
//!
//! Four properties it exists to hold.
//!
//! * **Idempotence, keyed on the venue's own identifiers, enforced by the
//!   database.** The window is re-walked on every reconnect, retry and
//!   restart, and the same fill will be offered many times — by the backfill,
//!   by the `userFills` subscribe snapshot, and by an overlapping page (the
//!   venue's `startTime` is *inclusive*, measured). Nothing may be recorded
//!   twice, and the only place a check and a write are one operation is the
//!   storage engine: `events.idem_key` carries `fill:<account>:<tid>` under a
//!   partial unique index, so [`Ledger::record_fill`] either writes the row or
//!   reports it already recorded, atomically. This module holds **no** index
//!   of what it has seen. It cannot, safely: any such index is consulted
//!   before the append, and the interval between the two is exactly when a
//!   resumed socket replays its subscribe snapshot.
//! * **A fill is never dropped for failing to match.** Every fill is
//!   classified `Attribution::Attributed`, `Attribution::Manual` or
//!   `Attribution::External` and recorded either way
//!   (`docs/specs/history.md` §2, `docs/spec.md` item 33). An unmatched fill
//!   is a [`Finding`], not an error: it means the operator traded elsewhere,
//!   or was liquidated, and both belong in the history.
//! * **After a timeout the only safe move is query-by-cloid.** `docs/spec.md`
//!   item 19. `UnknownOutcome` is the type an unresolved order arrives as, and
//!   its only method is a query — see that type for why it has no `resend`.
//! * **A gap closes only once its window is proven contiguous.** A page that
//!   cannot be advanced, a request that failed, or a walk that ran past its
//!   page budget leaves `reconciled_ts_ms` null, so the staleness overlay
//!   (`docs/spec.md` item 34) stays up and the work list keeps the gap. One
//!   gap that cannot be closed does not stop the others: `reconcile_all`
//!   carries the failure in that gap's [`GapStatus::Failed`] and keeps going,
//!   because a permanently failing window must not hold every later window
//!   hostage.
//! * **The window is venue time, never local time.** `feed_gaps` stores the
//!   local wall clock, `userFillsByTime` answers in the venue's. Handing one
//!   to the other shifts the window by exactly the clock skew, and a fill the
//!   shifted window missed is lost silently because the gap is marked
//!   reconciled anyway. The walk starts from an **anchor** — the newest
//!   instant the venue stamped on a fill the chain already held when the
//!   socket died. No host reading narrows it: the host clock appears only as a
//!   thirty-day outer bound, past which there is no anchor at all and the walk
//!   falls back to [`FIRST_RUN_LOOKBACK_MS`]. See [`outage_window`].
//!
//! # What the venue actually does
//!
//! Measured against public mainnet `POST /info` on 2026-09-04 (read-only, no
//! key). These are the facts the walk in `backfill_fills` is built on:
//!
//! | Fact | Measurement |
//! |---|---|
//! | `userFillsByTime` returns **oldest first** | a 100,000 s window returned rows in ascending `time` |
//! | it caps a page at `USER_FILLS_PAGE_LIMIT` rows | two separate windows returned exactly 2,000 |
//! | it truncates from the **newest** end, with no cursor | `endTime` 1776800000000 answered with a newest row of 1776775402431 |
//! | `startTime` is **inclusive** | re-requesting from the last row's `time` returned that row again |
//! | **the cap can cut a millisecond in half** | the page ended mid-millisecond: 2 rows at 1776775402431, and the next page returned **28** at that same millisecond |
//! | `userFills` (no window) returns newest first | opposite order to `userFillsByTime`; not used here, and named so nobody swaps one for the other |
//! | `orderStatus` answers `unknownOid` for a retired order | a cloid that certainly traded in April 2026 answers `unknownOid` today |
//!
//! The fifth row is the one that decides the design. Advancing the cursor to
//! `last_row_time + 1` — the obvious way to avoid re-reading the boundary row
//! — would have silently dropped **26 real fills** on that single measured
//! window. So the cursor advances to the last row's timestamp exactly, the
//! overlap is expected, and duplicates are removed by `tid`. The cost is one
//! repeated row per page; the alternative is the failure this whole module
//! exists to prevent.
//!
//! The seventh row is why [`Settlement::Retired`] is documented as *not*
//! proof of absence: the venue retires order records, so `unknownOid` means
//! "the venue no longer knows", never "it never existed". The fill backfill
//! is the authority on whether an order traded.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use rust_decimal::Decimal;
use serde_json::{Value, json};

use oppen_hl::info::OrderRef;
use oppen_hl::types::{Fill, OpenOrder, OrderStatusResponse};
use oppen_hl::wire::Cloid;
use oppen_hl::{Address, Error as VenueError, InfoClient};

use crate::Network;
use crate::ledger::{EventKind, Gap, Ledger, LedgerError, NewFill, now_ms};

/// Rows the venue returns for one `userFillsByTime` request, at most.
///
/// Measured on mainnet 2026-09-04: two different windows over an active
/// account each answered with exactly 2,000 rows and a newest row well inside
/// the requested `endTime`. The page is truncated from the newest end and
/// carries no cursor, so a caller that widens the window instead of paging
/// loses the tail silently — the same shape as
/// [`oppen_hl::types::FUNDING_HISTORY_PAGE_LIMIT`], at four times the size.
const USER_FILLS_PAGE_LIMIT: usize = 2_000;

/// How many pages one window may take before the walk gives up.
///
/// A bound rather than a `loop`: a venue that answers a full page forever
/// would otherwise spin against the rate budget with the operator's overlay
/// still up and no error anywhere. At the measured cap this is a million
/// fills in one gap, which is far past any reconnect and well into "something
/// is wrong". Exceeding it is [`ReconcileError::TooManyPages`] and leaves the
/// gap unreconciled, which is the honest outcome.
const DEFAULT_MAX_PAGES: usize = 512;

/// Prefix of the `feed_gaps.scope` a `userFills` subscription writes.
///
/// This is [`oppen_hl::ws::Subscription::key`]'s spelling for that channel.
/// The pairing is asserted in this module's tests rather than assumed, so a
/// rename in the pool fails a test here instead of silently making every fills
/// gap unrecognisable and permanently unreconciled.
const USER_FILLS_SCOPE_PREFIX: &str = "userFills:";

/// Prefix of the `feed_gaps.scope` an `orderUpdates` subscription writes.
/// See [`USER_FILLS_SCOPE_PREFIX`].
const ORDER_UPDATES_SCOPE_PREFIX: &str = "orderUpdates:";

/// The payload key a fill event stores the venue trade id under.
///
/// Written into the chained body for the audit export and read by the schema's
/// upgrade backfill, which keys rows an older build wrote. Idempotence itself
/// is `events.idem_key`, not this.
const FILL_TID_FIELD: &str = "tid";

/// The payload key a fill event stores its container address under. Read by the
/// same upgrade backfill as [`FILL_TID_FIELD`].
const FILL_ACCOUNT_FIELD: &str = "account";

/// The payload key a fill event stores the **venue's** timestamp under.
const FILL_TS_FIELD: &str = "ts_ms";

/// The payload key an intent or operator action carries its client order id
/// under. See [`Attributions`] for why it is searched at any depth.
const CLOID_FIELD: &str = "cloid";
/// The guardrail clearance's name for the decision-time mark. Spec F calls
/// the same number the *arrival mid*, and the fill row stamps it under that
/// name — the clearance field is hashed into the chain and cannot be renamed,
/// but nothing stops the row that consumes it from using the spec's word.
const REFERENCE_PX_FIELD: &str = "reference_px";

/// How deep [`find_cloid`] will search a payload.
///
/// Bounded because the walk is recursive and a payload is written by another
/// component: a stack overflow is a panic on an input path, which
/// `AGENTS.md` forbids.
const MAX_PAYLOAD_DEPTH: usize = 8;
/// Basis points per unit, for [`slip_bps`].
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// How far either side of the anchor the outage is taken to reach.
///
/// The anchor ([`Ledger::newest_fill_ts_ms`]) is the newest instant the venue
/// stamped on a fill the chain already held when the socket died. It is a real
/// venue instant at which oppen was demonstrably being served, but it is a
/// *stamping* time and delivery is not stamping: the venue stamps a fill when
/// its matching engine books it and delivers it over a socket some time later.
/// Fills booked close together can arrive out of order, and one booked just
/// before the socket died may never have been delivered at all — so a fill
/// stamped slightly **before** the anchor can still be missing from the chain,
/// and the walk has to start slightly before it. The same quantity bounds the
/// far end, where the venue's own instant of the reconnect sits a little past
/// the anchor plus the measured outage.
///
/// Five minutes is far past any delivery reordering the venue exhibits. It is
/// not free and it is not expensive: at the measured page cap of
/// `USER_FILLS_PAGE_LIMIT` rows it is a single request for any account not
/// trading faster than six fills a second, and everything it re-reads is
/// refused by the fill key, so over-reaching costs the request and nothing
/// else. Under-reaching costs a fill, permanently. That asymmetry, not a round
/// number, is why it is minutes rather than seconds.
///
/// It is **not** a clock-skew allowance. The host clock never narrows the
/// window in [`outage_window`] — it only bounds an anchor from above, with
/// thirty days of slack — so there is no skew for these five minutes to
/// absorb.
const VENUE_CLOCK_MARGIN_MS: u64 = 5 * 60 * 1_000;

/// How far back a container with no anchor is walked.
///
/// A gap on an account the chain holds no fill for has no venue instant to
/// reason from at all — the first run of `docs/specs/history.md` §3.2. Walking
/// from the epoch is not the honest answer to that: it spends the page budget
/// on an account's entire history, hits [`ReconcileError::TooManyPages`] on any
/// account that has one, and leaves the gap permanently unclosable. So the
/// first run walks a bounded window instead, ending at the venue's own now and
/// beginning `docs/decisions.md` D-d's thirty days before the host's account of
/// when the socket dropped.
///
/// This is the only quantity a host reading is used for, and it is used with
/// thirty days of slack around it precisely because it cannot be trusted to
/// minutes. [`outage_window`] spends it twice: here, and as the outer bound
/// past which a stored anchor is not believed and this branch is taken instead.
/// What the operator gets in exchange for the bound is a stated boundary:
/// [`Finding::FirstRunWindow`] carries it, history before it is unknown rather
/// than empty (`docs/specs/history.md` §3.2 step 3), and no fill recovered by
/// such a walk is stamped as belonging to the outage, because nothing here
/// establishes when in venue time the outage was.
const FIRST_RUN_LOOKBACK_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// What can go wrong reconciling.
///
/// `AGENTS.md` conventions: `thiserror`, no bare strings, and nothing on an
/// input path panics. Every variant here leaves the gap unreconciled on
/// purpose — the caller retries, and the retry is free because the walk is
/// idempotent.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("feed session is already bound to another network or account")]
    FeedScopeMismatch,
    /// The ledger refused a read or a write.
    #[error("reconcile ledger error: {0}")]
    Ledger(#[from] LedgerError),
    /// The venue refused a query, or the transport failed.
    #[error("reconcile venue error: {0}")]
    Venue(#[from] VenueError),
    /// A full page's rows all share one millisecond, so the cursor cannot
    /// advance without either re-reading forever or skipping past rows that
    /// were never returned.
    ///
    /// Reported rather than worked around because both workarounds lose data:
    /// advancing past the millisecond drops the rows the cap cut off (26 of
    /// them on the one window measured, see the module docs), and not
    /// advancing loops. The operator sees a gap that would not close, which is
    /// true, instead of a history that is quietly short.
    #[error(
        "the fills page at {at_ms} is full ({rows} rows) and every row shares that millisecond: \
         the window cannot be paged without losing rows"
    )]
    PageStalled { at_ms: u64, rows: usize },
    /// The walk hit its page budget. See `ReconcileConfig::max_pages`.
    #[error("backfill from {start_ms} exceeded {max_pages} pages")]
    TooManyPages { start_ms: u64, max_pages: usize },
    /// A gap's record ends before it starts, or carries a timestamp outside
    /// unix milliseconds. Only reachable by editing the database by hand;
    /// refused rather than clamped, because the row is the only statement
    /// oppen has about when it stopped listening, and a reconciler that
    /// invents a replacement for it marks a window done that it never
    /// established. The gap stays open and the overlay stays up.
    #[error("gap {gap_id} has an unusable window {start_ms}..{end_ms}")]
    UnusableWindow {
        gap_id: i64,
        start_ms: i64,
        end_ms: i64,
    },
    /// The ledger's chain and the venue source are on different networks.
    ///
    /// `docs/decisions.md` R4: "a mainnet number that is actually a testnet
    /// number is the worst bug this product can ship". R4 makes the two
    /// networks two database files; this is the other half of it, because a
    /// file boundary does nothing about a mainnet [`InfoClient`] pointed at
    /// the testnet chain. Refused at construction, so no such reconciler
    /// exists to be called.
    #[error(
        "reconcile source is on {venue:?} but this ledger's chain is {ledger:?} \
         (docs/decisions.md R4)"
    )]
    NetworkMismatch {
        ledger: Network,
        /// The network the [`ReconcileSource`] talks to. Not named `source`:
        /// `thiserror` reads that name as the error's cause.
        venue: Network,
    },
    /// A venue timestamp did not fit the ledger's signed milliseconds.
    #[error("venue timestamp {0} is out of range")]
    TimestampOutOfRange(u64),
    /// A gap scope named an address that does not parse.
    #[error("gap scope {scope:?} does not name a valid address")]
    UnreadableScope { scope: String },
}

/// Reconcile result alias.
type Result<T> = std::result::Result<T, ReconcileError>;

/// The venue reads this module needs, and nothing else.
///
/// A trait rather than a bare [`InfoClient`] for one reason: the P2 gate has
/// to be provable without a funded account. Every query here is public and
/// key-free, but a *test* still cannot depend on a live venue having the exact
/// fills a 30-second window needs, so the gate is driven from recorded
/// fixtures through this trait. [`InfoClient`] implements it, so the
/// production path is the same code with a different source.
///
/// It is deliberately four methods wide. `docs/spec.md` item 9 names the three
/// queries, and a wider trait would let this module reach for state it has no
/// business reconciling against. [`ReconcileSource::network`] is the fourth
/// and is not a query: `docs/decisions.md` R4 needs the network a source talks
/// to be readable, and it is the source that knows it.
pub trait ReconcileSource {
    /// Which network this source talks to.
    ///
    /// Checked against [`Ledger::network`] by [`Reconciler::new`], which
    /// refuses the pair when they disagree. It has to come off the source
    /// because a caller repeating the network by hand would only be asserting
    /// the thing R4 is worried about.
    fn network(&self) -> Network;

    /// Fills for `user` from `start_ms`, **oldest first**, capped at
    /// `USER_FILLS_PAGE_LIMIT` rows and truncated from the newest end.
    /// `start_ms` is inclusive, and `end_ms` of `None` means "to the venue's
    /// own now". See the module docs for the measurements.
    fn user_fills_by_time(
        &self,
        user: Address,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> impl Future<Output = std::result::Result<Vec<Fill>, VenueError>> + Send;

    /// Every order resting on the book for `user`, with its cloid where one
    /// was supplied. The authority on what is still live after a gap.
    fn frontend_open_orders(
        &self,
        user: Address,
    ) -> impl Future<Output = std::result::Result<Vec<OpenOrder>, VenueError>> + Send;

    /// One order's state by exchange id or client id (`docs/spec.md` item 19).
    fn order_status(
        &self,
        user: Address,
        order: OrderRef,
    ) -> impl Future<Output = std::result::Result<OrderStatusResponse, VenueError>> + Send;
}

/// The production [`ReconcileSource`]: an [`InfoClient`] that still knows
/// which network it is.
///
/// [`InfoClient`] turns a [`Network`] into a base URL at construction and
/// keeps the URL, so the network cannot be read back off it — and
/// `docs/decisions.md` R4 needs it read back. The client is built here from
/// the network rather than handed in beside it, so the two cannot disagree,
/// and [`Reconciler::new`] then has something to check the ledger against.
/// There is no `ReconcileSource` impl on a bare `InfoClient`: the unchecked
/// pairing is not representable rather than merely discouraged.
#[derive(Debug)]
pub struct VenueSource {
    network: Network,
    info: InfoClient,
}

impl VenueSource {
    /// Build a source for `network`.
    pub fn new(network: Network) -> std::result::Result<Self, VenueError> {
        Ok(VenueSource {
            network,
            info: InfoClient::new(network)?,
        })
    }
}

impl ReconcileSource for VenueSource {
    fn network(&self) -> Network {
        self.network
    }

    async fn user_fills_by_time(
        &self,
        user: Address,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> std::result::Result<Vec<Fill>, VenueError> {
        self.info.user_fills_by_time(user, start_ms, end_ms).await
    }

    async fn frontend_open_orders(
        &self,
        user: Address,
    ) -> std::result::Result<Vec<OpenOrder>, VenueError> {
        self.info.frontend_open_orders(user).await
    }

    async fn order_status(
        &self,
        user: Address,
        order: OrderRef,
    ) -> std::result::Result<OrderStatusResponse, VenueError> {
        self.info.order_status(user, order).await
    }
}

/// How a fill was joined to the record (`docs/specs/history.md` §2).
///
/// The ledger is authoritative for *why* something happened and the venue for
/// *what* happened; this is the outcome of joining them. Every fill gets one,
/// including the ones that match nothing.
#[derive(Debug)]
enum Attribution {
    /// Matched to a recorded [`EventKind::OrderIntent`] by cloid, so the fill
    /// carries the agent, and through the intent row the reason and the
    /// guardrail verdict that allowed it. The intent's hash is carried as well
    /// as its seq so the link survives someone renumbering rows, the same
    /// reasoning as [`crate::ledger::IntentReceipt::hash`].
    Attributed {
        agent_id: String,
        intent_seq: u64,
        intent_hash: String,
        /// Spec F's arrival mid: the price the guardrails measured the order
        /// against, carried from the intent row so the fill can be scored
        /// against the decision that caused it. `None` for an intent that
        /// recorded no price.
        arrival_mid: Option<Decimal>,
    },
    /// Matched to an [`EventKind::OperatorAction`] — the human's own ticket in
    /// oppen (`docs/spec.md` item 33).
    Manual { action_seq: u64 },
    /// No matching intent. Traded outside oppen, or a liquidation nobody
    /// requested. `docs/spec.md` item 33 reserves the `manual · external`
    /// bucket for exactly this, and `docs/specs/history.md` §2 is explicit
    /// that it is a finding rather than an error.
    External,
}

impl Attribution {
    /// The stored discriminator. Hashed into the chain through the payload, so
    /// it is a storage format and stable once written.
    fn as_str(&self) -> &'static str {
        match self {
            Attribution::Attributed { .. } => "attributed",
            Attribution::Manual { .. } => "manual",
            Attribution::External => "external",
        }
    }

    /// The agent a fill is booked against, if any. `None` for manual and
    /// external fills: `docs/decisions.md` R4's sibling rule applies here too,
    /// in that a fabricated agent id in the ledger would be indistinguishable
    /// from a real one.
    fn agent_id(&self) -> Option<&str> {
        match self {
            Attribution::Attributed { agent_id, .. } => Some(agent_id),
            Attribution::Manual { .. } | Attribution::External => None,
        }
    }
}

/// Something worth an operator's attention that is not a failure.
///
/// `docs/specs/history.md` §2: an unmatched fill "means either the operator
/// traded elsewhere or oppen missed an event, and the two are distinguished by
/// whether the ledger has a gap over that window". Both are recorded; neither
/// stops the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A fill that matched no intent and no operator action.
    ///
    /// `cloid` is the one the fill carried, when it had one. A fill with a
    /// cloid that matches nothing is the more interesting case: oppen puts a
    /// cloid on everything it places (item 19), so this is either another
    /// tool's order or a row whose intent has been redacted.
    UnattributedFill {
        tid: u64,
        oid: u64,
        coin: String,
        ts_ms: i64,
        cloid: Option<String>,
    },
    /// An order oppen believed was live is not resting and the venue no longer
    /// knows the cloid. See [`Settlement::Retired`] — this is not proof it
    /// never existed, only that the order record is gone.
    OrderRetired { cloid: String },
    /// A gap was reconciled on a container the chain held no usable anchor
    /// for — no fill at all, or none whose venue stamp can be believed — so the
    /// walk covered [`FIRST_RUN_LOOKBACK_MS`] rather than a window derived from
    /// the venue's own clock.
    ///
    /// `docs/specs/history.md` §3.2 step 3: what is older than the proven
    /// window is "explicitly marked unknown rather than assumed empty". This is
    /// that mark. It is a finding and not an error because closing the gap was
    /// still the right thing to do — the alternative is a gap that can never
    /// close on a fresh install.
    FirstRunWindow { account: String, start_ms: u64 },
}

/// What one backfill recovered.
///
/// Returned rather than logged so the console can state what closing a gap
/// actually produced, and so a test can assert on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Fills the venue returned, after removing the inclusive-boundary
    /// overlap between pages.
    pub fills_seen: usize,
    pub fills_recorded: usize,
    /// Fills already in the chain. Not an anomaly: the window overlaps what
    /// the live feed already delivered, and a retry re-walks it entirely.
    pub duplicates: usize,
    /// Of the recorded fills, how many matched an intent, an operator action,
    /// and nothing at all.
    pub attributed: usize,
    pub manual: usize,
    pub external: usize,
    /// Everything worth reading that was not a failure.
    pub findings: Vec<Finding>,
}

/// An order oppen cannot account for, and the only thing it is allowed to do
/// about it.
///
/// `docs/spec.md` item 19: "the only safe move after `timeout_unknown_outcome`
/// is query-by-cloid, never blind retry." A blind resend after a timeout is
/// how one order becomes two positions, and the venue will happily accept the
/// second one.
///
/// So this type has no `resend`, no `place_again`, no `into_order` and no
/// constructor of its own. It is produced only by [`OrderReconciliation`], it
/// carries the cloid and the account it belongs to, and the single thing a
/// caller can do with it is [`UnknownOutcome::settle`], which is a query. The
/// rule is not a comment somewhere near the retry loop; there is no retry loop
/// to put a comment near.
#[derive(Debug)]
struct UnknownOutcome {
    user: Address,
    cloid: Cloid,
}

impl UnknownOutcome {
    /// Ask the venue what became of it. The only move item 19 allows.
    async fn settle<S: ReconcileSource>(&self, source: &S) -> Result<Settlement> {
        let response = source
            .order_status(self.user, OrderRef::Cloid(self.cloid.clone()))
            .await?;
        Ok(match response {
            OrderStatusResponse::Order { order } => Settlement::Known {
                oid: order.order.oid,
                status: order.status,
                status_ts_ms: order.status_timestamp,
            },
            OrderStatusResponse::UnknownOid => Settlement::Retired,
        })
    }
}

/// What the venue said about an order oppen could not account for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// The venue still has the order and reports its state.
    ///
    /// `status` is the venue's own word: `open`, `filled`, `canceled`,
    /// `triggered`, `rejected`, `marginCanceled`, … Kept verbatim rather than
    /// mapped, because a status this build has not seen must not be silently
    /// folded into one it has.
    Known {
        oid: u64,
        status: String,
        status_ts_ms: u64,
    },
    /// The venue answers `unknownOid`.
    ///
    /// **This is not proof the order never existed.** Measured on mainnet
    /// 2026-09-04: a cloid that demonstrably traded in April 2026 answers
    /// `unknownOid` today, because the venue retires order records. Reading
    /// this as "it never landed, so resend" is precisely the mistake item 19
    /// forbids. The fill backfill is the authority on whether the order
    /// traded; this answer only says the order record is gone.
    Retired,
}

/// The state of an account's orders after a gap.
#[derive(Debug)]
struct OrderReconciliation {
    resting: Vec<OpenOrder>,
    unknown: Vec<UnknownOutcome>,
}

impl OrderReconciliation {
    /// Partition the orders the caller believed were live against what the
    /// venue says is resting.
    ///
    /// Pure and synchronous so the item 19 rule is testable without a network:
    /// anything in `pending` that `frontendOpenOrders` does not list comes back
    /// as an [`UnknownOutcome`], which can only be resolved by a query.
    ///
    /// `pending` is supplied by the caller rather than inferred from the
    /// ledger. Whether an order is still live is the execution layer's own
    /// bookkeeping — an order can rest for a day without producing a single
    /// ledger row — and guessing it here would either miss orders or invent
    /// them.
    fn partition(user: Address, pending: &[Cloid], open_orders: Vec<OpenOrder>) -> Self {
        let resting_cloids: BTreeSet<&str> = open_orders
            .iter()
            .filter_map(|order| order.cloid.as_ref())
            .map(Cloid::as_str)
            .collect();
        // Sorted and deduped by the cloid's own text: the caller's order is
        // not a promise, and `AGENTS.md` invariant 6 wants a deterministic
        // surface. `Cloid` has no `Ord`, so the string is the key.
        let mut by_text: BTreeMap<&str, &Cloid> = BTreeMap::new();
        for cloid in pending {
            by_text.insert(cloid.as_str(), cloid);
        }
        let unknown = by_text
            .into_iter()
            .filter(|(text, _)| !resting_cloids.contains(text))
            .map(|(_, cloid)| UnknownOutcome {
                user,
                cloid: cloid.clone(),
            })
            .collect();
        OrderReconciliation {
            resting: open_orders,
            unknown,
        }
    }

    /// Everything the venue says is on the book.
    fn resting(&self) -> &[OpenOrder] {
        &self.resting
    }

    /// Settle every unknown order by query, in cloid order.
    ///
    /// Sequential rather than concurrent: `docs/spec.md` item 10 gives the
    /// whole address one request budget, and a reconcile that fans out after a
    /// reconnect is the worst moment to spend it.
    async fn settle_all<S: ReconcileSource>(&self, source: &S) -> Result<Vec<(Cloid, Settlement)>> {
        let mut out = Vec::with_capacity(self.unknown.len());
        for order in &self.unknown {
            out.push((order.cloid.clone(), order.settle(source).await?));
        }
        Ok(out)
    }
}

/// What a `feed_gaps.scope` names.
///
/// The pool writes the scope as [`oppen_hl::ws::Subscription::key`], so this is
/// that key read back. Market-data scopes are recognised and returned rather
/// than treated as an error: candle backfill is a different component, and a
/// scope this module cannot prove must not be marked reconciled by it.
#[derive(Debug, PartialEq, Eq)]
enum GapScope {
    /// `userFills:<address>` — the fills feed for one account.
    UserFills(Address),
    /// `orderUpdates:<address>` — the order-state feed for one account.
    OrderUpdates(Address),
    /// Anything else. Not this module's to close.
    Other(String),
}

impl GapScope {
    /// Read a stored scope string.
    ///
    /// A scope that names one of the two account channels but whose address
    /// does not parse is [`ReconcileError::UnreadableScope`] rather than
    /// [`GapScope::Other`]: it is a corrupted row for a feed this module owns,
    /// and silently reclassifying it as somebody else's problem would strand
    /// the gap forever.
    fn parse(scope: &str) -> Result<Self> {
        let unreadable = || ReconcileError::UnreadableScope {
            scope: scope.to_owned(),
        };
        if let Some(rest) = scope.strip_prefix(USER_FILLS_SCOPE_PREFIX) {
            return Ok(GapScope::UserFills(
                Address::parse(rest).map_err(|_| unreadable())?,
            ));
        }
        if let Some(rest) = scope.strip_prefix(ORDER_UPDATES_SCOPE_PREFIX) {
            return Ok(GapScope::OrderUpdates(
                Address::parse(rest).map_err(|_| unreadable())?,
            ));
        }
        Ok(GapScope::Other(scope.to_owned()))
    }

    /// The account this scope covers, if it covers one.
    fn account(&self) -> Option<Address> {
        match self {
            GapScope::UserFills(user) | GapScope::OrderUpdates(user) => Some(*user),
            GapScope::Other(_) => None,
        }
    }
}

/// What happened to one gap.
#[derive(Debug)]
pub enum GapStatus {
    /// The window was walked to a short page and `reconciled_ts_ms` is now
    /// set.
    Reconciled,
    /// The socket has not come back, so oppen is still missing fills as they
    /// happen. Nothing to backfill yet: there is no instant to backfill *to*,
    /// because the feed that would take over from the walk is still down.
    StillOpen,
    /// The scope is not an account feed. Left for whoever owns it, with
    /// `reconciled_ts_ms` untouched.
    NotAnAccountFeed,
    /// This gap could not be closed, and the typed reason why.
    ///
    /// `reconciled_ts_ms` is untouched, so the gap stays on the work list and
    /// `docs/spec.md` item 34's overlay stays up — the honest state. It is a
    /// status rather than a returned error because
    /// [`Reconciler::reconcile_all`] must not let one gap that will never
    /// close stop every later gap from ever being walked.
    Failed(ReconcileError),
}

/// The report for one gap.
#[derive(Debug)]
pub struct GapOutcome {
    pub gap_id: i64,
    pub scope: String,
    pub status: GapStatus,
    /// What the fills walk recovered. Empty for a scope that carries no fills,
    /// and empty for a [`GapStatus::Failed`] gap: rows written before the
    /// failure are already durable and are counted as duplicates on the retry,
    /// so the counts describe a gap that finished, never a partial one.
    pub recovered: Recovered,
    /// How many orders the venue reports resting, for an account scope that
    /// was checked. `None` when no order query was made.
    pub resting_orders: Option<usize>,
    /// Orders the caller believed live that the venue is not resting, settled
    /// by query. `docs/spec.md` item 19 — never resent.
    pub settled: Vec<(String, Settlement)>,
}

/// Knobs with a defensible default each.
#[derive(Debug, Clone, Copy)]
struct ReconcileConfig {
    /// Page budget for one window. See [`DEFAULT_MAX_PAGES`].
    max_pages: usize,
    /// How many rows a full page holds. Overridable only so a test can prove
    /// the split-millisecond and stall behaviours without fabricating 2,000
    /// fixture rows per page; production uses [`USER_FILLS_PAGE_LIMIT`].
    page_limit: usize,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        ReconcileConfig {
            max_pages: DEFAULT_MAX_PAGES,
            page_limit: USER_FILLS_PAGE_LIMIT,
        }
    }
}

/// Where a cloid was seen in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IntentRef {
    seq: u64,
    hash: String,
    agent_id: Option<String>,
    /// The price the guardrails measured this order against, which is spec F's
    /// **arrival mid**: the market as it stood at the instant the decision was
    /// taken, before anything was sent.
    ///
    /// Read here rather than looked up later because [`Attributions::read`]
    /// already has the intent payload open to find the cloid, so the arrival
    /// price costs nothing extra — and because a fill can only be measured
    /// against the decision that caused it, which is the row this join is
    /// about. `None` for a hand-written intent that carries no price.
    arrival_mid: Option<Decimal>,
}

/// The join from a client order id to the row that explains it.
///
/// `docs/specs/history.md` §2: the venue is authoritative for *what* happened
/// and the ledger for *why*, and this is the ledger half. Two questions, one
/// read: which intent claimed this cloid, and which operator ticket did.
///
/// **Read at the moment of use, never cached.** The rows that attribute a fill
/// are written by the execution path, which this module never sees, so a copy
/// kept across calls is stale by construction. A cached copy also needs a
/// cursor, a refresh and an answer for a cursor the ledger has retired — and
/// that answer was an infinite loop on the production path. There is no cursor
/// here to get wrong.
///
/// **The cloid is searched at any depth, up to [`MAX_PAYLOAD_DEPTH`].** The
/// payload of an intent row is written by the execution path, and at this phase
/// its shape is not fixed — the guardrail engine's `Clearance` nests the cloid
/// one level down inside `kind`, a hand-built intent payload would put it at
/// the root. Binding the join to one exact pointer would make a later,
/// reasonable payload change silently reclassify every attributed fill as
/// external, which is a data-loss-shaped bug that no test outside this module
/// would catch. Searching for the field is the version that degrades safely.
///
/// A redacted intent's cloid stops matching, because [`Ledger::redact`] nulls
/// the payload the cloid was in. That costs attribution on a row the operator
/// chose to strip, and it costs nothing else: idempotence does not pass through
/// here.
#[derive(Debug, Default)]
struct Attributions {
    intents: BTreeMap<String, IntentRef>,
    manual: BTreeMap<String, u64>,
}

impl Attributions {
    /// Read the chain's intent and operator rows.
    ///
    /// Two reads served by the `events_kind` index, and their cost is the
    /// chain's intent and operator rows — not the whole chain, and not the
    /// fills, which are the bulk of it. [`Reconciler::apply_fills`] skips this
    /// entirely for a batch in which no fill carries a cloid, which is every
    /// batch that can only attribute to `external`.
    ///
    /// A later row wins over an earlier one carrying the same cloid: a cloid is
    /// oppen's own 128-bit identifier, so a repeat is a re-placement rather than
    /// a collision, and the newest row is the one that describes the order that
    /// traded.
    fn read(ledger: &Ledger) -> Result<Self> {
        let mut found = Attributions::default();
        for event in ledger.events_of_kind(EventKind::OrderIntent)? {
            let Some(payload) = event.payload.as_ref() else {
                continue;
            };
            if let Some(cloid) = find_cloid(payload, 0) {
                found.intents.insert(
                    cloid,
                    IntentRef {
                        seq: event.seq,
                        hash: event.hash,
                        agent_id: event.agent_id,
                        arrival_mid: find_reference_px(payload, 0),
                    },
                );
            }
        }
        for event in ledger.events_of_kind(EventKind::OperatorAction)? {
            let Some(payload) = event.payload.as_ref() else {
                continue;
            };
            if let Some(cloid) = find_cloid(payload, 0) {
                found.manual.insert(cloid, event.seq);
            }
        }
        Ok(found)
    }

    /// Classify one fill.
    ///
    /// An intent wins over an operator action carrying the same cloid: a cloid
    /// is oppen's own 128-bit identifier, so the collision means the operator
    /// row is about the agent's order, and the agent attribution is the one
    /// that carries the reason and the guardrail verdict.
    fn of(&self, cloid: Option<&Cloid>) -> Attribution {
        let Some(cloid) = cloid else {
            return Attribution::External;
        };
        if let Some(intent) = self.intents.get(cloid.as_str()) {
            // An intent with no agent is not representable through
            // `record_intent`, which requires one. If a hand-written row has
            // none, the fill is still recorded — as external, because an
            // attributed fill with no agent would be a claim nobody made.
            if let Some(agent_id) = &intent.agent_id {
                return Attribution::Attributed {
                    agent_id: agent_id.clone(),
                    intent_seq: intent.seq,
                    intent_hash: intent.hash.clone(),
                    arrival_mid: intent.arrival_mid,
                };
            }
        }
        if let Some(seq) = self.manual.get(cloid.as_str()) {
            return Attribution::Manual { action_seq: *seq };
        }
        Attribution::External
    }
}

/// The client order id a payload carries, at any depth. See [`Attributions`].
///
/// An object's own `cloid` is read before any of its children, so a field at the
/// payload root always beats one nested under it — that is the property
/// [`Attributions`] relies on when it says the join degrades safely across a
/// payload shape change. Below the root the walk is depth-first in sorted key
/// order: it is **not** breadth-first, and which of two *nested* cloids wins is
/// not a promise. What is promised is that the answer never depends on
/// `serde_json`'s map iteration order, which the `preserve_order` feature — any
/// crate in the workspace can switch it on — would otherwise decide, and that
/// the classification of a fill is therefore reproducible.
///
/// A `cloid` field holding something that is not a cloid is skipped and the walk
/// continues, so a payload cannot claim an order by writing nonsense into that
/// key.
fn find_cloid(payload: &Value, depth: usize) -> Option<String> {
    if depth > MAX_PAYLOAD_DEPTH {
        return None;
    }
    let read = |value: &Value| {
        Cloid::parse(value.as_str()?)
            .ok()
            .map(|cloid| cloid.as_str().to_owned())
    };
    match payload {
        Value::Object(map) => {
            if let Some(found) = map.get(CLOID_FIELD).and_then(read) {
                return Some(found);
            }
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.into_iter()
                .find_map(|next| map.get(next).and_then(|inner| find_cloid(inner, depth + 1)))
        }
        Value::Array(items) => items.iter().find_map(|item| find_cloid(item, depth + 1)),
        _ => None,
    }
}

/// The decision-time reference price a payload carries, at any depth.
///
/// Same discipline as [`find_cloid`] and for the same reason: the field lives
/// inside the serialized `Clearance`, nested under `kind`, and pinning that
/// path here would make the join a hostage to the guardrail engine's struct
/// layout. Root-first, then depth-first in sorted key order, so the answer
/// never depends on `serde_json`'s map iteration order and the number a fill
/// is measured against is reproducible from the row that recorded it.
///
/// Only a positive price is accepted. A zero or negative one is not a cheap
/// arrival — it is a payload that cannot be measured against, and dividing by
/// it would manufacture a slippage figure out of a broken row.
fn find_reference_px(payload: &Value, depth: usize) -> Option<Decimal> {
    if depth > MAX_PAYLOAD_DEPTH {
        return None;
    }
    let read = |value: &Value| {
        value
            .as_str()
            .and_then(|text| Decimal::from_str_exact(text).ok())
            .filter(|px| *px > Decimal::ZERO)
    };
    match payload {
        Value::Object(map) => {
            if let Some(found) = map.get(REFERENCE_PX_FIELD).and_then(read) {
                return Some(found);
            }
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.into_iter().find_map(|next| {
                map.get(next)
                    .and_then(|inner| find_reference_px(inner, depth + 1))
            })
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_reference_px(item, depth + 1)),
        _ => None,
    }
}

/// Realized slippage against the arrival mid, in basis points, **signed**.
///
/// Positive is cost: a buy that filled above the decision-time price, or a
/// sell that filled below it. Negative is price improvement, and it is
/// reported rather than clamped — the guardrail's own
/// `adverse_slippage_bps` floors at zero because it is deciding whether to
/// refuse and only the costly direction can do that, but a TCA statistic that
/// floors at zero has a mean biased upward by every fill that went well, which
/// makes the whole report useless for the comparison it exists to support.
///
/// `None` when the arithmetic overflows, which leaves the field off the row
/// rather than putting a wrong number in the chain.
fn slip_bps(is_buy: bool, fill_px: Decimal, arrival_mid: Decimal) -> Option<Decimal> {
    let moved = if is_buy {
        fill_px.checked_sub(arrival_mid)
    } else {
        arrival_mid.checked_sub(fill_px)
    }?;
    moved.checked_div(arrival_mid)?.checked_mul(BPS)
}

/// Walk `userFillsByTime` from `start_ms` to the venue's own now, and return
/// every distinct fill.
///
/// This is the whole of the venue's paging contract in one place, and every
/// line of it is a measurement (see the module docs):
///
/// * a page shorter than `page_limit` ends the walk — the venue has nothing
///   more;
/// * a full page advances the cursor to that page's **newest timestamp**, not
///   past it, because the cap can cut a millisecond in half; the boundary rows
///   come back and are removed by `tid`;
/// * a full page whose newest timestamp equals the cursor cannot advance at
///   all, and that is [`ReconcileError::PageStalled`] rather than a silent
///   skip or an infinite loop.
///
/// **There is no end bound.** `endTime` is a venue instant and the only end
/// oppen could name is the local clock's idea of when the socket came back —
/// the same skew that makes `startTime` unsafe, and worse, because a
/// too-early end drops the fills at the tail of the outage while the gap is
/// marked reconciled anyway. Omitting `endTime` asks the venue for everything
/// through its own now, which is by construction at or after the reconnect, so
/// the walk provably meets the live feed that has already resumed. The extra
/// rows it reads on the way are dropped by `tid`, and the walk still
/// terminates: a page is short exactly when the venue has nothing newer.
///
/// Nothing is written here. Recording is `Reconciler::apply_fills`, so a
/// window can be walked and inspected without touching the chain.
async fn backfill_fills<S: ReconcileSource>(
    source: &S,
    user: Address,
    start_ms: u64,
    config: ReconcileConfig,
) -> Result<(Vec<Fill>, usize)> {
    let mut cursor = start_ms;
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut out: Vec<Fill> = Vec::new();
    let mut pages = 0usize;

    loop {
        if pages >= config.max_pages {
            return Err(ReconcileError::TooManyPages {
                start_ms,
                max_pages: config.max_pages,
            });
        }
        let page = source.user_fills_by_time(user, cursor, None).await?;
        pages += 1;
        let rows = page.len();
        let mut newest = cursor;
        for fill in page {
            newest = newest.max(fill.time);
            if seen.insert(fill.tid) {
                out.push(fill);
            }
        }
        if rows < config.page_limit {
            // The venue served everything it has.
            return Ok((out, pages));
        }
        if newest <= cursor {
            return Err(ReconcileError::PageStalled {
                at_ms: cursor,
                rows,
            });
        }
        cursor = newest;
    }
}

/// The component that closes gaps.
///
/// Holds a borrowed [`Ledger`], a [`ReconcileSource`] and its page contract,
/// and **no state of its own**. Every question it asks of the chain — has this
/// fill been recorded, whose order was it, when did the venue last stamp
/// something oppen held — is answered by the database at the moment it is
/// asked. That is what makes a second reconciler, a restart mid-window and a
/// retry all indistinguishable from a first run.
///
/// Every ledger call blocks, so an async caller runs
/// [`Reconciler::reconcile_all`] on a blocking pool the same way it runs any
/// other ledger write (`docs/decisions.md` R1 keeps the core runtime-free).
#[derive(Debug)]
pub struct Reconciler<'a, S> {
    ledger: &'a Ledger,
    source: S,
    config: ReconcileConfig,
}

impl<'a, S: ReconcileSource> Reconciler<'a, S> {
    /// Build a reconciler.
    ///
    /// Refuses a source that is not on the ledger's own network
    /// ([`ReconcileError::NetworkMismatch`], `docs/decisions.md` R4).
    pub fn new(ledger: &'a Ledger, source: S) -> Result<Self> {
        Self::with_config(ledger, source, ReconcileConfig::default())
    }

    /// Build one with a non-default page contract. See
    /// [`ReconcileConfig::page_limit`].
    fn with_config(ledger: &'a Ledger, source: S, config: ReconcileConfig) -> Result<Self> {
        // Before anything reads or writes: R4 keeps the two networks in two
        // files precisely so a testnet number can never be read as a mainnet
        // one, and a mainnet source pointed at the testnet chain would walk
        // straight through that boundary — recording real mainnet fills into
        // the testnet chain, or answering a mainnet gap with testnet fills.
        // Nothing downstream can tell the difference afterwards, so it is
        // refused here and no such reconciler exists.
        let (ledger_network, source_network) = (ledger.network(), source.network());
        if ledger_network != source_network {
            return Err(ReconcileError::NetworkMismatch {
                ledger: ledger_network,
                venue: source_network,
            });
        }
        Ok(Reconciler {
            ledger,
            source,
            config,
        })
    }

    /// Work every gap that has not been proven backfilled, oldest first.
    ///
    /// **A gap that fails does not stop the ones after it.** Its failure
    /// becomes that gap's [`GapStatus::Failed`] and the walk moves on. The
    /// alternative — returning the first error — reads like the stricter
    /// choice and is the more dangerous one: gaps come off the work list in id
    /// order, so a single window that can never close (a page the venue
    /// stalls on, a scope row corrupted by hand) permanently blocks every
    /// later window from ever being backfilled, and those are the recent ones.
    /// Continuing costs nothing that matters, because a failed gap is not
    /// marked reconciled: it stays on the work list, `docs/spec.md` item 34's
    /// overlay stays up, and the retry is free.
    ///
    /// Only the work-list read itself is fatal. If the ledger cannot say which
    /// gaps are open there is no list to be honest about.
    pub async fn reconcile_all(&self, pending: &[Cloid]) -> Result<Vec<GapOutcome>> {
        let gaps = self.ledger.unreconciled_gaps()?;
        let mut outcomes = Vec::with_capacity(gaps.len());
        for gap in &gaps {
            let outcome = match self.reconcile_gap(gap, pending).await {
                Ok(outcome) => outcome,
                Err(error) => GapOutcome {
                    gap_id: gap.gap_id,
                    scope: gap.scope.clone(),
                    status: GapStatus::Failed(error),
                    recovered: Recovered::default(),
                    resting_orders: None,
                    settled: Vec::new(),
                },
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// Close one gap: backfill its window, reconcile its orders, and mark it
    /// reconciled only if the window was proven contiguous.
    ///
    /// `pending` is the caller's set of client order ids it believes are live.
    /// Anything in it that the venue is not resting comes back settled by
    /// query (`docs/spec.md` item 19).
    /// Which of the two account feeds the gap covers decides the work:
    /// a `userFills` window is backfilled, an `orderUpdates` window is
    /// answered by re-reading the order set, and each is marked reconciled
    /// only once that has actually happened.
    ///
    /// **Pass a [`Gap`] read from [`Ledger::unreconciled_gaps`]**, which is
    /// what [`Reconciler::reconcile_all`] does. The value
    /// [`Ledger::open_gap`] hands back is a snapshot taken *before* the socket
    /// came back, so its `closed_ts_ms` is `None` for good and reconciling
    /// against it reports [`GapStatus::StillOpen`] forever. Re-reading it here
    /// would hide the mistake at the cost of a query on every call and a
    /// second, silent definition of which gap is being worked; saying so is
    /// the cheaper honest option.
    async fn reconcile_gap(&self, gap: &Gap, pending: &[Cloid]) -> Result<GapOutcome> {
        let scope = GapScope::parse(&gap.scope)?;
        let unfinished = |status| GapOutcome {
            gap_id: gap.gap_id,
            scope: gap.scope.clone(),
            status,
            recovered: Recovered::default(),
            resting_orders: None,
            settled: Vec::new(),
        };
        let Some(account) = scope.account() else {
            return Ok(unfinished(GapStatus::NotAnAccountFeed));
        };
        let Some(closed_ts_ms) = gap.closed_ts_ms else {
            // `Ledger::mark_gap_reconciled` refuses an open gap anyway; this
            // returns the reason instead of an error, because a socket that is
            // still down is the normal state, not a failure.
            return Ok(unfinished(GapStatus::StillOpen));
        };

        let mut recovered = Recovered::default();
        if matches!(scope, GapScope::UserFills(_)) {
            // The anchor is read at `gap.open_seq`, so nothing written after
            // the socket died — by a later backfill, or by the live feed once
            // it resumed — can drag the start past the fills it is meant to
            // recover.
            let anchor = self
                .ledger
                .newest_fill_ts_ms(&account.to_string(), gap.open_seq)?;
            let window = outage_window(gap, closed_ts_ms, anchor)?;
            // Fills first, and the gap is marked only after everything below
            // has succeeded. The opposite order would mark a window
            // reconciled on the strength of an order query while the fills
            // walk had not run.
            let (fills, _) =
                backfill_fills(&self.source, account, window.start_ms, self.config).await?;
            recovered = self.apply_fills(account, &fills, window.recovered_from(gap.gap_id))?;
            if window.outage_ends_ms.is_none() {
                recovered.findings.push(Finding::FirstRunWindow {
                    account: account.to_string(),
                    start_ms: window.start_ms,
                });
            }
        }

        // A `userFills` gap costs no order knowledge, so the order query runs
        // for it only when the caller has something outstanding. An
        // `orderUpdates` gap is *made* of missed transitions, so it always
        // re-reads the book: `docs/spec.md` item 9 names `frontendOpenOrders`
        // plus `orderStatus` by cloid as what closes it.
        let query_orders = matches!(scope, GapScope::OrderUpdates(_)) || !pending.is_empty();
        let (resting_orders, settled) = if query_orders {
            let open_orders = self.source.frontend_open_orders(account).await?;
            let reconciliation = OrderReconciliation::partition(account, pending, open_orders);
            let settled = reconciliation.settle_all(&self.source).await?;
            for (cloid, settlement) in &settled {
                if matches!(settlement, Settlement::Retired) {
                    recovered.findings.push(Finding::OrderRetired {
                        cloid: cloid.as_str().to_owned(),
                    });
                }
            }
            (
                Some(reconciliation.resting().len()),
                settled
                    .into_iter()
                    .map(|(cloid, settlement)| (cloid.as_str().to_owned(), settlement))
                    .collect(),
            )
        } else {
            (None, Vec::new())
        };

        // Only now: the window was walked to a short page, every fill in it is
        // in the chain, and anything outstanding has been queried.
        self.ledger.mark_gap_reconciled(gap.gap_id, now_ms())?;

        Ok(GapOutcome {
            gap_id: gap.gap_id,
            scope: gap.scope.clone(),
            status: GapStatus::Reconciled,
            recovered,
            resting_orders,
            settled,
        })
    }

    /// Record fills. One row per fill, each keyed on the venue's own
    /// identifiers, and the database decides which ones are new.
    ///
    /// This is the only writer of [`EventKind::Fill`] rows, and it is also the
    /// path the live `userFills` feed will use, including for the subscribe
    /// snapshot the venue replays on every reconnect — the same batch through
    /// the same door.
    ///
    /// **There is no dedupe here to be stale.** Every fill is offered to
    /// [`Ledger::record_fill`], which writes it or reports it already
    /// recorded, in one statement, under the partial unique index. A fill that
    /// another writer chained a microsecond ago is refused by the index, not by
    /// this function's memory of what it has seen; a batch that carries the
    /// same trade twice records it once for the same reason.
    ///
    /// Fills are sorted by `(time, tid)` before they are appended, so the
    /// chain order of a recovered window does not depend on which page a row
    /// arrived in. Two runs over the same window produce the same chain.
    ///
    /// Nothing is counted or reported until the row is durable: a fill that
    /// turns out to be a duplicate must not push a
    /// [`Finding::UnattributedFill`] the operator has already seen.
    fn apply_fills(
        &self,
        account: Address,
        fills: &[Fill],
        recovered_from: Option<RecoveredWindow>,
    ) -> Result<Recovered> {
        let mut ordered: Vec<&Fill> = fills.iter().collect();
        ordered.sort_by_key(|fill| (fill.time, fill.tid));

        let mut recovered = Recovered {
            fills_seen: fills.len(),
            ..Recovered::default()
        };
        // One read, and only when it can change an answer: a batch in which no
        // fill carries a cloid attributes to nothing whatever the chain holds.
        let attributions = if ordered.iter().any(|fill| fill.cloid.is_some()) {
            Attributions::read(self.ledger)?
        } else {
            Attributions::default()
        };
        let account_text = account.to_string();

        for fill in ordered {
            let attribution = attributions.of(fill.cloid.as_ref());
            let ts_ms = i64::try_from(fill.time)
                .map_err(|_| ReconcileError::TimestampOutOfRange(fill.time))?;
            let payload = fill_payload(account, fill, &attribution, recovered_from);
            let appended = self.ledger.record_fill(&NewFill {
                account: &account_text,
                tid: fill.tid,
                ts_ms,
                agent_id: attribution.agent_id(),
                payload: &payload,
            })?;
            if appended.is_none() {
                recovered.duplicates += 1;
                continue;
            }
            match &attribution {
                Attribution::Attributed { .. } => recovered.attributed += 1,
                Attribution::Manual { .. } => recovered.manual += 1,
                Attribution::External => {
                    recovered.external += 1;
                    recovered.findings.push(Finding::UnattributedFill {
                        tid: fill.tid,
                        oid: fill.oid,
                        coin: fill.coin.clone(),
                        ts_ms,
                        cloid: fill.cloid.as_ref().map(|c| c.as_str().to_owned()),
                    });
                }
            }
            recovered.fills_recorded += 1;
        }
        Ok(recovered)
    }
}

/// Which gap's reconcile pass recovered a fill, and how far into venue time
/// that gap's outage is taken to reach.
///
/// Carried into the chained payload as `recovered_from_gap`. A fill stamped
/// with it is one this walk found missing and recovered while closing that gap.
/// A fill past `horizon_ms` is recorded like any other and carries no stamp,
/// because the walk has no end bound and therefore meets fills that happened
/// after the socket came back — those belong to the live feed, and claiming
/// them for the outage would be a false statement in the one table kept
/// forever.
#[derive(Debug, Clone, Copy)]
struct RecoveredWindow {
    gap_id: i64,
    horizon_ms: u64,
}

/// The venue-time window one gap's fills walk covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    /// Where the walk starts, in **venue** milliseconds.
    start_ms: u64,
    /// The venue instant past which a fill is no longer the outage's.
    ///
    /// `None` when the chain holds no usable anchor for this container: there
    /// is then no venue instant to reason from, so no fill can be attributed to
    /// the outage and none is stamped. See [`FIRST_RUN_LOOKBACK_MS`].
    outage_ends_ms: Option<u64>,
}

impl Window {
    /// The provenance a fill recovered inside this window carries.
    fn recovered_from(&self, gap_id: i64) -> Option<RecoveredWindow> {
        self.outage_ends_ms
            .map(|horizon_ms| RecoveredWindow { gap_id, horizon_ms })
    }
}

/// The window this gap's fills walk covers, in **venue** milliseconds.
///
/// `feed_gaps` is written from the host's wall clock: `opened_ts_ms` is when
/// the pool noticed the socket die and `closed_ts_ms` is when it noticed the
/// socket return. `userFillsByTime` bounds are the venue's own clock. Handing
/// the first straight to the second — which is what this function used to do —
/// shifts the window by exactly the skew between them, and it fails in the
/// silent direction: a host clock ahead of the venue asks for a window that
/// begins after the fills it was supposed to recover, the venue truthfully
/// answers with nothing, the walk ends on a short page, and the gap is marked
/// reconciled. The fill is gone from the record with no error anywhere. That
/// is the P2 gate failing while reporting success.
///
/// **Taking the earlier of the two clocks did not fix that**, which is why it
/// is gone. `min(anchor, opened_ms)` defends only while the host runs slow; a
/// host running fast leaves the anchor deciding alone, and an anchor that a
/// later walk had already pushed past the outage then carried the window with
/// it. Two readings do not make a clock. So this reads **one**:
///
/// * `anchor` — the newest instant the venue itself stamped on a fill the
///   chain already held **at this gap's own chain position**
///   ([`Ledger::newest_fill_ts_ms`]). Every such row was durable before the
///   socket died, so it is a venue instant at which oppen was demonstrably
///   being served, and nothing recorded afterwards can move it.
///
/// One thing does bound that anchor: it may not postdate the disconnect by
/// more than [`FIRST_RUN_LOOKBACK_MS`]. A stored stamp that claims a venue
/// instant a month past the moment the host says the socket died is not a
/// record of oppen being served — it is a bad venue timestamp or an edited row
/// — and believing it starts the walk after everything the gap exists to
/// recover. Such an anchor is discarded, not clamped, so the host clock can
/// only ever widen this window and never shrink it.
///
/// The start is that anchor less [`VENUE_CLOCK_MARGIN_MS`], because delivery is
/// not stamping — see that constant. The far end is the anchor plus the outage
/// and the same margin. The outage is a **difference** of two host readings, so
/// the skew that makes each one unusable cancels: what is left is the host
/// clock's drift over one outage, which is microseconds over the thirty seconds
/// this module exists for. It bounds only the provenance stamp; the walk itself
/// carries no end bound, for the reason given on [`backfill_fills`].
///
/// The far end can land before the outage really ended — an account that was
/// idle for an hour before the socket dropped has an anchor an hour early — and
/// that direction is the safe one: a fill from inside the outage then goes into
/// the chain without the stamp. It is recorded, classified, exported and
/// counted either way. The opposite direction writes a claim that is false.
///
/// With no anchor — none recorded, or none believable — there is no venue
/// clock to reason from at all, and [`FIRST_RUN_LOOKBACK_MS`] says what happens
/// instead.
///
/// The one refusal: a gap row whose own timestamps are not usable — negative,
/// or closing before it opened. That row is the only statement oppen has about
/// when it stopped listening, and a reconciler that substitutes a guess for it
/// would mark done a window it never established. `AGENTS.md`: an unevaluable
/// state must never permit an action. The gap stays open and the overlay stays
/// up.
fn outage_window(gap: &Gap, closed_ts_ms: i64, anchor: Option<u64>) -> Result<Window> {
    let unusable = || ReconcileError::UnusableWindow {
        gap_id: gap.gap_id,
        start_ms: gap.opened_ts_ms,
        end_ms: closed_ts_ms,
    };
    let opened_ms = u64::try_from(gap.opened_ts_ms).map_err(|_| unusable())?;
    let closed_ms = u64::try_from(closed_ts_ms).map_err(|_| unusable())?;
    let outage_ms = closed_ms.checked_sub(opened_ms).ok_or_else(unusable)?;

    // An anchor is a venue instant at which oppen was *being served*, so it
    // cannot postdate the disconnect. One that does is not an anchor at all,
    // and taking it starts the walk after the fills the gap exists to recover:
    // the venue truthfully answers with nothing, the page is short, and the gap
    // is marked reconciled. That is the P2 gate failing while reporting
    // success, so an incredible anchor is discarded and the no-anchor branch
    // below answers instead. The bound is a host reading and is given the same
    // thirty days of slack [`FIRST_RUN_LOOKBACK_MS`] already trusts it with, so
    // no skew reaches it and the window still comes from the chain; and
    // discarding can only widen the walk, never narrow it, because the fallback
    // starts thirty days before the host's own account of the disconnect.
    let anchor =
        anchor.filter(|anchor_ms| *anchor_ms <= opened_ms.saturating_add(FIRST_RUN_LOOKBACK_MS));

    Ok(match anchor {
        Some(anchor_ms) => Window {
            start_ms: anchor_ms.saturating_sub(VENUE_CLOCK_MARGIN_MS),
            outage_ends_ms: Some(
                anchor_ms
                    .saturating_add(outage_ms)
                    .saturating_add(VENUE_CLOCK_MARGIN_MS),
            ),
        },
        None => Window {
            start_ms: opened_ms.saturating_sub(FIRST_RUN_LOOKBACK_MS),
            outage_ends_ms: None,
        },
    })
}

/// The chained payload of one fill.
///
/// Every money field is a decimal string: `AGENTS.md` says money is `Decimal`
/// and never `f64`, and [`crate::ledger`]'s canonical encoder refuses a JSON
/// float outright. The strings are produced by `Decimal::to_string` rather
/// than by serialising the `Decimal`, because `rust_decimal`'s `serde-float`
/// feature can be switched on by any crate in the workspace through feature
/// unification, and the one table kept forever must not have its contents
/// decided by feature resolution.
///
/// `docs/specs/history.md` §5 requires the PnL decomposition to survive into
/// storage, so `fee`, `builder_fee` and `closed_pnl` are separate fields and
/// no net number is stored in their place.
fn fill_payload(
    account: Address,
    fill: &Fill,
    attribution: &Attribution,
    recovered_from: Option<RecoveredWindow>,
) -> Value {
    // Stamped only for a fill the outage can actually account for. See
    // [`RecoveredWindow`] and [`outage_window`].
    let gap_id = recovered_from
        .filter(|window| fill.time <= window.horizon_ms)
        .map(|window| window.gap_id);
    let mut payload = json!({
        FILL_ACCOUNT_FIELD: account.to_string(),
        "attribution": attribution.as_str(),
        "builder_fee": fill.builder_fee.map(|fee| fee.to_string()),
        "cloid": fill.cloid.as_ref().map(|cloid| cloid.as_str()),
        "closed_pnl": fill.closed_pnl.to_string(),
        "coin": fill.coin,
        "crossed": fill.crossed,
        "dir": fill.dir,
        "fee": fill.fee.to_string(),
        "fee_token": fill.fee_token,
        "oid": fill.oid,
        "px": fill.px.to_string(),
        "recovered_from_gap": gap_id,
        "side": if fill.side.is_buy() { "buy" } else { "sell" },
        "start_position": fill.start_position.to_string(),
        "sz": fill.sz.to_string(),
        FILL_TID_FIELD: fill.tid,
        FILL_TS_FIELD: fill.time,
        "venue_hash": fill.hash,
    });
    match attribution {
        Attribution::Attributed {
            agent_id,
            intent_seq,
            intent_hash,
            arrival_mid,
        } => {
            payload["agent_id"] = json!(agent_id);
            payload["intent_seq"] = json!(intent_seq);
            payload["intent_hash"] = json!(intent_hash);
            // Spec F's TCA foundation. Stamped rather than derived later: the
            // arrival mid is a fact about the instant the decision was taken,
            // and a chained row that carries it can be audited without
            // re-reading the intent and trusting that nothing moved. Both
            // fields or neither — a slippage with no arrival price to check it
            // against is a number nobody can verify.
            if let Some(arrival_mid) = arrival_mid
                && let Some(slip) = slip_bps(fill.side.is_buy(), fill.px, *arrival_mid)
            {
                payload["arrival_mid"] = json!(arrival_mid.to_string());
                payload["slip_bps"] = json!(slip.to_string());
            }
        }
        Attribution::Manual { action_seq } => {
            payload["action_seq"] = json!(action_seq);
        }
        Attribution::External => {}
    }
    payload
}

#[cfg(test)]
mod tests {
    //! The P2 gate lives here.
    //!
    //! `README.md`'s phase table states P2 as "zero fills lost across a 30 s
    //! disconnect", and `AGENTS.md` says a ledger change needs the
    //! disconnect-reconcile test. These are those tests.
    //!
    //! There is no funded key, so the venue is a fixture. The fixture is not a
    //! convenient stub: [`FakeVenue::user_fills_by_time`] reproduces the
    //! paging contract **measured** against public mainnet on 2026-09-04 —
    //! ascending order, a hard row cap, truncation from the newest end, and an
    //! inclusive `startTime`. The tests that matter are the ones that would
    //! fail against that contract if the walk were written the obvious way.
    //! The live path itself is unproven; see the ignored test at the end,
    //! which re-measures the contract against the real venue.

    use std::str::FromStr;
    use std::sync::Mutex;

    use rust_decimal::Decimal;
    use tempfile::TempDir;

    use oppen_hl::types::Side;
    use oppen_hl::ws::Subscription;

    use crate::ledger::{Anchor, Event, MAX_PAGE, NewEvent, NewIntent};

    use super::*;

    /// Unix ms, fixed so a chain built by a test is reproducible.
    const T0: i64 = 1_780_000_000_000;

    fn dec(text: &str) -> Decimal {
        Decimal::from_str(text).expect("decimal literal")
    }

    fn account() -> Address {
        Address::parse("0x1111111111111111111111111111111111111111").expect("address literal")
    }

    fn cloid(n: u128) -> Cloid {
        Cloid::from_bytes(n.to_be_bytes())
    }

    fn open(dir: &TempDir) -> Ledger {
        Ledger::open(dir.path(), Network::Testnet).expect("open ledger")
    }

    fn fill_at(tid: u64, ts_ms: u64, cloid: Option<Cloid>) -> Fill {
        Fill {
            coin: "ETH".to_owned(),
            px: dec("2307.3"),
            sz: dec("1.5"),
            side: Side::B,
            time: ts_ms,
            start_position: dec("0"),
            dir: "Open Long".to_owned(),
            closed_pnl: dec("-12.5"),
            hash: format!("0x{tid:064x}"),
            oid: 900_000 + tid,
            crossed: true,
            fee: dec("1.23"),
            fee_token: "USDC".to_owned(),
            builder_fee: Some(dec("0.01")),
            tid,
            cloid,
        }
    }

    /// **The writer and the reader, against each other.** `fill_payload`
    /// stamps the TCA fields and `tca::ScoredFill` reads them back, and the
    /// two agreeing is the whole of spec F's per-fill measurement. Written
    /// this way because the first version of the reader looked for `time`
    /// where the writer stamps `ts_ms`, so every fill came back unscoreable
    /// and a hand-written fixture agreed with the mistake — a test that
    /// invents its own payload proves only that the test and the bug share an
    /// author.
    ///
    /// The arithmetic is hand-computed, per P6's gate: a buy filled at
    /// 2307.3 against an arrival mid of 2300 paid 7.3 on 2300, which is
    /// 31.739130434782608695652173913 bps.
    #[test]
    fn what_fill_payload_stamps_is_what_the_execution_report_reads() {
        let cloid = Cloid::parse("0x00000000000000000000000000000001").expect("cloid");
        let fill = fill_at(1, T0 as u64, Some(cloid));
        let attribution = Attribution::Attributed {
            agent_id: "alpha".to_owned(),
            intent_seq: 7,
            intent_hash: "hash".to_owned(),
            arrival_mid: Some(dec("2300")),
        };
        let payload = fill_payload(account(), &fill, &attribution, None);

        let scored = crate::tca::ScoredFill::from_payload(&payload)
            .expect("a payload this module wrote must be one the report can read");
        assert_eq!(scored.symbol, "ETH");
        assert_eq!(scored.agent_id, "alpha");
        assert_eq!(scored.ts_ms, T0);
        assert_eq!(scored.slip_bps.round_dp(6), dec("31.739130"));
        assert_eq!(scored.fee_usd, dec("1.23"));
        assert_eq!(scored.closed_pnl_usd, dec("-12.5"));
        assert!(scored.crossed);
        // 2307.3 x 1.5.
        assert_eq!(scored.notional_usd, dec("3460.95"));

        // And a fill with no arrival mid carries neither field, so the report
        // counts it unscored rather than scoring it against nothing.
        let blind = fill_payload(
            account(),
            &fill,
            &Attribution::Attributed {
                agent_id: "alpha".to_owned(),
                intent_seq: 7,
                intent_hash: "hash".to_owned(),
                arrival_mid: None,
            },
            None,
        );
        assert!(blind.get("slip_bps").is_none());
        assert!(blind.get("arrival_mid").is_none());
        assert!(crate::tca::ScoredFill::from_payload(&blind).is_none());
    }

    fn open_order(oid: u64, cloid: Option<Cloid>) -> OpenOrder {
        OpenOrder {
            coin: "ETH".to_owned(),
            side: Side::B,
            limit_px: dec("2300"),
            sz: dec("1"),
            orig_sz: dec("1"),
            oid,
            timestamp: 1,
            order_type: "Limit".to_owned(),
            tif: Some(oppen_hl::wire::Tif::Gtc),
            reduce_only: false,
            is_trigger: false,
            trigger_px: None,
            trigger_condition: None,
            is_position_tpsl: false,
            cloid,
        }
    }

    /// The venue as measured, not as convenient.
    ///
    /// `user_fills_by_time` sorts ascending, filters to the requested window
    /// inclusively at both ends, and truncates to `page_limit` from the newest
    /// end with no cursor. That is exactly what mainnet did on 2026-09-04, and
    /// it is why a fixture is worth more here than a stub that hands back
    /// whatever the test wants.
    #[derive(Debug)]
    struct FakeVenue {
        network: Network,
        /// Per container address, because `userFillsByTime` is: a fixture that
        /// answers every account with the same rows cannot tell a test that
        /// one container's walk stalled while another's finished.
        fills: BTreeMap<String, Vec<Fill>>,
        page_limit: usize,
        open_orders: Vec<OpenOrder>,
        statuses: BTreeMap<String, OrderStatusResponse>,
        requests: Mutex<Vec<(u64, Option<u64>)>>,
    }

    impl FakeVenue {
        fn new(fills: Vec<Fill>) -> Self {
            FakeVenue {
                // Every test ledger here is `Network::Testnet`, which is also
                // oppen's default (`AGENTS.md` invariant 5).
                network: Network::Testnet,
                fills: BTreeMap::from([(account().to_string(), fills)]),
                page_limit: USER_FILLS_PAGE_LIMIT,
                open_orders: Vec::new(),
                statuses: BTreeMap::new(),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn with_fills_for(mut self, user: Address, fills: Vec<Fill>) -> Self {
            self.fills.insert(user.to_string(), fills);
            self
        }

        fn on_network(mut self, network: Network) -> Self {
            self.network = network;
            self
        }

        fn with_page_limit(mut self, page_limit: usize) -> Self {
            self.page_limit = page_limit;
            self
        }

        fn with_open_orders(mut self, orders: Vec<OpenOrder>) -> Self {
            self.open_orders = orders;
            self
        }

        fn with_status(mut self, key: &str, status: OrderStatusResponse) -> Self {
            self.statuses.insert(key.to_owned(), status);
            self
        }

        fn requests(&self) -> Vec<(u64, Option<u64>)> {
            self.requests
                .lock()
                .map(|log| log.clone())
                .unwrap_or_default()
        }
    }

    impl ReconcileSource for FakeVenue {
        fn network(&self) -> Network {
            self.network
        }

        async fn user_fills_by_time(
            &self,
            user: Address,
            start_ms: u64,
            end_ms: Option<u64>,
        ) -> std::result::Result<Vec<Fill>, VenueError> {
            if let Ok(mut log) = self.requests.lock() {
                log.push((start_ms, end_ms));
            }
            let mut rows: Vec<Fill> = self
                .fills
                .get(&user.to_string())
                .into_iter()
                .flatten()
                .filter(|fill| {
                    fill.time >= start_ms && end_ms.is_none_or(|end_ms| fill.time <= end_ms)
                })
                .cloned()
                .collect();
            rows.sort_by_key(|fill| (fill.time, fill.tid));
            rows.truncate(self.page_limit);
            Ok(rows)
        }

        async fn frontend_open_orders(
            &self,
            _user: Address,
        ) -> std::result::Result<Vec<OpenOrder>, VenueError> {
            Ok(self.open_orders.clone())
        }

        async fn order_status(
            &self,
            _user: Address,
            order: OrderRef,
        ) -> std::result::Result<OrderStatusResponse, VenueError> {
            let key = match order {
                OrderRef::Oid(oid) => oid.to_string(),
                OrderRef::Cloid(cloid) => cloid.as_str().to_owned(),
            };
            Ok(self
                .statuses
                .get(&key)
                .cloned()
                .unwrap_or(OrderStatusResponse::UnknownOid))
        }
    }

    /// Every fill row in the chain, read back the way an audit export would.
    fn chain_fills(ledger: &Ledger) -> Vec<Event> {
        let mut out = Vec::new();
        let mut cursor = 0u64;
        loop {
            let page = ledger.get_events(cursor, MAX_PAGE).expect("read page");
            if page.events.is_empty() {
                return out;
            }
            for event in &page.events {
                if event.kind == EventKind::Fill {
                    out.push(event.clone());
                }
            }
            cursor = page.next_cursor;
        }
    }

    fn chain_tids(ledger: &Ledger) -> BTreeSet<u64> {
        chain_fills(ledger)
            .iter()
            .filter_map(|event| {
                event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("tid"))
                    .and_then(Value::as_u64)
            })
            .collect()
    }

    fn head(ledger: &Ledger) -> Anchor {
        ledger.chain_head().expect("chain head")
    }

    /// Read a gap back off the work list.
    ///
    /// The [`Gap`] [`Ledger::open_gap`] returns predates its own close, so its
    /// `closed_ts_ms` is `None` forever no matter what happens afterwards.
    /// [`Reconciler::reconcile_gap`] would correctly report
    /// [`GapStatus::StillOpen`] for it. Tests take the same route the
    /// reconciler does — see the note on `reconcile_gap`.
    fn reload_gap(ledger: &Ledger, gap_id: i64) -> Gap {
        ledger
            .unreconciled_gaps()
            .expect("work list")
            .into_iter()
            .find(|gap| gap.gap_id == gap_id)
            .expect("the gap is still on the work list")
    }

    /// Record an agent intent carrying `cloid`, the way the execution path
    /// will, and return nothing: the reconciler finds it by scanning.
    fn record_intent(ledger: &Ledger, agent: &str, cloid: &Cloid) {
        ledger
            .record_intent(&NewIntent {
                agent_id: agent,
                ts_ms: T0,
                payload: &json!({
                    "cloid": cloid.as_str(),
                    "coin": "ETH",
                    "px": "2300.0",
                    "reason": "agent-authored text, inert",
                }),
                snapshot: None,
            })
            .expect("record intent");
    }

    /// The gate. A socket drops for 30 seconds, three fills happen inside the
    /// window, and not one of them may be missing afterwards.
    ///
    /// The three are deliberately one of each kind: an agent's order, the
    /// operator's own ticket, and something oppen never asked for — a
    /// liquidation or a trade from the Hyperliquid web app. All three land;
    /// `docs/specs/history.md` §2 forbids dropping the third for failing to
    /// match.
    ///
    /// One fill is already in the chain before the socket drops, because that
    /// is the shape a live socket dies in: it had been delivering. That row is
    /// also the venue-time anchor the walk starts from, and the walk re-reads
    /// it by construction — the venue's `startTime` is inclusive — so this
    /// gate proves the anchor fill is not chained a second time as well as
    /// proving nothing was lost.
    #[tokio::test]
    async fn zero_fills_lost_across_a_thirty_second_disconnect() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let start = u64::try_from(T0).expect("epoch fits");

        let agent_cloid = cloid(1);
        let manual_cloid = cloid(2);
        record_intent(&ledger, "scout", &agent_cloid);
        ledger
            .append(&NewEvent {
                kind: EventKind::OperatorAction,
                ts_ms: T0,
                agent_id: None,
                payload: &json!({ "ticket": "close half", "cloid": manual_cloid.as_str() }),
                snapshot: None,
            })
            .expect("operator ticket");

        // The last fill the live feed delivered before the socket died.
        let delivered = fill_at(10, start, None);
        Reconciler::new(&ledger, FakeVenue::new(Vec::new()))
            .expect("reconciler")
            .apply_fills(user, std::slice::from_ref(&delivered), None)
            .expect("the live feed had been delivering");

        let scope = Subscription::UserFills { user }.key();
        let gap = ledger
            .open_gap(&scope, T0 + 1_000, Some("1006 outbox_overflow"))
            .expect("open gap");
        ledger
            .close_gap(gap.gap_id, T0 + 31_000)
            .expect("close gap 30s later");

        let fills = vec![
            delivered,
            fill_at(11, start + 5_000, Some(agent_cloid.clone())),
            fill_at(12, start + 15_000, Some(manual_cloid.clone())),
            fill_at(13, start + 25_000, None),
        ];

        let reconciler = Reconciler::new(&ledger, FakeVenue::new(fills)).expect("reconciler");
        let outcomes = reconciler.reconcile_all(&[]).await.expect("reconcile");

        assert_eq!(outcomes.len(), 1, "one gap, one outcome");
        let outcome = &outcomes[0];
        assert!(
            matches!(outcome.status, GapStatus::Reconciled),
            "{:?}",
            outcome.status
        );
        assert_eq!(outcome.recovered.fills_seen, 4);
        assert_eq!(outcome.recovered.fills_recorded, 3);
        assert_eq!(
            outcome.recovered.duplicates, 1,
            "the anchor fill came back and must not be chained twice"
        );
        assert_eq!(outcome.recovered.attributed, 1);
        assert_eq!(outcome.recovered.manual, 1);
        assert_eq!(outcome.recovered.external, 1);

        // Not one fill missing.
        assert_eq!(chain_tids(&ledger), BTreeSet::from([10, 11, 12, 13]));

        // The walk starts from what the venue stamped on the fill the chain
        // already held, less the delivery margin — no host reading anywhere.
        // The end is open: the venue's own now is the only instant provably at
        // or after the reconnect.
        assert_eq!(
            reconciler.source.requests(),
            vec![(start - VENUE_CLOCK_MARGIN_MS, None)],
            "one short page ends the walk"
        );

        // The chain still verifies, and the gap is off the work list.
        let report = ledger.verify().expect("verify");
        assert!(report.first_break.is_none(), "chain broke: {report:?}");
        assert!(ledger.unreconciled_gaps().expect("gaps").is_empty());

        // The attributed fill names the agent, and does so from the intent
        // rather than from anything the caller supplied.
        let rows = chain_fills(&ledger);
        assert_eq!(rows.len(), 4);
        let attributed = rows
            .iter()
            .find(|event| {
                event
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("tid"))
                    .and_then(Value::as_u64)
                    == Some(11)
            })
            .expect("the attributed fill");
        assert_eq!(attributed.agent_id.as_deref(), Some("scout"));
        let payload = attributed.payload.as_ref().expect("payload");
        assert_eq!(payload["attribution"], json!("attributed"));
        assert_eq!(payload["recovered_from_gap"], json!(gap.gap_id));
    }

    /// The P2 gate again, this time with the two clocks disagreeing.
    ///
    /// `feed_gaps` is stamped from the host's wall clock and
    /// `userFillsByTime` answers in the venue's. Here the host is an hour
    /// ahead — a laptop back from sleep before NTP has stepped it, which is
    /// the exact machine this module exists for. Passing the gap's own
    /// timestamps through as venue bounds asks for an hour in the future, the
    /// venue truthfully answers with nothing, the short page ends the walk,
    /// and the gap is marked reconciled with three real fills missing and no
    /// error anywhere.
    ///
    /// The chain is seeded with one fill first, so the account has a venue
    /// instant to reason from: that reading, not the host clock, is what has
    /// to decide the start.
    #[tokio::test]
    async fn a_host_clock_running_ahead_does_not_shrink_the_window() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let venue_now = u64::try_from(T0).expect("epoch fits");

        // One fill already in the chain, stamped by the venue.
        let seed = fill_at(70, venue_now, None);
        Reconciler::new(&ledger, FakeVenue::new(Vec::new()))
            .expect("reconciler")
            .apply_fills(user, std::slice::from_ref(&seed), None)
            .expect("seed the chain");

        // The socket drops and returns 30 s later, both stamped by a host
        // clock that is an hour fast.
        const SKEW_MS: i64 = 60 * 60 * 1_000;
        let scope = Subscription::UserFills { user }.key();
        let opened = ledger
            .open_gap(&scope, T0 + SKEW_MS, None)
            .expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + SKEW_MS + 30_000)
            .expect("close gap 30s later");

        // The fills the outage swallowed, stamped by the venue — an hour
        // before the window the host would have asked for.
        let missed = vec![
            fill_at(71, venue_now + 5_000, None),
            fill_at(72, venue_now + 15_000, None),
            fill_at(73, venue_now + 25_000, None),
        ];
        let mut venue_fills = vec![seed];
        venue_fills.extend(missed);

        let reconciler = Reconciler::new(&ledger, FakeVenue::new(venue_fills)).expect("reconciler");
        let outcomes = reconciler.reconcile_all(&[]).await.expect("reconcile");

        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(outcomes[0].status, GapStatus::Reconciled),
            "{:?}",
            outcomes[0].status
        );
        assert_eq!(
            chain_tids(&ledger),
            BTreeSet::from([70, 71, 72, 73]),
            "a fill inside the outage was never fetched, and the gap was closed anyway"
        );

        // The request is derived from the venue's own stamp, not the host's:
        // the seeded fill's timestamp, less the margin. Never the hour-ahead
        // gap row, and never with an end the host clock invented.
        assert_eq!(
            reconciler.source.requests(),
            vec![(venue_now - VENUE_CLOCK_MARGIN_MS, None)]
        );
    }

    /// The same skew at the other end of the window.
    ///
    /// A host clock an hour *behind* the venue makes `closed_ts_ms` an hour
    /// early. Handing that to `endTime` truncates the window before the fills
    /// the outage swallowed even reach it — the start can be perfect and the
    /// tail is still lost, and the gap is still marked reconciled. There is no
    /// end oppen can name safely, so the walk names none and takes the venue's
    /// own now, which is by construction at or after the reconnect.
    #[tokio::test]
    async fn a_host_clock_running_behind_does_not_truncate_the_window() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let venue_now = u64::try_from(T0).expect("epoch fits");
        const SKEW_MS: i64 = 60 * 60 * 1_000;

        let seed = fill_at(74, venue_now, None);
        Reconciler::new(&ledger, FakeVenue::new(Vec::new()))
            .expect("reconciler")
            .apply_fills(user, std::slice::from_ref(&seed), None)
            .expect("seed the chain");

        let scope = Subscription::UserFills { user }.key();
        let opened = ledger
            .open_gap(&scope, T0 - SKEW_MS, None)
            .expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 - SKEW_MS + 30_000)
            .expect("close gap 30s later");

        let mut venue_fills = vec![seed];
        venue_fills.extend([
            fill_at(75, venue_now + 5_000, None),
            fill_at(76, venue_now + 25_000, None),
        ]);

        let reconciler = Reconciler::new(&ledger, FakeVenue::new(venue_fills)).expect("reconciler");
        let outcomes = reconciler.reconcile_all(&[]).await.expect("reconcile");

        assert!(
            matches!(outcomes[0].status, GapStatus::Reconciled),
            "{:?}",
            outcomes[0].status
        );
        assert_eq!(
            chain_tids(&ledger),
            BTreeSet::from([74, 75, 76]),
            "the tail of the outage fell outside an end the host clock invented"
        );
        // An hour of skew moves nothing: the start comes off the anchor and
        // the walk carries no end bound at all.
        assert_eq!(
            reconciler.source.requests(),
            vec![(venue_now - VENUE_CLOCK_MARGIN_MS, None)],
        );
    }

    /// The window is derived from the chain, and the host clock is not an
    /// input to it.
    ///
    /// The old shape took `min(anchor, gap.opened_ts_ms)`, which defends only
    /// while the host runs slow. Run it fast and the anchor decides alone —
    /// and an anchor a *later* walk had already pushed past the outage then
    /// carried the window with it, past the very fills the walk exists to
    /// recover, with the gap marked reconciled anyway.
    ///
    /// So: one gap, three host clocks an hour apart, one anchor. One answer.
    #[test]
    fn the_window_comes_from_the_chain_and_never_from_the_host_clock() {
        let gap = |opened_ts_ms: i64| Gap {
            gap_id: 1,
            scope: "userFills:x".to_owned(),
            opened_ts_ms,
            closed_ts_ms: Some(opened_ts_ms + 30_000),
            reconciled_ts_ms: None,
            open_seq: 1,
            close_seq: Some(2),
            note: None,
        };
        let anchor = u64::try_from(T0).expect("epoch fits");
        const HOUR_MS: i64 = 60 * 60 * 1_000;

        let expected = Window {
            start_ms: anchor - VENUE_CLOCK_MARGIN_MS,
            outage_ends_ms: Some(anchor + 30_000 + VENUE_CLOCK_MARGIN_MS),
        };
        for skew in [-HOUR_MS, 0, HOUR_MS] {
            let opened = T0 + skew;
            assert_eq!(
                outage_window(&gap(opened), opened + 30_000, Some(anchor)).expect("window"),
                expected,
                "a host clock {skew} ms out moved the window"
            );
        }

        // With no anchor there is no venue instant to reason from: the walk
        // covers a bounded lookback instead of the epoch, and claims no
        // outage window at all.
        let opened = T0 + HOUR_MS;
        let first_run = outage_window(&gap(opened), opened + 30_000, None).expect("window");
        assert_eq!(
            first_run,
            Window {
                start_ms: u64::try_from(opened).expect("fits") - FIRST_RUN_LOOKBACK_MS,
                outage_ends_ms: None,
            }
        );
        assert!(
            first_run.start_ms > 0,
            "a first run must not walk from the epoch"
        );
    }

    /// Idempotence, which is the whole property.
    ///
    /// The same batch is applied three times through three **separate**
    /// reconcilers. A fresh reconciler rebuilds its index from the chain, so
    /// this also covers the case the in-memory version would miss: oppen
    /// restarting halfway through a reconcile and walking the window again.
    #[tokio::test]
    async fn applying_the_same_batch_three_times_leaves_the_ledger_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let start = u64::try_from(T0).expect("epoch fits");
        let fills = vec![
            fill_at(21, start + 1, None),
            fill_at(22, start + 2, None),
            fill_at(23, start + 3, None),
        ];

        let first = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        let recovered = first.apply_fills(user, &fills, None).expect("apply once");
        assert_eq!(recovered.fills_recorded, 3);
        assert_eq!(recovered.duplicates, 0);
        let after_first = head(&ledger);

        for attempt in 2..=3 {
            let again =
                Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("fresh reconciler");
            let repeat = again.apply_fills(user, &fills, None).expect("apply again");
            assert_eq!(repeat.fills_recorded, 0, "attempt {attempt} wrote a row");
            assert_eq!(repeat.duplicates, 3, "attempt {attempt} lost the dedupe");
            assert_eq!(
                head(&ledger),
                after_first,
                "attempt {attempt} moved the chain head"
            );
        }

        assert_eq!(chain_fills(&ledger).len(), 3);
        assert!(ledger.verify().expect("verify").first_break.is_none());
    }

    /// Re-running a whole gap is free, including the ledger row that says the
    /// gap is closed. This is the reconnect-flap case: the reconciler is
    /// pointed at the same window twice.
    #[tokio::test]
    async fn reconciling_the_same_gap_twice_records_nothing_new() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let scope = Subscription::UserFills { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 30_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        let start = u64::try_from(T0).expect("epoch fits");
        let fills = vec![fill_at(31, start + 10, None), fill_at(32, start + 20, None)];

        let reconciler =
            Reconciler::new(&ledger, FakeVenue::new(fills.clone())).expect("reconciler");
        let first = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("first pass");
        assert_eq!(first.recovered.fills_recorded, 2);
        let after_first = head(&ledger);

        let second = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("second pass");
        assert_eq!(second.recovered.fills_recorded, 0);
        assert_eq!(second.recovered.duplicates, 2);
        assert!(matches!(second.status, GapStatus::Reconciled));
        assert_eq!(head(&ledger), after_first);
    }

    /// An unmatched fill is a finding, never a dropped row.
    #[tokio::test]
    async fn an_unmatched_fill_lands_in_external_rather_than_vanishing() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let start = u64::try_from(T0).expect("epoch fits");
        // A liquidation carries no cloid; a trade from another tool carries
        // one oppen never issued. Neither matches, and both must be kept.
        let orphan = cloid(0xdead);
        let fills = vec![
            fill_at(41, start + 1, None),
            fill_at(42, start + 2, Some(orphan.clone())),
        ];

        let reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        let recovered = reconciler.apply_fills(user, &fills, None).expect("apply");

        assert_eq!(recovered.fills_recorded, 2);
        assert_eq!(recovered.external, 2);
        assert_eq!(recovered.attributed, 0);
        assert_eq!(chain_tids(&ledger), BTreeSet::from([41, 42]));
        assert_eq!(recovered.findings.len(), 2, "both are findings");
        assert!(recovered.findings.contains(&Finding::UnattributedFill {
            tid: 42,
            oid: 900_042,
            coin: "ETH".to_owned(),
            ts_ms: T0 + 2,
            cloid: Some(orphan.as_str().to_owned()),
        }));
        for event in chain_fills(&ledger) {
            let payload = event.payload.as_ref().expect("payload");
            assert_eq!(payload["attribution"], json!("external"));
            assert!(
                event.agent_id.is_none(),
                "an external fill must not name an agent"
            );
        }
    }

    /// A cloid nested inside the payload still attributes the fill.
    ///
    /// The guardrail engine's `Clearance` carries the cloid one level down
    /// inside `kind`, and a hand-built intent payload puts it at the root.
    /// Binding the join to one exact pointer would silently reclassify every
    /// attributed fill as external the day that shape changed.
    #[tokio::test]
    async fn a_nested_cloid_still_joins_the_fill_to_its_intent() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let nested = cloid(7);
        ledger
            .record_intent(&NewIntent {
                agent_id: "carry",
                ts_ms: T0,
                payload: &json!({
                    "clearance": { "kind": { "cleared": "order", "cloid": nested.as_str() } },
                    "reason": "nested the way Clearance serialises",
                }),
                snapshot: None,
            })
            .expect("intent");

        let reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        let start = u64::try_from(T0).expect("epoch fits");
        let recovered = reconciler
            .apply_fills(user, &[fill_at(51, start + 1, Some(nested))], None)
            .expect("apply");
        assert_eq!(recovered.attributed, 1);
        assert_eq!(recovered.external, 0);
    }

    /// The measured venue behaviour that decides the whole walk.
    ///
    /// The page cap can end a page **inside** a millisecond. On mainnet on
    /// 2026-09-04 a full page carried 2 of the 28 fills stamped
    /// 1776775402431; the next page, requested from that same millisecond,
    /// carried all 28. Advancing the cursor to `last + 1` would have dropped
    /// 26 real fills.
    ///
    /// This reproduces that shape at a small page size: six fills at distinct
    /// milliseconds, then five sharing one, then three after it. The walk must
    /// come back with all fourteen.
    #[tokio::test]
    async fn a_full_page_advances_to_its_last_timestamp_and_never_past_it() {
        let mut fills: Vec<Fill> = (0..6).map(|i| fill_at(100 + i, 1_000 + i, None)).collect();
        fills.extend((0..5).map(|i| fill_at(200 + i, 1_006, None)));
        fills.extend((0..3).map(|i| fill_at(300 + i, 1_007, None)));
        let split_ms: Vec<u64> = (200..205).collect();

        let venue = FakeVenue::new(fills).with_page_limit(8);
        let config = ReconcileConfig {
            max_pages: 16,
            page_limit: 8,
        };
        let (recovered, pages) = backfill_fills(&venue, account(), 0, config)
            .await
            .expect("walk the window");

        let tids: BTreeSet<u64> = recovered.iter().map(|fill| fill.tid).collect();
        assert_eq!(tids.len(), 14, "a fill was lost paging the window");
        for tid in &split_ms {
            assert!(
                tids.contains(tid),
                "tid {tid} sat in the millisecond the page cap cut in half"
            );
        }
        assert_eq!(pages, 3);
        // The second request starts *at* the boundary millisecond, not after
        // it. That inclusive re-read is what recovers the rows the cap cut,
        // and the duplicate it returns is removed by tid.
        assert_eq!(
            venue.requests(),
            vec![(0, None), (1_006, None), (1_007, None)]
        );
    }

    /// A page that is full and cannot advance is reported, not looped on and
    /// not skipped past.
    #[tokio::test]
    async fn a_full_page_inside_one_millisecond_stalls_rather_than_looping() {
        let fills: Vec<Fill> = (0..6).map(|i| fill_at(400 + i, 5_000, None)).collect();
        let venue = FakeVenue::new(fills).with_page_limit(4);
        let config = ReconcileConfig {
            max_pages: 8,
            page_limit: 4,
        };
        let error = backfill_fills(&venue, account(), 0, config)
            .await
            .expect_err("an unadvanceable cursor must not be papered over");
        assert!(
            matches!(
                error,
                ReconcileError::PageStalled {
                    at_ms: 5_000,
                    rows: 4
                }
            ),
            "got {error}"
        );
        assert_eq!(venue.requests().len(), 2, "it stopped instead of spinning");
    }

    /// A walk that fails leaves the gap on the work list and the overlay up.
    ///
    /// `docs/spec.md` item 34: the stale overlay comes down when the window is
    /// proven backfilled, and a window that could not be paged is not proven.
    #[tokio::test]
    async fn a_failed_walk_leaves_the_gap_unreconciled() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let scope = Subscription::UserFills { user }.key();
        let gap = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(gap.gap_id, T0 + 30_000)
            .expect("close gap");

        let start = u64::try_from(T0).expect("epoch fits");
        let fills: Vec<Fill> = (0..6).map(|i| fill_at(500 + i, start + 10, None)).collect();
        let venue = FakeVenue::new(fills).with_page_limit(4);
        let config = ReconcileConfig {
            max_pages: 8,
            page_limit: 4,
        };
        let reconciler = Reconciler::with_config(&ledger, venue, config).expect("reconciler");

        let outcomes = reconciler
            .reconcile_all(&[])
            .await
            .expect("the run reports");
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(
                outcomes[0].status,
                GapStatus::Failed(ReconcileError::PageStalled { .. })
            ),
            "{:?}",
            outcomes[0].status
        );

        let still_open = ledger.unreconciled_gaps().expect("gaps");
        assert_eq!(still_open.len(), 1);
        assert_eq!(still_open[0].gap_id, gap.gap_id);
        assert!(still_open[0].reconciled_ts_ms.is_none());
        assert!(
            chain_fills(&ledger).is_empty(),
            "nothing is recorded from a walk that could not finish"
        );
    }

    /// The page budget is a bound, not a suggestion.
    #[tokio::test]
    async fn a_walk_that_never_ends_hits_the_page_budget() {
        // One fill per millisecond against a two-row page: every page is full,
        // every page advances, and the walk is simply longer than the budget.
        // (A one-row page would stall instead, correctly — its only row is
        // always the boundary row, so the cursor can never move.)
        let fills: Vec<Fill> = (0..40u64)
            .map(|i| fill_at(600 + i, 7_000 + i, None))
            .collect();
        let venue = FakeVenue::new(fills).with_page_limit(2);
        let config = ReconcileConfig {
            max_pages: 3,
            page_limit: 2,
        };
        let error = backfill_fills(&venue, account(), 0, config)
            .await
            .expect_err("the budget must bite");
        assert!(
            matches!(error, ReconcileError::TooManyPages { max_pages: 3, .. }),
            "{error}"
        );
        assert_eq!(venue.requests().len(), 3);
    }

    /// Item 19, encoded: an order the venue is not resting can only be
    /// queried. There is no method on [`UnknownOutcome`] that places anything,
    /// so a caller cannot get this wrong by mistake.
    #[tokio::test]
    async fn an_order_that_is_not_resting_can_only_be_settled_by_query() {
        let user = account();
        let resting = cloid(0xa1);
        let vanished = cloid(0xa2);
        let venue = FakeVenue::new(Vec::new())
            .with_open_orders(vec![open_order(1, Some(resting.clone()))])
            .with_status(vanished.as_str(), OrderStatusResponse::UnknownOid);

        let reconciliation = OrderReconciliation::partition(
            user,
            &[resting.clone(), vanished.clone()],
            venue.open_orders.clone(),
        );
        assert_eq!(reconciliation.resting().len(), 1);
        assert_eq!(reconciliation.unknown.len(), 1);
        assert_eq!(reconciliation.unknown[0].cloid, vanished);
        assert_eq!(reconciliation.unknown[0].user, user);

        let settled = reconciliation.settle_all(&venue).await.expect("settle");
        assert_eq!(settled, vec![(vanished, Settlement::Retired)]);
    }

    /// And when the venue does still know the order, its state comes back
    /// verbatim rather than mapped into a word this build invented.
    #[tokio::test]
    async fn a_known_order_settles_to_the_venue_status_verbatim() {
        let user = account();
        let pending = cloid(0xb1);
        let venue = FakeVenue::new(Vec::new()).with_status(
            pending.as_str(),
            OrderStatusResponse::Order {
                order: oppen_hl::types::OrderStatusEntry {
                    order: oppen_hl::types::OrderStatusOrder {
                        coin: "ETH".to_owned(),
                        side: Side::B,
                        limit_px: dec("2300"),
                        sz: dec("1"),
                        oid: 4_242,
                        timestamp: 9,
                        orig_sz: dec("1"),
                        cloid: Some(pending.clone()),
                    },
                    status: "marginCanceled".to_owned(),
                    status_timestamp: 1_234,
                },
            },
        );

        let reconciliation =
            OrderReconciliation::partition(user, std::slice::from_ref(&pending), Vec::new());
        let settled = reconciliation.settle_all(&venue).await.expect("settle");
        assert_eq!(
            settled,
            vec![(
                pending,
                Settlement::Known {
                    oid: 4_242,
                    status: "marginCanceled".to_owned(),
                    status_ts_ms: 1_234,
                }
            )]
        );
    }

    /// An `orderUpdates` gap is closed by re-reading the book, and a pending
    /// order the venue no longer knows becomes a finding rather than a resend.
    #[tokio::test]
    async fn an_order_updates_gap_is_closed_by_rereading_the_book() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let scope = Subscription::OrderUpdates { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 5_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        let gone = cloid(0xc1);
        let venue = FakeVenue::new(Vec::new()).with_open_orders(vec![open_order(9, None)]);
        let reconciler = Reconciler::new(&ledger, venue).expect("reconciler");
        let outcome = reconciler
            .reconcile_gap(&gap, std::slice::from_ref(&gone))
            .await
            .expect("reconcile");

        assert!(matches!(outcome.status, GapStatus::Reconciled));
        assert_eq!(outcome.resting_orders, Some(1));
        assert_eq!(outcome.settled.len(), 1);
        assert_eq!(outcome.settled[0].1, Settlement::Retired);
        assert!(outcome.recovered.findings.contains(&Finding::OrderRetired {
            cloid: gone.as_str().to_owned(),
        }));
        // No fills walk on this scope: the fills feed has its own gap.
        assert!(reconciler.source.requests().is_empty());
        assert!(ledger.unreconciled_gaps().expect("gaps").is_empty());
    }

    /// A market-data gap belongs to a different component, and this one must
    /// not mark it done.
    #[tokio::test]
    async fn a_market_data_gap_is_left_alone() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let gap = ledger
            .open_gap(
                &Subscription::Bbo {
                    coin: "BTC".to_owned(),
                }
                .key(),
                T0,
                None,
            )
            .expect("open gap");
        ledger.close_gap(gap.gap_id, T0 + 1_000).expect("close gap");

        let reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        let outcomes = reconciler.reconcile_all(&[]).await.expect("reconcile");
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].status, GapStatus::NotAnAccountFeed));
        assert_eq!(
            ledger.unreconciled_gaps().expect("gaps").len(),
            1,
            "an unclosable gap must stay on the work list"
        );
    }

    /// A gap whose socket has not come back has no window to backfill.
    #[tokio::test]
    async fn an_open_gap_is_reported_rather_than_guessed_at() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let gap = ledger
            .open_gap(&Subscription::UserFills { user }.key(), T0, None)
            .expect("open gap");

        let reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");
        assert!(matches!(outcome.status, GapStatus::StillOpen));
        assert_eq!(ledger.unreconciled_gaps().expect("gaps").len(), 1);
    }

    /// Money reaches the chain as decimal strings.
    ///
    /// `AGENTS.md`: `Decimal`, never `f64`, on the one table kept forever. The
    /// ledger's canonical encoder refuses a JSON float outright, so a
    /// regression here is a failed append rather than a wrong number — but the
    /// stored shape is asserted anyway, because "it did not crash" is not the
    /// property.
    #[tokio::test]
    async fn every_money_field_is_stored_as_a_decimal_string() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        let start = u64::try_from(T0).expect("epoch fits");
        reconciler
            .apply_fills(account(), &[fill_at(61, start, None)], None)
            .expect("apply");

        let rows = chain_fills(&ledger);
        assert_eq!(rows.len(), 1);
        let payload = rows[0].payload.as_ref().expect("payload");
        for field in [
            "px",
            "sz",
            "fee",
            "closed_pnl",
            "start_position",
            "builder_fee",
        ] {
            assert!(
                payload[field].is_string(),
                "{field} is {:?}, not a decimal string",
                payload[field]
            );
        }
        assert_eq!(payload["px"], json!("2307.3"));
        assert_eq!(payload["closed_pnl"], json!("-12.5"));
        assert!(payload["tid"].is_u64(), "identifiers stay integers");
    }

    /// The scope strings this module parses are the pool's own keys.
    ///
    /// Asserted rather than assumed: a rename in `oppen_hl::ws` would
    /// otherwise make every account gap unrecognisable, and the symptom would
    /// be gaps that never close rather than a compile error.
    #[test]
    fn the_scope_prefixes_match_the_pools_subscription_keys() {
        let user = account();
        assert_eq!(
            Subscription::UserFills { user }.key(),
            format!("{USER_FILLS_SCOPE_PREFIX}{user}")
        );
        assert_eq!(
            Subscription::OrderUpdates { user }.key(),
            format!("{ORDER_UPDATES_SCOPE_PREFIX}{user}")
        );
        assert_eq!(
            GapScope::parse(&Subscription::UserFills { user }.key()).expect("parse"),
            GapScope::UserFills(user)
        );
        assert_eq!(
            GapScope::parse(&Subscription::OrderUpdates { user }.key()).expect("parse"),
            GapScope::OrderUpdates(user)
        );
        assert_eq!(
            GapScope::parse("bbo:BTC").expect("parse"),
            GapScope::Other("bbo:BTC".to_owned())
        );
        // A corrupted account scope is refused, not quietly handed to
        // somebody else.
        assert!(matches!(
            GapScope::parse("userFills:not-an-address"),
            Err(ReconcileError::UnreadableScope { .. })
        ));
    }

    /// An intent recorded after the reconciler was built still attributes.
    ///
    /// The index is a cache of a chain other code writes to. The execution
    /// path appends intents whenever an agent orders, and a `Reconciler` that
    /// indexed once at construction never sees them: the fill matches
    /// nothing, lands as `external`, and the agent, the reason and the
    /// guardrail verdict are gone from the record. Nothing errors, and no
    /// count is off — the fill is there, booked to nobody.
    #[tokio::test]
    async fn an_intent_recorded_after_construction_still_attributes_its_fill() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let scope = Subscription::UserFills { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 30_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        let late = cloid(0x1a7e);
        let start = u64::try_from(T0).expect("epoch fits");
        let venue = FakeVenue::new(vec![fill_at(81, start + 10, Some(late.clone()))]);
        let reconciler = Reconciler::new(&ledger, venue).expect("reconciler");

        // Only now does the execution path record the intent.
        record_intent(&ledger, "scout", &late);

        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");
        assert_eq!(
            outcome.recovered.attributed, 1,
            "the fill was booked to nobody: the index never saw the intent"
        );
        assert_eq!(outcome.recovered.external, 0);
        let row = chain_fills(&ledger).pop().expect("the fill row");
        assert_eq!(row.agent_id.as_deref(), Some("scout"));
    }

    /// A fill another writer already chained is not chained a second time.
    ///
    /// The chain is append-only, so a duplicate cannot be taken back out. A
    /// reconciler that indexed at construction does not know about rows a
    /// restarting process, or the live feed, appended since.
    #[tokio::test]
    async fn a_fill_chained_after_construction_is_not_appended_twice() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let scope = Subscription::UserFills { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 30_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        let start = u64::try_from(T0).expect("epoch fits");
        let fill = fill_at(82, start + 10, None);
        let reconciler =
            Reconciler::new(&ledger, FakeVenue::new(vec![fill.clone()])).expect("reconciler");

        // Somebody else chains the fill after this reconciler indexed.
        let other = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        other
            .apply_fills(user, std::slice::from_ref(&fill), None)
            .expect("the other writer records it");
        assert_eq!(chain_fills(&ledger).len(), 1);

        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");
        assert_eq!(outcome.recovered.fills_recorded, 0);
        assert_eq!(outcome.recovered.duplicates, 1);
        assert_eq!(
            chain_fills(&ledger).len(),
            1,
            "tid 82 is in the append-only chain twice and cannot be removed"
        );
    }

    /// A fill cannot reach the chain without its idempotence key.
    ///
    /// The old shape read the `tid` back out of the payload to dedupe, which
    /// made every writer's payload *shape* part of the idempotence property:
    /// [`Ledger::record_outcome`] nests what it is handed under `outcome`, and
    /// a search that returned the first `tid` in a row re-chained the rest of
    /// them. None of that is representable now. A fill row has exactly one
    /// door, that door takes the trade id as an argument, and the generic
    /// paths refuse the kind outright.
    #[tokio::test]
    async fn a_fill_cannot_be_chained_without_its_idempotence_key() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let start = u64::try_from(T0).expect("epoch fits");
        let fill = fill_at(83, start + 10, None);
        let payload = fill_payload(user, &fill, &Attribution::External, None);

        assert!(matches!(
            ledger.append(&NewEvent {
                kind: EventKind::Fill,
                ts_ms: T0 + 10,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            }),
            Err(LedgerError::UseRecordFill)
        ));

        let receipt = ledger
            .record_intent(&NewIntent {
                agent_id: "scout",
                ts_ms: T0,
                payload: &json!({ "coin": "ETH" }),
                snapshot: None,
            })
            .expect("record intent");
        assert!(matches!(
            ledger.record_outcome(&receipt, EventKind::Fill, T0 + 10, &payload),
            Err(LedgerError::UseRecordFill)
        ));
        assert!(
            chain_fills(&ledger).is_empty(),
            "a refused append must not leave a row"
        );

        // And the one door records it exactly once, however many times it is
        // offered.
        let reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        for _ in 0..3 {
            reconciler
                .apply_fills(user, std::slice::from_ref(&fill), None)
                .expect("apply");
        }
        assert_eq!(chain_fills(&ledger).len(), 1);
    }

    /// The duplicate is refused by the database, inside the write.
    ///
    /// This is the property an in-memory index could not have. Its refresh
    /// closed the window before the walk and not during it, and during the
    /// walk is exactly when a resumed socket replays its subscribe snapshot.
    /// Here the second write is offered with the first one's row already
    /// committed and no cache anywhere in between: the partial unique index
    /// decides, in the same statement that would have inserted.
    #[tokio::test]
    async fn a_second_write_of_one_trade_is_refused_by_the_index_not_by_a_cache() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let other =
            Address::parse("0x2222222222222222222222222222222222222222").expect("second container");
        let payload = json!({ "coin": "ETH" });
        let (user_text, other_text) = (user.to_string(), other.to_string());
        fn one_trade<'a>(account: &'a str, payload: &'a Value) -> NewFill<'a> {
            NewFill {
                account,
                tid: 4_242,
                ts_ms: T0,
                agent_id: None,
                payload,
            }
        }

        let first = ledger
            .record_fill(&one_trade(&user_text, &payload))
            .expect("record")
            .expect("the first write lands");
        let head_after_first = head(&ledger);
        assert!(
            ledger
                .record_fill(&one_trade(&user_text, &payload))
                .expect("record")
                .is_none(),
            "the same trade id was chained twice"
        );
        assert_eq!(
            head(&ledger),
            head_after_first,
            "a refused write moved the chain head"
        );
        assert_eq!(chain_fills(&ledger).len(), 1);
        assert_eq!(first.seq, 1);

        // The key is the pair, not the trade id alone: a trade has two sides,
        // and an operator running two containers can be both of them.
        assert!(
            ledger
                .record_fill(&one_trade(&other_text, &payload))
                .expect("record")
                .is_some(),
            "the other side of the trade landed on a different container"
        );
        assert_eq!(chain_fills(&ledger).len(), 2);
        assert!(ledger.verify().expect("verify").first_break.is_none());
    }

    /// A first run has no venue instant to reason from, and says so.
    ///
    /// Walking from the epoch was the old answer, and it could not close: a
    /// container with any history exhausts the page budget before it reaches
    /// the outage, so the gap stayed open and the overlay stayed up forever.
    /// The walk is bounded instead, the boundary is reported, and no fill it
    /// finds is claimed for an outage whose venue-time window is unknown.
    #[tokio::test]
    async fn a_first_run_gap_walks_a_bounded_window_and_says_so() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let start = u64::try_from(T0).expect("epoch fits");
        let scope = Subscription::UserFills { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 30_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        let venue = FakeVenue::new(vec![fill_at(84, start + 10_000, None)]);
        let reconciler = Reconciler::new(&ledger, venue).expect("reconciler");
        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");

        assert!(matches!(outcome.status, GapStatus::Reconciled));
        assert_eq!(outcome.recovered.fills_recorded, 1);
        assert_eq!(
            reconciler.source.requests(),
            vec![(start - FIRST_RUN_LOOKBACK_MS, None)],
            "a first run must walk a bounded window, not all of history"
        );
        assert!(
            outcome
                .recovered
                .findings
                .contains(&Finding::FirstRunWindow {
                    account: user.to_string(),
                    start_ms: start - FIRST_RUN_LOOKBACK_MS,
                }),
            "the completeness boundary was not reported: {:?}",
            outcome.recovered.findings
        );
        let row = chain_fills(&ledger).pop().expect("the fill row");
        assert_eq!(
            row.payload.as_ref().expect("payload")["recovered_from_gap"],
            json!(null),
            "nothing here establishes that this fill fell inside the outage"
        );
    }

    /// A fill from after the reconnect is recorded, and is not claimed for the
    /// outage.
    ///
    /// The walk has no end bound, so it meets fills that happened after the
    /// socket came back — those are the live feed's. Stamping them
    /// `recovered_from_gap` writes a false statement into the one table kept
    /// forever.
    #[tokio::test]
    async fn a_fill_after_the_reconnect_is_not_stamped_as_recovered() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let venue_now = u64::try_from(T0).expect("epoch fits");

        let anchor = fill_at(85, venue_now, None);
        Reconciler::new(&ledger, FakeVenue::new(Vec::new()))
            .expect("reconciler")
            .apply_fills(user, std::slice::from_ref(&anchor), None)
            .expect("the live feed had been delivering");

        let scope = Subscription::UserFills { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 30_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        // One fill inside the outage, one an hour after the socket came back.
        let inside = fill_at(86, venue_now + 10_000, None);
        let after = fill_at(87, venue_now + 60 * 60 * 1_000, None);
        let venue = FakeVenue::new(vec![anchor, inside, after]);
        let reconciler = Reconciler::new(&ledger, venue).expect("reconciler");
        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");

        assert!(matches!(outcome.status, GapStatus::Reconciled));
        assert_eq!(outcome.recovered.fills_recorded, 2);
        let stamp = |tid: u64| {
            chain_fills(&ledger)
                .into_iter()
                .find_map(|event| {
                    let payload = event.payload?;
                    (payload["tid"] == json!(tid)).then(|| payload["recovered_from_gap"].clone())
                })
                .expect("the fill row")
        };
        assert_eq!(stamp(86), json!(gap.gap_id), "the outage's own fill");
        assert_eq!(
            stamp(87),
            json!(null),
            "a fill from an hour after the reconnect was claimed for the outage"
        );
    }

    /// An anchor that postdates the disconnect is not an anchor.
    ///
    /// The anchor is the newest venue stamp on a fill the chain held before the
    /// socket died, and `MAX(ts_ms)` believes whatever is in the row. One fill
    /// stamped in the future — a bad venue timestamp, or an edited file, which
    /// `Ledger` treats as in scope because the database is user-writable — then
    /// starts every later walk on that container after the fills it exists to
    /// recover. The venue truthfully answers with nothing, the page is short,
    /// and the gap is marked reconciled: the P2 gate failing while reporting
    /// success, permanently, because an append-only chain cannot drop the row.
    ///
    /// So an anchor more than [`FIRST_RUN_LOOKBACK_MS`] past the host's account
    /// of the disconnect is discarded and the bounded first-run walk answers
    /// instead. That widens the window and never narrows it, and the operator is
    /// told the boundary rather than shown an empty recovery.
    #[tokio::test]
    async fn an_anchor_from_the_future_is_discarded_rather_than_believed() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let venue_now = u64::try_from(T0).expect("epoch fits");
        let decade = 10 * 365 * 24 * 60 * 60 * 1_000;

        // One fill the venue stamped ten years out, durable before the gap
        // opens and so inside the anchor's own chain window.
        let poisoned = fill_at(91, venue_now + decade, None);
        let seeder = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        seeder
            .apply_fills(user, std::slice::from_ref(&poisoned), None)
            .expect("a stamp nobody checked");

        let scope = Subscription::UserFills { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 30_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        let missed = fill_at(92, venue_now + 10_000, None);
        let venue = FakeVenue::new(vec![poisoned, missed]);
        let reconciler = Reconciler::new(&ledger, venue).expect("reconciler");
        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");

        assert_eq!(
            reconciler.source.requests(),
            vec![(venue_now - FIRST_RUN_LOOKBACK_MS, None)],
            "a fill stamped in the future moved the window past the outage"
        );
        assert!(
            chain_tids(&ledger).contains(&92),
            "the missed fill was never fetched, and the gap closed anyway"
        );
        assert!(matches!(outcome.status, GapStatus::Reconciled));
        assert_eq!(
            outcome.recovered.findings,
            vec![
                Finding::UnattributedFill {
                    tid: 92,
                    oid: 900_092,
                    coin: "ETH".to_owned(),
                    ts_ms: T0 + 10_000,
                    cloid: None,
                },
                Finding::FirstRunWindow {
                    account: user.to_string(),
                    start_ms: venue_now - FIRST_RUN_LOOKBACK_MS,
                },
            ],
            "the completeness boundary of a walk with no believable anchor was not reported"
        );
        // Nothing is claimed for the outage: with no anchor there is no venue
        // instant that says when the outage was.
        let stamped = chain_fills(&ledger).into_iter().any(|event| {
            event
                .payload
                .is_some_and(|payload| payload["recovered_from_gap"] != json!(null))
        });
        assert!(
            !stamped,
            "a fill was claimed for an outage with no known window"
        );
    }

    /// The cloid at the payload root beats every nested one, and the walk below
    /// the root is deterministic.
    ///
    /// [`Attributions`] argues the join "degrades safely" across a payload shape
    /// change on exactly one property: an object's own `cloid` is read before any
    /// of its children. That is what this pins. What it deliberately does *not*
    /// pin as a rule is that the shallowest occurrence anywhere wins — the walk
    /// is depth-first in sorted key order, so a cloid two levels under an early
    /// key is found before one a single level under a later key. The second
    /// assertion locks that answer so the doc comment and the code cannot drift
    /// apart again.
    #[test]
    fn the_cloid_at_the_payload_root_beats_every_nested_one() {
        let (root, nested) = (cloid(11), cloid(12));
        assert_eq!(
            find_cloid(
                &json!({ "cloid": root.as_str(), "a": { "cloid": nested.as_str() } }),
                0
            ),
            Some(root.as_str().to_owned()),
            "a nested cloid outranked the one at the root"
        );

        let (early, late) = (cloid(13), cloid(14));
        assert_eq!(
            find_cloid(
                &json!({
                    "a": { "b": { "cloid": early.as_str() } },
                    "z": { "cloid": late.as_str() }
                }),
                0
            ),
            Some(early.as_str().to_owned()),
            "the walk is depth-first in sorted key order, not breadth-first"
        );
    }

    /// A fill recorded after the disconnect cannot move that gap's anchor.
    ///
    /// This is the failure the two-clock `min` hid. The anchor is read at the
    /// gap's own chain position, so a row written after the socket died — by
    /// an overlapping walk, or by the live feed once it resumed — is not a
    /// statement about what oppen was being served during the outage and
    /// cannot drag the window past the fills it exists to recover.
    #[tokio::test]
    async fn a_fill_recorded_after_the_disconnect_cannot_move_the_anchor() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();
        let venue_now = u64::try_from(T0).expect("epoch fits");

        let anchor = fill_at(88, venue_now, None);
        let seeder = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        seeder
            .apply_fills(user, std::slice::from_ref(&anchor), None)
            .expect("delivered before the socket died");

        let scope = Subscription::UserFills { user }.key();
        let opened = ledger.open_gap(&scope, T0, None).expect("open gap");
        ledger
            .close_gap(opened.gap_id, T0 + 30_000)
            .expect("close gap");
        let gap = reload_gap(&ledger, opened.gap_id);

        // A day later another walk recovers a much newer fill onto the same
        // container, so its venue stamp is now the newest in the chain.
        let day = 24 * 60 * 60 * 1_000;
        let late = fill_at(89, venue_now + day, None);
        seeder
            .apply_fills(user, std::slice::from_ref(&late), None)
            .expect("a later walk");

        let missed = fill_at(90, venue_now + 10_000, None);
        let venue = FakeVenue::new(vec![anchor, missed, late]);
        let reconciler = Reconciler::new(&ledger, venue).expect("reconciler");
        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");

        assert!(matches!(outcome.status, GapStatus::Reconciled));
        assert_eq!(
            reconciler.source.requests(),
            vec![(venue_now - VENUE_CLOCK_MARGIN_MS, None)],
            "the window started after the fill it was supposed to recover"
        );
        assert!(
            chain_tids(&ledger).contains(&90),
            "the missed fill was never fetched, and the gap closed anyway"
        );
    }

    /// One gap that can never close does not stop the ones after it.
    ///
    /// Gaps come off the work list in id order, so returning the first error
    /// means the oldest broken window blocks every later one — and the later
    /// ones are the recent trading. Both gaps here are real: the first stalls
    /// on a page it cannot advance, the second is an ordinary 30 s outage with
    /// a fill in it.
    #[tokio::test]
    async fn a_gap_that_cannot_close_does_not_block_the_ones_behind_it() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let blocked = account();
        let behind =
            Address::parse("0x2222222222222222222222222222222222222222").expect("second container");
        let start = u64::try_from(T0).expect("epoch fits");

        let stuck = ledger
            .open_gap(&Subscription::UserFills { user: blocked }.key(), T0, None)
            .expect("open the first gap");
        ledger
            .close_gap(stuck.gap_id, T0 + 1_000)
            .expect("close it");
        let later = ledger
            .open_gap(
                &Subscription::UserFills { user: behind }.key(),
                T0 + 10_000,
                None,
            )
            .expect("open the second gap");
        ledger
            .close_gap(later.gap_id, T0 + 40_000)
            .expect("close it");

        // Four fills in one millisecond against a two-row page: the first
        // gap's walk can never advance its cursor. The fifth is the second
        // gap's, at a millisecond of its own.
        let stalling: Vec<Fill> = (0..4).map(|i| fill_at(90 + i, start + 5, None)).collect();
        let config = ReconcileConfig {
            max_pages: 8,
            page_limit: 2,
        };
        let venue = FakeVenue::new(stalling)
            .with_fills_for(behind, vec![fill_at(95, start + 20_000, None)])
            .with_page_limit(2);
        let reconciler = Reconciler::with_config(&ledger, venue, config).expect("reconciler");

        let outcomes = reconciler
            .reconcile_all(&[])
            .await
            .expect("the run reports");
        assert_eq!(outcomes.len(), 2, "both gaps were worked");
        assert!(
            matches!(
                outcomes[0].status,
                GapStatus::Failed(ReconcileError::PageStalled { .. })
            ),
            "{:?}",
            outcomes[0].status
        );
        assert!(
            matches!(outcomes[1].status, GapStatus::Reconciled),
            "the second gap never got walked: {:?}",
            outcomes[1].status
        );

        // The stalled gap keeps its overlay; the one behind it does not.
        let open_gaps = ledger.unreconciled_gaps().expect("gaps");
        assert_eq!(open_gaps.len(), 1);
        assert_eq!(open_gaps[0].gap_id, stuck.gap_id);
        assert!(
            chain_tids(&ledger).contains(&95),
            "the second gap's fill is lost behind the first gap's failure"
        );
    }

    /// A mainnet source and a testnet chain never meet.
    ///
    /// `docs/decisions.md` R4: "a mainnet number that is actually a testnet
    /// number is the worst bug this product can ship". R4 puts the two
    /// networks in two files, which says nothing about a client pointed at the
    /// wrong one — a mainnet `InfoClient` would answer a testnet gap with real
    /// mainnet fills and chain them, and nothing downstream could tell.
    /// Refused where the pair is made, so no such reconciler exists to call.
    #[test]
    fn a_source_on_another_network_is_refused_at_construction() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        assert_eq!(ledger.network(), Network::Testnet);

        let error = Reconciler::new(
            &ledger,
            FakeVenue::new(Vec::new()).on_network(Network::Mainnet),
        )
        .expect_err("a mainnet source must not reconcile a testnet chain");
        assert!(
            matches!(
                error,
                ReconcileError::NetworkMismatch {
                    ledger: Network::Testnet,
                    venue: Network::Mainnet,
                }
            ),
            "{error}"
        );

        // And the matching pair is accepted, so the check is the network and
        // not the construction.
        assert!(Reconciler::new(&ledger, FakeVenue::new(Vec::new())).is_ok());
    }

    /// A gap row that runs backwards is refused rather than clamped.
    #[test]
    fn an_inverted_window_is_refused() {
        let gap = Gap {
            gap_id: 1,
            scope: "userFills:x".to_owned(),
            opened_ts_ms: T0 + 10,
            closed_ts_ms: Some(T0),
            reconciled_ts_ms: None,
            open_seq: 1,
            close_seq: Some(2),
            note: None,
        };
        // Refused with an anchor and without one: the gap row is the only
        // account oppen has of when it stopped listening, and a row that
        // closes before it opens is not an account of anything.
        assert!(matches!(
            outage_window(&gap, T0, None),
            Err(ReconcileError::UnusableWindow { gap_id: 1, .. })
        ));
        assert!(matches!(
            outage_window(&gap, T0, Some(1_000)),
            Err(ReconcileError::UnusableWindow { gap_id: 1, .. })
        ));
    }

    /// A `cloid` key holding something that is not a cloid claims nothing.
    #[test]
    fn a_malformed_cloid_field_does_not_attribute_anything() {
        assert_eq!(find_cloid(&json!({ "cloid": "nonsense" }), 0), None);
        assert_eq!(find_cloid(&json!({ "cloid": 7 }), 0), None);
        let good = cloid(3);
        assert_eq!(
            find_cloid(&json!({ "a": [{ "cloid": good.as_str() }] }), 0),
            Some(good.as_str().to_owned())
        );
        // A malformed value at the root does not end the search: the walk
        // steps over it and keeps looking.
        assert_eq!(
            find_cloid(
                &json!({ "cloid": "nonsense", "a": { "cloid": good.as_str() } }),
                0
            ),
            Some(good.as_str().to_owned())
        );
        // Deep enough to be a denial of service is deep enough to refuse.
        let mut nested = json!({ "cloid": good.as_str() });
        for _ in 0..(MAX_PAYLOAD_DEPTH + 2) {
            nested = json!({ "wrap": nested });
        }
        assert_eq!(find_cloid(&nested, 0), None);
    }

    /// Re-measures the venue contract this module is built on, against the
    /// live public endpoint. Read-only and key-free; `docs/specs/fair-value.md`
    /// §14.2 says so explicitly for the info surface.
    ///
    /// The window and address are the ones measured on 2026-09-04. If the
    /// venue stops serving fills that old this test starts failing on
    /// retention rather than on a regression — read the assertion that fails
    /// before believing it.
    #[tokio::test]
    #[ignore = "hits the public mainnet info endpoint"]
    async fn live_user_fills_by_time_pages_the_way_this_module_assumes() {
        let info = VenueSource::new(Network::Mainnet).expect("client");
        let user =
            Address::parse("0x85ecf584f25db6f146718b86d493e33c5af72052").expect("measured account");

        let first = info
            .user_fills_by_time(user, 1_776_700_000_000, Some(1_776_800_000_000))
            .await
            .expect("first page");
        assert_eq!(
            first.len(),
            USER_FILLS_PAGE_LIMIT,
            "the page cap moved; every constant in this module depends on it"
        );
        assert!(
            first.windows(2).all(|pair| pair[0].time <= pair[1].time),
            "userFillsByTime is oldest-first"
        );
        let newest = first.last().expect("a full page").time;
        assert!(
            newest < 1_776_800_000_000,
            "the page truncated from the newest end, with no cursor"
        );

        let second = info
            .user_fills_by_time(user, newest, Some(1_776_800_000_000))
            .await
            .expect("second page");
        // startTime is inclusive, and the cap cut a millisecond in half: the
        // second page carries strictly more rows at the boundary than the
        // first did. Advancing to `newest + 1` would have dropped them.
        let carried = |rows: &[Fill]| rows.iter().filter(|fill| fill.time == newest).count();
        assert!(
            carried(&second) > carried(&first),
            "the boundary millisecond was not split; re-read the module docs"
        );

        // And the whole walk recovers every distinct row across both pages.
        // The walk has no end bound, so this pages from the measured start to
        // the venue's own now. It recovers every distinct row on the way.
        let (all, pages) =
            backfill_fills(&info, user, 1_776_700_000_000, ReconcileConfig::default())
                .await
                .expect("walk");
        let tids: BTreeSet<u64> = all.iter().map(|fill| fill.tid).collect();
        assert_eq!(tids.len(), all.len(), "the walk returned a duplicate");
        assert!(all.len() > USER_FILLS_PAGE_LIMIT, "the walk did not page");
        assert!(pages >= 2);
    }

    /// The order half of `docs/spec.md` item 9, against the live public
    /// endpoint: `frontendOpenOrders` parses, and a cloid that is not resting
    /// settles by query.
    ///
    /// The account is a public mainnet address that had 48 resting orders when
    /// this was written. Its book is not under this test's control, so the
    /// assertions are split: the ones about a *known* order run only when the
    /// address actually has one, and the `unknownOid` path is asserted
    /// unconditionally because a cloid nobody has ever placed is always
    /// unknown.
    ///
    /// This does **not** prove the signing or placement path. No funded key
    /// exists; everything here is read-only.
    #[tokio::test]
    #[ignore = "hits the public mainnet info endpoint"]
    async fn live_orders_reconcile_and_settle_by_cloid() {
        let info = VenueSource::new(Network::Mainnet).expect("client");
        let user =
            Address::parse("0x399965e15d4e61ec3529cc98b7f7ebb93b733336").expect("measured account");

        let open_orders = info
            .frontend_open_orders(user)
            .await
            .expect("frontendOpenOrders must deserialize whole");

        // A cloid the venue cannot know: 128 bits of a fixed pattern nobody
        // placed. Its settlement is `Retired`, which is the answer this
        // module documents as *not* proof of absence.
        let never_placed = cloid(0x0bad_0bad_0bad_0bad_0bad_0bad_0bad_0badu128);
        let reconciliation = OrderReconciliation::partition(
            user,
            std::slice::from_ref(&never_placed),
            open_orders.clone(),
        );
        assert_eq!(reconciliation.unknown.len(), 1);
        let settled = reconciliation.settle_all(&info).await.expect("settle");
        assert_eq!(settled, vec![(never_placed, Settlement::Retired)]);

        // And when the address does have a resting order with a cloid, the
        // same query returns its live state verbatim.
        let Some(resting) = open_orders.iter().find_map(|order| order.cloid.clone()) else {
            return;
        };
        let known = OrderReconciliation::partition(
            user,
            std::slice::from_ref(&resting),
            open_orders.clone(),
        );
        assert!(
            known.unknown.is_empty(),
            "a resting cloid is not an unknown outcome"
        );
        // Partitioned against an empty book it *is* unknown, which is the
        // shape a reconnect produces before `frontendOpenOrders` is read.
        let forced = OrderReconciliation::partition(user, &[resting], Vec::new());
        let settled = forced.settle_all(&info).await.expect("settle");
        assert!(
            matches!(settled.first(), Some((_, Settlement::Known { status, .. })) if !status.is_empty()),
            "a resting order settles to a venue status, got {settled:?}"
        );
    }
}
