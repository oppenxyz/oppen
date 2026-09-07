//! The append-only, hash-chained event ledger.
//!
//! `docs/spec.md` D6 makes this one table the source for `get_events`, the live
//! activity stream and the audit export. There is no second event store, and
//! `AGENTS.md` invariant 7 says building one is a blocking review finding.
//!
//! Four properties this module exists to hold:
//!
//! * **One file, one chain, one cursor space per network** (`docs/decisions.md`
//!   R4). The seq is the agent's `get_events` cursor, so testnet row 4,812 and
//!   mainnet row 4,812 must not be the same cursor position. The file boundary
//!   makes that unrepresentable; [`hash::genesis_hash`] binds the chain to the
//!   network name so a renamed file is caught too.
//! * **The chain commits to content hashes, not content** (`docs/decisions.md`
//!   R5). The payload sits beside the chain in the same row, so [`Ledger::redact`]
//!   can null it and the chain still verifies. That is what makes retention
//!   legal without making a gap indistinguishable from tampering (D-e).
//! * **Record of record only.** Intents, decisions, refusals, fills, operator
//!   actions, approvals, kill-switch changes, guardrail trips, WS
//!   disconnect/reconnect and alerts are chained. Candles, book snapshots,
//!   samples and projections live in unchained side tables, because they are
//!   recomputable and pruning them must not look like an attack.
//! * **The intent is durable before the signer runs.** [`Ledger::record_intent`]
//!   commits under `synchronous = FULL` and hands back an [`IntentReceipt`] that
//!   cannot be constructed anywhere else. A signing path that demands one by
//!   reference cannot be reached with an unrecorded intent, so a power cut
//!   cannot produce an order with no record of why. The receipt names its own
//!   chain, so a testnet receipt cannot authorise a mainnet outcome (R4).
//!
//! Every call blocks on SQLite. Async callers run them on a blocking pool; the
//! ledger deliberately holds no runtime dependency of its own
//! (`docs/decisions.md` R1).
//!
//! # What tamper evidence here does and does not mean
//!
//! `docs/spec.md` item 29 says tamper-evident, not tamper-proof, and the exact
//! shape of that matters:
//!
//! * A rewritten row, a deleted row from the middle, a payload that no longer
//!   matches the chain, and a payload silently nulled without a chained
//!   [`EventKind::PayloadRedacted`] row covering it are all caught by
//!   [`Ledger::verify`] against the file alone.
//! * Erasing the *end* of the chain, or rewriting a row and recomputing every
//!   hash after it, cannot be caught from inside the file: `chain_head` is in
//!   the same file the attacker is editing. That is what [`anchor`] is for, and
//!   the default [`FileAnchor`] is an ordinary sidecar file, **not** a security
//!   boundary. Read that module before relying on it; the keychain-backed
//!   implementation is a later phase.
//! * An agent must not be able to reach any of this. [`Ledger::agent_view`]
//!   hands out the read-only surface (`AGENTS.md` invariant 3); `&Ledger`
//!   itself is operator-only.

mod anchor;
mod export;
mod hash;
mod schema;
mod submission;
mod verify;

#[cfg(test)]
mod coordination_tests;
#[cfg(test)]
mod tests;

use std::fs::{File, TryLockError};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use anchor::{Anchor, FileAnchor, HeadAnchor};
pub use submission::{
    SubmissionError, SubmissionJournal, SubmissionReceipt, SubmissionResolution, SubmissionState,
};
pub use verify::{BreakReason, ChainBreak, ChainReport};

use crate::Network;

/// Largest page [`Ledger::get_events`] will return.
///
/// A cap the agent cannot raise: an unbounded page is an unbounded MCP response,
/// and an agent that has fallen far behind should page rather than ask for
/// everything at once.
pub const MAX_PAGE: usize = 1_000;

/// Wait this long for another writer before giving up.
///
/// One process owns this file, but WAL mode still has the UI reader and the
/// feed writer contending. Failing fast here would surface as a lost fill.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Every column of `events`, in the order [`event_from_row`] reads them.
pub(crate) const SELECT_EVENT_COLUMNS: &str = "seq, ts_ms, kind, agent_id, payload, payload_hash, \
     prev_hash, hash, redacted_at, redaction_reason, snapshot_id, snapshot_hash";

/// What can go wrong in the ledger.
///
/// `AGENTS.md` conventions: errors are `thiserror`, and nothing on an input path
/// panics — the database file is user-writable by definition, so every read of
/// it is treated as untrusted.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// SQLite refused the statement.
    #[error("ledger sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A payload failed to serialise or a stored payload failed to parse.
    #[error("ledger json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Export could not write.
    #[error("ledger export io error: {0}")]
    Io(#[from] std::io::Error),
    /// A previous holder of the connection lock panicked. Reported rather than
    /// re-panicked so a poisoned mutex cannot take the whole daemon down.
    #[error("ledger connection lock is poisoned")]
    Poisoned,
    /// The file was written by a different network's chain. `docs/decisions.md`
    /// R4: this is refused, never filtered.
    #[error("ledger belongs to {found}, opened as {expected}")]
    NetworkMismatch {
        /// The network the caller asked for.
        expected: &'static str,
        /// The network recorded in the file.
        found: String,
    },
    /// The file could not be put in WAL mode, so a commit is not durable in the
    /// way the intent-before-signer ordering requires.
    #[error("ledger journal mode is {0}, expected wal")]
    JournalMode(String),
    /// The schema is newer than this build understands.
    #[error("ledger schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew {
        /// Version stamped in the file.
        found: i64,
        /// Highest version this build writes.
        supported: usize,
    },
    /// A seq in the file is negative or beyond `i64`. Only reachable by editing
    /// the file directly.
    #[error("ledger seq is out of range")]
    SeqOutOfRange,
    /// A stored event kind is not one this build knows.
    #[error("unknown ledger event kind {0:?}")]
    UnknownKind(String),
    /// The stored payload for `seq` is not valid JSON. Verification will say
    /// what happened to it.
    #[error("payload of event {seq} is not valid json")]
    PayloadNotJson {
        /// The row whose payload could not be parsed.
        seq: u64,
    },
    /// No event at that seq.
    #[error("no ledger event at seq {0}")]
    NoSuchEvent(u64),
    /// The payload was already redacted. Redaction is not idempotent on
    /// purpose: a second redaction of the same row means the caller lost track
    /// of what it was deleting.
    #[error("event {0} is already redacted")]
    AlreadyRedacted(u64),
    /// A redaction row was appended through the generic path. Redaction has one
    /// door for the same reason an intent does: [`Ledger::redact`] nulls the
    /// payload and appends the row that explains it in one transaction, so a
    /// chained redaction always corresponds to a real one.
    #[error("redact a payload with redact, so the tombstone and its explanation are one write")]
    UseRedact,
    /// An order intent was appended through the generic path, which would give
    /// a durable row but no [`IntentReceipt`] — and the receipt is the thing
    /// that keeps the record ahead of the signer.
    #[error("append an order intent with record_intent, so the signer can require its receipt")]
    UseRecordIntent,
    /// A fill was appended through a path that carries no idempotence key.
    ///
    /// Fills have one door for the same reason an intent does. The venue's own
    /// trade id is what makes re-walking a window a no-op, `record_fill` is the
    /// only writer that supplies it, and a fill row written without one is a
    /// duplicate an append-only chain cannot give back. Every other kind is
    /// still free to use the generic path.
    #[error("append a fill with record_fill, so the venue's trade id keys the row")]
    UseRecordFill,
    #[error("submission lifecycle events must use SubmissionJournal")]
    UseSubmissionJournal,
    /// A [`EventKind::PayloadRedacted`] row was itself passed to
    /// [`Ledger::redact`]. Its payload is `{redacted_seq, reason}` — two
    /// operator-authored fields with no agent text in them, so there is no
    /// retention argument for nulling it, and doing so erases the record of
    /// which row was redacted and why.
    #[error("event {0} is a redaction and its own explanation cannot be redacted")]
    RedactionIsNotRedactable(u64),
    /// The receipt was issued by a different chain. `docs/decisions.md` R4: a
    /// mainnet number that is actually a testnet number is the worst bug this
    /// product can ship, and the receipt is the type handed to the signer.
    #[error("receipt belongs to chain {found}, this ledger is chain {expected}")]
    ReceiptFromAnotherChain {
        /// Genesis hash of this ledger's chain.
        expected: String,
        /// Genesis hash recorded in the receipt.
        found: String,
    },
    /// The receipt names a row of this chain that no longer hashes to what the
    /// receipt says. The intent it refers to is not the intent that is there.
    #[error("receipt for event {seq} expects hash {expected}, the row hashes to {found}")]
    ReceiptRowMismatch {
        /// The seq the receipt names.
        seq: u64,
        /// The hash the receipt carries.
        expected: String,
        /// The hash the row actually has.
        found: String,
    },
    /// A snapshot body was written, or read back, under an id that is already
    /// committed to a different body. `docs/decisions.md` R6: the book at the
    /// moment an agent decided is the one class of data that cannot be
    /// backfilled, so a replaced book must never be served as the
    /// decision-time book.
    #[error("snapshot {snapshot_id} is committed to {expected}, this body hashes to {found}")]
    SnapshotBodyConflict {
        /// The snapshot id.
        snapshot_id: String,
        /// The hash already committed to.
        expected: String,
        /// The hash of the body offered or found.
        found: String,
    },
    /// A payload carried a JSON float. `AGENTS.md` conventions: money and prices
    /// are `Decimal`, never `f64`, and this is the table kept forever.
    #[error("payload field {pointer} is a float; money and prices belong here as decimal strings")]
    FloatInPayload {
        /// RFC 6901 pointer to the offending field; empty for the root.
        pointer: String,
    },
    /// A payload nested deeper than the ledger will walk. Refused rather than
    /// recursed into, because a stack overflow is a panic on an input path.
    #[error("payload nests too deeply at {pointer}")]
    PayloadTooDeep {
        /// RFC 6901 pointer to where the limit was hit.
        pointer: String,
    },
    /// No feed gap with that id.
    #[error("no feed gap {0}")]
    NoSuchGap(i64),
    /// The gap already has a close, so its window is already known.
    #[error("feed gap {0} is already closed")]
    GapAlreadyClosed(i64),
    /// A gap cannot be reconciled before its window has an end.
    #[error("feed gap {0} is still open")]
    GapStillOpen(i64),
}

/// Ledger result alias.
pub type Result<T> = std::result::Result<T, LedgerError>;

/// The kinds of thing that go in the chain.
///
/// `docs/decisions.md` R5 draws the line: this is the record of record. If a row
/// of a given kind can be recomputed from a venue query, it does not belong
/// here. The set matches the `get_events` taxonomy in `docs/spec.md` item 18.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// An agent asked for an order. Written and committed before signing.
    OrderIntent,
    /// An agent's own account of why, carried as untrusted text
    /// (`AGENTS.md` invariant 9).
    AgentDecision,
    /// A request oppen refused. `docs/decisions.md` D-c makes the refusal the
    /// onboarding, so it has to be in the record.
    Refusal,
    /// A fill, attributed to the intent that caused it where one exists.
    Fill,
    /// An order moved between resting, filled, cancelled or rejected.
    OrderStateChange,
    /// A durable reservation written before signing and submission.
    SubmissionStarted,
    /// Authoritative evidence that a reservation is no longer in flight.
    SubmissionResolved,
    /// Something the human did: a manual ticket, a flatten, a setting change.
    OperatorAction,
    /// An approval-mode proposal was approved, rejected or expired
    /// (`docs/spec.md` item 28).
    ApprovalDecision,
    /// The kill switch changed, per agent or global (`docs/spec.md` item 26).
    KillSwitchChanged,
    /// A guardrail or the loss circuit breaker tripped (`docs/spec.md` items
    /// 24 and 25).
    GuardrailTrip,
    /// A feed dropped. Always paired with a row in `feed_gaps`.
    WsDisconnected,
    /// A feed came back.
    WsReconnected,
    /// A condition alert fired (`docs/spec.md` item 22).
    Alert,
    /// An agent wallet is approaching its 90-day expiry
    /// (`docs/decisions.md` D-b).
    AgentWalletExpiryWarning,
    /// A payload was redacted. The redaction is itself in the record, so a
    /// tombstone is accountable rather than merely legal.
    PayloadRedacted,
}

impl EventKind {
    /// The stored wire name. Hashed into the chain, so it is stable forever
    /// once written.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::OrderIntent => "order_intent",
            EventKind::AgentDecision => "agent_decision",
            EventKind::Refusal => "refusal",
            EventKind::Fill => "fill",
            EventKind::OrderStateChange => "order_state_change",
            EventKind::SubmissionStarted => "submission_started",
            EventKind::SubmissionResolved => "submission_resolved",
            EventKind::OperatorAction => "operator_action",
            EventKind::ApprovalDecision => "approval_decision",
            EventKind::KillSwitchChanged => "kill_switch_changed",
            EventKind::GuardrailTrip => "guardrail_trip",
            EventKind::WsDisconnected => "ws_disconnected",
            EventKind::WsReconnected => "ws_reconnected",
            EventKind::Alert => "alert",
            EventKind::AgentWalletExpiryWarning => "agent_wallet_expiry_warning",
            EventKind::PayloadRedacted => "payload_redacted",
        }
    }
}

impl std::str::FromStr for EventKind {
    type Err = LedgerError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "order_intent" => Ok(EventKind::OrderIntent),
            "agent_decision" => Ok(EventKind::AgentDecision),
            "refusal" => Ok(EventKind::Refusal),
            "fill" => Ok(EventKind::Fill),
            "order_state_change" => Ok(EventKind::OrderStateChange),
            "submission_started" => Ok(EventKind::SubmissionStarted),
            "submission_resolved" => Ok(EventKind::SubmissionResolved),
            "operator_action" => Ok(EventKind::OperatorAction),
            "approval_decision" => Ok(EventKind::ApprovalDecision),
            "kill_switch_changed" => Ok(EventKind::KillSwitchChanged),
            "guardrail_trip" => Ok(EventKind::GuardrailTrip),
            "ws_disconnected" => Ok(EventKind::WsDisconnected),
            "ws_reconnected" => Ok(EventKind::WsReconnected),
            "alert" => Ok(EventKind::Alert),
            "agent_wallet_expiry_warning" => Ok(EventKind::AgentWalletExpiryWarning),
            "payload_redacted" => Ok(EventKind::PayloadRedacted),
            other => Err(LedgerError::UnknownKind(other.to_owned())),
        }
    }
}

/// A reference to the book at the moment a decision was taken.
///
/// `docs/decisions.md` R6: the hash goes in the chained row, the body lives in
/// the prunable `book_snapshots` table. The book at the moment an agent decided
/// is the one class of data that cannot be backfilled, so the plumbing exists
/// before the capture policy does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRef<'a> {
    /// Primary key in `book_snapshots`.
    pub id: &'a str,
    /// Hash of the snapshot body, chained into the row.
    pub hash: &'a str,
}

/// An event about to be appended.
#[derive(Debug, Clone)]
pub struct NewEvent<'a> {
    /// What happened.
    pub kind: EventKind,
    /// When, in unix milliseconds. Passed in rather than read from the clock so
    /// that a replay, a backfill and a test all produce the same chain.
    pub ts_ms: i64,
    /// Which agent it is attributed to. `None` for operator and system rows.
    pub agent_id: Option<&'a str>,
    /// The body. Serialised to canonical JSON; money and prices belong in it as
    /// decimal strings, never JSON floats (`AGENTS.md` conventions).
    pub payload: &'a Value,
    /// Decision-time book reference, if one was captured.
    pub snapshot: Option<SnapshotRef<'a>>,
}

/// An order intent about to be recorded, ahead of signing.
#[derive(Debug, Clone)]
pub struct NewIntent<'a> {
    /// The agent asking. An intent always has one; a manual ticket records an
    /// [`EventKind::OperatorAction`] instead.
    pub agent_id: &'a str,
    /// Unix milliseconds.
    pub ts_ms: i64,
    /// The order as the agent asked for it, plus its `reason`.
    pub payload: &'a Value,
    /// Decision-time book reference, if one was captured.
    pub snapshot: Option<SnapshotRef<'a>>,
}

/// A fill about to be recorded, with the venue identifiers that key it.
///
/// `account` and `tid` are not decoration: together they are the row's
/// idempotence key, so offering the same fill twice writes once. They are a
/// *pair* rather than the `tid` alone because a trade has two sides, and an
/// operator running two containers can be both of them — the same venue trade
/// id then legitimately appears once per container.
#[derive(Debug, Clone)]
pub(crate) struct NewFill<'a> {
    /// Container address the fill landed on, lowercase and `0x`-prefixed.
    pub account: &'a str,
    /// The venue's own trade id.
    pub tid: u64,
    /// The **venue's** timestamp for the fill, in unix milliseconds.
    pub ts_ms: i64,
    /// The agent it is attributed to, if the join found one.
    pub agent_id: Option<&'a str>,
    /// The chained body.
    pub payload: &'a Value,
}

/// What an append assigned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Appended {
    /// The row's position in the chain, and the cursor value an agent will see.
    pub seq: u64,
    /// The row's chain hash.
    pub hash: String,
}

/// Proof that an intent is durable on disk.
///
/// The fields are private and there is no public constructor, so the only way
/// to hold one is to have called [`Ledger::record_intent`], which does not
/// return until the row is committed under `synchronous = FULL`. A signing
/// entry point that takes `&IntentReceipt` therefore cannot be reached with an
/// unrecorded intent: the ordering is enforced by the type system rather than
/// by everyone remembering it. `AGENTS.md` invariant 1 puts the guardrail check
/// in the same place — between holding this receipt and calling the signer.
///
/// The receipt also names the chain that issued it and the agent that asked.
/// `docs/decisions.md` R4 makes one chain per network, and this is the one type
/// that crosses from the ledger into the signer: without the chain field a
/// testnet receipt was accepted by the mainnet ledger, which is precisely the
/// confusion R4 calls the worst bug this product can ship. Without the agent
/// field the outcome could be attributed to an agent that never asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentReceipt {
    chain: String,
    seq: u64,
    hash: String,
    agent_id: String,
}

impl IntentReceipt {
    /// Genesis hash of the chain that issued this receipt.
    ///
    /// The genesis rather than the network name, because it binds the file's
    /// actual chain: two testnet files are two chains only if their genesis
    /// differs, and [`hash::genesis_hash`] makes the network part of it.
    pub fn chain(&self) -> &str {
        &self.chain
    }

    /// The chain position of the intent row.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The chain hash of the intent row. Outcomes carry it so the link survives
    /// even if seqs are renumbered by someone editing the file.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The agent whose intent this is. Outcomes are attributed to it rather
    /// than to a separately supplied id, so an outcome cannot be booked against
    /// an agent that did not ask.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

/// A stored event.
///
/// Field order is the JSON Lines key order (`AGENTS.md` invariant 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    /// Chain position, and the agent's cursor value.
    pub seq: u64,
    /// Unix milliseconds.
    pub ts_ms: i64,
    /// What happened.
    pub kind: EventKind,
    /// Attributed agent, if any.
    pub agent_id: Option<String>,
    /// The body, or `None` once redacted. A redacted row still verifies.
    pub payload: Option<Value>,
    /// What the chain committed to, whether or not the payload is still here.
    pub payload_hash: String,
    /// Hash of the previous row.
    pub prev_hash: String,
    /// This row's chain hash.
    pub hash: String,
    /// When the payload was redacted, in unix milliseconds.
    ///
    /// Convenience for display only. It is not in the row-hash preimage, so it
    /// proves nothing: the chained [`EventKind::PayloadRedacted`] row is the
    /// evidence, and [`Ledger::verify`] reads that and never this.
    pub redacted_at: Option<i64>,
    /// Why it was redacted. Display only, for the same reason as
    /// [`Event::redacted_at`]; the chained redaction row carries the reason the
    /// chain actually commits to.
    pub redaction_reason: Option<String>,
    /// Decision-time book snapshot id (`docs/decisions.md` R6).
    pub snapshot_id: Option<String>,
    /// Decision-time book snapshot hash (`docs/decisions.md` R6).
    pub snapshot_hash: Option<String>,
}

/// One page of events for an agent cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventPage {
    /// The events, ascending by seq, contiguous.
    pub events: Vec<Event>,
    /// The cursor to pass next time. Unchanged from the request when the page
    /// is empty, so a caller that polls an idle ledger does not drift.
    pub next_cursor: u64,
    /// The cursor asked for events that are no longer retained, or came from a
    /// chain this file is not. `docs/spec.md` item 18 requires this to be
    /// explicit: an agent must never silently receive a page with a hole in it.
    pub resync_required: bool,
    /// The newest seq in the ledger, so a caller knows how far behind it is
    /// without a second call.
    pub head_seq: u64,
}

/// Who a sub-account belongs to.
///
/// `docs/decisions.md` R2 keeps the discriminator in the schema while the
/// product rule stays open: whether a workflow gets its own sub-account or
/// binds to an agent's is a question real usage answers better than reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerType {
    /// A paired trading agent (`docs/spec.md` D1: one sub-account per agent).
    Agent,
    /// A workflow definition (`docs/specs/workflows.md`).
    Workflow,
}

impl OwnerType {
    /// Stored wire name, matching the schema's `CHECK` constraint.
    pub fn as_str(self) -> &'static str {
        match self {
            OwnerType::Agent => "agent",
            OwnerType::Workflow => "workflow",
        }
    }
}

impl std::str::FromStr for OwnerType {
    type Err = LedgerError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "agent" => Ok(OwnerType::Agent),
            "workflow" => Ok(OwnerType::Workflow),
            other => Err(LedgerError::UnknownKind(other.to_owned())),
        }
    }
}

/// The owning entity of a sub-account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Owner {
    /// Agent or workflow.
    pub owner_type: OwnerType,
    /// Identifier within that namespace.
    pub owner_id: String,
}

/// A Hyperliquid sub-account oppen knows about.
///
/// `docs/decisions.md` R3: oppen discovers every sub-account under the master
/// and the operator ticks which ones it records. Default off for anything oppen
/// did not provision — it watches what it made and asks before watching you.
/// `docs/specs/history.md` 3.4: rows are never deleted, only marked inactive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubAccount {
    /// Address, lowercase and `0x`-prefixed.
    pub address: String,
    /// Operator-facing name.
    pub name: String,
    /// `None` for an account oppen merely discovered.
    pub owner: Option<Owner>,
    /// Whether oppen records this account's history (R3 opt-in).
    pub recorded: bool,
    /// Whether oppen created it.
    pub provisioned_by_oppen: bool,
    /// Retiring an agent deletes the pairing, not the past.
    pub active: bool,
    /// When the row was first written, in unix milliseconds.
    pub created_ts_ms: i64,
}

/// A window during which a feed was down.
///
/// `docs/spec.md` item 9 and `docs/specs/history.md` 3.3: the venue has no
/// server-side cursor, so the only way to tell a missing fill from an absent
/// one is to know exactly when oppen was not listening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gap {
    /// Row id, used to close and reconcile the gap.
    pub gap_id: i64,
    /// Which feed, e.g. a subscription name plus the account it covers.
    pub scope: String,
    /// When the feed dropped, in unix milliseconds.
    pub opened_ts_ms: i64,
    /// When it came back. `None` while still down.
    pub closed_ts_ms: Option<i64>,
    /// When the window was proven backfilled. `None` until then; the UI keeps
    /// its stale overlay up until this is set.
    pub reconciled_ts_ms: Option<i64>,
    /// Seq of the chained [`EventKind::WsDisconnected`] row.
    pub open_seq: u64,
    /// Seq of the chained [`EventKind::WsReconnected`] row.
    pub close_seq: Option<u64>,
    /// Free-text detail, e.g. the close code.
    pub note: Option<String>,
}

/// Unix milliseconds now.
///
/// Millisecond precision because that is what Hyperliquid stamps; `AGENTS.md`
/// invariant 6 forbids inventing finer precision than a field needs. A clock
/// before the epoch yields `0` rather than a panic — the ledger never panics on
/// an input path, and a wrong timestamp is a smaller failure than a lost row.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// The stored name of a network.
///
/// Local to this module rather than taken from `oppen-hl`: it is written into
/// the database and hashed into the genesis value, so it is a storage format
/// that must not move when a display string does.
pub(crate) fn network_key(network: Network) -> &'static str {
    match network {
        Network::Testnet => "testnet",
        Network::Mainnet => "mainnet",
    }
}

/// The event ledger.
///
/// Holds one SQLite connection behind a mutex. `rusqlite::Connection` is `Send`
/// but not `Sync`, and the ledger is written by the feed pool and read by the
/// MCP surface at the same time, so serialising here is what makes `&Ledger`
/// shareable at all. Every method blocks.
#[derive(Debug)]
pub struct Ledger {
    connection: Mutex<Connection>,
    coordination_path: PathBuf,
    network: Network,
    genesis: String,
    anchor: Option<Box<dyn HeadAnchor>>,
}

impl Ledger {
    /// Open an existing ledger for the operator console without creating or
    /// migrating it. SQLite enforces read-only access; the normal coordinated
    /// page reader remains the single event source (spec #31, D6 and R4).
    pub(crate) fn open_readonly(dir: &Path, network: Network) -> Result<Self> {
        let path = std::fs::canonicalize(dir.join(crate::db_file_name(network)))?;
        let connection =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        schema::supported_version(&connection)?;
        let found: String = connection.query_row(
            "SELECT value FROM ledger_meta WHERE key = 'network'",
            [],
            |row| row.get(0),
        )?;
        let expected = network_key(network);
        if found != expected {
            return Err(LedgerError::NetworkMismatch { expected, found });
        }
        let mut coordination_path = path.clone().into_os_string();
        coordination_path.push(".lock");
        Ok(Self {
            connection: Mutex::new(connection),
            coordination_path: PathBuf::from(coordination_path),
            network,
            genesis: hash::genesis_hash(network),
            anchor: Some(Box::new(FileAnchor::beside(&path))),
        })
    }

    /// Open the ledger for `network` inside `dir`.
    ///
    /// The file name comes from [`crate::db_file_name`], which is what makes
    /// `docs/decisions.md` R4 structural: two networks are two files and cannot
    /// share a rowid space.
    pub fn open(dir: &Path, network: Network) -> Result<Self> {
        Self::open_at(&dir.join(crate::db_file_name(network)), network)
    }

    /// Open a ledger at an explicit path, anchored by the default sidecar.
    ///
    /// For tooling that is handed a file — a backup, an export to verify — and
    /// for tests. Product code should use [`Ledger::open`] so the naming rule
    /// stays in one place.
    pub fn open_at(path: &Path, network: Network) -> Result<Self> {
        // The default anchor and lock must share an identity for symlink and
        // relative-path aliases of the same database.
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let canonical = std::fs::canonicalize(path)?;
        let anchor = FileAnchor::beside(&canonical);
        match std::fs::canonicalize(FileAnchor::beside(path).path()) {
            Ok(legacy) => {
                let target = match std::fs::canonicalize(anchor.path()) {
                    Ok(target) => target,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        anchor.path().to_owned()
                    }
                    Err(error) => return Err(error.into()),
                };
                if legacy != target {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                        "legacy anchor beside a database alias requires explicit migration; refusing to discard it").into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Self::open_anchored(&canonical, network, Some(Box::new(anchor)))
    }

    /// Open a ledger with a chosen head anchor, or with none.
    ///
    /// `None` means the last row of the chain is whatever the database file
    /// says it is, and suffix truncation is undetectable — use it only for a
    /// file that is being inspected rather than kept, such as an export handed
    /// over for a one-off check.
    ///
    /// When an anchor is supplied and holds nothing yet, the current head is
    /// adopted into it. Adoption trusts the file at that first open; every
    /// rewind after it is caught. See [`anchor`] for what the default sidecar
    /// does and does not stop.
    pub fn open_anchored(
        path: &Path,
        network: Network,
        anchor: Option<Box<dyn HeadAnchor>>,
    ) -> Result<Self> {
        let mut connection = Connection::open(path)?;
        let mut coordination_path = std::fs::canonicalize(path)?.into_os_string();
        coordination_path.push(".lock");
        let coordination_path = PathBuf::from(coordination_path);
        let _coordination = acquire_coordination(&coordination_path)?;
        configure(&connection)?;
        schema::migrate(&connection)?;
        let genesis = hash::genesis_hash(network);
        bind_network(&mut connection, network, &genesis)?;
        if let Some(anchor) = &anchor
            && anchor.load()?.is_none()
        {
            let (seq, hash) = head(&connection)?;
            anchor.store(&Anchor { seq, hash })?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
            coordination_path,
            network,
            genesis,
            anchor,
        })
    }

    /// Which network this file's chain belongs to.
    pub fn network(&self) -> Network {
        self.network
    }

    /// The chain head as it stands: the last seq and its row hash.
    ///
    /// Exposed so a caller that keeps its own record of the head — a backup
    /// job, an operator writing it down, a future keychain anchor — can read it
    /// without a private field. Pair it with [`Ledger::verify_against`].
    pub fn chain_head(&self) -> Result<Anchor> {
        let guard = self.lock()?;
        let (seq, hash) = head(&guard)?;
        Ok(Anchor { seq, hash })
    }

    /// The read-only surface an agent may hold, scoped to that agent.
    ///
    /// `AGENTS.md` invariant 3: no agent-reachable path modifies guardrails, the
    /// approval setting, the kill switch or the agent registry. `docs/spec.md`
    /// D6 makes `get_events` an agent-facing call served from this same
    /// `Ledger`, so an MCP tool handed a `&Ledger` would be one line away from
    /// [`Ledger::redact`] or [`Ledger::upsert_sub_account`]. Hand `oppen-mcp` an
    /// [`AgentView`] instead and the invariant is a type error rather than a
    /// review finding.
    ///
    /// Takes `&Arc<Self>` and the view owns its handle: `oppen-mcp`'s gateway is
    /// built once and cloned per session, so it cannot hold a borrow, and giving
    /// it an `Arc<Ledger>` to borrow from per call would put the operator
    /// surface back within reach — the exact reach this type removes.
    pub fn agent_view(self: &Arc<Self>, agent_id: impl Into<String>) -> AgentView {
        AgentView {
            ledger: Arc::clone(self),
            agent_id: agent_id.into(),
        }
    }

    /// Append one event and return the seq it was assigned.
    ///
    /// The whole append — read the head, write the row, advance the head — is
    /// one immediate transaction, so an interruption leaves either a complete
    /// row or nothing. A half-written row would be indistinguishable from
    /// tampering on the next verification.
    ///
    /// Three kinds are refused here on purpose, each because it has exactly one
    /// door. [`EventKind::OrderIntent`] would be durable but would not produce
    /// an [`IntentReceipt`], which is the whole mechanism keeping the record
    /// ahead of the signer — use [`Ledger::record_intent`].
    /// [`EventKind::PayloadRedacted`] is the evidence [`Ledger::verify`] demands
    /// before it accepts a null payload, so it must never be writable without
    /// the null it explains — use [`Ledger::redact`]. [`EventKind::Fill`] would
    /// be durable but unkeyed, so the next walk over the same window would chain
    /// the trade a second time — use `record_fill`.
    pub fn append(&self, event: &NewEvent<'_>) -> Result<Appended> {
        self.refuse_one_door_kind(event.kind)?;
        self.append_committed(event)
    }

    /// The kinds that may not go through a generic append.
    ///
    /// One list, so [`Ledger::append`] and [`Ledger::record_outcome`] cannot
    /// drift apart on which kinds have their own door.
    fn refuse_one_door_kind(&self, kind: EventKind) -> Result<()> {
        match kind {
            EventKind::OrderIntent => Err(LedgerError::UseRecordIntent),
            EventKind::PayloadRedacted => Err(LedgerError::UseRedact),
            EventKind::Fill => Err(LedgerError::UseRecordFill),
            EventKind::SubmissionStarted | EventKind::SubmissionResolved => {
                Err(LedgerError::UseSubmissionJournal)
            }
            _ => Ok(()),
        }
    }

    /// Record one fill, keyed on the venue's own identifiers.
    ///
    /// `Ok(None)` means this container already has that trade id in the chain.
    /// That is the normal result of a correct re-run — the window is re-walked
    /// on every reconnect, retry and restart, and the venue's `startTime` is
    /// inclusive — not an error, and the caller counts it as a duplicate rather
    /// than handling a failure.
    ///
    /// **There is no read before the write.** The uniqueness is the partial
    /// index in `schema.rs`, evaluated by SQLite inside the same statement that
    /// inserts the row, so there is no interval during which a second writer can
    /// slip the same fill in. An in-memory index of what has already been
    /// recorded cannot have that property however carefully it is refreshed: the
    /// refresh closes the window before the walk and not during it.
    pub(crate) fn record_fill(&self, fill: &NewFill<'_>) -> Result<Option<Appended>> {
        let idem_key = fill_idem_key(fill.account, fill.tid);
        let mut guard = self.lock()?;
        let transaction = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let appended = append_keyed_in_tx(
            &transaction,
            &NewEvent {
                kind: EventKind::Fill,
                ts_ms: fill.ts_ms,
                agent_id: fill.agent_id,
                payload: fill.payload,
                snapshot: None,
            },
            &idem_key,
        )?;
        transaction.commit()?;
        if let Some(appended) = &appended {
            self.note_head(appended)?;
        }
        Ok(appended)
    }

    /// The newest **venue** timestamp among the fills recorded for `account` at
    /// a chain position before `before_seq`.
    ///
    /// This is the reconciler's only venue-clock reading, and `before_seq` is
    /// what makes it trustworthy. A fill row's `ts_ms` is the instant the venue
    /// stamped it, and a row written before the disconnect is proof oppen was
    /// still being served then. Rows written *after* that chain position — by a
    /// later backfill, or by the live feed once it resumed — say nothing about
    /// the outage and would drag the answer forward past the fills it is meant
    /// to defend, so they are excluded by position rather than by any clock.
    ///
    /// A stored timestamp below the epoch (only reachable by editing the file)
    /// reads as "no anchor", which widens the caller's window instead of
    /// narrowing it. A timestamp in the **future** is returned as it stands —
    /// nothing here knows what "now" is — and it is the caller that must decide
    /// whether to believe it, because an anchor past the disconnect starts a
    /// walk after the fills the gap exists to recover. `reconcile::outage_window`
    /// is where that is decided.
    pub(crate) fn newest_fill_ts_ms(&self, account: &str, before_seq: u64) -> Result<Option<u64>> {
        let before = i64::try_from(before_seq).map_err(|_| LedgerError::SeqOutOfRange)?;
        let (low, high) = fill_key_range(account);
        let guard = self.lock()?;
        // The key range alone selects this account's fills: `fill_idem_key` is
        // the only producer of a key, so no other kind of row can fall in it.
        let newest: Option<i64> = guard.query_row(
            "SELECT MAX(ts_ms) FROM events WHERE idem_key >= ?1 AND idem_key < ?2 AND seq < ?3",
            params![low, high, before],
            |row| row.get(0),
        )?;
        Ok(newest.and_then(|ts_ms| u64::try_from(ts_ms).ok()))
    }

    /// Every chained row of one kind, oldest first.
    ///
    /// Served by the `events_kind` index. The reconciler reads the intent and
    /// operator rows through it at the moment it needs them, which is what lets
    /// it hold no cursor of its own.
    /// Every fill attributed to one agent inside a time window, newest last.
    ///
    /// For spec F's execution report. Narrow on purpose: it answers one
    /// question rather than exposing the chain by kind, and it filters in SQL
    /// on the `events_kind` index rather than reading the fills — which are
    /// the bulk of the chain — into memory to throw most of them away.
    ///
    /// The window is on `ts_ms`, which for a fill row is the **venue's** own
    /// timestamp, not oppen's. That is the right clock for execution analysis:
    /// a fill recovered from an outage days later still belongs to the moment
    /// it traded, and scoring it into the window oppen happened to learn about
    /// it would put the outage in the report instead of the execution.
    ///
    /// Redacted rows carry no payload and are skipped by the caller, which is
    /// the honest treatment: a tombstone is not a fill that cost nothing.
    fn fills_for(&self, agent_id: &str, from_ms: i64, to_ms: i64) -> Result<Vec<Value>> {
        let guard = self.lock()?;
        let mut statement = guard.prepare(
            "SELECT payload FROM events \
             WHERE kind = ?1 AND agent_id = ?2 AND ts_ms >= ?3 AND ts_ms <= ?4 \
             ORDER BY seq ASC",
        )?;
        let mut rows =
            statement.query(params![EventKind::Fill.as_str(), agent_id, from_ms, to_ms])?;
        let mut payloads = Vec::new();
        while let Some(row) = rows.next()? {
            let text: Option<String> = row.get(0)?;
            if let Some(text) = text
                && let Ok(value) = serde_json::from_str(&text)
            {
                payloads.push(value);
            }
        }
        Ok(payloads)
    }

    pub(crate) fn events_of_kind(&self, kind: EventKind) -> Result<Vec<Event>> {
        let guard = self.lock()?;
        let mut statement = guard.prepare(&format!(
            "SELECT {SELECT_EVENT_COLUMNS} FROM events WHERE kind = ?1 ORDER BY seq ASC"
        ))?;
        let mut rows = statement.query(params![kind.as_str()])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(event_from_row(row)?);
        }
        Ok(events)
    }

    /// Record an order intent and return proof it is on disk.
    ///
    /// This is the only way to obtain an [`IntentReceipt`], and it does not
    /// return until the row is committed with `synchronous = FULL`. Give the
    /// signing path a `&IntentReceipt` parameter and the ordering "intent
    /// durable, then guardrails, then sign" becomes the only representable one:
    /// a power cut can lose the order, but it cannot leave an order with no
    /// record of why it was placed.
    pub fn record_intent(&self, intent: &NewIntent<'_>) -> Result<IntentReceipt> {
        let appended = self.append_committed(&NewEvent {
            kind: EventKind::OrderIntent,
            ts_ms: intent.ts_ms,
            agent_id: Some(intent.agent_id),
            payload: intent.payload,
            snapshot: intent.snapshot,
        })?;
        Ok(IntentReceipt {
            chain: self.genesis.clone(),
            seq: appended.seq,
            hash: appended.hash,
            agent_id: intent.agent_id.to_owned(),
        })
    }

    /// Append and commit, with no check on the kind.
    ///
    /// Private so that [`EventKind::OrderIntent`] and
    /// [`EventKind::PayloadRedacted`] each have exactly one public door.
    fn append_committed(&self, event: &NewEvent<'_>) -> Result<Appended> {
        let mut guard = self.lock()?;
        let transaction = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let appended = append_in_tx(&transaction, event)?;
        transaction.commit()?;
        self.note_head(&appended)?;
        Ok(appended)
    }

    /// Record what became of a recorded intent.
    ///
    /// Wraps the caller's payload with the intent's seq and hash so the link is
    /// structural rather than a naming convention, which is what
    /// `docs/specs/history.md` 2 needs to join "why it happened" to "what
    /// actually filled".
    ///
    /// The receipt is checked against this chain before anything is written:
    /// it must have been issued by this genesis, and the row it names must
    /// still hash to what the receipt says. Without that a receipt from the
    /// testnet ledger produced a mainnet fill citing an intent hash no mainnet
    /// row has — a mainnet number that is actually a testnet number, which
    /// `docs/decisions.md` R4 calls the worst bug this product can ship. The
    /// check and the append share one transaction so the row cannot change
    /// between them.
    ///
    /// Attribution comes from the receipt rather than from a parameter, so an
    /// outcome cannot be booked against an agent that never asked.
    pub fn record_outcome(
        &self,
        receipt: &IntentReceipt,
        kind: EventKind,
        ts_ms: i64,
        outcome: &Value,
    ) -> Result<Appended> {
        self.refuse_one_door_kind(kind)?;
        if receipt.chain != self.genesis {
            return Err(LedgerError::ReceiptFromAnotherChain {
                expected: self.genesis.clone(),
                found: receipt.chain.clone(),
            });
        }
        let seq_key = i64::try_from(receipt.seq).map_err(|_| LedgerError::SeqOutOfRange)?;
        let payload = serde_json::json!({
            "intent_seq": receipt.seq,
            "intent_hash": receipt.hash,
            "outcome": outcome,
        });

        let mut guard = self.lock()?;
        let transaction = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found: Option<String> = transaction
            .query_row(
                "SELECT hash FROM events WHERE seq = ?1",
                params![seq_key],
                |row| row.get(0),
            )
            .optional()?;
        match found {
            None => return Err(LedgerError::NoSuchEvent(receipt.seq)),
            // The row hash commits to the kind, so a matching hash is also
            // proof the row is the order intent the receipt was issued for.
            Some(found) if found != receipt.hash => {
                return Err(LedgerError::ReceiptRowMismatch {
                    seq: receipt.seq,
                    expected: receipt.hash.clone(),
                    found,
                });
            }
            Some(_) => {}
        }
        let appended = append_in_tx(
            &transaction,
            &NewEvent {
                kind,
                ts_ms,
                agent_id: Some(&receipt.agent_id),
                payload: &payload,
                snapshot: None,
            },
        )?;
        transaction.commit()?;
        self.note_head(&appended)?;
        Ok(appended)
    }

    /// Read a page of events after `since_seq`.
    ///
    /// `docs/spec.md` item 18: the cursor is the durable rowid, and a cursor too
    /// old returns an explicit `resync_required` rather than a page with a hole
    /// in it. A cursor ahead of the head is also a resync — that is what a
    /// mainnet cursor presented to a testnet file looks like, and R4 would
    /// rather refuse than serve it.
    pub fn get_events(&self, since_seq: u64, limit: usize) -> Result<EventPage> {
        self.page(None, since_seq, limit)
    }

    /// A page of the events one agent may see (`docs/decisions.md` C6).
    ///
    /// The agent's own rows, plus the account-wide ones no agent owns — a kill
    /// switch, a feed dropping, an alert. Item 18's taxonomy still arrives
    /// whole; what does not arrive is another agent's intents and reason
    /// strings.
    pub(crate) fn get_events_for_agent(
        &self,
        agent_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<EventPage> {
        self.page(Some(agent_id), since_seq, limit)
    }

    fn page(&self, scope: Option<&str>, since_seq: u64, limit: usize) -> Result<EventPage> {
        let guard = self.lock()?;
        let head_seq = head(&guard)?.0;

        let retained_from: Option<i64> =
            guard.query_row("SELECT MIN(seq) FROM events", [], |row| row.get(0))?;
        let retained_from = match retained_from {
            Some(value) => Some(u64::try_from(value).map_err(|_| LedgerError::SeqOutOfRange)?),
            None => None,
        };

        let cursor_ahead = since_seq > head_seq;
        // Saturating: an agent's cursor arrives over the wire, and a debug build
        // must not be arithmetic-panicked by a caller sending u64::MAX.
        let cursor_too_old = retained_from.is_some_and(|first| since_seq.saturating_add(1) < first);
        if cursor_ahead || cursor_too_old {
            return Ok(EventPage {
                events: Vec::new(),
                next_cursor: since_seq,
                resync_required: true,
                head_seq,
            });
        }

        let limit = limit.min(MAX_PAGE);
        let since = i64::try_from(since_seq).map_err(|_| LedgerError::SeqOutOfRange)?;
        let capped = i64::try_from(limit).map_err(|_| LedgerError::SeqOutOfRange)?;

        // `events_agent (agent_id, seq)` covers the scoped form; the unscoped
        // one walks the primary key. Neither reads a row it will not return.
        let mut statement = guard.prepare(&format!(
            "SELECT {SELECT_EVENT_COLUMNS} FROM events WHERE seq > ?1{} ORDER BY seq ASC LIMIT ?2",
            match scope {
                Some(_) => " AND (agent_id = ?3 OR agent_id IS NULL)",
                None => "",
            }
        ))?;
        let mut rows = match scope {
            Some(agent_id) => statement.query(params![since, capped, agent_id])?,
            None => statement.query(params![since, capped])?,
        };
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(event_from_row(row)?);
        }

        // A short page means the scan reached the head, so everything up to it
        // has been considered and the cursor may skip whatever was filtered
        // out. Without this an agent polling a ledger of somebody else's events
        // would sit at the same cursor forever, re-scanning the same rows and
        // reporting itself permanently behind `head_seq`. A full page stops at
        // the last row returned, because rows beyond it were never looked at.
        let next_cursor = if events.len() == limit {
            events.last().map_or(since_seq, |event| event.seq)
        } else {
            head_seq
        };
        Ok(EventPage {
            events,
            next_cursor,
            resync_required: false,
            head_seq,
        })
    }

    /// Fetch one event by seq.
    pub fn event(&self, seq: u64) -> Result<Option<Event>> {
        let guard = self.lock()?;
        let mut statement = guard.prepare(&format!(
            "SELECT {SELECT_EVENT_COLUMNS} FROM events WHERE seq = ?1"
        ))?;
        let mut rows = statement.query(params![
            i64::try_from(seq).map_err(|_| LedgerError::SeqOutOfRange)?
        ])?;
        match rows.next()? {
            Some(row) => Ok(Some(event_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Walk the chain from genesis and report the first broken link.
    ///
    /// `docs/spec.md` item 29. Run on export and behind the "chain broken"
    /// banner.
    ///
    /// Includes the anchored-head check when this ledger has an anchor, which
    /// is what makes erasing the end of the chain evident. See [`anchor`] for
    /// the limits of the default sidecar.
    pub fn verify(&self) -> Result<ChainReport> {
        // Append holds this lock through anchor storage; read both under the
        // same lock so concurrent writes cannot make the witness look stale.
        let guard = self.lock()?;
        let anchor = match &self.anchor {
            Some(anchor) => {
                match anchor.load()? {
                    Some(witnessed) => Some(witnessed),
                    // `open_anchored` populates the anchor at open, adopting
                    // the head when the sidecar is empty. Reading `None` here
                    // therefore means the sidecar was REMOVED since. Folding
                    // that into "unanchored" would be a fail-open on the one
                    // file an attacker deletes first.
                    None => {
                        let (head_seq, head_hash) = verify::head_of(&guard)?;
                        return Ok(ChainReport {
                            rows_checked: 0,
                            head_seq,
                            head_hash: head_hash.clone(),
                            first_break: Some(ChainBreak {
                                seq: head_seq,
                                reason: BreakReason::AnchorMissing,
                            }),
                        });
                    }
                }
            }
            None => None,
        };
        verify::walk(&guard, &self.genesis, anchor.as_ref())
    }

    /// Walk the chain against a head the caller kept elsewhere.
    ///
    /// For an operator checking a file against a head they wrote down, a backup
    /// verified against the anchor of the machine that made it, or the
    /// keychain-backed anchor of a later phase.
    pub fn verify_against(&self, anchor: &Anchor) -> Result<ChainReport> {
        let guard = self.lock()?;
        verify::walk(&guard, &self.genesis, Some(anchor))
    }

    /// Record a committed head in the anchor, if there is one.
    ///
    /// Called after the commit, never before: an anchor that is behind the
    /// chain only costs detection of the rows appended since, whereas an anchor
    /// ahead of the chain is the exact signature of truncation and would report
    /// a break every time a machine lost power mid-append.
    ///
    /// Every caller holds the connection and cross-process file locks across
    /// this call. Letting either go first would let
    /// two appends commit in one order and anchor in the other, leaving the
    /// anchor pointing at the earlier of the two.
    ///
    /// A failure here is returned even though the row is already committed. The
    /// alternative is a ledger that quietly stops being able to prove its own
    /// tail, which is worse than a caller that learns its append is only
    /// half-protected: `record_intent` failing closed means the signer is never
    /// reached.
    fn note_head(&self, appended: &Appended) -> Result<()> {
        match &self.anchor {
            Some(anchor) => anchor.store(&Anchor {
                seq: appended.seq,
                hash: appended.hash.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Null a payload while leaving the chain intact.
    ///
    /// `docs/decisions.md` R5 chains `hash(payload)`, so removing the payload
    /// does not touch any hash and the chain still verifies. D-e keeps the
    /// record itself forever: a gap in a hash chain is indistinguishable from
    /// tampering, so retention deletes content, never rows. The null and the
    /// chained [`EventKind::PayloadRedacted`] row that explains it are written
    /// in one transaction, and [`Ledger::verify`] treats a null payload with no
    /// chained redaction covering it as a break.
    ///
    /// A redaction row cannot itself be redacted. Its payload is
    /// `{redacted_seq, reason}` — operator-authored, no agent text — so there
    /// is no retention argument for nulling it, and nulling it would erase the
    /// very evidence verification looks for.
    pub fn redact(&self, seq: u64, reason: &str, ts_ms: i64) -> Result<Appended> {
        let seq_key = i64::try_from(seq).map_err(|_| LedgerError::SeqOutOfRange)?;
        let mut guard = self.lock()?;
        let transaction = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing: Option<(bool, String)> = transaction
            .query_row(
                "SELECT payload IS NULL, kind FROM events WHERE seq = ?1",
                params![seq_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            None => return Err(LedgerError::NoSuchEvent(seq)),
            Some((true, _)) => return Err(LedgerError::AlreadyRedacted(seq)),
            Some((false, kind)) => {
                if kind == EventKind::PayloadRedacted.as_str() {
                    return Err(LedgerError::RedactionIsNotRedactable(seq));
                }
            }
        }

        transaction.execute(
            "UPDATE events SET payload = NULL, redacted_at = ?2, redaction_reason = ?3 WHERE seq = ?1",
            params![seq_key, ts_ms, reason],
        )?;
        let payload = serde_json::json!({ "redacted_seq": seq, "reason": reason });
        let appended = append_in_tx(
            &transaction,
            &NewEvent {
                kind: EventKind::PayloadRedacted,
                ts_ms,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
        )?;
        transaction.commit()?;
        self.note_head(&appended)?;
        Ok(appended)
    }

    /// Record that a feed went down, opening a gap over the window it missed.
    ///
    /// Writes the chained [`EventKind::WsDisconnected`] row and the gap row in
    /// one transaction, so there is never a disconnect event without a window
    /// to reconcile.
    ///
    /// Idempotent per scope: a scope that already has an open gap gets that gap
    /// back, with no second window and no second event. A flapping socket
    /// otherwise orphaned every gap but the last — nothing closes a gap but its
    /// own id, so the first stayed on [`Ledger::unreconciled_gaps`] and under
    /// the staleness overlay (`docs/spec.md` item 34) permanently, while the
    /// reconciler retried a window with no end.
    pub fn open_gap(&self, scope: &str, ts_ms: i64, note: Option<&str>) -> Result<Gap> {
        let mut guard = self.lock()?;
        let transaction = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let open: Option<(i64, i64, i64, Option<String>)> = transaction
            .query_row(
                "SELECT gap_id, opened_ts_ms, open_seq, note FROM feed_gaps \
                 WHERE scope = ?1 AND closed_ts_ms IS NULL ORDER BY gap_id ASC",
                params![scope],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((gap_id, opened_ts_ms, open_seq, note)) = open {
            drop(transaction);
            return Ok(Gap {
                gap_id,
                scope: scope.to_owned(),
                opened_ts_ms,
                closed_ts_ms: None,
                // An open gap cannot be reconciled: mark_gap_reconciled refuses
                // one that has no close.
                reconciled_ts_ms: None,
                open_seq: u64::try_from(open_seq).map_err(|_| LedgerError::SeqOutOfRange)?,
                close_seq: None,
                note,
            });
        }

        let payload = serde_json::json!({ "scope": scope, "note": note });
        let appended = append_in_tx(
            &transaction,
            &NewEvent {
                kind: EventKind::WsDisconnected,
                ts_ms,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
        )?;
        let open_seq = i64::try_from(appended.seq).map_err(|_| LedgerError::SeqOutOfRange)?;
        transaction.execute(
            "INSERT INTO feed_gaps (scope, opened_ts_ms, open_seq, note) VALUES (?1, ?2, ?3, ?4)",
            params![scope, ts_ms, open_seq, note],
        )?;
        let gap_id = transaction.last_insert_rowid();
        transaction.commit()?;
        self.note_head(&appended)?;
        Ok(Gap {
            gap_id,
            scope: scope.to_owned(),
            opened_ts_ms: ts_ms,
            closed_ts_ms: None,
            reconciled_ts_ms: None,
            open_seq: appended.seq,
            close_seq: None,
            note: note.map(str::to_owned),
        })
    }

    /// Record that the feed came back, closing the gap's window.
    ///
    /// The window is now known: `[opened_ts_ms, ts_ms]` is exactly what the
    /// reconciler backfills before the stale overlay comes down
    /// (`docs/specs/history.md` 3.3).
    pub fn close_gap(&self, gap_id: i64, ts_ms: i64) -> Result<Appended> {
        let mut guard = self.lock()?;
        let transaction = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let row: Option<(String, i64, Option<i64>)> = transaction
            .query_row(
                "SELECT scope, opened_ts_ms, closed_ts_ms FROM feed_gaps WHERE gap_id = ?1",
                params![gap_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (scope, opened_ts_ms, closed_ts_ms) = row.ok_or(LedgerError::NoSuchGap(gap_id))?;
        if closed_ts_ms.is_some() {
            return Err(LedgerError::GapAlreadyClosed(gap_id));
        }

        let payload = serde_json::json!({
            "scope": scope,
            "gap_id": gap_id,
            "down_ms": ts_ms.saturating_sub(opened_ts_ms),
        });
        let appended = append_in_tx(
            &transaction,
            &NewEvent {
                kind: EventKind::WsReconnected,
                ts_ms,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
        )?;
        let close_seq = i64::try_from(appended.seq).map_err(|_| LedgerError::SeqOutOfRange)?;
        transaction.execute(
            "UPDATE feed_gaps SET closed_ts_ms = ?2, close_seq = ?3 WHERE gap_id = ?1",
            params![gap_id, ts_ms, close_seq],
        )?;
        transaction.commit()?;
        self.note_head(&appended)?;
        Ok(appended)
    }

    /// Mark a closed gap's window as backfilled.
    ///
    /// Separate from [`Ledger::close_gap`] because reconnecting and having
    /// caught up are different facts: the socket is back long before
    /// `userFillsByTime` has been paged over the window, and only the second
    /// one means nothing was lost.
    pub fn mark_gap_reconciled(&self, gap_id: i64, ts_ms: i64) -> Result<()> {
        let guard = self.lock()?;
        let closed: Option<Option<i64>> = guard
            .query_row(
                "SELECT closed_ts_ms FROM feed_gaps WHERE gap_id = ?1",
                params![gap_id],
                |row| row.get(0),
            )
            .optional()?;
        match closed {
            None => return Err(LedgerError::NoSuchGap(gap_id)),
            Some(None) => return Err(LedgerError::GapStillOpen(gap_id)),
            Some(Some(_)) => {}
        }
        guard.execute(
            "UPDATE feed_gaps SET reconciled_ts_ms = ?2 WHERE gap_id = ?1",
            params![gap_id, ts_ms],
        )?;
        Ok(())
    }

    /// Every gap whose window has not been proven backfilled, oldest first.
    ///
    /// The reconciler's work list, and what the staleness overlay reads.
    pub fn unreconciled_gaps(&self) -> Result<Vec<Gap>> {
        let guard = self.lock()?;
        let mut statement = guard.prepare(
            "SELECT gap_id, scope, opened_ts_ms, closed_ts_ms, reconciled_ts_ms, open_seq, \
             close_seq, note FROM feed_gaps WHERE reconciled_ts_ms IS NULL ORDER BY gap_id ASC",
        )?;
        let mut rows = statement.query([])?;
        let mut gaps = Vec::new();
        while let Some(row) = rows.next()? {
            let open_seq: i64 = row.get(5)?;
            let close_seq: Option<i64> = row.get(6)?;
            gaps.push(Gap {
                gap_id: row.get(0)?,
                scope: row.get(1)?,
                opened_ts_ms: row.get(2)?,
                closed_ts_ms: row.get(3)?,
                reconciled_ts_ms: row.get(4)?,
                open_seq: u64::try_from(open_seq).map_err(|_| LedgerError::SeqOutOfRange)?,
                close_seq: close_seq
                    .map(|value| u64::try_from(value).map_err(|_| LedgerError::SeqOutOfRange))
                    .transpose()?,
                note: row.get(7)?,
            });
        }
        Ok(gaps)
    }

    /// Store a decision-time book snapshot and return the reference to chain.
    ///
    /// `docs/decisions.md` R6 is plumbing only at this phase: this writes a body
    /// and hands back `(id, hash)` for [`NewEvent::snapshot`]. What to capture,
    /// and when, is not decided here.
    ///
    /// An id is bound to one body for good. Re-capturing the same body is
    /// idempotent and returns the same hash; offering a *different* body under
    /// an id that a chained row already points at is
    /// [`LedgerError::SnapshotBodyConflict`], not a silent replacement. R6
    /// exists because the book at the moment an agent decided cannot be
    /// backfilled, and an `INSERT OR REPLACE` made the chained hash decorative:
    /// the row still named a book, and the book it named had been swapped.
    pub fn put_snapshot(
        &self,
        snapshot_id: &str,
        ts_ms: i64,
        coin: &str,
        body: &Value,
    ) -> Result<String> {
        let canonical = hash::canonical_json(body)?;
        let snapshot_hash = hash::payload_hash(canonical.as_bytes());
        let guard = self.lock()?;
        guard.execute(
            "INSERT INTO book_snapshots (snapshot_id, snapshot_hash, ts_ms, coin, body) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (snapshot_id) DO NOTHING",
            params![snapshot_id, &snapshot_hash, ts_ms, coin, canonical],
        )?;
        let stored: String = guard.query_row(
            "SELECT snapshot_hash FROM book_snapshots WHERE snapshot_id = ?1",
            params![snapshot_id],
            |row| row.get(0),
        )?;
        if stored != snapshot_hash {
            return Err(LedgerError::SnapshotBodyConflict {
                snapshot_id: snapshot_id.to_owned(),
                expected: stored,
                found: snapshot_hash,
            });
        }
        Ok(snapshot_hash)
    }

    /// Read a snapshot body back, if it has not been pruned.
    ///
    /// `expected_hash` is the `snapshot_hash` from the chained row that refers
    /// to this snapshot. The stored body is rehashed and checked against it, so
    /// a body that was replaced after the row was chained is
    /// [`LedgerError::SnapshotBodyConflict`] rather than an answer. Reading it
    /// without the chained hash was the whole weakness: the hash was stored and
    /// then never used, so a pruned-and-replaced book was served as the
    /// decision-time book.
    pub fn snapshot_body(&self, snapshot_id: &str, expected_hash: &str) -> Result<Option<Value>> {
        let guard = self.lock()?;
        let body: Option<String> = guard
            .query_row(
                "SELECT body FROM book_snapshots WHERE snapshot_id = ?1",
                params![snapshot_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(text) = body else {
            return Ok(None);
        };
        // Rehashed from the body, never read from the `snapshot_hash` column:
        // that column is in the same unchained table as the body and is edited
        // by the same hand.
        let found = hash::payload_hash(text.as_bytes());
        if found != expected_hash {
            return Err(LedgerError::SnapshotBodyConflict {
                snapshot_id: snapshot_id.to_owned(),
                expected: expected_hash.to_owned(),
                found,
            });
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// Drop snapshot bodies older than `ts_ms`, returning how many went.
    ///
    /// `docs/decisions.md` D-e: recomputable and unchained inputs are prunable.
    /// The chained references survive, so a pruned snapshot leaves a row that
    /// still says a snapshot was taken and what it hashed to.
    pub fn prune_snapshots_before(&self, ts_ms: i64) -> Result<usize> {
        let guard = self.lock()?;
        Ok(guard.execute(
            "DELETE FROM book_snapshots WHERE ts_ms < ?1",
            params![ts_ms],
        )?)
    }

    /// Insert or update a sub-account.
    ///
    /// `created_ts_ms` is kept from the first write: rediscovering an account
    /// does not make it new.
    pub fn upsert_sub_account(&self, account: &SubAccount) -> Result<()> {
        let guard = self.lock()?;
        guard.execute(
            "INSERT INTO sub_accounts (address, name, owner_type, owner_id, recorded, \
             provisioned_by_oppen, active, created_ts_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (address) DO UPDATE SET name = excluded.name, \
             owner_type = excluded.owner_type, owner_id = excluded.owner_id, \
             recorded = excluded.recorded, provisioned_by_oppen = excluded.provisioned_by_oppen, \
             active = excluded.active",
            params![
                account.address,
                account.name,
                account
                    .owner
                    .as_ref()
                    .map(|owner| owner.owner_type.as_str()),
                account.owner.as_ref().map(|owner| owner.owner_id.as_str()),
                account.recorded,
                account.provisioned_by_oppen,
                account.active,
                account.created_ts_ms,
            ],
        )?;
        Ok(())
    }

    /// Every known sub-account, ordered by address.
    ///
    /// Ordered rather than left to the storage engine: `AGENTS.md` asks for
    /// deterministic output on anything serialised.
    pub fn sub_accounts(&self) -> Result<Vec<SubAccount>> {
        let guard = self.lock()?;
        let mut statement = guard.prepare(
            "SELECT address, name, owner_type, owner_id, recorded, provisioned_by_oppen, active, \
             created_ts_ms FROM sub_accounts ORDER BY address ASC",
        )?;
        let mut rows = statement.query([])?;
        let mut accounts = Vec::new();
        while let Some(row) = rows.next()? {
            accounts.push(sub_account_from_row(row)?);
        }
        Ok(accounts)
    }

    /// One sub-account by address.
    pub fn sub_account(&self, address: &str) -> Result<Option<SubAccount>> {
        let guard = self.lock()?;
        let mut statement = guard.prepare(
            "SELECT address, name, owner_type, owner_id, recorded, provisioned_by_oppen, active, \
             created_ts_ms FROM sub_accounts WHERE address = ?1",
        )?;
        let mut rows = statement.query(params![address])?;
        match rows.next()? {
            Some(row) => Ok(Some(sub_account_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Write the whole chain as CSV, returning the row count.
    ///
    /// `docs/specs/history.md` 3.5: the export is the tax-and-accounting path as
    /// much as the audit one, so it has to open in a spreadsheet.
    pub fn export_csv<W: std::io::Write>(&self, out: &mut W) -> Result<u64> {
        let guard = self.lock()?;
        export::to_csv(&guard, out)
    }

    /// Write the whole chain as JSON Lines, returning the row count.
    pub fn export_jsonl<W: std::io::Write>(&self, out: &mut W) -> Result<u64> {
        let guard = self.lock()?;
        export::to_jsonl(&guard, out)
    }

    /// Take the connection lock, turning poisoning into an error.
    ///
    /// `AGENTS.md` conventions forbid a panic on an input path, and a panic in
    /// one ledger call should not make every later call panic too.
    fn lock(&self) -> Result<LedgerGuard<'_>> {
        let connection = self.connection.lock().map_err(|_| LedgerError::Poisoned)?;
        let coordination = acquire_coordination(&self.coordination_path)?;
        Ok(LedgerGuard {
            connection,
            _coordination: coordination,
        })
    }
}

// SQLite releases its write lock at COMMIT, before the sidecar can be fsynced.
// Keep a separate OS lock through both steps and verification. Never unlink
// this lock file: a second inode would allow two cooperating writers through.
struct LedgerGuard<'a> {
    connection: MutexGuard<'a, Connection>,
    _coordination: File,
}

impl Deref for LedgerGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl DerefMut for LedgerGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

fn acquire_coordination(path: &Path) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if started.elapsed() < BUSY_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(LedgerError::Io(error.into())),
        }
    }
}

/// The slice of the ledger an agent may hold.
///
/// `docs/spec.md` D6 makes `get_events` an agent-facing call served from the
/// same `Ledger` that owns [`Ledger::redact`], [`Ledger::upsert_sub_account`]
/// and the gap and snapshot tables. `AGENTS.md` invariant 3 says no
/// agent-reachable path modifies the agent registry, so `oppen-mcp` is handed
/// one of these and never a `&Ledger`: reading events is all this type can
/// express, and the invariant becomes a compile error instead of a thing to
/// remember in review. Operator-only Tauri commands keep the `&Ledger`.
#[derive(Debug, Clone)]
pub struct AgentView {
    ledger: Arc<Ledger>,
    agent_id: String,
}

impl AgentView {
    /// Read a page of the events this agent may see, after `since_seq`.
    ///
    /// Scoped: this agent's own rows plus the account-wide ones no agent owns.
    /// See [`Ledger::get_events_for_agent`] and `docs/decisions.md` C6.
    pub fn get_events(&self, since_seq: u64, limit: usize) -> Result<EventPage> {
        self.ledger
            .get_events_for_agent(&self.agent_id, since_seq, limit)
    }

    /// Fetch one event by seq, or `None` when it is not this agent's to read.
    ///
    /// An event that exists but belongs to another agent is `None` rather than
    /// an error: the two are indistinguishable to a caller that may not know it
    /// exists, and saying which would leak the thing the scope withholds.
    pub fn event(&self, seq: u64) -> Result<Option<Event>> {
        Ok(self.ledger.event(seq)?.filter(|event| {
            event
                .agent_id
                .as_ref()
                .is_none_or(|owner| *owner == self.agent_id)
        }))
    }

    /// This agent's fills inside a time window, for spec F's execution report.
    ///
    /// On the agent view rather than on [`EventViews`] so the scoping is
    /// structural: there is no argument here that could name another agent,
    /// which is the same reason `EventViews` exposes narrow capabilities, not the
    /// `Arc<Ledger>` behind it.
    pub fn fills_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<Value>> {
        self.ledger.fills_for(&self.agent_id, from_ms, to_ms)
    }

    /// The agent this view speaks for.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

/// Hands out agent views and the execution-only submission journal.
///
/// `oppen-mcp` resolves the agent from the pairing token on every request
/// (`docs/spec.md` item 15), so it needs to build a view *per call* rather than
/// hold one — and the obvious way to do that, keeping an `Arc<Ledger>`, would
/// put `redact` and `upsert_sub_account` back within reach of a tool. This owns
/// the `Arc` privately without exposing operator mutations, so `AGENTS.md` invariant 3
/// stays a compile error rather than a review note.
#[derive(Debug, Clone)]
pub struct EventViews(Arc<Ledger>);

impl EventViews {
    pub fn new(ledger: Arc<Ledger>) -> Self {
        EventViews(ledger)
    }

    /// Execution-only capability: no registry, redaction or policy mutations.
    pub fn submissions(&self) -> SubmissionJournal {
        SubmissionJournal::new(self.0.clone())
    }

    /// The read-only slice belonging to `agent_id`.
    pub fn for_agent(&self, agent_id: impl Into<String>) -> AgentView {
        self.0.agent_view(agent_id)
    }
}

/// Set the pragmas the durability guarantee depends on.
///
/// WAL so a reader never blocks the writer that is recording a fill, and
/// `synchronous = FULL` so a commit means the bytes are on the platter. FULL is
/// the price of "the intent row is durable before the signer runs": with
/// `NORMAL`, a WAL commit can still be lost to a power cut, which is exactly the
/// case that would leave an order with no record of why.
fn configure(connection: &Connection) -> Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    let mode: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(LedgerError::JournalMode(mode));
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

/// Bind the file to a network, or refuse it if it already belongs to another.
///
/// `docs/decisions.md` R4 calls a mainnet number that is actually a testnet
/// number the worst bug this product can ship. The file name usually keeps them
/// apart; this catches the renamed, copied or restored file that the name no
/// longer describes.
fn bind_network(connection: &mut Connection, network: Network, genesis: &str) -> Result<()> {
    let expected = network_key(network);
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: Option<String> = transaction
        .query_row(
            "SELECT value FROM ledger_meta WHERE key = 'network'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match found {
        Some(found) if found != expected => {
            return Err(LedgerError::NetworkMismatch { expected, found });
        }
        Some(_) => {}
        None => {
            transaction.execute(
                "INSERT INTO ledger_meta (key, value) VALUES ('network', ?1)",
                params![expected],
            )?;
            transaction.execute(
                "INSERT INTO chain_head (id, seq, hash) VALUES (0, 0, ?1)",
                params![genesis],
            )?;
        }
    }
    transaction.commit()?;
    Ok(())
}

/// Read the chain head: the last seq and its hash.
fn head(connection: &Connection) -> Result<(u64, String)> {
    let (seq, hash): (i64, String) =
        connection.query_row("SELECT seq, hash FROM chain_head WHERE id = 0", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    Ok((
        u64::try_from(seq).map_err(|_| LedgerError::SeqOutOfRange)?,
        hash,
    ))
}

/// The idempotence key of one fill.
///
/// A storage format: it is what the partial unique index holds, so moving it
/// orphans every fill already keyed and the next walk records them all again.
/// The account comes first so that one container's fills are a contiguous range
/// of the index, which is how [`Ledger::newest_fill_ts_ms`] reads them.
fn fill_idem_key(account: &str, tid: u64) -> String {
    format!("fill:{account}:{tid}")
}

/// The half-open key range covering every fill of one container.
///
/// The upper bound is the prefix with its final `:` (0x3A) raised to `;`
/// (0x3B). Every key in the range is the prefix followed by decimal digits, all
/// of which sort below `;`, and a different account diverges from the prefix
/// before that byte — so the range is exactly this container's fills.
fn fill_key_range(account: &str) -> (String, String) {
    let low = format!("fill:{account}:");
    let high = format!("fill:{account};");
    (low, high)
}

/// A chained row, built from the head and ready to insert.
///
/// Split out so the keyed and unkeyed inserts share one definition of what a
/// row *is* — the hash preimage above all. The two differ only in the statement
/// that writes them.
struct ChainedRow<'a> {
    seq: u64,
    seq_key: i64,
    kind: &'static str,
    ts_ms: i64,
    agent_id: Option<&'a str>,
    canonical: String,
    payload_hash: String,
    prev_hash: String,
    row_hash: String,
    snapshot_id: Option<&'a str>,
    snapshot_hash: Option<&'a str>,
}

impl ChainedRow<'_> {
    /// What the caller gets back once the row is in.
    fn appended(self) -> Appended {
        Appended {
            seq: self.seq,
            hash: self.row_hash,
        }
    }
}

/// Build the next row of the chain.
///
/// Taking the seq from `chain_head` under the transaction's write lock, rather
/// than from `MAX(seq)` or SQLite's rowid allocator, is what makes the seq
/// strictly monotonic even after a row is deleted — and the cursor in
/// `docs/spec.md` D6 only works if it is.
///
/// The payload is canonicalised here rather than by `serde_json::to_string`, and
/// here rather than in `append_committed`, so that every chained row — including
/// the ones redact, open_gap and close_gap write directly — goes through the one
/// encoder that sorts keys and refuses floats.
fn chain_row<'a>(transaction: &Transaction<'_>, event: &NewEvent<'a>) -> Result<ChainedRow<'a>> {
    let (head_seq, prev_hash) = head(transaction)?;
    let seq = head_seq.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
    let seq_key = i64::try_from(seq).map_err(|_| LedgerError::SeqOutOfRange)?;

    let canonical = hash::canonical_json(event.payload)?;
    let payload_hash = hash::payload_hash(canonical.as_bytes());
    let kind = event.kind.as_str();
    let snapshot_id = event.snapshot.map(|snapshot| snapshot.id);
    let snapshot_hash = event.snapshot.map(|snapshot| snapshot.hash);
    let row_hash = hash::row_hash(&hash::RowHashInput {
        prev_hash: &prev_hash,
        seq,
        kind,
        ts_ms: event.ts_ms,
        agent_id: event.agent_id,
        payload_hash: &payload_hash,
        snapshot_id,
        snapshot_hash,
    });
    Ok(ChainedRow {
        seq,
        seq_key,
        kind,
        ts_ms: event.ts_ms,
        agent_id: event.agent_id,
        canonical,
        payload_hash,
        prev_hash,
        row_hash,
        snapshot_id,
        snapshot_hash,
    })
}

/// Move the head to a row that has just been written.
fn advance_head(transaction: &Transaction<'_>, row: &ChainedRow<'_>) -> Result<()> {
    transaction.execute(
        "UPDATE chain_head SET seq = ?1, hash = ?2 WHERE id = 0",
        params![row.seq_key, row.row_hash],
    )?;
    Ok(())
}

/// Append inside an existing transaction.
///
/// Shared by [`Ledger::append`] and by every operation that has to write a
/// chained row and a side-table row atomically. The row carries no idempotence
/// key, so the insert either writes or raises — there is no third outcome to
/// report.
fn append_in_tx(transaction: &Transaction<'_>, event: &NewEvent<'_>) -> Result<Appended> {
    let row = chain_row(transaction, event)?;
    transaction.execute(
        "INSERT INTO events (seq, ts_ms, kind, agent_id, payload, payload_hash, prev_hash, hash, \
         snapshot_id, snapshot_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            row.seq_key,
            row.ts_ms,
            row.kind,
            row.agent_id,
            row.canonical,
            row.payload_hash,
            row.prev_hash,
            row.row_hash,
            row.snapshot_id,
            row.snapshot_hash,
        ],
    )?;
    advance_head(transaction, &row)?;
    Ok(row.appended())
}

/// Append inside an existing transaction, keyed for idempotence.
///
/// `Ok(None)` means the key is already in the chain and nothing was written.
/// The conflict target names the partial index exactly, so this swallows a
/// repeated `idem_key` and nothing else: a violation of any other constraint
/// still raises. The head is advanced only when a row actually landed, so a
/// duplicate leaves the chain — and its hash — untouched.
fn append_keyed_in_tx(
    transaction: &Transaction<'_>,
    event: &NewEvent<'_>,
    idem_key: &str,
) -> Result<Option<Appended>> {
    let row = chain_row(transaction, event)?;
    let written = transaction.execute(
        "INSERT INTO events (seq, ts_ms, kind, agent_id, payload, payload_hash, prev_hash, hash, \
         snapshot_id, snapshot_hash, idem_key) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT (idem_key) WHERE idem_key IS NOT NULL DO NOTHING",
        params![
            row.seq_key,
            row.ts_ms,
            row.kind,
            row.agent_id,
            row.canonical,
            row.payload_hash,
            row.prev_hash,
            row.row_hash,
            row.snapshot_id,
            row.snapshot_hash,
            idem_key,
        ],
    )?;
    if written == 0 {
        return Ok(None);
    }
    advance_head(transaction, &row)?;
    Ok(Some(row.appended()))
}

/// Build an [`Event`] from a row selected with [`SELECT_EVENT_COLUMNS`].
pub(crate) fn event_from_row(row: &Row<'_>) -> Result<Event> {
    let seq_raw: i64 = row.get(0)?;
    let seq = u64::try_from(seq_raw).map_err(|_| LedgerError::SeqOutOfRange)?;
    let kind: String = row.get(2)?;
    let payload: Option<String> = row.get(4)?;
    let payload = match payload {
        Some(text) => {
            Some(serde_json::from_str(&text).map_err(|_| LedgerError::PayloadNotJson { seq })?)
        }
        None => None,
    };
    Ok(Event {
        seq,
        ts_ms: row.get(1)?,
        kind: kind.parse()?,
        agent_id: row.get(3)?,
        payload,
        payload_hash: row.get(5)?,
        prev_hash: row.get(6)?,
        hash: row.get(7)?,
        redacted_at: row.get(8)?,
        redaction_reason: row.get(9)?,
        snapshot_id: row.get(10)?,
        snapshot_hash: row.get(11)?,
    })
}

/// Build a [`SubAccount`] from a row of the registry query.
fn sub_account_from_row(row: &Row<'_>) -> Result<SubAccount> {
    let owner_type: Option<String> = row.get(2)?;
    let owner_id: Option<String> = row.get(3)?;
    let owner = match (owner_type, owner_id) {
        (Some(kind), Some(id)) => Some(Owner {
            owner_type: kind.parse()?,
            owner_id: id,
        }),
        _ => None,
    };
    Ok(SubAccount {
        address: row.get(0)?,
        name: row.get(1)?,
        owner,
        recorded: row.get(4)?,
        provisioned_by_oppen: row.get(5)?,
        active: row.get(6)?,
        created_ts_ms: row.get(7)?,
    })
}

/// Writes every guardrail verdict into the chained ledger.
///
/// D6 makes this ledger the single record of why an order happened, and the
/// engine's [`AuditSink`](crate::guardrail::AuditSink) doc says the ledger
/// module implements it. Until this existed the trait shipped with only a
/// test-only implementation, so a released build had no way to record a
/// clearance at all.
///
/// The event kind is chosen from the outcome rather than passed in, so a
/// caller cannot file a refusal as an approval:
///
/// - an order clearance is [`EventKind::OrderIntent`], committed through
///   [`Ledger::record_intent`] *before* signing;
/// - a cancel or dead-man clearance is [`EventKind::AgentDecision`]; it
///   records permission to act, not a venue-confirmed order state change;
/// - a refusal is [`EventKind::Refusal`], which `docs/decisions.md` D-c
///   requires in the record because the refusal is the onboarding;
/// - an operator mutation is [`EventKind::OperatorAction`].
pub struct LedgerAuditSink {
    ledger: std::sync::Arc<Ledger>,
}

impl LedgerAuditSink {
    pub fn new(ledger: std::sync::Arc<Ledger>) -> Self {
        Self { ledger }
    }
}

impl std::fmt::Debug for LedgerAuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerAuditSink").finish_non_exhaustive()
    }
}

impl crate::guardrail::AuditSink for LedgerAuditSink {
    fn record(
        &self,
        entry: &crate::guardrail::AuditEntry<'_>,
    ) -> std::result::Result<(), crate::guardrail::AuditError> {
        use crate::guardrail::{AuditOutcome, ClearedKind};

        let (kind, mut payload) = match &entry.outcome {
            AuditOutcome::Cleared(clearance) => (
                match clearance.kind {
                    ClearedKind::Order { .. } => EventKind::OrderIntent,
                    ClearedKind::Cancel { .. } | ClearedKind::ScheduleCancel { .. } => {
                        EventKind::AgentDecision
                    }
                },
                serde_json::to_value(clearance).map_err(|e| crate::guardrail::AuditError {
                    detail: e.to_string(),
                })?,
            ),
            AuditOutcome::Refused(refusal) => (
                EventKind::Refusal,
                serde_json::json!({
                    "refusal": refusal.to_string(),
                    "refusal_detail": refusal,
                }),
            ),
            AuditOutcome::Operator(action) => (
                EventKind::OperatorAction,
                serde_json::to_value(action).map_err(|e| crate::guardrail::AuditError {
                    detail: e.to_string(),
                })?,
            ),
        };

        // The agent's own words travel with the row. Untrusted text
        // (`AGENTS.md` invariant 9): stored verbatim, never interpreted.
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "reason".into(),
                serde_json::Value::String(entry.reason.to_owned()),
            );
        }

        if let AuditOutcome::Cleared(clearance) = &entry.outcome
            && matches!(clearance.kind, ClearedKind::Order { .. })
        {
            return self
                .ledger
                .record_intent(&NewIntent {
                    agent_id: clearance.agent.as_str(),
                    ts_ms: entry.at_ms as i64,
                    payload: &payload,
                    snapshot: None,
                })
                .map(|_| ())
                .map_err(|e| crate::guardrail::AuditError {
                    detail: e.to_string(),
                });
        }

        self.ledger
            .append(&NewEvent {
                kind,
                ts_ms: entry.at_ms as i64,
                agent_id: entry.agent.map(|agent| agent.as_str()),
                payload: &payload,
                snapshot: None,
            })
            .map(|_| ())
            .map_err(|e| crate::guardrail::AuditError {
                detail: e.to_string(),
            })
    }
}
