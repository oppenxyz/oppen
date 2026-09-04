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
//! * **Idempotence, keyed on the venue's own identifiers.** The window is
//!   re-walked on every reconnect, retry and restart, and the same fill will
//!   be offered many times — by the backfill, by the `userFills` subscribe
//!   snapshot, and by an overlapping page (the venue's `startTime` is
//!   *inclusive*, measured). Nothing may be recorded twice, so
//!   [`LedgerIndex`] reads the `tid` of every fill already in the chain and
//!   dedupes against that. It is rebuilt from the chain rather than kept in
//!   memory precisely so that a restart mid-reconcile changes nothing.
//! * **A fill is never dropped for failing to match.** Every fill is
//!   classified [`Attribution::Attributed`], [`Attribution::Manual`] or
//!   [`Attribution::External`] and recorded either way
//!   (`docs/specs/history.md` §2, `docs/spec.md` item 33). An unmatched fill
//!   is a [`Finding`], not an error: it means the operator traded elsewhere,
//!   or was liquidated, and both belong in the history.
//! * **After a timeout the only safe move is query-by-cloid.** `docs/spec.md`
//!   item 19. [`UnknownOutcome`] is the type an unresolved order arrives as,
//!   and its only method is a query — see that type for why it has no
//!   `resend`.
//! * **A gap closes only once its window is proven contiguous.** A page that
//!   cannot be advanced, a request that failed, or a walk that ran past its
//!   page budget leaves `reconciled_ts_ms` null, so the staleness overlay
//!   (`docs/spec.md` item 34) stays up and the work list keeps the gap.
//!
//! # What the venue actually does
//!
//! Measured against public mainnet `POST /info` on 2026-09-04 (read-only, no
//! key). These are the facts the walk in [`backfill_fills`] is built on:
//!
//! | Fact | Measurement |
//! |---|---|
//! | `userFillsByTime` returns **oldest first** | a 100,000 s window returned rows in ascending `time` |
//! | it caps a page at [`USER_FILLS_PAGE_LIMIT`] rows | two separate windows returned exactly 2,000 |
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

use serde::Serialize;
use serde_json::{Value, json};

use oppen_hl::info::OrderRef;
use oppen_hl::types::{Fill, OpenOrder, OrderStatusResponse};
use oppen_hl::wire::Cloid;
use oppen_hl::ws::GapWindow;
use oppen_hl::{Address, Error as VenueError, InfoClient};

use crate::ledger::{Event, EventKind, Gap, Ledger, LedgerError, MAX_PAGE, NewEvent, now_ms};

/// Rows the venue returns for one `userFillsByTime` request, at most.
///
/// Measured on mainnet 2026-09-04: two different windows over an active
/// account each answered with exactly 2,000 rows and a newest row well inside
/// the requested `endTime`. The page is truncated from the newest end and
/// carries no cursor, so a caller that widens the window instead of paging
/// loses the tail silently — the same shape as
/// [`oppen_hl::types::FUNDING_HISTORY_PAGE_LIMIT`], at four times the size.
pub const USER_FILLS_PAGE_LIMIT: usize = 2_000;

/// How many pages one window may take before the walk gives up.
///
/// A bound rather than a `loop`: a venue that answers a full page forever
/// would otherwise spin against the rate budget with the operator's overlay
/// still up and no error anywhere. At the measured cap this is a million
/// fills in one gap, which is far past any reconnect and well into "something
/// is wrong". Exceeding it is [`ReconcileError::TooManyPages`] and leaves the
/// gap unreconciled, which is the honest outcome.
pub const DEFAULT_MAX_PAGES: usize = 512;

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
/// Idempotence is keyed on this, so it is a storage format: moving it orphans
/// every fill already in the chain and the next reconcile records them all a
/// second time.
const FILL_TID_FIELD: &str = "tid";

/// The payload key an intent or operator action carries its client order id
/// under. See [`LedgerIndex`] for why it is searched at any depth.
const CLOID_FIELD: &str = "cloid";

/// How deep [`LedgerIndex`] will search a payload for a `cloid`.
///
/// Bounded because the walk is recursive and a payload is written by another
/// component: a stack overflow is a panic on an input path, which
/// `AGENTS.md` forbids.
const MAX_CLOID_DEPTH: usize = 8;

/// What can go wrong reconciling.
///
/// `AGENTS.md` conventions: `thiserror`, no bare strings, and nothing on an
/// input path panics. Every variant here leaves the gap unreconciled on
/// purpose — the caller retries, and the retry is free because the walk is
/// idempotent.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
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
    PageStalled {
        /// The millisecond the walk is stuck on.
        at_ms: u64,
        /// How many rows came back, for the report.
        rows: usize,
    },
    /// The walk hit [`ReconcileConfig::max_pages`].
    #[error("backfill of {start_ms}..{end_ms} exceeded {max_pages} pages")]
    TooManyPages {
        /// Window start, unix ms.
        start_ms: u64,
        /// Window end, unix ms.
        end_ms: u64,
        /// The budget that was exceeded.
        max_pages: usize,
    },
    /// A gap's window ends before it starts, or carries a timestamp outside
    /// unix milliseconds. Only reachable by editing the database by hand;
    /// refused rather than clamped, because a clamped window is a window that
    /// silently covers the wrong time.
    #[error("gap {gap_id} has an unusable window {start_ms}..{end_ms}")]
    UnusableWindow {
        /// Which gap.
        gap_id: i64,
        /// Recorded start, unix ms.
        start_ms: i64,
        /// Recorded end, unix ms.
        end_ms: i64,
    },
    /// A venue timestamp did not fit the ledger's signed milliseconds.
    #[error("venue timestamp {0} is out of range")]
    TimestampOutOfRange(u64),
    /// A gap scope named an address that does not parse.
    #[error("gap scope {scope:?} does not name a valid address")]
    UnreadableScope {
        /// The scope string as stored.
        scope: String,
    },
}

/// Reconcile result alias.
pub type Result<T> = std::result::Result<T, ReconcileError>;

/// The venue reads this module needs, and nothing else.
///
/// A trait rather than a bare [`InfoClient`] for one reason: the P2 gate has
/// to be provable without a funded account. Every query here is public and
/// key-free, but a *test* still cannot depend on a live venue having the exact
/// fills a 30-second window needs, so the gate is driven from recorded
/// fixtures through this trait. [`InfoClient`] implements it, so the
/// production path is the same code with a different source.
///
/// It is deliberately three methods wide. `docs/spec.md` item 9 names exactly
/// these three, and a wider trait would let this module reach for state it has
/// no business reconciling against.
pub trait ReconcileSource {
    /// Fills for `user` in `[start_ms, end_ms]`, **oldest first**, capped at
    /// [`USER_FILLS_PAGE_LIMIT`] rows and truncated from the newest end.
    /// `start_ms` is inclusive. See the module docs for the measurements.
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

impl ReconcileSource for InfoClient {
    async fn user_fills_by_time(
        &self,
        user: Address,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> std::result::Result<Vec<Fill>, VenueError> {
        InfoClient::user_fills_by_time(self, user, start_ms, end_ms).await
    }

    async fn frontend_open_orders(
        &self,
        user: Address,
    ) -> std::result::Result<Vec<OpenOrder>, VenueError> {
        InfoClient::frontend_open_orders(self, user).await
    }

    async fn order_status(
        &self,
        user: Address,
        order: OrderRef,
    ) -> std::result::Result<OrderStatusResponse, VenueError> {
        InfoClient::order_status(self, user, order).await
    }
}

/// How a fill was joined to the record (`docs/specs/history.md` §2).
///
/// The ledger is authoritative for *why* something happened and the venue for
/// *what* happened; this is the outcome of joining them. Every fill gets one,
/// including the ones that match nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "attribution", rename_all = "snake_case")]
pub enum Attribution {
    /// Matched to a recorded [`EventKind::OrderIntent`] by cloid, so the fill
    /// carries the agent, and through the intent row the reason and the
    /// guardrail verdict that allowed it.
    Attributed {
        /// The agent whose intent this fill answers.
        agent_id: String,
        /// Chain position of the intent row.
        intent_seq: u64,
        /// Chain hash of the intent row. Carried as well as the seq so the
        /// link survives someone renumbering rows, the same reasoning as
        /// [`crate::ledger::IntentReceipt::hash`].
        intent_hash: String,
    },
    /// Matched to an [`EventKind::OperatorAction`] — the human's own ticket in
    /// oppen (`docs/spec.md` item 33).
    Manual {
        /// Chain position of the operator action row.
        action_seq: u64,
    },
    /// No matching intent. Traded outside oppen, or a liquidation nobody
    /// requested. `docs/spec.md` item 33 reserves the `manual · external`
    /// bucket for exactly this, and `docs/specs/history.md` §2 is explicit
    /// that it is a finding rather than an error.
    External,
}

impl Attribution {
    /// The stored discriminator. Hashed into the chain through the payload, so
    /// it is a storage format and stable once written.
    pub fn as_str(&self) -> &'static str {
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
    pub fn agent_id(&self) -> Option<&str> {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "finding", rename_all = "snake_case")]
pub enum Finding {
    /// A fill that matched no intent and no operator action.
    UnattributedFill {
        /// Venue trade id, the fill's identity.
        tid: u64,
        /// Venue order id.
        oid: u64,
        /// Symbol.
        coin: String,
        /// Venue timestamp, ms.
        ts_ms: i64,
        /// The cloid the fill carried, when it had one. A fill with a cloid
        /// that matches nothing is the more interesting case: oppen puts a
        /// cloid on everything it places (item 19), so this is either another
        /// tool's order or a row whose intent has been redacted.
        cloid: Option<String>,
    },
    /// An order oppen believed was live is not resting and the venue no longer
    /// knows the cloid. See [`Settlement::Retired`] — this is not proof it
    /// never existed, only that the order record is gone.
    OrderRetired {
        /// The client order id that was queried.
        cloid: String,
    },
}

/// What one backfill recovered.
///
/// Returned rather than logged so the console can state what closing a gap
/// actually produced, and so a test can assert on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Recovered {
    /// Fills the venue returned, after removing the inclusive-boundary
    /// overlap between pages.
    pub fills_seen: usize,
    /// Fills appended to the chain by this run.
    pub fills_recorded: usize,
    /// Fills already in the chain. Not an anomaly: the window overlaps what
    /// the live feed already delivered, and a retry re-walks it entirely.
    pub duplicates: usize,
    /// Of the recorded fills, how many matched an intent.
    pub attributed: usize,
    /// How many matched an operator action.
    pub manual: usize,
    /// How many matched nothing.
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
/// public constructor. It is produced only by [`OrderReconciliation`], it
/// carries the cloid and the account it belongs to, and the single thing a
/// caller can do with it is [`UnknownOutcome::settle`], which is a query. The
/// rule is not a comment somewhere near the retry loop; there is no retry loop
/// to put a comment near.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownOutcome {
    user: Address,
    cloid: Cloid,
}

impl UnknownOutcome {
    /// The client order id whose fate is unknown.
    pub fn cloid(&self) -> &Cloid {
        &self.cloid
    }

    /// The account it was placed for.
    pub fn account(&self) -> Address {
        self.user
    }

    /// Ask the venue what became of it. The only move item 19 allows.
    pub async fn settle<S: ReconcileSource>(&self, source: &S) -> Result<Settlement> {
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

/// What the venue said about an [`UnknownOutcome`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "settlement", rename_all = "snake_case")]
pub enum Settlement {
    /// The venue still has the order and reports its state.
    Known {
        /// Exchange order id.
        oid: u64,
        /// The venue's own status word: `open`, `filled`, `canceled`,
        /// `triggered`, `rejected`, `marginCanceled`, … Kept verbatim rather
        /// than mapped, because a status this build has not seen must not be
        /// silently folded into one it has.
        status: String,
        /// When the venue stamped that status, ms.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderReconciliation {
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
    pub fn partition(user: Address, pending: &[Cloid], open_orders: Vec<OpenOrder>) -> Self {
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
    pub fn resting(&self) -> &[OpenOrder] {
        &self.resting
    }

    /// Everything the caller believed was live that the venue is not resting.
    pub fn unknown(&self) -> &[UnknownOutcome] {
        &self.unknown
    }

    /// Settle every unknown order by query, in cloid order.
    ///
    /// Sequential rather than concurrent: `docs/spec.md` item 10 gives the
    /// whole address one request budget, and a reconcile that fans out after a
    /// reconnect is the worst moment to spend it.
    pub async fn settle_all<S: ReconcileSource>(
        &self,
        source: &S,
    ) -> Result<Vec<(Cloid, Settlement)>> {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapScope {
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
    pub fn parse(scope: &str) -> Result<Self> {
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
    pub fn account(&self) -> Option<Address> {
        match self {
            GapScope::UserFills(user) | GapScope::OrderUpdates(user) => Some(*user),
            GapScope::Other(_) => None,
        }
    }
}

/// What happened to one gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GapStatus {
    /// The window was walked to its end and `reconciled_ts_ms` is now set.
    Reconciled,
    /// The socket has not come back, so the window has no end yet. Nothing to
    /// backfill: a window with an open end would be backfilled against a
    /// moving target and could never be proven contiguous.
    StillOpen,
    /// The scope is not an account feed. Left for whoever owns it, with
    /// `reconciled_ts_ms` untouched.
    NotAnAccountFeed,
}

/// The report for one gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GapOutcome {
    /// Which gap.
    pub gap_id: i64,
    /// Its stored scope.
    pub scope: String,
    /// What was done about it.
    pub status: GapStatus,
    /// What the fills walk recovered. Empty for a scope that carries no fills.
    pub recovered: Recovered,
    /// How many orders the venue reports resting, for an account scope that
    /// was checked. `None` when no order query was made.
    pub resting_orders: Option<usize>,
    /// Orders the caller believed live that the venue is not resting, settled
    /// by query. `docs/spec.md` item 19 — never resent.
    pub settled: Vec<(String, Settlement)>,
}

/// Knobs with a defensible default each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileConfig {
    /// Page budget for one window. See [`DEFAULT_MAX_PAGES`].
    pub max_pages: usize,
    /// How many rows a full page holds. Overridable only so a test can prove
    /// the split-millisecond and stall behaviours without fabricating 2,000
    /// fixture rows per page; production uses [`USER_FILLS_PAGE_LIMIT`].
    pub page_limit: usize,
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
}

/// The chain, indexed for the two questions reconciling asks of it.
///
/// * *Have I already recorded this fill?* — the set of every `tid` in an
///   [`EventKind::Fill`] row. This is what makes re-applying a batch a no-op,
///   and it is read from the chain rather than remembered in memory so that a
///   crash mid-window, a restart, or a second process changes nothing.
/// * *Whose order was this?* — cloid to intent, and cloid to operator action.
///
/// The index is built by paging [`Ledger::get_events`] and refreshed
/// incrementally from the last seq it saw, so steady-state reconciling reads
/// only the rows appended since. The first build is a full scan.
///
/// **The cloid is searched at any depth, up to [`MAX_CLOID_DEPTH`].** The
/// payload of an intent row is written by the execution path, and at this
/// phase its shape is not fixed — the guardrail engine's `Clearance` nests the
/// cloid one level down inside `kind`, a hand-built intent payload would put it
/// at the root. Binding the join to one exact pointer would make a later,
/// reasonable payload change silently reclassify every attributed fill as
/// external, which is a data-loss-shaped bug that no test outside this module
/// would catch. Searching for the field is the version that degrades safely.
///
/// **A redacted payload cannot be indexed.** [`Ledger::redact`] nulls the
/// payload, so a redacted fill row's `tid` is invisible here and a redacted
/// intent's cloid stops matching. `docs/decisions.md` D-e keeps records
/// forever and redaction is an operator action on agent-authored text, so this
/// is not an expected path — but it is the one hole in the idempotence
/// property, and it closes when `fills` gets its own table with `tid` as the
/// primary key (`docs/specs/history.md` §3.1).
#[derive(Debug, Default)]
pub struct LedgerIndex {
    intents: BTreeMap<String, IntentRef>,
    manual: BTreeMap<String, u64>,
    applied_tids: BTreeSet<u64>,
    cursor: u64,
}

impl LedgerIndex {
    /// Build the index by walking the whole chain.
    pub fn build(ledger: &Ledger) -> Result<Self> {
        let mut index = LedgerIndex::default();
        index.refresh(ledger)?;
        Ok(index)
    }

    /// Take in every row appended since the last look.
    ///
    /// A `resync_required` answer restarts the scan from genesis. It cannot
    /// happen while D-e holds — records are never deleted — so treating it as
    /// "rebuild" rather than as an error costs nothing and fails safe if that
    /// ever changes.
    pub fn refresh(&mut self, ledger: &Ledger) -> Result<()> {
        loop {
            let page = ledger.get_events(self.cursor, MAX_PAGE)?;
            if page.resync_required {
                self.intents.clear();
                self.manual.clear();
                self.applied_tids.clear();
                self.cursor = 0;
                continue;
            }
            if page.events.is_empty() {
                return Ok(());
            }
            for event in &page.events {
                self.observe(event);
            }
            self.cursor = page.next_cursor;
        }
    }

    /// Index one row.
    fn observe(&mut self, event: &Event) {
        let Some(payload) = event.payload.as_ref() else {
            return;
        };
        match event.kind {
            EventKind::Fill => {
                if let Some(tid) = payload.get(FILL_TID_FIELD).and_then(Value::as_u64) {
                    self.applied_tids.insert(tid);
                }
            }
            EventKind::OrderIntent => {
                if let Some(cloid) = find_cloid(payload, 0) {
                    self.intents.insert(
                        cloid,
                        IntentRef {
                            seq: event.seq,
                            hash: event.hash.clone(),
                            agent_id: event.agent_id.clone(),
                        },
                    );
                }
            }
            EventKind::OperatorAction => {
                if let Some(cloid) = find_cloid(payload, 0) {
                    self.manual.insert(cloid, event.seq);
                }
            }
            _ => {}
        }
    }

    /// Whether this trade id is already in the chain.
    pub fn contains_fill(&self, tid: u64) -> bool {
        self.applied_tids.contains(&tid)
    }

    /// Classify one fill.
    ///
    /// An intent wins over an operator action carrying the same cloid: a cloid
    /// is oppen's own 128-bit identifier, so the collision means the operator
    /// row is about the agent's order, and the agent attribution is the one
    /// that carries the reason and the guardrail verdict.
    pub fn attribution(&self, cloid: Option<&Cloid>) -> Attribution {
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
                };
            }
        }
        if let Some(seq) = self.manual.get(cloid.as_str()) {
            return Attribution::Manual { action_seq: *seq };
        }
        Attribution::External
    }
}

/// Find a `cloid` field anywhere in a payload, deterministically.
///
/// Objects are walked in sorted key order — `serde_json`'s map iteration order
/// depends on the `preserve_order` feature, which any crate in the workspace
/// can switch on, and the classification of a fill must not depend on that.
/// The first well-formed cloid wins; a `cloid` field holding something that is
/// not a cloid is skipped rather than accepted, so a payload cannot claim an
/// order by writing nonsense into that key.
fn find_cloid(value: &Value, depth: usize) -> Option<String> {
    if depth > MAX_CLOID_DEPTH {
        return None;
    }
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            if let Some(text) = map.get(CLOID_FIELD).and_then(Value::as_str)
                && let Ok(cloid) = Cloid::parse(text)
            {
                return Some(cloid.as_str().to_owned());
            }
            keys.into_iter()
                .find_map(|key| map.get(key).and_then(|inner| find_cloid(inner, depth + 1)))
        }
        Value::Array(items) => items.iter().find_map(|item| find_cloid(item, depth + 1)),
        _ => None,
    }
}

/// Walk `userFillsByTime` over exactly one window and return every distinct
/// fill in it.
///
/// This is the whole of the venue's paging contract in one place, and every
/// line of it is a measurement (see the module docs):
///
/// * the cursor starts at `start_ms` and every request carries `end_ms`, so
///   the walk covers exactly the window it was given;
/// * a page shorter than `page_limit` ends the walk — the venue has nothing
///   more in the window;
/// * a full page advances the cursor to that page's **newest timestamp**, not
///   past it, because the cap can cut a millisecond in half; the boundary rows
///   come back and are removed by `tid`;
/// * a full page whose newest timestamp equals the cursor cannot advance at
///   all, and that is [`ReconcileError::PageStalled`] rather than a silent
///   skip or an infinite loop.
///
/// Nothing is written here. Recording is [`Reconciler::apply_fills`], so a
/// caller can walk a window and inspect it without touching the chain.
pub async fn backfill_fills<S: ReconcileSource>(
    source: &S,
    user: Address,
    window: GapWindow,
    config: ReconcileConfig,
) -> Result<(Vec<Fill>, usize)> {
    let mut cursor = window.start_ms;
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut out: Vec<Fill> = Vec::new();
    let mut pages = 0usize;

    loop {
        if pages >= config.max_pages {
            return Err(ReconcileError::TooManyPages {
                start_ms: window.start_ms,
                end_ms: window.end_ms,
                max_pages: config.max_pages,
            });
        }
        let page = source
            .user_fills_by_time(user, cursor, Some(window.end_ms))
            .await?;
        pages += 1;
        let rows = page.len();
        let mut newest = cursor;
        for fill in page {
            newest = newest.max(fill.time);
            // A fill outside the requested window is kept rather than
            // filtered: it is a real fill for this account, dropping it would
            // be the exact failure this module exists to prevent, and
            // recording it twice is impossible anyway.
            if seen.insert(fill.tid) {
                out.push(fill);
            }
        }
        if rows < config.page_limit {
            // The venue served everything it has in the window.
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
/// Holds a borrowed [`Ledger`], a [`ReconcileSource`] and the chain index.
/// Every ledger call blocks, so an async caller runs
/// [`Reconciler::apply_fills`] on a blocking pool the same way it runs any
/// other ledger write (`docs/decisions.md` R1 keeps the core runtime-free).
#[derive(Debug)]
pub struct Reconciler<'a, S> {
    ledger: &'a Ledger,
    source: S,
    index: LedgerIndex,
    config: ReconcileConfig,
}

impl<'a, S: ReconcileSource> Reconciler<'a, S> {
    /// Build a reconciler and index the chain.
    pub fn new(ledger: &'a Ledger, source: S) -> Result<Self> {
        Self::with_config(ledger, source, ReconcileConfig::default())
    }

    /// Build one with a non-default page contract. See
    /// [`ReconcileConfig::page_limit`].
    pub fn with_config(ledger: &'a Ledger, source: S, config: ReconcileConfig) -> Result<Self> {
        Ok(Reconciler {
            ledger,
            source,
            index: LedgerIndex::build(ledger)?,
            config,
        })
    }

    /// Work every gap that has not been proven backfilled, oldest first.
    ///
    /// Stops at the first failure and returns it. Everything recorded before
    /// that point is already durable, and re-running is free: the walk is
    /// idempotent, and a gap that was not marked reconciled is still on the
    /// work list. Failing the whole call rather than swallowing one gap's
    /// error keeps `docs/spec.md` item 34's overlay honest — a reconcile that
    /// reported success with one window silently unwalked is the failure mode
    /// this module exists to remove.
    pub async fn reconcile_all(&mut self, pending: &[Cloid]) -> Result<Vec<GapOutcome>> {
        let gaps = self.ledger.unreconciled_gaps()?;
        let mut outcomes = Vec::with_capacity(gaps.len());
        for gap in &gaps {
            outcomes.push(self.reconcile_gap(gap, pending).await?);
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
    pub async fn reconcile_gap(&mut self, gap: &Gap, pending: &[Cloid]) -> Result<GapOutcome> {
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
            let window = window_of(gap, closed_ts_ms)?;
            // Fills first, and the gap is marked only after everything below
            // has succeeded. The opposite order would mark a window
            // reconciled on the strength of an order query while the fills
            // walk had not run.
            let (fills, _) = backfill_fills(&self.source, account, window, self.config).await?;
            recovered = self.apply_fills(account, &fills, Some(gap.gap_id))?;
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

    /// Record fills, skipping any already in the chain.
    ///
    /// This is the idempotence boundary and the only writer of
    /// [`EventKind::Fill`] rows. It is also the path the live `userFills`
    /// feed uses, including for the subscribe snapshot the venue replays on
    /// every reconnect — the same batch through the same door, deduped the
    /// same way.
    ///
    /// Fills are sorted by `(time, tid)` before they are appended, so the
    /// chain order of a recovered window does not depend on which page a row
    /// arrived in. Two runs over the same window produce the same chain.
    pub fn apply_fills(
        &mut self,
        account: Address,
        fills: &[Fill],
        gap_id: Option<i64>,
    ) -> Result<Recovered> {
        let mut ordered: Vec<&Fill> = fills.iter().collect();
        ordered.sort_by_key(|fill| (fill.time, fill.tid));

        let mut recovered = Recovered {
            fills_seen: fills.len(),
            ..Recovered::default()
        };
        for fill in ordered {
            if self.index.contains_fill(fill.tid) {
                recovered.duplicates += 1;
                continue;
            }
            let attribution = self.index.attribution(fill.cloid.as_ref());
            let ts_ms = i64::try_from(fill.time)
                .map_err(|_| ReconcileError::TimestampOutOfRange(fill.time))?;
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
            let payload = fill_payload(account, fill, &attribution, gap_id);
            self.ledger.append(&NewEvent {
                kind: EventKind::Fill,
                ts_ms,
                agent_id: attribution.agent_id(),
                payload: &payload,
                snapshot: None,
            })?;
            // Only after the append: a failed write must leave the tid
            // unrecorded so the retry picks it up again.
            self.index.applied_tids.insert(fill.tid);
            recovered.fills_recorded += 1;
        }
        Ok(recovered)
    }
}

/// The window a gap covers, as unsigned venue milliseconds.
///
/// The ledger stores signed milliseconds because SQLite has no unsigned
/// integer; the venue's are unsigned. A negative or inverted window is refused
/// rather than clamped — see [`ReconcileError::UnusableWindow`].
fn window_of(gap: &Gap, closed_ts_ms: i64) -> Result<GapWindow> {
    let unusable = || ReconcileError::UnusableWindow {
        gap_id: gap.gap_id,
        start_ms: gap.opened_ts_ms,
        end_ms: closed_ts_ms,
    };
    let start_ms = u64::try_from(gap.opened_ts_ms).map_err(|_| unusable())?;
    let end_ms = u64::try_from(closed_ts_ms).map_err(|_| unusable())?;
    if end_ms < start_ms {
        return Err(unusable());
    }
    Ok(GapWindow { start_ms, end_ms })
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
    gap_id: Option<i64>,
) -> Value {
    let mut payload = json!({
        "account": account.to_string(),
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
        "ts_ms": fill.time,
        "venue_hash": fill.hash,
    });
    match attribution {
        Attribution::Attributed {
            agent_id,
            intent_seq,
            intent_hash,
        } => {
            payload["agent_id"] = json!(agent_id);
            payload["intent_seq"] = json!(intent_seq);
            payload["intent_hash"] = json!(intent_hash);
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

    use crate::Network;
    use crate::ledger::{Anchor, NewIntent};

    use super::*;

    /// A window that starts before every fixture fill and ends after all of
    /// them, for tests about paging rather than about windows.
    const WIDE: GapWindow = GapWindow {
        start_ms: 0,
        end_ms: u64::MAX,
    };

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
        fills: Vec<Fill>,
        page_limit: usize,
        open_orders: Vec<OpenOrder>,
        statuses: BTreeMap<String, OrderStatusResponse>,
        requests: Mutex<Vec<(u64, Option<u64>)>>,
    }

    impl FakeVenue {
        fn new(fills: Vec<Fill>) -> Self {
            FakeVenue {
                fills,
                page_limit: USER_FILLS_PAGE_LIMIT,
                open_orders: Vec::new(),
                statuses: BTreeMap::new(),
                requests: Mutex::new(Vec::new()),
            }
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
        async fn user_fills_by_time(
            &self,
            _user: Address,
            start_ms: u64,
            end_ms: Option<u64>,
        ) -> std::result::Result<Vec<Fill>, VenueError> {
            if let Ok(mut log) = self.requests.lock() {
                log.push((start_ms, end_ms));
            }
            let mut rows: Vec<Fill> = self
                .fills
                .iter()
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
    #[tokio::test]
    async fn zero_fills_lost_across_a_thirty_second_disconnect() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir);
        let user = account();

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

        let scope = Subscription::UserFills { user }.key();
        let gap = ledger
            .open_gap(&scope, T0 + 1_000, Some("1006 outbox_overflow"))
            .expect("open gap");
        ledger
            .close_gap(gap.gap_id, T0 + 31_000)
            .expect("close gap 30s later");

        let start = u64::try_from(T0).expect("epoch fits");
        let fills = vec![
            fill_at(11, start + 5_000, Some(agent_cloid.clone())),
            fill_at(12, start + 15_000, Some(manual_cloid.clone())),
            fill_at(13, start + 25_000, None),
        ];

        let mut reconciler =
            Reconciler::new(&ledger, FakeVenue::new(fills)).expect("index the chain");
        let outcomes = reconciler.reconcile_all(&[]).await.expect("reconcile");

        assert_eq!(outcomes.len(), 1, "one gap, one outcome");
        let outcome = &outcomes[0];
        assert_eq!(outcome.status, GapStatus::Reconciled);
        assert_eq!(outcome.recovered.fills_seen, 3);
        assert_eq!(outcome.recovered.fills_recorded, 3);
        assert_eq!(outcome.recovered.attributed, 1);
        assert_eq!(outcome.recovered.manual, 1);
        assert_eq!(outcome.recovered.external, 1);

        // Not one fill missing.
        assert_eq!(chain_tids(&ledger), BTreeSet::from([11, 12, 13]));

        // The window covered was exactly the gap's, both ends inclusive.
        let venue_start = u64::try_from(T0 + 1_000).expect("epoch fits");
        let venue_end = u64::try_from(T0 + 31_000).expect("epoch fits");
        assert_eq!(
            reconciler.source.requests(),
            vec![(venue_start, Some(venue_end))],
            "one short page ends the walk"
        );

        // The chain still verifies, and the gap is off the work list.
        let report = ledger.verify().expect("verify");
        assert!(report.first_break.is_none(), "chain broke: {report:?}");
        assert!(ledger.unreconciled_gaps().expect("gaps").is_empty());

        // The attributed fill names the agent, and does so from the intent
        // rather than from anything the caller supplied.
        let rows = chain_fills(&ledger);
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

        let mut first = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("reconciler");
        let recovered = first.apply_fills(user, &fills, None).expect("apply once");
        assert_eq!(recovered.fills_recorded, 3);
        assert_eq!(recovered.duplicates, 0);
        let after_first = head(&ledger);

        for attempt in 2..=3 {
            let mut again =
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

        let mut reconciler =
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
        assert_eq!(second.status, GapStatus::Reconciled);
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

        let mut reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("index");
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

        let mut reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("index");
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
        let (recovered, pages) = backfill_fills(&venue, account(), WIDE, config)
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
            vec![
                (0, Some(u64::MAX)),
                (1_006, Some(u64::MAX)),
                (1_007, Some(u64::MAX)),
            ]
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
        let error = backfill_fills(&venue, account(), WIDE, config)
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
        let mut reconciler =
            Reconciler::with_config(&ledger, venue, config).expect("index the chain");

        let error = reconciler
            .reconcile_all(&[])
            .await
            .expect_err("the window could not be proven contiguous");
        assert!(
            matches!(error, ReconcileError::PageStalled { .. }),
            "{error}"
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
        let error = backfill_fills(&venue, account(), WIDE, config)
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
        assert_eq!(reconciliation.unknown().len(), 1);
        assert_eq!(reconciliation.unknown()[0].cloid(), &vanished);
        assert_eq!(reconciliation.unknown()[0].account(), user);

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
        let mut reconciler = Reconciler::new(&ledger, venue).expect("index");
        let outcome = reconciler
            .reconcile_gap(&gap, std::slice::from_ref(&gone))
            .await
            .expect("reconcile");

        assert_eq!(outcome.status, GapStatus::Reconciled);
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

        let mut reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("index");
        let outcomes = reconciler.reconcile_all(&[]).await.expect("reconcile");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, GapStatus::NotAnAccountFeed);
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

        let mut reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("index");
        let outcome = reconciler
            .reconcile_gap(&gap, &[])
            .await
            .expect("reconcile");
        assert_eq!(outcome.status, GapStatus::StillOpen);
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
        let mut reconciler = Reconciler::new(&ledger, FakeVenue::new(Vec::new())).expect("index");
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

    /// A gap window that runs backwards is refused rather than clamped.
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
        assert!(matches!(
            window_of(&gap, T0),
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
        // Deep enough to be a denial of service is deep enough to refuse.
        let mut nested = json!({ "cloid": good.as_str() });
        for _ in 0..(MAX_CLOID_DEPTH + 2) {
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
        let info = InfoClient::new(Network::Mainnet).expect("client");
        let user =
            Address::parse("0x85ecf584f25db6f146718b86d493e33c5af72052").expect("measured account");

        let first = ReconcileSource::user_fills_by_time(
            &info,
            user,
            1_776_700_000_000,
            Some(1_776_800_000_000),
        )
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

        let second =
            ReconcileSource::user_fills_by_time(&info, user, newest, Some(1_776_800_000_000))
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
        let (all, pages) = backfill_fills(
            &info,
            user,
            GapWindow {
                start_ms: 1_776_700_000_000,
                end_ms: 1_776_800_000_000,
            },
            ReconcileConfig::default(),
        )
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
        let info = InfoClient::new(Network::Mainnet).expect("client");
        let user =
            Address::parse("0x399965e15d4e61ec3529cc98b7f7ebb93b733336").expect("measured account");

        let open_orders = ReconcileSource::frontend_open_orders(&info, user)
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
        assert_eq!(reconciliation.unknown().len(), 1);
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
            known.unknown().is_empty(),
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
