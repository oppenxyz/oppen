//! The engine, the type that proves it ran, and the signing call it gates.
//!
//! [`Cleared`] is the whole point of the module. It has private fields, no
//! `Clone`, no `Default`, and a constructor that is private to this file —
//! reachable only from the success branch of [`GuardrailEngine::decide`].
//! [`GuardrailEngine::sign_cleared`] takes one by value.
//!
//! [`PreSignGate`] is the gate: a private type pairing the engine with the
//! clearance it is signing, so the evaluation runs *inside*
//! `ExchangeRequest::sign_checked`, after the request is fully assembled and
//! before the key is touched (`AGENTS.md` invariant 1: "checked in Rust
//! immediately before signing").
//!
//! What the compiler checks, exactly:
//!
//! 1. **A `Cleared` cannot exist unless an evaluation produced it.** No
//!    public constructor, no `Clone`, no `Default`.
//! 2. **One clearance authorises one signature.** `sign_cleared` takes it by
//!    value and `Cleared` is not `Clone`.
//! 3. **`sign_cleared` cannot skip the gate.** It has no parameter for the
//!    checker; it builds one.
//! 4. **The gate cannot be handed an action nobody evaluated.** The checker
//!    type is private, so `sign_checked(…, &engine)` — the engine stamping
//!    an arbitrary `Action` for a caller — does not compile.
//!
//! What it does not check: that nothing *else* signs. [`crate::guardrail`]'s
//! module doc states that residual and why it is the honest one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use rust_decimal::Decimal;
use serde::Serialize;

use oppen_hl::exchange::{PreSign, PreSignCheck, SignError};
use oppen_hl::meta::Asset;
use oppen_hl::order::{OrderKind, OrderSpec};
use oppen_hl::wire::{BuilderInfo, CancelByCloidWire, CancelWire, Cloid, Grouping, OrderType};
use oppen_hl::{Action, Address, AgentKey, ExchangeRequest, Network};

use crate::keys::{AgentWallet, KeyStore, KeyStoreError};
use crate::ledger::approval::{ApprovalJournal, Candidate, ReviewCommitment, ReviewEvidence};
use crate::ledger::{
    Appended, AuthorizedRoute, LedgerAuditSink, PolicyJournal, SubmissionError, SubmissionJournal,
    SubmissionReceipt,
};

use super::breaker::{self, BudgetScope, LossBudget, LossKind};
use super::bucket::{BucketError, TokenBucket};
use super::config::{
    APPROVAL_TTL_MS, AgentGuardrails, GlobalRateBudget, LossLimits, MAX_REASON_BYTES, OrderRate,
};
use super::deadman::{DEAD_MAN_MIN_LEAD_MS, DeadManIntent};
use super::kill::{Engagement, KillEffect, KillReason, KillScope, KillSwitch};
use super::refusal::{ReduceOnlyBreach, Refusal, Unevaluable, VenueRule};
use super::snapshot::{AccountSnapshot, Exposure, MarketRef, MarketSnapshotRef};
use super::store::{
    GuardrailStore, PersistedState, PolicyVersion, SqliteGuardrailStore, StoreError,
};
use super::{AgentId, CancelContext, CancelIntent, CancelTarget};

#[path = "activation.rs"]
mod activation;
#[path = "kill_release.rs"]
mod kill_release;
pub use activation::{
    ActivationDisplay, ActivationEvidence, ActivationObservation, ActivationReceipt,
    ActivationReview,
};
pub use kill_release::{
    KillReleaseDisplay, KillReleaseError, KillReleaseMember, KillReleaseReceipt,
    KillReleaseResolution, KillReleaseReview,
};

/// One basis point is a ten-thousandth.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
const HUNDRED: Decimal = Decimal::from_parts(100, 0, 0, false, 0);
/// The two in spec F's `risk_budget / (2 * sigma_day)`: the cap is set so
/// that a *two*-sigma day, not a one-sigma day, costs the whole budget.
const TWO: Decimal = Decimal::from_parts(2, 0, 0, false, 0);

/// What an agent is asking to do, in decimals, before anything is rounded.
///
/// This is a request, not an order: nothing here reaches the wire until the
/// engine has rounded it to the asset's rules and every predicate has passed.
///
/// There is deliberately **no approval field**. Approval is not something a
/// request can assert about itself — see [`Proposal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderIntent {
    /// None means normalized-only evidence; never infer a market request from IOC.
    pub original: Option<super::OriginalRequest>,
    pub symbol: String,
    pub is_buy: bool,
    pub px: Decimal,
    pub sz: Decimal,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub cloid: Option<Cloid>,
    pub grouping: Grouping,
    /// Attached in official builds via `OPPEN_BUILDER_ADDRESS` (D7). It does
    /// not affect any predicate; it travels with the intent so the action the
    /// engine builds is the complete one.
    pub builder: Option<BuilderInfo>,
    /// An agent may bind itself tighter than its guardrail allows. The
    /// effective limit is the minimum of the two; it can never be looser.
    pub max_slippage_bps: Option<Decimal>,
    /// Spec item 19 requires a reason on every execution tool call. Item 30:
    /// this is an untrusted claim, rendered as inert plain text, never
    /// interpreted here. Bounded and control-character-checked on the way in:
    /// see [`Refusal::ReasonTooLong`].
    pub reason: String,
}

/// A queued order waiting for an operator (spec item 28).
///
/// The engine mints these and holds them. A caller receives only the id, in
/// [`Refusal::ApprovalRequired`], and hands it back to
/// [`GuardrailEngine::operator_approve_proposal`], which looks up **its own
/// stored intent** rather than trusting a re-supplied one. So there is no
/// value a caller can construct that asserts "this was approved", and no way
/// to approve one order and then sign a different one — which is what a
/// caller-supplied approval field allowed, along with skipping the order-rate
/// charge entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub(crate) id: String,
    pub(crate) agent: AgentId,
    pub(crate) intent: ProposalIntent,
    pub(crate) route: AuthorizedRoute,
    pub(crate) expires_at_ms: u64,
    pub(crate) cancel_provenance: Option<super::CancelProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ProposalIntent {
    Order(OrderIntent),
    Cancel(CancelIntent),
}

impl Proposal {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Who asked. `pending_proposals` is fleet-wide, so the approvals queue
    /// needs this to attribute a proposal to a roster card (item 32).
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// The original authorized account, not a lookup through today's registry.
    pub fn account(&self) -> Address {
        self.route.binding.container
    }

    /// What the agent asked for. Item 28 re-prices at approval time, so the
    /// operator console shows this against the current market to display the
    /// drift.
    pub fn intent(&self) -> &ProposalIntent {
        &self.intent
    }

    pub fn order_intent(&self) -> Option<&OrderIntent> {
        match &self.intent {
            ProposalIntent::Order(intent) => Some(intent),
            _ => None,
        }
    }

    pub fn cancel_intent(&self) -> Option<&CancelIntent> {
        match &self.intent {
            ProposalIntent::Cancel(intent) => Some(intent),
            _ => None,
        }
    }

    /// Item 28's TTL, and the `expires_at` of the MCP `pending_approval`
    /// result. Without it the queue cannot show the countdown.
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// An operator's retained review, not a clearance. Only this engine can build
/// one, and confirmation consumes it. Display data cannot reconstruct authority.
///
/// ```compile_fail
/// use oppen_core::guardrail::ApprovalReview;
/// fn duplicate(review: &ApprovalReview) -> ApprovalReview { review.clone() }
/// ```
///
/// ```compile_fail
/// use oppen_core::guardrail::ApprovalReview;
/// let _: ApprovalReview = serde_json::from_str("{}").unwrap();
/// ```
pub struct ApprovalReview {
    evidence: ReviewEvidence,
    candidate: ProposalIntent,
    action: Action,
    commitment: ReviewCommitment,
    display: ApprovalReviewDisplay,
}

impl std::fmt::Debug for ApprovalReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalReview")
            .field("display", &self.display)
            .finish_non_exhaustive()
    }
}

impl ApprovalReview {
    pub fn proposal_id(&self) -> &str {
        self.display.proposal_id()
    }
    pub fn agent(&self) -> &AgentId {
        self.display.agent()
    }
    pub fn account(&self) -> Address {
        self.display.account()
    }
    pub fn symbol(&self) -> Option<&str> {
        match &self.display {
            ApprovalReviewDisplay::Order(display) => Some(&display.symbol),
            _ => None,
        }
    }
    pub fn display(&self) -> &ApprovalReviewDisplay {
        &self.display
    }
}

#[derive(Debug, Clone, Serialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalReviewDisplay {
    Order(OrderApprovalReviewDisplay),
    Cancel(CancelApprovalReviewDisplay),
}

impl ApprovalReviewDisplay {
    pub fn proposal_id(&self) -> &str {
        match self {
            Self::Order(d) => &d.proposal_id,
            Self::Cancel(d) => &d.proposal_id,
        }
    }
    pub fn agent(&self) -> &AgentId {
        match self {
            Self::Order(d) => &d.agent,
            Self::Cancel(d) => &d.agent,
        }
    }
    pub fn account(&self) -> Address {
        match self {
            Self::Order(d) => d.account,
            Self::Cancel(d) => d.account,
        }
    }
    pub fn expires_at_ms(&self) -> u64 {
        match self {
            Self::Order(d) => d.expires_at_ms,
            Self::Cancel(d) => d.expires_at_ms,
        }
    }
    pub fn order(&self) -> Option<&OrderApprovalReviewDisplay> {
        match self {
            Self::Order(d) => Some(d),
            _ => None,
        }
    }
    pub fn cancel(&self) -> Option<&CancelApprovalReviewDisplay> {
        match self {
            Self::Cancel(d) => Some(d),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelApprovalReviewDisplay {
    pub proposal_id: String,
    pub agent: AgentId,
    pub account: Address,
    pub targets: Vec<CancelTarget>,
    pub reason: String,
    pub route: AuthorizedRoute,
    pub policy_revision: u64,
    pub policy_hash: String,
    pub reviewed_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderApprovalReviewDisplay {
    pub proposal_id: String,
    pub agent: AgentId,
    pub account: Address,
    pub symbol: String,
    pub original: Option<super::OriginalRequest>,
    #[serde(with = "rust_decimal::serde::str")]
    pub original_px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub reference_px: Decimal,
    pub reference_at_ms: u64,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub drift_bps: Option<Decimal>,
    pub asset_index: u32,
    pub is_buy: bool,
    #[serde(with = "rust_decimal::serde::str")]
    pub px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub sz: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub notional_usd: Decimal,
    pub reduce_only: bool,
    pub order_type: OrderType,
    pub cloid: Cloid,
    pub grouping: Grouping,
    pub builder: Option<BuilderInfo>,
    pub route: AuthorizedRoute,
    pub policy_revision: u64,
    pub policy_hash: String,
    pub reviewed_at_ms: u64,
    pub expires_at_ms: u64,
}

/// How much of each guardrail this order consumes, for the utilization block
/// spec item 16 puts in `get_state` and for the continuous loss-budget gauge
/// spec F wants before the breaker fires.
///
/// `None` where the ratio is undefined because the limit is zero or unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Utilization {
    pub order_notional_pct: Option<Decimal>,
    pub position_notional_pct: Option<Decimal>,
    pub daily_loss_pct: Option<Decimal>,
    /// The other half of the breaker. It fires on two budgets and this block
    /// reported one, so an agent one dollar from a drawdown kill read a
    /// utilization block with nothing in it about drawdown — and `preflight`,
    /// whose whole job is to say where an order would sit against each cap,
    /// was silent on the cap that was about to stop it.
    pub drawdown_pct: Option<Decimal>,
    /// Where the post-fill position sits against spec F's vol-scaled cap.
    /// `None` when no `max_risk_usd` is configured, which is the default —
    /// the same "absent means unset" the loss percentages use, and the
    /// reason it is not simply `100` when the option is off.
    pub vol_scaled_position_pct: Option<Decimal>,
    pub leverage: Decimal,
    pub order_tokens_remaining: Decimal,
    /// What is left of spec item 10's address-wide request budget. Item 16
    /// puts it in `get_state` next to the per-agent number, because they
    /// throttle for different reasons and an agent that only sees its own cap
    /// cannot tell why it is being refused.
    ///
    /// **One bucket, and item 10 says there should be N.** This was written
    /// for the pre-revision model where one master account held every
    /// sub-account, so one address meant one budget. Under the revised D1
    /// (V2) each container is its own Hyperliquid address and `userRateLimit`
    /// is metered per address, so item 10 re-derives to "N budgets that are
    /// additive but not fungible" (`docs/decisions.md`, "what this revises
    /// elsewhere"). A single shared bucket errs only in the safe direction —
    /// the fleet together can never outspend one address's allowance — but it
    /// refuses one container's orders because a *different* container was
    /// busy, and it cannot answer "how much of my own address's budget is
    /// left". Fixing it is a per-container bucket seeded from that
    /// container's own `userRateLimit`, which changes
    /// [`GuardrailEngine::operator_set_global_rate_budget`]'s signature and
    /// needs its own decision record rather than a patch here.
    pub global_tokens_remaining: Decimal,
}

/// What a preflight found (`docs/spec.md` item 20).
///
/// Deliberately *not* a [`Cleared`]: this says what the guardrails would do,
/// and carries no authority to do it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// True when every predicate passed. The order may still be refused when
    /// it is actually sent — the feed moves, and the rate token this did not
    /// spend may be gone by then.
    pub would_clear: bool,
    /// Where the order would sit against each cap. Present only when it would
    /// clear; a refusal names its own limit.
    pub utilization: Option<Utilization>,
    /// The predicate that would refuse it, with the observed value and the
    /// limit. `approval_required` carries an empty `approval_id` here — no
    /// proposal was minted, because nothing was asked for.
    pub refusal: Option<Refusal>,
}

/// What was cleared, in the terms the guardrails evaluated it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "cleared", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClearedKind {
    Order {
        symbol: String,
        is_buy: bool,
        /// The rounded price actually on the wire, not what was asked for.
        px: Decimal,
        sz: Decimal,
        notional_usd: Decimal,
        reduce_only: bool,
        slippage_bps: Decimal,
        /// The market price the notional caps were measured against.
        reference_px: Decimal,
        /// What the slippage was measured against: the market reference for
        /// a limit order, the order's own trigger for a stop.
        slippage_reference_px: Decimal,
        /// Item 19 puts a cloid on everything and makes query-by-cloid the
        /// only safe move after `timeout_unknown_outcome`; item 9 reconciles
        /// `frontendOpenOrders` and `orderStatus` by it. Without it here the
        /// ledger row saying why an order was allowed cannot be joined to the
        /// fill it produced. `None` only when the caller supplied none.
        cloid: Option<Cloid>,
        /// The book snapshot the decision was taken against
        /// (`docs/decisions.md` R6). Nullable until a capture policy exists;
        /// the hash is what the chained ledger row commits to.
        snapshot_id: Option<String>,
        snapshot_hash: Option<String>,
    },
    /// Risk-reducing, so it clears while the kill switch is engaged.
    Cancel { count: usize },
    /// Agent-requested cancellation, never the runtime cleanup exemption.
    DiscretionaryCancel {
        targets: Vec<CancelTarget>,
        observed_at_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        provenance: Option<super::CancelProvenance>,
    },
    /// The dead-man's switch (spec item 27). `None` disarms.
    ScheduleCancel { cancel_at_ms: Option<u64> },
}

/// The audit record of one successful evaluation.
///
/// Written to the ledger before the clearance is handed back, and returned
/// alongside the signed request by [`super::sign_cleared`], so the row that
/// says why an order was allowed and the request that was signed are the same
/// evaluation (D6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Clearance {
    /// Set only by retained-review confirmation after exact action comparison.
    /// Omitted for legacy/direct evaluations and policy-exempt cleanup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_review_digest: Option<String>,
    /// Whose container this was evaluated against. **Every** clearance names
    /// one, including the dead-man's switch: spec item 27 arms
    /// `scheduleCancel` *per container*, "N times, not once", and an
    /// agent-less clearance is a leftover of the pre-revision model where one
    /// master account spoke for the fleet.
    ///
    /// It is also the identity [`GuardrailEngine::sign_cleared`] loads the
    /// signing key from. The route separately identifies the container and
    /// the API wallet approved to sign for it; those addresses are not equal
    /// merely because a top-level account omits `vaultAddress`.
    pub agent: AgentId,
    /// Complete evaluated authority, revalidated under the signing permit.
    pub route: AuthorizedRoute,
    /// Authenticated policy event sequence. Zero only for policy-exempt cleanup.
    pub policy_revision: u64,
    /// The container's `vaultAddress` where the venue granted a sub-account
    /// (D1), copied from the authenticated route rather than from the caller.
    /// `None` is D1 V2's top-level container, which sends no `vaultAddress`
    /// at all. A clearance measured against agent X's positions and caps must
    /// not be signable with agent Y's `vaultAddress`, and the only way to
    /// guarantee that is for the binding to travel with the clearance.
    pub vault_address: Option<Address>,
    /// The network the engine that produced this is bound to (R4). A testnet
    /// clearance signed for mainnet is what R4 calls the worst bug this
    /// product can ship, so the network is not a parameter of signing.
    pub network: Network,
    pub evaluated_at_ms: u64,
    pub kind: ClearedKind,
    pub utilization: Utilization,
}

/// Proof that the guardrail engine evaluated this action and allowed it.
///
/// The only constructor is private to this file and is called only from the
/// success branch of [`GuardrailEngine::decide`]. Deliberately **not**
/// `Clone`: a clearance is spent by the signature it authorises, so it cannot
/// be replayed into a second order.
#[derive(Debug)]
pub struct Cleared {
    feed_stamp: Option<crate::feed::FeedStamp>,
    action: Action,
    clearance: Clearance,
    approval_deadline_ms: Option<u64>,
}

impl Cleared {
    /// Private on purpose. Moving this line, widening it to `pub(crate)`, or
    /// adding a second constructor breaks `AGENTS.md` invariant 1.
    fn new(action: Action, clearance: Clearance) -> Self {
        Cleared {
            feed_stamp: None,
            action,
            clearance,
            approval_deadline_ms: None,
        }
    }

    /// The exact action that was evaluated. Built by the engine from the
    /// rounded price and size the predicates ran against, never from
    /// caller-supplied bytes, so what was checked and what gets signed cannot
    /// drift apart.
    ///
    /// Test-only on purpose. `Action` is `Clone`, so a public accessor would
    /// offer exactly the shape the by-value [`super::sign_cleared`] exists to
    /// prevent: clone the action out of a clearance and sign it as many times
    /// as you like. Nothing outside the tests needs it — a caller that wants
    /// the action after signing reads it off the `ExchangeRequest`.
    #[cfg(test)]
    pub(crate) fn action(&self) -> &Action {
        &self.action
    }

    pub fn clearance(&self) -> &Clearance {
        &self.clearance
    }

    pub(crate) fn matches_reviewed_action(&self, action: &Action) -> bool {
        &self.action == action
    }

    fn into_parts(
        self,
    ) -> (
        Action,
        Clearance,
        Option<u64>,
        Option<crate::feed::FeedStamp>,
    ) {
        (
            self.action,
            self.clearance,
            self.approval_deadline_ms,
            self.feed_stamp,
        )
    }
}

/// A ledger write failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct AuditError {
    pub detail: String,
}

/// The verdict, for the audit record.
#[derive(Debug)]
#[non_exhaustive]
pub enum AuditOutcome<'a> {
    Cleared(&'a Clearance),
    Refused(&'a Refusal),
    /// An operator changed the rules rather than an agent trying to act
    /// under them. Item 18's taxonomy names guardrail trips, approval
    /// decisions and kill-switch changes as events; without this row an
    /// export cannot answer why an order refused yesterday cleared today,
    /// which is the question the ledger exists for.
    Operator(&'a OperatorAction),
}

/// One operator mutation, with enough of the before and after state that the
/// row explains the change rather than merely noting one happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "operator_action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OperatorAction {
    /// Operator claim only; a racing stop can still reject this request.
    PolicyAcknowledgmentRequested { revision: u64, stop_generation: u64 },
    /// A newly paired agent got D-c's near-zero defaults.
    AgentRegistered {
        config: Box<AgentGuardrails>,
        /// Historical field; policy registration grants no route and emits None.
        vault_address: Option<Address>,
    },
    /// `before` is `None` when the agent had no stored configuration.
    GuardrailsChanged {
        before: Option<Box<AgentGuardrails>>,
        after: Box<AgentGuardrails>,
    },
    AccountLimitsChanged {
        before: LossLimits,
        after: LossLimits,
    },
    GlobalRateBudgetChanged {
        before: GlobalRateBudget,
        after: GlobalRateBudget,
    },
    KillEngaged {
        scope: KillScope,
        reason: KillReason,
        /// False when the scope was already engaged, so a repeated press is
        /// still recorded but is distinguishable from the trip that stopped
        /// trading.
        newly_engaged: bool,
        cancel_for: BTreeSet<AgentId>,
    },
    KillReleased {
        scope: KillScope,
        /// False when nothing was engaged in that scope.
        was_engaged: bool,
    },
    /// Item 18: approval decisions are events.
    ProposalRejected { approval_id: String },
}

/// One row for the append-only ledger.
#[derive(Debug)]
pub struct AuditEntry<'a> {
    /// `None` only for a fleet-scoped operator mutation — an account-wide
    /// limit, a global kill. Every *clearance* names an agent
    /// ([`Clearance::agent`]); this is `Option` for the operator rows beside
    /// them.
    pub agent: Option<&'a AgentId>,
    pub at_ms: u64,
    /// The agent's own words. Untrusted (item 30).
    pub reason: &'a str,
    pub outcome: AuditOutcome<'a>,
}

/// Opaque authority retained until signing finishes. The ledger implementation
/// holds its coordination lock; test sinks may use the unit implementation.
pub trait SigningPermit {
    fn publish_submission(
        &mut self,
        _evidence: &GuardedSignature<'_>,
    ) -> Result<Appended, Refusal> {
        Err(submission_refusal(
            "signing permit cannot publish submission evidence",
        ))
    }
    fn validate_submission(&self, _submission: &SignedSubmission) -> Result<(), Refusal> {
        Err(submission_refusal(
            "signing permit cannot verify submission evidence",
        ))
    }
}

/// Borrowed proof constructed only after successful guarded crypto. Not a
/// caller-supplied request certification API.
pub struct GuardedSignature<'a> {
    journal: &'a SubmissionJournal,
    receipt: &'a SubmissionReceipt,
    request: &'a ExchangeRequest,
    clearance: &'a Clearance,
    wallet: &'a AgentWallet,
    signer: Address,
    signed_at_ms: u64,
}

impl GuardedSignature<'_> {
    pub(crate) fn journal(&self) -> &SubmissionJournal {
        self.journal
    }
    pub(crate) fn receipt(&self) -> &SubmissionReceipt {
        self.receipt
    }
    pub(crate) fn request(&self) -> &ExchangeRequest {
        self.request
    }
    pub(crate) fn clearance(&self) -> &Clearance {
        self.clearance
    }
    pub(crate) fn wallet(&self) -> &AgentWallet {
        self.wallet
    }
    pub(crate) fn signer(&self) -> Address {
        self.signer
    }
    pub(crate) fn signed_at_ms(&self) -> u64 {
        self.signed_at_ms
    }
}

/// One in-memory transport capability. The ledger stores its digest, never
/// executable request bytes. Dropping this does not release a reservation.
pub struct SignedSubmission {
    feed_stamp: Option<crate::feed::FeedStamp>,
    owner: Arc<()>,
    journal: SubmissionJournal,
    receipt: SubmissionReceipt,
    signed: Appended,
    request: ExchangeRequest,
    clearance: Clearance,
    deadline: Option<u64>,
    wallet: AgentWallet,
    signer: Address,
}

/// One engine-owned discretionary cancellation; no public request extraction.
pub struct SignedCancellation {
    owner: Arc<()>,
    request: ExchangeRequest,
    clearance: Clearance,
    deadline: Option<u64>,
    wallet: AgentWallet,
    signer: Address,
}

struct Dispatch<'a> {
    owner: &'a Arc<()>,
    request: &'a ExchangeRequest,
    clearance: &'a Clearance,
    deadline: Option<u64>,
    wallet: &'a AgentWallet,
    signer: Address,
    submission: Option<&'a SignedSubmission>,
}

impl SignedSubmission {
    pub(crate) fn validate_in(
        &self,
        connection: &rusqlite::Connection,
        registry: &crate::ledger::RegistryJournal,
    ) -> Result<(), Refusal> {
        self.journal
            .verify_owner(registry)
            .map_err(submission_refusal)?;
        self.journal
            .verify_signed_in(
                connection,
                &self.receipt,
                &self.signed,
                &self.request,
                &self.clearance,
            )
            .map_err(submission_refusal)
    }
}

type SignedParts = (
    ExchangeRequest,
    Clearance,
    Option<Appended>,
    AgentWallet,
    Address,
);

#[derive(Debug, thiserror::Error)]
pub enum SubmissionPostError {
    #[error("submission not dispatched: {0}")]
    NotSent(Refusal),
    #[error(transparent)]
    Transport(oppen_hl::Error),
    #[error("submission result could not be durably recorded: {0}")]
    JournalUncertain(SubmissionError),
}

fn submission_refusal(error: impl std::fmt::Display) -> Refusal {
    Unevaluable::SubmissionAuthority {
        detail: error.to_string(),
    }
    .into()
}

#[cfg(test)]
impl SigningPermit for () {}

/// Where evaluations are recorded.
///
/// D6 makes the hash-chained SQLite ledger the single source for
/// `get_events`, the activity stream and the audit export, so the ledger
/// module implements this. It is a trait here so the guardrail engine does
/// not depend on the ledger's schema, and so a failing write can be injected
/// in a test — [`Unevaluable::AuditWriteFailed`] is a fail-closed path and
/// the only honest way to test it is to make a write fail.
pub trait AuditSink: Send + Sync {
    fn record(&self, entry: &AuditEntry<'_>) -> Result<(), AuditError>;
    /// Production approval dispositions require the actual committed intent receipt.
    fn record_with_receipt(
        &self,
        entry: &AuditEntry<'_>,
    ) -> Result<Option<crate::ledger::Appended>, AuditError> {
        self.record(entry)?;
        Ok(None)
    }
    /// Revalidate durable execution authority inside the final signing gate.
    fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal>;
    fn before_sign(
        &self,
        clearance: &Clearance,
        actual_wallet: &AgentWallet,
        actual_signer: Address,
    ) -> Result<Box<dyn SigningPermit + '_>, Refusal>;
}

/// Operator-side failures. Distinct from [`Refusal`], which is what an agent
/// receives: nothing here is ever handed to an agent.
#[derive(Debug, thiserror::Error)]
pub enum GuardrailError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("guardrail config field {field} is invalid: {detail}")]
    InvalidConfig { field: String, detail: String },
    #[error("policy requires operator reconciliation: {detail}")]
    Policy { detail: String },
    #[error("approval authority unavailable: {detail}")]
    Approval { detail: String },
}

/// The exact verified policy and local stop evidence reviewed by the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PolicyAcknowledgment {
    pub revision: u64,
    pub stop_generation: u64,
}

/// Local HALT has already taken effect; only its durable work remains.
#[derive(Debug)]
pub struct PendingOperatorKill {
    owner: Arc<()>,
    incarnation: Arc<()>,
    requested_reason: KillReason,
    effect: KillEffect,
    engagement: Engagement,
    generation: u64,
    at_ms: u64,
}

impl PendingOperatorKill {
    pub fn effect(&self) -> &KillEffect {
        &self.effect
    }
    pub fn stop_generation(&self) -> u64 {
        self.generation
    }
}

/// Local observation only, not a fresh verification or activation guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PolicyStatus {
    pub cached_revision: Option<u64>,
    pub acknowledgment: Option<PolicyAcknowledgment>,
    pub stop_generation: u64,
    /// Other gates, including durable kills and pilot stops, still apply.
    pub admission_inhibited: bool,
}

#[derive(Debug)]
struct EngineState {
    policy_revision: u64,
    acknowledged: Option<PolicyAcknowledgment>,
    activation_scope: Option<AuthorizedRoute>,
    stop_generation: u64,
    emergency: BTreeMap<KillScope, Engagement>,
    kill_incarnations: BTreeMap<KillScope, Arc<()>>,
    guardrails: BTreeMap<AgentId, AgentGuardrails>,
    account_limits: LossLimits,
    kill: KillSwitch,
    buckets: BTreeMap<AgentId, TokenBucket>,
    active: BTreeSet<AgentId>,
    /// Spec item 28. `BTreeMap` so `pending_proposals` has one order
    /// (`AGENTS.md` invariant 6).
    proposals: BTreeMap<String, Proposal>,
    /// Monotonic, so two proposals minted in the same millisecond still get
    /// distinct ids.
    proposal_seq: u64,
    /// Spec item 10's address-wide budget, shared by every agent.
    global_budget: GlobalRateBudget,
    global_bucket: TokenBucket,
    /// [`KillEffect`]s produced by a circuit-breaker trip, waiting for the
    /// caller to drain them with
    /// [`GuardrailEngine::take_pending_kill_effects`].
    ///
    /// Spec item 26 makes cancelling resting orders part of what engaging the
    /// switch *does*, and item 25's whole point is that an agent grinding the
    /// account down overnight has to be stopped — leaving its working orders
    /// live is the failure mode. The breaker engages the switch from inside
    /// an evaluation, which returns a [`Refusal`], and a refusal is the wrong
    /// place to name a fleet-wide cancel set: it goes to one agent, and the
    /// roster is not that agent's business. So the effect is queued instead.
    ///
    /// Bounded: the switch is idempotent and only a *newly* engaged scope
    /// queues, so this holds at most one entry per scope.
    pending_effects: Vec<KillEffect>,
}

impl EngineState {
    fn inhibit(&mut self) {
        self.acknowledged = None;
        self.activation_scope = None;
        // Exhaustion is permanently inhibited, never an ABA generation wrap.
        self.stop_generation = self.stop_generation.saturating_add(1);
    }

    fn policy(&self) -> PersistedState {
        PersistedState {
            guardrails: self.guardrails.clone(),
            kill: self.kill.clone(),
            account_limits: self.account_limits,
        }
    }

    fn publish(&mut self, version: PolicyVersion) -> Result<(), GuardrailError> {
        if version.revision == 0 || version.revision < self.policy_revision {
            return Err(GuardrailError::Policy {
                detail: "policy observation regressed".into(),
            });
        }
        if version.revision == self.policy_revision && version.state != self.policy() {
            return Err(GuardrailError::Policy {
                detail: "policy changed without a revision".into(),
            });
        }
        self.policy_revision = version.revision;
        self.guardrails = version.state.guardrails;
        self.account_limits = version.state.account_limits;
        self.kill = version.state.kill;
        Ok(())
    }

    fn effective_kill(&self) -> KillSwitch {
        let mut kill = self.kill.clone();
        for (scope, engagement) in &self.emergency {
            kill.engage(scope.clone(), engagement.clone());
        }
        kill
    }

    fn check_acknowledgment(&self) -> Result<(), Refusal> {
        if self.policy_revision == 0
            || self.acknowledged
                != Some(PolicyAcknowledgment {
                    revision: self.policy_revision,
                    stop_generation: self.stop_generation,
                })
        {
            return Err(Unevaluable::PolicyAuthority {
                detail: "operator reconciliation acknowledgment required".into(),
            }
            .into());
        }
        Ok(())
    }

    fn check_scoped_acknowledgment(
        &self,
        route: &AuthorizedRoute,
        supervised: bool,
    ) -> Result<(), Refusal> {
        self.check_acknowledgment()?;
        if (supervised || self.activation_scope.is_some())
            && self.activation_scope.as_ref() != Some(route)
        {
            return Err(Unevaluable::PolicyAuthority {
                detail: "acknowledgment does not cover this route".into(),
            }
            .into());
        }
        Ok(())
    }

    fn stop(&mut self, scope: KillScope, engagement: Engagement) -> KillEffect {
        let newly_engaged = self
            .effective_kill()
            .engage(scope.clone(), engagement.clone());
        self.emergency.entry(scope.clone()).or_insert(engagement);
        self.inhibit();
        let effect = KillEffect {
            cancel_for: cancel_targets(self, &scope),
            scope,
            newly_engaged,
        };
        if newly_engaged && !self.pending_effects.iter().any(|e| e.scope == effect.scope) {
            self.pending_effects.push(effect.clone());
        }
        effect
    }

    /// The agent's bucket, rebuilt from scratch if the operator changed the
    /// rate. A rate change restores a full bucket, which is the generous
    /// reading; the alternative is that lowering a rate retroactively
    /// overdraws an agent that had already spent under the old one.
    fn bucket_mut(&mut self, agent: &AgentId, rate: OrderRate, now_ms: u64) -> &mut TokenBucket {
        let bucket = self
            .buckets
            .entry(agent.clone())
            .or_insert_with(|| TokenBucket::new(rate, now_ms));
        if bucket.rate() != rate {
            *bucket = TokenBucket::new(rate, now_ms);
        }
        bucket
    }

    /// Drops proposals whose TTL has passed (spec item 28). Swept lazily on
    /// every mint and lookup rather than on a timer, so `oppen-core` stays
    /// free of a scheduler (R1).
    fn sweep_proposals(&mut self, now_ms: u64) {
        self.proposals.retain(|_, p| !p.is_expired(now_ms));
    }
}

/// The guardrail engine: one per network, shared by every caller that can
/// reach the signer.
///
/// `Arc<dyn …>` rather than generics because the desktop app, the MCP
/// gateway and the workflow runner all hold the same instance, and a type
/// parameter would leak into every one of their signatures.
pub struct GuardrailEngine {
    activation_authority: Option<Arc<PolicyJournal>>,
    supervised_alpha: bool,
    feed: Arc<crate::feed::FeedSession>,
    submission_owner: Arc<()>,
    submissions: Option<crate::ledger::SubmissionJournal>,
    approvals: Option<ApprovalJournal>,
    store: Arc<dyn GuardrailStore>,
    sink: Arc<dyn AuditSink>,
    /// Where the agent wallets live. Held by the engine rather than passed to
    /// [`GuardrailEngine::sign_cleared`] for the same reason the network is:
    /// a key supplied per call is a key the caller can get wrong, and on a
    /// top-level container (D1 V2) the approved API wallet determines the
    /// account at the venue. Its address must match the registry grant.
    keys: Arc<dyn KeyStore>,
    /// R4: one engine per network, and every clearance it produces carries
    /// this. Fixed at construction because there is no operator gesture that
    /// should move a running engine from testnet to mainnet — switching
    /// networks means a different database file and a different engine.
    network: Network,
    state: Mutex<EngineState>,
    mutations: Mutex<()>,
}

impl std::fmt::Debug for GuardrailEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardrailEngine")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl GuardrailEngine {
    /// Construct one-source policy and signing authority. Opening never acknowledges
    /// policy: even a verified restart starts order-inhibited (ES18).
    pub fn new(
        authority: Arc<PolicyJournal>,
        keys: Arc<dyn KeyStore>,
        feed: Arc<crate::feed::FeedSession>,
    ) -> Result<Self, GuardrailError> {
        let network = authority.network();
        let submissions = authority.submissions(false);
        let approvals = ApprovalJournal::new(authority.clone());
        let mut engine = Self::build(
            Arc::new(SqliteGuardrailStore::new(authority.clone())),
            Arc::new(LedgerAuditSink::new(authority.clone())),
            keys,
            network,
            feed,
        )?;
        engine.submissions = Some(submissions);
        engine.approvals = Some(approvals);
        engine.activation_authority = Some(authority);
        Ok(engine)
    }

    /// Supervised alpha always requires authenticated pilot consent for orders,
    /// including reduce-only orders. Missing consent never disables this check.
    pub fn new_supervised_alpha(
        authority: Arc<PolicyJournal>,
        keys: Arc<dyn KeyStore>,
        feed: Arc<crate::feed::FeedSession>,
    ) -> Result<Self, GuardrailError> {
        let network = authority.network();
        if network != Network::Testnet {
            return Err(GuardrailError::InvalidConfig {
                field: "network".into(),
                detail: "supervised alpha requires testnet".into(),
            });
        }
        let submissions = authority.submissions(true);
        let approvals = ApprovalJournal::new(authority.clone());
        let mut engine = Self::build(
            Arc::new(SqliteGuardrailStore::new(authority.clone())),
            Arc::new(LedgerAuditSink::supervised(authority.clone())),
            keys,
            network,
            feed,
        )?;
        engine.submissions = Some(submissions);
        engine.approvals = Some(approvals);
        engine.activation_authority = Some(authority);
        engine.supervised_alpha = true;
        Ok(engine)
    }

    /// Reservations inherit the exact authority and pilot requirement of signing.
    pub fn submissions(&self) -> Result<crate::ledger::SubmissionJournal, GuardrailError> {
        self.submissions
            .clone()
            .ok_or_else(|| GuardrailError::Policy {
                detail: "synthetic engine has no durable submission authority".into(),
            })
    }

    /// Explicit synthetic authority and persistence seams, never a production constructor.
    #[cfg(test)]
    pub(crate) fn from_parts(
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
        keys: Arc<dyn KeyStore>,
        network: Network,
        feed: Arc<crate::feed::FeedSession>,
    ) -> Result<Self, GuardrailError> {
        Self::build(store, sink, keys, network, feed)
    }

    fn build(
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
        keys: Arc<dyn KeyStore>,
        network: Network,
        feed: Arc<crate::feed::FeedSession>,
    ) -> Result<Self, GuardrailError> {
        if keys.network() != network {
            return Err(GuardrailError::InvalidConfig {
                field: "keys.network".to_owned(),
                detail: format!(
                    "the key store holds {:?} wallets and this engine is bound to {network:?}",
                    keys.network()
                ),
            });
        }
        let global_budget = GlobalRateBudget::default();
        let mut state = EngineState {
            policy_revision: 0,
            acknowledged: None,
            activation_scope: None,
            stop_generation: 0,
            emergency: BTreeMap::new(),
            kill_incarnations: BTreeMap::new(),
            guardrails: BTreeMap::new(),
            account_limits: LossLimits::UNSET,
            kill: KillSwitch::new(),
            buckets: BTreeMap::new(),
            active: BTreeSet::new(),
            proposals: BTreeMap::new(),
            proposal_seq: 0,
            global_budget,
            global_bucket: TokenBucket::new(global_budget.rate, 0),
            pending_effects: Vec::new(),
        };
        // These empty projections are not policy authority. A failed load must
        // still leave registry-authenticated cleanup usable.
        if let Ok(version) = store.load() {
            let _ = state.publish(version);
        }
        Ok(Self {
            activation_authority: None,
            supervised_alpha: false,
            feed,
            submissions: None,
            submission_owner: Arc::new(()),
            approvals: None,
            store,
            sink,
            keys,
            network,
            state: Mutex::new(state),
            mutations: Mutex::new(()),
        })
    }

    /// The exact live session used by every order signing and dispatch gate.
    pub fn feed(&self) -> Arc<crate::feed::FeedSession> {
        self.feed.clone()
    }

    fn mutation_lock(&self) -> Result<MutexGuard<'_, ()>, GuardrailError> {
        self.mutations.lock().map_err(|_| {
            self.state().inhibit();
            GuardrailError::Policy {
                detail: "policy mutation lock poisoned".into(),
            }
        })
    }

    fn refresh_policy(&self) -> Result<(), GuardrailError> {
        let version = match self.store.load() {
            Ok(version) => version,
            Err(error) => {
                self.state().inhibit();
                return Err(error.into());
            }
        };
        let mut state = self.state();
        if let Err(error) = state.publish(version) {
            state.inhibit();
            return Err(error);
        }
        Ok(())
    }

    /// Read verified policy and the stop generation for explicit operator review.
    /// This performs synchronous ledger I/O and does not enable orders.
    pub fn policy_observation(&self) -> Result<PolicyAcknowledgment, GuardrailError> {
        self.refresh_policy()?;
        let state = self.state();
        Ok(PolicyAcknowledgment {
            revision: state.policy_revision,
            stop_generation: state.stop_generation,
        })
    }

    /// Cached status for read-only runtime views. No ledger I/O, refresh, or
    /// acknowledgment. Independent policy changes are detected by verified
    /// reads and the final signing permit, not by this local observation.
    pub fn policy_status(&self) -> PolicyStatus {
        let state = self.state();
        PolicyStatus {
            cached_revision: (state.policy_revision != 0).then_some(state.policy_revision),
            acknowledgment: state.acknowledged,
            stop_generation: state.stop_generation,
            admission_inhibited: state.check_acknowledgment().is_err(),
        }
    }

    /// Acknowledge exactly the reviewed policy and local stop evidence. Venue
    /// reconciliation is the operator caller's obligation, not inferred here.
    /// Durable kills and emergency engagements still require explicit release.
    pub fn operator_acknowledge_policy(
        &self,
        observed: PolicyAcknowledgment,
        at_ms: u64,
    ) -> Result<(), GuardrailError> {
        if self.supervised_alpha {
            return Err(GuardrailError::Policy {
                detail: "supervised alpha requires bound activation review".into(),
            });
        }
        let _mutation = self.mutation_lock()?;
        self.refresh_policy()?;
        {
            let mut state = self.state();
            if observed.revision != state.policy_revision
                || observed.stop_generation != state.stop_generation
                || observed.stop_generation == u64::MAX
            {
                state.inhibit();
                return Err(GuardrailError::Policy {
                    detail: "policy or stop generation changed during reconciliation".into(),
                });
            }
            state.acknowledged = None;
        }
        let action = OperatorAction::PolicyAcknowledgmentRequested {
            revision: observed.revision,
            stop_generation: observed.stop_generation,
        };
        if let Err(error) = self.sink.record(&AuditEntry {
            agent: None,
            at_ms,
            reason: "operator acknowledgment request; venue validation is caller-owned",
            outcome: AuditOutcome::Operator(&action),
        }) {
            self.state().inhibit();
            return Err(GuardrailError::Policy {
                detail: format!("acknowledgment audit failed: {error}"),
            });
        }
        let mut state = self.state();
        if observed.revision != state.policy_revision
            || observed.stop_generation != state.stop_generation
            || observed.stop_generation == u64::MAX
        {
            state.inhibit();
            return Err(GuardrailError::Policy {
                detail: "policy or stop generation changed during reconciliation".into(),
            });
        }
        state.acknowledged = Some(observed);
        Ok(())
    }

    /// Caller serializes mutations, but never holds engine state across CAS.
    /// The candidate is tied to its captured revision; no full-state rebasing.
    fn commit_policy(
        &self,
        expected: u64,
        next: &PersistedState,
        at_ms: u64,
    ) -> Result<(), GuardrailError> {
        // A panic or uncertain durable outcome must not retain admission.
        self.state().acknowledged = None;
        let version = match self.store.compare_exchange(expected, next, at_ms) {
            Ok(version) => version,
            Err(error) => {
                self.state().inhibit();
                return Err(error.into());
            }
        };
        let mut state = self.state();
        if let Err(error) = state.publish(version) {
            state.inhibit();
            return Err(error);
        }
        Ok(())
    }

    fn policy_candidate(&self) -> Result<(u64, PersistedState), GuardrailError> {
        let state = self.state();
        if state.policy_revision == 0 {
            return Err(GuardrailError::Policy {
                detail: "verified policy unavailable".into(),
            });
        }
        Ok((state.policy_revision, state.policy()))
    }

    /// A poisoned lock is recovered rather than propagated: the state behind
    /// it is a set of plain values with no partially-applied invariant, and
    /// refusing every order because an unrelated thread panicked would take
    /// the kill switch and the cancel path down with it.
    fn state(&self) -> MutexGuard<'_, EngineState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- operator surface (D3, `AGENTS.md` invariant 3) -----------------
    //
    // Nothing below is reachable from an agent. `oppen-mcp` exposes no tool
    // that calls these; they are operator-only Tauri commands from the
    // console. The naming is deliberate so a review of the MCP surface can
    // grep for it.

    /// Register policy defaults only. Route authority is granted separately.
    pub fn register_agent(
        &self,
        agent: &AgentId,
        now_ms: u64,
    ) -> Result<AgentGuardrails, GuardrailError> {
        let _mutation = self.mutation_lock()?;
        let (revision, mut next) = self.policy_candidate()?;
        if let Some(existing) = next.guardrails.get(agent) {
            return Ok(existing.clone());
        }
        let config = AgentGuardrails::default();
        next.guardrails.insert(agent.clone(), config.clone());
        self.commit_policy(revision, &next, now_ms)?;
        self.record_operator(
            Some(agent),
            now_ms,
            &OperatorAction::AgentRegistered {
                config: Box::new(config.clone()),
                vault_address: None,
            },
        );
        Ok(config)
    }

    /// Replaces one agent's guardrails. Validates first, so a value that
    /// could not be evaluated is rejected when it is typed rather than when
    /// an order needs a verdict.
    pub fn operator_set_guardrails(
        &self,
        agent: &AgentId,
        config: AgentGuardrails,
        now_ms: u64,
    ) -> Result<(), GuardrailError> {
        if let Err((field, detail)) = config.validate() {
            return Err(GuardrailError::InvalidConfig {
                field: field.to_owned(),
                detail,
            });
        }
        let _mutation = self.mutation_lock()?;
        let (revision, mut next) = self.policy_candidate()?;
        let before = next.guardrails.insert(agent.clone(), config.clone());
        self.commit_policy(revision, &next, now_ms)?;
        self.record_operator(
            Some(agent),
            now_ms,
            &OperatorAction::GuardrailsChanged {
                before: before.map(Box::new),
                after: Box::new(config),
            },
        );
        Ok(())
    }

    pub fn operator_set_account_limits(
        &self,
        limits: LossLimits,
        now_ms: u64,
    ) -> Result<(), GuardrailError> {
        let _mutation = self.mutation_lock()?;
        let (revision, mut next) = self.policy_candidate()?;
        let before = std::mem::replace(&mut next.account_limits, limits);
        self.commit_policy(revision, &next, now_ms)?;
        self.record_operator(
            None,
            now_ms,
            &OperatorAction::AccountLimitsChanged {
                before,
                after: limits,
            },
        );
        Ok(())
    }

    /// Sets spec item 10's address-wide request budget, normally from the
    /// venue's own `userRateLimit` after a reconnect.
    ///
    /// Not persisted: the live figure is re-read from the venue, so a stored
    /// copy would only ever be a stale one.
    pub fn operator_set_global_rate_budget(
        &self,
        budget: GlobalRateBudget,
        now_ms: u64,
    ) -> Result<(), GuardrailError> {
        if let Err((field, detail)) = budget.validate() {
            return Err(GuardrailError::InvalidConfig {
                field: field.to_owned(),
                detail,
            });
        }
        let before = {
            let mut state = self.state();
            let before = std::mem::replace(&mut state.global_budget, budget);
            if state.global_bucket.rate() != budget.rate {
                state.global_bucket = TokenBucket::new(budget.rate, now_ms);
            }
            before
        };
        self.record_operator(
            None,
            now_ms,
            &OperatorAction::GlobalRateBudgetChanged {
                before,
                after: budget,
            },
        );
        Ok(())
    }

    /// Engages the kill switch and reports whose resting orders must now be
    /// cancelled (spec item 26). Persisted before it is reported, so an
    /// engagement the operator has been shown is an engagement that survives
    /// a restart.
    pub fn operator_engage_kill(
        &self,
        scope: KillScope,
        reason: KillReason,
        now_ms: u64,
    ) -> Result<KillEffect, GuardrailError> {
        self.persist_operator_kill(self.begin_operator_kill(scope, reason, now_ms))
    }

    /// I/O-free request-boundary HALT, including while earlier durable work waits.
    pub fn begin_operator_kill(
        &self,
        scope: KillScope,
        reason: KillReason,
        now_ms: u64,
    ) -> PendingOperatorKill {
        let requested_reason = reason.clone();
        let engagement = Engagement {
            engaged_at_ms: now_ms,
            reason,
        };
        let mut state = self.state();
        let effect = state.stop(scope.clone(), engagement.clone());
        let engagement = state.emergency.get(&scope).cloned().unwrap_or(engagement);
        let incarnation = state
            .kill_incarnations
            .entry(scope)
            .or_insert_with(|| Arc::new(()))
            .clone();
        PendingOperatorKill {
            owner: self.submission_owner.clone(),
            incarnation,
            requested_reason,
            effect,
            engagement,
            generation: state.stop_generation,
            at_ms: now_ms,
        }
    }

    /// Synchronous persistence; the native owner retains this work through IPC loss.
    pub fn persist_operator_kill(
        &self,
        pending: PendingOperatorKill,
    ) -> Result<KillEffect, GuardrailError> {
        if !Arc::ptr_eq(&pending.owner, &self.submission_owner) {
            return Err(GuardrailError::Policy {
                detail: "pending HALT belongs to another engine".into(),
            });
        }
        let _mutation = self.mutation_lock()?;
        if !self
            .state()
            .kill_incarnations
            .get(&pending.effect.scope)
            .is_some_and(|current| Arc::ptr_eq(current, &pending.incarnation))
        {
            return Err(GuardrailError::Policy {
                detail: "pending HALT was replaced or explicitly released".into(),
            });
        }
        // A preceding release may have committed before reporting uncertainty.
        // Re-read verified policy before this restrictive, one-scope mutation;
        // refresh neither acknowledges policy nor clears emergency overlays.
        self.refresh_policy()?;
        let (revision, mut next) = self.policy_candidate()?;
        next.kill
            .engage(pending.effect.scope.clone(), pending.engagement.clone());
        self.commit_policy(revision, &next, pending.at_ms)?;
        self.record_operator(
            None,
            pending.at_ms,
            &OperatorAction::KillEngaged {
                scope: pending.effect.scope.clone(),
                reason: pending.requested_reason,
                newly_engaged: pending.effect.newly_engaged,
                cancel_for: pending.effect.cancel_for.clone(),
            },
        );
        Ok(pending.effect)
    }

    pub fn operator_release_kill(
        &self,
        scope: &KillScope,
        now_ms: u64,
    ) -> Result<bool, GuardrailError> {
        let _mutation = self.mutation_lock()?;
        let (revision, mut next, generation, had_emergency) = {
            let state = self.state();
            if state.policy_revision == 0 {
                return Err(GuardrailError::Policy {
                    detail: "verified policy unavailable".into(),
                });
            }
            (
                state.policy_revision,
                state.policy(),
                state.stop_generation,
                state.emergency.contains_key(scope),
            )
        };
        let released = next.kill.release(scope) || had_emergency;
        self.commit_policy(revision, &next, now_ms)?;
        {
            let mut state = self.state();
            if state.stop_generation == generation {
                state.emergency.remove(scope);
                state.kill_incarnations.remove(scope);
            } else {
                return Err(GuardrailError::Policy {
                    detail: "new stop arrived while releasing policy".into(),
                });
            }
        }
        self.record_operator(
            None,
            now_ms,
            &OperatorAction::KillReleased {
                scope: scope.clone(),
                was_engaged: released,
            },
        );
        Ok(released)
    }

    /// Every agent the engine knows about, so a `Global` kill effect is
    /// actionable without an id to look up (spec item 26).
    pub fn agents(&self) -> BTreeSet<AgentId> {
        self.state().guardrails.keys().cloned().collect()
    }

    /// Persisted pauses remain actionable after a runtime restart.
    /// Pure local supervision predicate, including startup and failed policy
    /// inhibition even when this agent is absent from the policy projection.
    /// The caller separately validates the bound registry identity.
    pub fn cancellation_needed(&self, agent: &AgentId) -> bool {
        let state = self.state();
        state.check_acknowledgment().is_err() || state.effective_kill().blocking(agent).is_some()
    }

    /// Cached effective pauses. Binding-specific supervision uses
    /// [`Self::cancellation_needed`] so an unavailable policy cannot hide it.
    pub fn paused_agents(&self) -> BTreeSet<AgentId> {
        let state = self.state();
        let kill = state.effective_kill();
        state
            .guardrails
            .keys()
            .filter(|agent| kill.blocking(agent).is_some())
            .cloned()
            .collect()
    }

    /// Takes the cancel effects queued by circuit-breaker trips.
    ///
    /// The caller must drain this after every evaluation and issue the
    /// cancels: item 26 makes cancelling resting orders part of what engaging
    /// the switch does, and a breaker trip engages it.
    pub fn take_pending_kill_effects(&self) -> Vec<KillEffect> {
        std::mem::take(&mut self.state().pending_effects)
    }

    pub fn guardrails(&self, agent: &AgentId) -> Option<AgentGuardrails> {
        self.state().guardrails.get(agent).cloned()
    }

    /// Resolve current registry authority, never a cached policy binding.
    pub fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        let route = self.sink.route_for_agent(agent)?;
        validate_route(&route, agent, self.network)?;
        Ok(route)
    }

    fn decision_route(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        if self.guardrails(agent).is_none() {
            return Err(Unevaluable::UnknownAgent {
                agent: agent.clone(),
            }
            .into());
        }
        self.route_for_agent(agent)
    }

    /// Read back after a restart, so a limit the operator set is one the
    /// risk console can still display (item 25, item 32). The per-agent
    /// equivalent is [`GuardrailEngine::guardrails`].
    pub fn account_limits(&self) -> LossLimits {
        self.state().account_limits
    }

    pub fn kill_switch(&self) -> KillSwitch {
        self.state().effective_kill()
    }

    /// Every loss budget this agent is measured against, as a gauge rather
    /// than as a verdict (`docs/spec.md` spec F, "loss-budget utilization %
    /// as a continuous gauge before the breaker").
    ///
    /// **A read, on the read path.** It takes no [`OrderIntent`], mints no
    /// proposal, spends no rate token and touches no mutable state — so
    /// `get_state` can call it on every poll, and so it cannot become a
    /// second way to reach the signer (`AGENTS.md` invariant 1). What it
    /// reports is [`breaker::check_all`]'s own arithmetic, which is why an
    /// agent watching this dial and an operator reading the kill switch can
    /// never be looking at different numbers.
    ///
    /// Both scopes, always: the account-wide budget of item 25 is shared with
    /// every other container, so an agent at 30% of its own daily budget can
    /// be one bad hour from a fleet-wide kill it had no way to see. Rows come
    /// out agent-before-account and daily-before-drawdown, the order
    /// [`breaker::check_all`] evaluates them in, so the first row that reads
    /// `tripped` is the one that would refuse.
    pub fn loss_budget(
        &self,
        agent: &AgentId,
        exposure: &Exposure,
        now_ms: u64,
    ) -> Vec<LossBudget> {
        let state = self.state();
        let mut rows = match state.guardrails.get(agent) {
            Some(config) => {
                breaker::gauge(BudgetScope::Agent, &config.loss, &exposure.agent, now_ms)
            }
            // An unregistered agent has no guardrails to be measured against.
            // It also cannot place an order, so there is no budget to gauge.
            None => Vec::new(),
        };
        if let Some(fleet) = exposure.fleet.as_ref() {
            rows.extend(breaker::gauge(
                BudgetScope::Account,
                &state.account_limits,
                fleet,
                now_ms,
            ));
        }
        rows
    }

    // ---- item 28: the approval queue -------------------------------------

    /// Proposals still waiting on an operator, for item 16's `get_state`.
    /// Expired ones are swept first, so nothing here is stale.
    pub fn pending_proposals(&self, now_ms: u64) -> Result<Vec<Proposal>, GuardrailError> {
        if let Some(journal) = &self.approvals {
            return journal
                .pending(now_ms)
                .map_err(|error| GuardrailError::Approval {
                    detail: error.to_string(),
                });
        }
        let mut state = self.state();
        state.sweep_proposals(now_ms);
        Ok(state.proposals.values().cloned().collect())
    }

    /// Builds a non-signable review from authenticated pending evidence. No
    /// proposal is claimed and no rate token is spent by preparing a review.
    pub fn operator_prepare_proposal(
        &self,
        id: &str,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
        now_ms: u64,
    ) -> Result<ApprovalReview, Refusal> {
        let journal = self
            .approvals
            .as_ref()
            .ok_or_else(|| approval_refusal("durable approval authority required for review"))?;
        let evidence = journal
            .prepare(id, now_ms)
            .map_err(approval_refusal)?
            .ok_or_else(|| Unevaluable::UnknownProposal {
                approval_id: id.to_owned(),
            })?;
        let proposal = &evidence.proposal;
        let intent = proposal
            .order_intent()
            .ok_or_else(|| approval_refusal("order review requires an order proposal"))?;
        let candidate = super::request::review_candidate(intent, asset, market, exposure)?;
        let evaluated = self.decide(
            &proposal.agent,
            &candidate,
            asset,
            market,
            exposure,
            now_ms,
            Mode::Approved(&proposal.route),
        )?;
        let commitment = ReviewCommitment::new(
            &evidence,
            &candidate,
            evaluated.action.clone(),
            &evaluated.clearance,
            now_ms,
            market.as_of_ms,
        )
        .map_err(approval_refusal)?;
        let ClearedKind::Order {
            px,
            sz,
            notional_usd,
            reference_px,
            ..
        } = evaluated.clearance.kind
        else {
            return Err(approval_refusal(
                "review evaluation did not produce an order",
            ));
        };
        let Action::Order { orders, .. } = &evaluated.action else {
            return Err(approval_refusal(
                "review evaluation did not produce an order action",
            ));
        };
        let [wire] = orders.as_slice() else {
            return Err(approval_refusal("review requires one order"));
        };
        let drift_bps = intent
            .original
            .as_ref()
            .and_then(|original| original.reference_px)
            .map(|original| {
                checked(
                    reference_px
                        .checked_sub(original)
                        .and_then(|delta| delta.checked_div(original))
                        .and_then(|ratio| ratio.checked_mul(BPS)),
                    "review drift",
                )
            })
            .transpose()?;
        let display = OrderApprovalReviewDisplay {
            proposal_id: proposal.id.clone(),
            agent: proposal.agent.clone(),
            account: proposal.account(),
            symbol: intent.symbol.clone(),
            original: intent.original.clone(),
            original_px: intent.px,
            reference_px,
            reference_at_ms: market.as_of_ms,
            drift_bps,
            asset_index: wire.a,
            is_buy: wire.b,
            px,
            sz,
            notional_usd,
            reduce_only: wire.r,
            order_type: wire.t.clone(),
            cloid: candidate
                .cloid
                .clone()
                .ok_or_else(|| approval_refusal("review requires a cloid"))?,
            grouping: candidate.grouping,
            builder: candidate.builder.clone(),
            route: proposal.route.clone(),
            policy_revision: evidence.policy_revision,
            policy_hash: evidence.policy_hash.clone(),
            reviewed_at_ms: now_ms,
            expires_at_ms: proposal.expires_at_ms,
        };
        Ok(ApprovalReview {
            evidence,
            candidate: ProposalIntent::Order(candidate),
            action: evaluated.action,
            commitment,
            display: ApprovalReviewDisplay::Order(display),
        })
    }

    /// Consumes only the retained candidate; callers cannot supply edited fields.
    /// Guard refusals after claiming are terminal and carry actual audit receipts.
    pub fn operator_confirm_review(
        &self,
        review: ApprovalReview,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let ApprovalReview {
            evidence,
            candidate: ProposalIntent::Order(candidate),
            action,
            commitment,
            display: ApprovalReviewDisplay::Order(display),
        } = review
        else {
            return Err(approval_refusal(
                "order confirmation requires an order review",
            ));
        };
        let journal = self.approvals.as_ref().ok_or_else(|| {
            approval_refusal("durable approval authority required for confirmation")
        })?;
        if now_ms >= display.expires_at_ms {
            return Err(Unevaluable::ApprovalExpired {
                expires_at_ms: display.expires_at_ms,
                now_ms,
            }
            .into());
        }
        if now_ms < display.reviewed_at_ms {
            return Err(Unevaluable::ApprovalReviewChanged {
                detail: "review clock moved backwards".into(),
            }
            .into());
        }
        // A close never silently changes size or side. Check against the original
        // proposal, retaining the reviewed quote timestamp in the actual candidate.
        let refreshed = super::request::review_candidate(
            evidence
                .proposal
                .order_intent()
                .ok_or_else(|| approval_refusal("order proposal required"))?,
            asset,
            market,
            exposure,
        )?;
        let mut comparable = refreshed;
        comparable.original = candidate.original.clone();
        if comparable != candidate {
            return Err(Unevaluable::ApprovalReviewChanged {
                detail: "rounded candidate changed".into(),
            }
            .into());
        }
        let review_digest = commitment.digest().map_err(approval_refusal)?;
        let claim = journal
            .claim_review(evidence, commitment, now_ms)
            .map_err(approval_refusal)?
            .ok_or_else(|| Unevaluable::UnknownProposal {
                approval_id: display.proposal_id.clone(),
            })?;
        let outcome = self
            .decide(
                &display.agent,
                &candidate,
                asset,
                market,
                exposure,
                now_ms,
                Mode::Approved(&display.route),
            )
            .and_then(|mut cleared| {
                if cleared.action != action
                    || cleared.clearance.policy_revision != display.policy_revision
                {
                    return Err(Unevaluable::ApprovalReviewChanged {
                        detail: "rounded action or policy changed".into(),
                    }
                    .into());
                }
                cleared.approval_deadline_ms = Some(display.expires_at_ms);
                cleared.clearance.approval_review_digest = Some(review_digest);
                Ok(cleared)
            });
        let receipt = self.record(Some(&display.agent), now_ms, &candidate.reason, &outcome)?;
        journal
            .finish(claim, &outcome, receipt.as_ref(), now_ms)
            .map_err(approval_refusal)?;
        outcome
    }

    /// Evaluates an agent-requested cancellation, never runtime cleanup.
    pub fn evaluate_cancel(
        &self,
        agent: &AgentId,
        intent: &CancelIntent,
        context: &CancelContext,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let outcome = self.decide_discretionary_cancel(agent, intent, context, now_ms, None, false);
        if !matches!(
            &outcome,
            Err(Refusal::Unevaluable(Unevaluable::ApprovalAuthority { .. }))
        ) {
            self.record(Some(agent), now_ms, &intent.reason, &outcome)?;
        }
        outcome
    }

    /// Retains exact observed targets without consuming the proposal or a rate token.
    pub fn operator_prepare_cancel_proposal(
        &self,
        id: &str,
        context: &CancelContext,
        now_ms: u64,
    ) -> Result<ApprovalReview, Refusal> {
        let journal = self
            .approvals
            .as_ref()
            .ok_or_else(|| approval_refusal("durable approval authority required"))?;
        let evidence = journal
            .prepare(id, now_ms)
            .map_err(approval_refusal)?
            .ok_or_else(|| Unevaluable::UnknownProposal {
                approval_id: id.into(),
            })?;
        let proposal = &evidence.proposal;
        let candidate = proposal
            .cancel_intent()
            .ok_or_else(|| approval_refusal("cancellation proposal required"))?
            .clone();
        let cleared = self.decide_discretionary_cancel(
            proposal.agent(),
            &candidate,
            context,
            now_ms,
            Some(&proposal.route),
            true,
        )?;
        if !matches!(&cleared.clearance.kind, ClearedKind::DiscretionaryCancel { provenance: Some(current), .. } if Some(current) == proposal.cancel_provenance.as_ref())
        {
            return Err(submission_refusal(
                "cancellation proposal lacks matching authenticated ownership",
            ));
        }
        let commitment = crate::ledger::approval::ReviewCommitment::new_cancel(
            &evidence,
            &candidate,
            cleared.action.clone(),
            &cleared.clearance,
            now_ms,
            context.observed_at_ms,
        )
        .map_err(approval_refusal)?;
        let display = CancelApprovalReviewDisplay {
            proposal_id: proposal.id().into(),
            agent: proposal.agent().clone(),
            account: proposal.account(),
            targets: candidate.targets.clone(),
            reason: candidate.reason.clone(),
            route: proposal.route.clone(),
            policy_revision: evidence.policy_revision,
            policy_hash: evidence.policy_hash.clone(),
            reviewed_at_ms: now_ms,
            expires_at_ms: proposal.expires_at_ms(),
        };
        Ok(ApprovalReview {
            evidence,
            candidate: ProposalIntent::Cancel(candidate),
            action: cleared.action,
            commitment,
            display: ApprovalReviewDisplay::Cancel(display),
        })
    }

    /// Consumes one retained cancellation review; no target filtering or expansion.
    pub fn operator_confirm_cancel_review(
        &self,
        review: ApprovalReview,
        context: &CancelContext,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let ApprovalReview {
            evidence,
            candidate: ProposalIntent::Cancel(candidate),
            action,
            commitment,
            display: ApprovalReviewDisplay::Cancel(display),
        } = review
        else {
            return Err(approval_refusal(
                "cancellation confirmation requires a cancellation review",
            ));
        };
        if now_ms >= display.expires_at_ms {
            return Err(Unevaluable::ApprovalExpired {
                expires_at_ms: display.expires_at_ms,
                now_ms,
            }
            .into());
        }
        if now_ms < display.reviewed_at_ms {
            return Err(Unevaluable::ClockWentBackwards {
                now_ms,
                last_ms: display.reviewed_at_ms,
            }
            .into());
        }
        let journal = self
            .approvals
            .as_ref()
            .ok_or_else(|| approval_refusal("durable approval authority required"))?;
        let digest = commitment.digest().map_err(approval_refusal)?;
        let expected_provenance = evidence.proposal.cancel_provenance.clone();
        let claim = journal
            .claim_review(evidence, commitment, now_ms)
            .map_err(approval_refusal)?
            .ok_or_else(|| Unevaluable::UnknownProposal {
                approval_id: display.proposal_id.clone(),
            })?;
        let outcome = self
            .decide_discretionary_cancel(
                &display.agent,
                &candidate,
                context,
                now_ms,
                Some(&display.route),
                false,
            )
            .and_then(|mut cleared| {
                if cleared.action != action
                    || cleared.clearance.policy_revision != display.policy_revision
                    || !matches!(&cleared.clearance.kind, ClearedKind::DiscretionaryCancel { provenance: Some(current), .. } if Some(current) == expected_provenance.as_ref())
                {
                    return Err(Unevaluable::ApprovalReviewChanged {
                        detail: "cancellation action or policy changed".into(),
                    }
                    .into());
                }
                cleared.approval_deadline_ms = Some(display.expires_at_ms);
                cleared.clearance.approval_review_digest = Some(digest);
                Ok(cleared)
            });
        let receipt = self.record(Some(&display.agent), now_ms, &candidate.reason, &outcome)?;
        journal
            .finish(claim, &outcome, receipt.as_ref(), now_ms)
            .map_err(approval_refusal)?;
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn decide_discretionary_cancel(
        &self,
        agent: &AgentId,
        intent: &CancelIntent,
        context: &CancelContext,
        now_ms: u64,
        reviewed_route: Option<&AuthorizedRoute>,
        preparing: bool,
    ) -> Result<Cleared, Refusal> {
        check_reason(&intent.reason)?;
        self.refresh_policy()
            .map_err(|error| Unevaluable::PolicyAuthority {
                detail: error.to_string(),
            })?;
        let route = self.decision_route(agent)?;
        if reviewed_route.is_some_and(|expected| expected != &route) {
            return Err(route_refusal("cancellation proposal route changed"));
        }
        let (config, policy_revision) =
            {
                let state = self.state();
                let config = state.guardrails.get(agent).cloned().ok_or_else(|| {
                    Unevaluable::UnknownAgent {
                        agent: agent.clone(),
                    }
                })?;
                (config, state.policy_revision)
            };
        if let Err((field, detail)) = config.validate() {
            return Err(Unevaluable::InvalidGuardrailConfig {
                field: field.into(),
                detail,
            }
            .into());
        }
        intent.check_context(
            context,
            route.binding.container,
            config.freshness.max_account_age_ms,
            now_ms,
        )?;
        let provenance = self
            .submissions()
            .map_err(submission_refusal)?
            .cancellation_ownership(&route, &intent.targets)
            .map_err(submission_refusal)?;
        let mut state = self.state();
        if state.policy_revision != policy_revision {
            return Err(Unevaluable::PolicyChanged.into());
        }
        if config.approval_required && reviewed_route.is_none() {
            drop(state);
            let journal = self.approvals.as_ref().ok_or_else(|| {
                approval_refusal("durable cancellation approval authority required")
            })?;
            let proposal = journal
                .mint_cancel(
                    agent.clone(),
                    intent.clone(),
                    route,
                    policy_revision,
                    now_ms,
                )
                .map_err(approval_refusal)?;
            return Err(Refusal::CancellationApprovalRequired {
                approval_id: proposal.id().into(),
                expires_at_ms: proposal.expires_at_ms(),
                targets: intent.targets.clone(),
            });
        }
        let remaining = if preparing {
            Decimal::ZERO
        } else {
            draw_global_reserve(&mut state.global_bucket, now_ms)
        };
        Ok(Cleared::new(
            intent.action(),
            Clearance {
                agent: agent.clone(),
                network: self.network,
                vault_address: route.binding.vault_address,
                route,
                policy_revision,
                approval_review_digest: None,
                evaluated_at_ms: now_ms,
                kind: ClearedKind::DiscretionaryCancel {
                    targets: intent.targets.clone(),
                    observed_at_ms: context.observed_at_ms,
                    provenance: Some(provenance),
                },
                utilization: Utilization::none_with_global(remaining),
            },
        ))
    }

    /// Legacy direct approval of the retained normalized intent, without repricing
    /// or a reviewed-candidate commitment. Re-evaluates every hard guard and
    /// consumes the proposal once; its rate token was already spent at minting.
    /// Native review/confirmation uses the separate retained-review APIs above.
    pub fn operator_approve_proposal(
        &self,
        approval_id: &str,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        // **Taken, not read.** The lookup and the removal used to sit under
        // separate locks with a full evaluation and a ledger write between
        // them, so two callers approving one id both walked away with a
        // `Cleared` — and `Mode::Approved` charges neither of them an
        // order-rate token, so one proposal minted two signable orders past a
        // cap of one. One proposal is one approval. A refused one is not
        // re-queued: a proposal is a snapshot of an intent, and re-clicking it
        // against a market that has moved is what item 28's re-pricing exists
        // to prevent.
        let claim = self
            .approvals
            .as_ref()
            .map(|journal| journal.claim(approval_id, now_ms))
            .transpose()
            .map_err(approval_refusal)?;
        let proposal = if let Some(claim) = &claim {
            claim
                .as_ref()
                .ok_or_else(|| Unevaluable::UnknownProposal {
                    approval_id: approval_id.to_owned(),
                })?
                .proposal()
                .clone()
        } else {
            let mut state = self.state();
            state.sweep_proposals(now_ms);
            state
                .proposals
                .remove(approval_id)
                .ok_or_else(|| Unevaluable::UnknownProposal {
                    approval_id: approval_id.to_owned(),
                })?
        };
        let intent = proposal
            .order_intent()
            .ok_or_else(|| approval_refusal("direct approval requires an order proposal"))?;
        let outcome = self
            .decide(
                &proposal.agent,
                intent,
                asset,
                market,
                exposure,
                now_ms,
                Mode::Approved(&proposal.route),
            )
            .map(|mut cleared| {
                cleared.approval_deadline_ms = Some(proposal.expires_at_ms);
                cleared
            });
        let receipt = self.record(Some(&proposal.agent), now_ms, &intent.reason, &outcome)?;
        if let (Some(journal), Some(Some(claim))) = (&self.approvals, claim) {
            if receipt.is_none() {
                return Err(approval_refusal(
                    "approval evaluation audit receipt unavailable",
                ));
            }
            journal
                .finish(claim, &outcome, receipt.as_ref(), now_ms)
                .map_err(approval_refusal)?;
        }
        outcome
    }

    /// Rejects a proposal. Item 18 makes approval decisions ledger events, so
    /// this is recorded even though nothing is signed.
    pub fn operator_reject_proposal(
        &self,
        approval_id: &str,
        now_ms: u64,
    ) -> Result<bool, GuardrailError> {
        if let Some(journal) = &self.approvals {
            return journal
                .reject(approval_id, now_ms)
                .map_err(|error| GuardrailError::Approval {
                    detail: error.to_string(),
                });
        }
        let removed = self.state().proposals.remove(approval_id).is_some();
        if removed {
            self.sink
                .record(&AuditEntry {
                    agent: None,
                    at_ms: now_ms,
                    reason: "operator",
                    outcome: AuditOutcome::Operator(&OperatorAction::ProposalRejected {
                        approval_id: approval_id.to_owned(),
                    }),
                })
                .map_err(|error| GuardrailError::Approval {
                    detail: error.to_string(),
                })?;
        }
        Ok(removed)
    }

    /// Marks an agent as connected or gone, which is what the dead-man's
    /// switch keys off (spec item 27).
    pub fn set_agent_active(&self, agent: &AgentId, active: bool) {
        let mut state = self.state();
        if active {
            state.active.insert(agent.clone());
        } else {
            state.active.remove(agent);
        }
    }

    pub fn active_agents(&self) -> usize {
        self.state().active.len()
    }

    /// Whether `scheduleCancel` should be armed for **this agent's container**
    /// right now (spec item 27).
    ///
    /// Per container, not per fleet. Item 27 is explicit that N containers are
    /// N independent arming duties with N separate ten-trigger daily budgets,
    /// and that "no container is covered by another's arming": an unarmed
    /// container is unprotected however many of its siblings are armed. A
    /// fleet-wide answer would arm one address and report the whole roster
    /// covered.
    pub fn dead_man_intent(
        &self,
        agent: &AgentId,
        now_ms: u64,
        armed_until_ms: Option<u64>,
    ) -> DeadManIntent {
        super::deadman::evaluate(now_ms, self.state().active.contains(agent), armed_until_ms)
    }

    // ---- the signing path ------------------------------------------------

    /// Evaluates one order against every guardrail and, if all of them pass,
    /// returns the only value [`super::sign_cleared`] accepts.
    ///
    /// Predicate order is deliberate. Cheap and unconditional checks run
    /// first so a paused agent is told it is paused without needing fresh
    /// market data; inputs are validated before anything is measured against
    /// them, so an unevaluable input never masquerades as a passing limit;
    /// the venue's own rounding runs before the notional caps, so the caps
    /// are measured on the numbers that would actually be signed; and the
    /// order-rate token is spent last, so a refused order costs an agent
    /// nothing.
    ///
    /// Fail-closed throughout: every input the engine cannot establish is a
    /// refusal, because uncertainty about whether a limit is breached is
    /// treated as a breach.
    pub fn evaluate(
        &self,
        agent: &AgentId,
        intent: &OrderIntent,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let outcome = self.decide(agent, intent, asset, market, exposure, now_ms, Mode::Fresh);
        // A journal error may follow COMMIT with an unpublished head. Do not
        // extend that uncertain tail with a best-effort refusal audit.
        if matches!(
            &outcome,
            Err(Refusal::Unevaluable(Unevaluable::ApprovalAuthority { .. }))
        ) {
            return outcome;
        }
        self.record(Some(agent), now_ms, &intent.reason, &outcome)?;
        outcome
    }

    /// Spec item 20: the same verdict, none of the effects.
    ///
    /// Runs every predicate [`GuardrailEngine::evaluate`] runs, in the same
    /// order, against the same state — and spends no order-rate token, draws
    /// no global request, mints no proposal and writes no ledger row. What it
    /// answers is "what would happen", so answering must not be a thing that
    /// happened.
    ///
    /// **It cannot return a [`Cleared`].** That type is the capability to
    /// sign, and a preflight that produced one would be a path to the signer
    /// that skipped the rate token — a second route of exactly the kind
    /// `AGENTS.md` invariant 1 forbids. The clearance is read for its
    /// utilization numbers and dropped inside this function.
    pub fn preflight(
        &self,
        agent: &AgentId,
        intent: &OrderIntent,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
        now_ms: u64,
    ) -> Verdict {
        match self.decide(
            agent,
            intent,
            asset,
            market,
            exposure,
            now_ms,
            Mode::Preflight,
        ) {
            Ok(cleared) => {
                let clearance = cleared.clearance();
                if let Some(journal) = &self.submissions
                    && let Err(error) = journal.preflight(clearance)
                {
                    return Verdict {
                        would_clear: false,
                        utilization: Some(clearance.utilization.clone()),
                        refusal: Some(error.into_refusal()),
                    };
                }
                Verdict {
                    would_clear: true,
                    utilization: Some(clearance.utilization.clone()),
                    refusal: None,
                }
            }
            Err(refusal) => Verdict {
                would_clear: false,
                utilization: None,
                refusal: Some(refusal),
            },
        }
    }

    /// Writes the verdict to the ledger.
    ///
    /// An **order** that cannot be recorded is downgraded to a refusal: D6
    /// makes the ledger the single record of why an order happened, and an
    /// order signed without one is unexplainable afterwards. A *refusal*
    /// that cannot be recorded is still a refusal — losing the row is bad,
    /// but the safe outcome already happened.
    ///
    /// A **risk-reducing** clearance — a cancel, or the dead-man's switch —
    /// is never blocked by a failed write. Fail-closed exists to stop new
    /// exposure; applying it to the actions that remove exposure inverts the
    /// property. A full or locked ledger disk would otherwise take away the
    /// operator's ability to stop trading while existing orders keep working,
    /// take the kill switch's own cancels down with it (item 26 makes those
    /// part of what engaging the switch does), and stop `scheduleCancel` from
    /// being re-armed or disarmed (item 27) — all while item 10 says to
    /// always reserve headroom for risk-reducing actions. So the row is
    /// logged as lost and the action proceeds.
    ///
    /// The order-rate token spent by a clearance is not refunded when the
    /// write fails. Overcharging an agent is the conservative direction.
    fn record(
        &self,
        agent: Option<&AgentId>,
        now_ms: u64,
        reason: &str,
        outcome: &Result<Cleared, Refusal>,
    ) -> Result<Option<crate::ledger::Appended>, Refusal> {
        let entry = AuditEntry {
            agent,
            at_ms: now_ms,
            reason,
            outcome: match outcome {
                Ok(cleared) => AuditOutcome::Cleared(cleared.clearance()),
                Err(refusal) => AuditOutcome::Refused(refusal),
            },
        };
        let e = match self.sink.record_with_receipt(&entry) {
            Ok(receipt) => return Ok(receipt),
            Err(error) => error,
        };
        let blocks = match outcome {
            Ok(cleared) => matches!(
                cleared.clearance().kind,
                ClearedKind::Order { .. } | ClearedKind::DiscretionaryCancel { .. }
            ),
            Err(_) => false,
        };
        if blocks {
            return Err(Unevaluable::AuditWriteFailed {
                detail: e.to_string(),
            }
            .into());
        }
        tracing::warn!(
            agent = ?agent,
            error = %e,
            "a guardrail outcome was not recorded in the ledger; \
             it was a refusal or a risk-reducing action, so it still stands"
        );
        Ok(None)
    }

    /// Records an operator mutation. Never refuses: the change has already
    /// been made and persisted, so failing here would report an error for
    /// something that happened. The lost row is logged instead.
    fn record_operator(&self, agent: Option<&AgentId>, now_ms: u64, action: &OperatorAction) {
        let entry = AuditEntry {
            agent,
            at_ms: now_ms,
            reason: "operator",
            outcome: AuditOutcome::Operator(action),
        };
        if let Err(e) = self.sink.record(&entry) {
            tracing::warn!(agent = ?agent, error = %e, "operator action not recorded in the ledger");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        agent: &AgentId,
        intent: &OrderIntent,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
        now_ms: u64,
        mode: Mode<'_>,
    ) -> Result<Cleared, Refusal> {
        check_reason(&intent.reason)?;
        if intent
            .original
            .as_ref()
            .is_some_and(|original| !original.matches_normalization(intent, asset))
        {
            return Err(Unevaluable::OriginalRequestMismatch.into());
        }
        self.refresh_policy()
            .map_err(|error| Unevaluable::PolicyAuthority {
                detail: error.to_string(),
            })?;
        let route = self.decision_route(agent)?;
        if let Mode::Approved(expected) = mode
            && expected != &route
        {
            return Err(route_refusal("proposal route changed since evaluation"));
        }
        check_order_approval_window(&route, now_ms)?;
        if exposure.account != route.binding.container {
            return Err(route_refusal(
                "exposure account does not match authorized container",
            ));
        }

        let mut state = self.state();

        let config =
            state
                .guardrails
                .get(agent)
                .cloned()
                .ok_or_else(|| Unevaluable::UnknownAgent {
                    agent: agent.clone(),
                })?;
        if let Err((field, detail)) = config.validate() {
            return Err(Unevaluable::InvalidGuardrailConfig {
                field: field.to_owned(),
                detail,
            }
            .into());
        }

        // Spec item 26. Cheap, needs no market data, and an agent that has
        // been stopped should hear that rather than a staleness complaint.
        if let Some((scope, engagement)) = state.effective_kill().blocking(agent) {
            return Err(Refusal::TradingPaused {
                scope,
                since_ms: engagement.engaged_at_ms,
                reason: engagement.reason.clone(),
            });
        }

        // The caller must supply the asset and the market tick for the symbol
        state.check_scoped_acknowledgment(&route, self.supervised_alpha)?;

        // it is asking about. A mismatch would measure the order against
        // another instrument's price, which is the worst silent failure in
        // this file.
        expect_same("asset", asset.name(), &intent.symbol)?;
        expect_same("market_ref", &market.symbol, &intent.symbol)?;

        let account = &exposure.agent;
        check_account(account, &config, now_ms)?;
        drop(
            self.feed
                .admit(exposure.feed_stamp.as_ref(), self.network, exposure.account)?,
        );

        let account_limits = state.account_limits;
        if !account_limits.is_unset() {
            match exposure.fleet.as_ref() {
                None => return Err(Unevaluable::MissingFleetState.into()),
                Some(fleet) => check_account(fleet, &config, now_ms)?,
            }
        }

        // Spec item 25. Trips the switch first, then refuses, so the next
        // order is refused as `TradingPaused` without needing fresh PnL.
        if let Some(breach) = breaker::check_all(
            agent,
            &config.loss,
            account,
            &account_limits,
            exposure.fleet.as_ref(),
        ) {
            let engagement = Engagement {
                engaged_at_ms: now_ms,
                reason: KillReason::LossLimit {
                    kind: breach.kind,
                    observed_usd: breach.observed_usd,
                    limit_usd: breach.limit_usd,
                },
            };
            state.stop(breach.scope.clone(), engagement.clone());
            let revision = state.policy_revision;
            let mut next = state.policy();
            next.kill.engage(breach.scope.clone(), engagement);
            drop(state);
            // Keep the exact evaluated revision. A conflicting writer must not
            // be overwritten by rebasing this full snapshot onto its revision.
            let persisted = self
                .mutation_lock()
                .and_then(|_mutation| self.commit_policy(revision, &next, now_ms));
            if let Err(error) = persisted {
                return Err(Unevaluable::StateWriteFailed {
                    detail: error.to_string(),
                }
                .into());
            }
            return Err(Refusal::LossLimit {
                scope: breach.scope,
                kind: breach.kind,
                observed_usd: breach.observed_usd,
                limit_usd: breach.limit_usd,
            });
        }

        let reference_px = check_market(market, &config, now_ms)?;

        // Spec item 24, and D-c's empty default: this is the first refusal a
        // freshly paired agent sees, and it carries the list to add to.
        if !config.symbols.contains(&intent.symbol) {
            return Err(Refusal::SymbolNotAllowed {
                symbol: intent.symbol.clone(),
                allowed: config.symbols.iter().cloned().collect(),
            });
        }

        // Round to the asset's rules before measuring anything, so every cap
        // below is measured on the numbers that would actually be signed
        // (spec item 8).
        let px = asset.round_price(intent.px);
        let sz = asset.round_size(intent.sz);
        let kind = match &intent.kind {
            OrderKind::Trigger {
                is_market,
                trigger_px,
                tpsl,
            } => OrderKind::Trigger {
                is_market: *is_market,
                trigger_px: asset.round_price(*trigger_px),
                tpsl: *tpsl,
            },
            other => other.clone(),
        };
        let spec = OrderSpec {
            is_buy: intent.is_buy,
            px,
            sz,
            kind: kind.clone(),
            reduce_only: intent.reduce_only,
            cloid: intent.cloid.clone(),
        };
        // Before `to_wire`, because `Asset::validate_order` computes the
        // notional with a bare `*` and `rust_decimal` panics on overflow.
        // Checking it here keeps that unreachable and gives the cap below the
        // number it needs.
        let notional_usd = checked(px.checked_mul(sz), "order notional")?;
        let position_szi = account.position_szi(&intent.symbol);
        let wire = spec
            .to_wire_for_position(asset, position_szi)
            .map_err(|e| Refusal::VenueRule(VenueRule::from(e)))?;

        let signed_sz = if intent.is_buy { sz } else { -sz };
        // Spec item 24 measures a *position* cap, and an order that has not
        // filled yet is still exposure the agent has committed to. Without
        // the working book here, both this cap and the leverage cap below
        // are bypassable by splitting one refused order into several resting
        // ones. Fail closed when it is absent, exactly as with the fleet
        // snapshot: a cap that cannot be measured is a cap that is not
        // enforced.
        let resting = account
            .resting
            .as_ref()
            .ok_or(Unevaluable::MissingRestingOrders)?;
        let (resting_buys, resting_sells) = resting.sides(&intent.symbol);
        let reduce_buys = resting
            .reduce_buys
            .get(&intent.symbol)
            .copied()
            .unwrap_or_default();
        let reduce_sells = resting
            .reduce_sells
            .get(&intent.symbol)
            .copied()
            .unwrap_or_default();
        if [resting_buys, resting_sells, reduce_buys, reduce_sells]
            .iter()
            .any(|size| *size < Decimal::ZERO)
        {
            return Err(Unevaluable::InputMismatch {
                field: "resting size".into(),
                expected: "non-negative sizes on both sides".into(),
                supplied: format!("{resting_buys}/{resting_sells}/{reduce_buys}/{reduce_sells}"),
            }
            .into());
        }
        if config.reduce_only {
            check_reduce_only(intent, position_szi, signed_sz, sz)?;
        }

        // Spec item 24, notional cap.
        if notional_usd > config.max_order_usd {
            return Err(Refusal::OrderNotional {
                symbol: intent.symbol.clone(),
                observed_usd: notional_usd,
                limit_usd: config.max_order_usd,
            });
        }

        // A clipped reduction may remove the starting position's offset
        // before opening orders fill. For each extreme, opposite-side fills
        // cannot help: reduce the initial offset, then fill the opening side.
        let worst_position = |buys: Decimal, sells: Decimal, rb: Decimal, rs: Decimal| {
            let long = checked(
                position_szi
                    .checked_add(rb.min((-position_szi).max(Decimal::ZERO)))
                    .and_then(|p| p.checked_add(buys)),
                "post-fill long",
            )?;
            let short = checked(
                position_szi
                    .checked_sub(rs.min(position_szi.max(Decimal::ZERO)))
                    .and_then(|p| p.checked_sub(sells)),
                "post-fill short",
            )?;
            Ok::<_, Refusal>(long.abs().max(short.abs()))
        };
        let position_before =
            worst_position(resting_buys, resting_sells, reduce_buys, reduce_sells)?;
        let mut sides = [resting_buys, resting_sells, reduce_buys, reduce_sells];
        let side = usize::from(!intent.is_buy) + if intent.reduce_only { 2 } else { 0 };
        sides[side] = checked(sides[side].checked_add(sz), "candidate working size")?;
        let position_after = worst_position(sides[0], sides[1], sides[2], sides[3])?;
        let genuine_reduction = intent.reduce_only
            && !position_szi.is_zero()
            && position_szi.is_sign_negative() != signed_sz.is_sign_negative()
            && position_after <= position_before;
        let position_after_usd = checked(
            position_after.abs().checked_mul(reference_px),
            "post-fill position notional",
        )?;
        let resting_usd = checked(
            resting_buys
                .checked_add(resting_sells)
                .and_then(|s| s.checked_mul(reference_px)),
            "resting notional",
        )?;
        if !genuine_reduction && position_after_usd > config.max_position_usd {
            return Err(Refusal::PositionNotional {
                symbol: intent.symbol.clone(),
                observed_usd: position_after_usd,
                resting_usd,
                limit_usd: config.max_position_usd,
            });
        }

        // Spec F's vol-scaled cap, checked after the fixed one and never
        // instead of it. The two compose as a minimum: a fixed cap is the
        // operator's hard ceiling and this only ever tightens it, so an
        // unset `max_risk_usd` leaves D-c's default-deny exactly as it was.
        let vol_scaled = vol_scaled_cap(&config, market, &intent.symbol)?;
        // **It binds only on orders that grow the position.** Unlike every
        // other cap here, this one *moves*: volatility doubles and the cap
        // halves, so a position that was inside it this morning can be over
        // it by noon without the agent having done anything. If the check
        // ignored direction, the agent's way out — trimming the position —
        // would be refused for leaving it still over a cap it is trying to
        // get under, and only an all-at-once exit would clear. Refusing a
        // risk-reducing order can only raise risk, which is the same reason
        // item 26 lets cancels through while the kill switch is engaged.
        //
        // Working orders remain reserved even if this reduction has not filled.
        let filled_position = checked(position_szi.checked_add(signed_sz), "filled position")?;
        let reduces_position = genuine_reduction
            || (!intent.reduce_only
                && position_after <= position_before
                && filled_position.abs() < position_szi.abs());
        if let Some(cap) = vol_scaled
            && !reduces_position
            && position_after_usd > cap.effective_cap_usd
        {
            return Err(Refusal::VolScaledPositionNotional {
                symbol: intent.symbol.clone(),
                observed_usd: position_after_usd,
                effective_cap_usd: cap.effective_cap_usd,
                risk_budget_usd: cap.risk_budget_usd,
                sigma_day_pct: cap.sigma_day.saturating_mul(HUNDRED),
                vol_scale: cap.vol_scale,
            });
        }

        // Spec item 24, leverage cap. D3: the operator sets it, and the
        // venue's own maximum for the asset is a hard bound below it.
        //
        // This symbol's current contribution — filled plus working — is
        // replaced by the post-fill number; every other symbol's positions
        // and working orders stay in, which is why the account total and the
        // resting total are added before the subtraction.
        // Remove exactly what the producer added, not size times today's
        // mark. Missing attribution gives no subtraction credit.
        let opening_notional = resting
            .notional_by_symbol
            .get(&intent.symbol)
            .copied()
            .unwrap_or_default();
        if opening_notional < Decimal::ZERO || opening_notional > resting.notional_usd {
            return Err(Unevaluable::InputMismatch {
                field: "resting notional".into(),
                expected: format!("symbol contribution between 0 and {}", resting.notional_usd),
                supplied: opening_notional.to_string(),
            }
            .into());
        }
        let symbol_before_usd = checked(
            position_szi
                .abs()
                .checked_mul(reference_px)
                .and_then(|n| n.checked_add(opening_notional)),
            "current symbol notional",
        )?;
        let account_before_usd = checked(
            account
                .total_position_notional_usd
                .checked_add(resting.notional_usd),
            "current account notional",
        )?;
        if let Some(limit_usd) = config.risk.max_open_exposure_usd
            && !genuine_reduction
        {
            let opening_usd = if intent.reduce_only {
                Decimal::ZERO
            } else {
                checked(sz.checked_mul(reference_px), "candidate opening notional")?
            };
            let observed_usd = checked(
                account_before_usd.checked_add(opening_usd),
                "gross account open exposure",
            )?;
            if observed_usd > limit_usd {
                return Err(Refusal::OpenExposure {
                    observed_usd,
                    limit_usd,
                });
            }
        }
        let total_after_usd = checked(
            account_before_usd
                .checked_sub(symbol_before_usd)
                .and_then(|n| n.checked_add(position_after_usd)),
            "post-fill account notional",
        )?
        .max(Decimal::ZERO);
        let leverage = checked(total_after_usd.checked_div(account.equity_usd), "leverage")?;
        let leverage_cap = config.risk.max_leverage.min(asset.info.max_leverage);
        if !genuine_reduction && leverage > Decimal::from(leverage_cap) {
            return Err(Refusal::Leverage {
                observed: leverage,
                limit: leverage_cap,
                equity_usd: account.equity_usd,
                position_notional_usd: total_after_usd,
            });
        }

        // Spec item 24, max slippage.
        let slippage_reference = match &kind {
            OrderKind::Trigger { trigger_px, .. } => *trigger_px,
            OrderKind::Limit { .. } => reference_px,
        };
        let slippage_bps = checked(
            adverse_slippage_bps(intent.is_buy, px, slippage_reference),
            "slippage",
        )?;
        let slippage_limit = match intent.max_slippage_bps {
            Some(agent_limit) => config.max_slippage_bps.min(agent_limit),
            None => config.max_slippage_bps,
        };
        if slippage_bps > slippage_limit {
            return Err(Refusal::Slippage {
                symbol: intent.symbol.clone(),
                observed_bps: slippage_bps,
                limit_bps: slippage_limit,
                reference_px: slippage_reference,
            });
        }

        // Spec item 24, order rate. Spent last, so nothing refused above
        // costs the agent budget. An approved proposal does not pay twice:
        // the token was spent when the proposal was minted, below.
        let rate = config.order_rate;
        let bucket = state.bucket_mut(agent, rate, now_ms);
        let feed_admission =
            self.feed
                .admit(exposure.feed_stamp.as_ref(), self.network, exposure.account)?;
        let spend = match mode {
            // Refill without taking. Refilling is time-based and idempotent —
            // it only advances the bucket to the clock it would reach on the
            // next call either way — so a preflight leaves the agent's budget
            // exactly where it found it.
            Mode::Approved(_) | Mode::Preflight => bucket.refill(now_ms),
            Mode::Fresh => bucket.try_take(now_ms),
        };
        spend.map_err(|e| bucket_refusal(e, rate, now_ms))?;
        // A preflight still answers the question a spend would have: is there
        // a token? Reported as the refusal the real call would hit, so an
        // agent is not told "clear" by a check that skipped the cap.
        if mode == Mode::Preflight {
            bucket.peek().map_err(|e| bucket_refusal(e, rate, now_ms))?;
        }
        let tokens_remaining = bucket.tokens();

        // Spec item 10 and item 24's "plus the global budget": the venue
        // meters requests per address, so every agent under the master shares
        // one budget that no per-agent cap can bound. An order may not draw
        // it below the reserve; a cancel may, which is why this refusal is
        // here and not in `decide_cancel`. Charged in the same pass as the
        // per-agent token, and for the same reason last.
        let global_budget = state.global_budget;
        let global_tokens_remaining =
            spend_global(&mut state.global_bucket, global_budget, mode, now_ms)?;

        // Spec item 28, checked last: a proposal that would have been refused
        // is refused rather than queued for a human to approve. The engine
        // mints and holds it; the caller gets a receipt, not a credential.
        // A preflight says approval *would* be required without minting a
        // proposal for a human to act on: item 20 answers a question, and
        // queuing work off the back of a question is an effect.
        if config.approval_required && mode == Mode::Preflight {
            return Err(Refusal::ApprovalRequired {
                symbol: intent.symbol.clone(),
                notional_usd,
                approval_id: String::new(),
                expires_at_ms: now_ms.saturating_add(APPROVAL_TTL_MS),
            });
        }
        if config.approval_required && mode == Mode::Fresh {
            if let Some(journal) = &self.approvals {
                let policy_revision = state.policy_revision;
                drop(feed_admission);
                drop(state);
                let proposal = journal
                    .mint(Candidate {
                        agent: agent.clone(),
                        intent: intent.clone(),
                        route,
                        policy_revision,
                        at_ms: now_ms,
                    })
                    .map_err(approval_refusal)?;
                return Err(Refusal::ApprovalRequired {
                    symbol: intent.symbol.clone(),
                    notional_usd,
                    approval_id: proposal.id,
                    expires_at_ms: proposal.expires_at_ms,
                });
            }
            let approval_id = state.mint_proposal(agent, intent, &route, now_ms);
            let expires_at_ms = now_ms.saturating_add(APPROVAL_TTL_MS);
            return Err(Refusal::ApprovalRequired {
                symbol: intent.symbol.clone(),
                notional_usd,
                approval_id,
                expires_at_ms,
            });
        }

        let action = Action::Order {
            orders: vec![wire],
            grouping: intent.grouping,
            builder: intent.builder.clone(),
        };
        let (snapshot_id, snapshot_hash) = match &market.snapshot {
            Some(MarketSnapshotRef { id, hash }) => (Some(id.clone()), Some(hash.clone())),
            None => (None, None),
        };
        let clearance = Clearance {
            approval_review_digest: None,
            agent: agent.clone(),
            policy_revision: state.policy_revision,
            vault_address: route.binding.vault_address,
            route,
            network: self.network,
            evaluated_at_ms: now_ms,
            kind: ClearedKind::Order {
                symbol: intent.symbol.clone(),
                is_buy: intent.is_buy,
                px,
                sz,
                notional_usd,
                reduce_only: intent.reduce_only,
                slippage_bps,
                reference_px,
                slippage_reference_px: slippage_reference,
                cloid: intent.cloid.clone(),
                snapshot_id,
                snapshot_hash,
            },
            utilization: Utilization {
                order_notional_pct: ratio_pct(notional_usd, config.max_order_usd),
                position_notional_pct: ratio_pct(position_after_usd, config.max_position_usd),
                daily_loss_pct: breaker::utilization_pct(
                    LossKind::Daily,
                    config.loss.max_daily_loss_usd,
                    account,
                ),
                drawdown_pct: breaker::utilization_pct(
                    LossKind::Drawdown,
                    config.loss.max_drawdown_usd,
                    account,
                ),
                vol_scaled_position_pct: vol_scaled
                    .and_then(|cap| ratio_pct(position_after_usd, cap.effective_cap_usd)),
                leverage,
                order_tokens_remaining: tokens_remaining,
                global_tokens_remaining,
            },
        };
        let mut cleared = Cleared::new(action, clearance);
        cleared.feed_stamp = exposure.feed_stamp.clone();
        Ok(cleared)
    }

    /// Clears a cancel by order id.
    ///
    /// Risk-reducing, so it clears while the kill switch is engaged — item 26
    /// makes cancelling resting orders part of what the switch *does*.
    ///
    /// It costs no per-agent rate token, and the address-wide budget of item
    /// 10 is charged but never allowed to refuse it: that budget exists to
    /// throttle agents before the venue does, and refusing a cancel to save a
    /// request is the one trade item 10 forbids ("always reserve headroom for
    /// risk-reducing actions"). A failed ledger write does not block it
    /// either — see [`GuardrailEngine::record`].
    pub fn clear_cancel(
        &self,
        agent: &AgentId,
        cancels: Vec<CancelWire>,
        reason: &str,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let count = cancels.len();
        self.clear_risk_reducing(agent, count, reason, now_ms, Action::Cancel { cancels })
    }

    /// Clears a cancel by client order id — the only safe move after a
    /// `timeout_unknown_outcome` (spec item 19).
    pub fn clear_cancel_by_cloid(
        &self,
        agent: &AgentId,
        cancels: Vec<CancelByCloidWire>,
        reason: &str,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let count = cancels.len();
        self.clear_risk_reducing(
            agent,
            count,
            reason,
            now_ms,
            Action::CancelByCloid { cancels },
        )
    }

    fn clear_risk_reducing(
        &self,
        agent: &AgentId,
        count: usize,
        reason: &str,
        now_ms: u64,
        action: Action,
    ) -> Result<Cleared, Refusal> {
        let outcome = self.decide_cancel(agent, count, reason, now_ms, action);
        self.record(Some(agent), now_ms, reason, &outcome)?;
        outcome
    }

    fn decide_cancel(
        &self,
        agent: &AgentId,
        count: usize,
        reason: &str,
        now_ms: u64,
        action: Action,
    ) -> Result<Cleared, Refusal> {
        check_reason(reason)?;
        let route = self.route_for_agent(agent)?;
        if count == 0 {
            return Err(Unevaluable::InputMismatch {
                field: "cancels".to_owned(),
                expected: "at least one order".to_owned(),
                supplied: "none".to_owned(),
            }
            .into());
        }
        let mut state = self.state();
        // Charged, never refused: the account budget has to stay honest about
        // requests oppen actually sends, but a cancel is the one thing it may
        // not stop. Saturates at zero rather than going negative.
        let global_tokens_remaining = draw_global_reserve(&mut state.global_bucket, now_ms);
        Ok(Cleared::new(
            action,
            Clearance {
                approval_review_digest: None,
                agent: agent.clone(),
                policy_revision: 0,
                vault_address: route.binding.vault_address,
                route,
                network: self.network,
                evaluated_at_ms: now_ms,
                kind: ClearedKind::Cancel { count },
                utilization: Utilization::none_with_global(global_tokens_remaining),
            },
        ))
    }

    /// Clears the dead-man's switch action for the current intent (spec item
    /// 27). `Ok(None)` when nothing needs to change.
    pub fn clear_dead_man(
        &self,
        agent: &AgentId,
        intent: DeadManIntent,
        now_ms: u64,
    ) -> Result<Option<Cleared>, Refusal> {
        match intent {
            DeadManIntent::Hold => Ok(None),
            DeadManIntent::Disarm => self.clear_schedule_cancel(agent, None, now_ms).map(Some),
            DeadManIntent::Arm { cancel_at_ms } => self
                .clear_schedule_cancel(agent, Some(cancel_at_ms), now_ms)
                .map(Some),
        }
    }

    /// Clears a `scheduleCancel` for one agent's container. `None` disarms.
    ///
    /// Risk-reducing, so nothing about the agent's guardrails blocks it — the
    /// switch exists so a dead process does not leave orders working. Named
    /// per agent because `scheduleCancel` is per address and item 27 makes N
    /// containers N independent arming duties.
    pub fn clear_schedule_cancel(
        &self,
        agent: &AgentId,
        cancel_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let outcome = self.decide_schedule_cancel(agent, cancel_at_ms, now_ms);
        self.record(Some(agent), now_ms, "dead-man's switch", &outcome)?;
        outcome
    }

    fn decide_schedule_cancel(
        &self,
        agent: &AgentId,
        cancel_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let route = self.route_for_agent(agent)?;
        if let Some(at) = cancel_at_ms {
            let earliest_ms = now_ms.saturating_add(DEAD_MAN_MIN_LEAD_MS);
            if at < earliest_ms {
                return Err(VenueRule::ScheduleCancelTooSoon {
                    cancel_at_ms: at,
                    earliest_ms,
                }
                .into());
            }
        }
        let mut state = self.state();
        let global_tokens_remaining = draw_global_reserve(&mut state.global_bucket, now_ms);
        Ok(Cleared::new(
            Action::ScheduleCancel { time: cancel_at_ms },
            Clearance {
                approval_review_digest: None,
                agent: agent.clone(),
                policy_revision: 0,
                vault_address: route.binding.vault_address,
                route,
                network: self.network,
                evaluated_at_ms: now_ms,
                kind: ClearedKind::ScheduleCancel { cancel_at_ms },
                utilization: Utilization::none_with_global(global_tokens_remaining),
            },
        ))
    }

    // ---- the signer ------------------------------------------------------

    /// Signs a cleared action, with this engine as the gate that runs inside
    /// the signer. This delegates to the same signing boundary as
    /// [`Self::sign_cleared_authorized`] (`AGENTS.md` invariant 1).
    ///
    /// It takes [`Cleared`] **by value** on purpose: a clearance is spent by
    /// the signature it authorises, so the same evaluation cannot be replayed
    /// into a second order. The returned [`Clearance`] is the audit record of
    /// the evaluation that authorised this exact request, for the
    /// hash-chained ledger (D6).
    ///
    /// **There is no checker parameter.** It builds a [`PreSignGate`] over
    /// this engine and *this clearance*, so a caller cannot supply a
    /// permissive gate for one call; the only way to sign through
    /// `oppen-core` is to sign through the engine that evaluated the order.
    ///
    /// **The signing key, the container and the network are not parameters.**
    /// The container and the network come out of the clearance; the key is
    /// loaded from the engine's own key store, for the agent the clearance
    /// names. When the key was a parameter this was the live hole rather than
    /// a theoretical one: under the revised D1 (V2) a Hyperliquid container is
    /// a *top-level* account that sends no `vaultAddress`, so the venue reads
    /// the account off the signature and **the key is the container**. A
    /// clearance evaluated against agent X's caps, equity and kill switch,
    /// signed with agent Y's key, executed on Y's capital under X's limits —
    /// including while Y was paused. Removing the parameter makes that
    /// unrepresentable rather than merely refused. The nonce and
    /// `expires_after` stay parameters: they belong to the submit queue (spec
    /// item 7), not to the risk decision.
    ///
    /// The gate re-runs at the signer rather than trusting the clearance, so
    /// a kill switch engaged in the gap between evaluating and signing still
    /// stops the order — see [`PreSignGate`] below. The caller supplies a live
    /// millisecond clock, not a timestamp captured before calling: the gate
    /// samples it after key loading and authority/state lock acquisition.
    /// That observation checks approval expiry and clearance freshness and
    /// timestamps any resulting refusal. Tests may supply a synthetic clock;
    /// production callers must read current time on each invocation (R1).
    pub fn sign_cleared(
        &self,
        cleared: Cleared,
        nonce: u64,
        expires_after: Option<u64>,
        clock: impl Fn() -> u64,
    ) -> Result<(ExchangeRequest, Clearance), SignClearedError> {
        self.sign_cleared_authorized(cleared, nonce, expires_after, clock, || Ok(()))
    }

    /// Adds live caller authority after key loading without replacing any core
    /// checks. The returned guard remains held through crypto, not through audit.
    pub fn sign_cleared_authorized<G>(
        &self,
        cleared: Cleared,
        nonce: u64,
        expires_after: Option<u64>,
        clock: impl Fn() -> u64,
        authorize: impl FnOnce() -> Result<G, Refusal>,
    ) -> Result<(ExchangeRequest, Clearance), SignClearedError> {
        if matches!(
            cleared.clearance.kind,
            ClearedKind::DiscretionaryCancel { .. }
        ) {
            return Err(SignClearedError::Refused(submission_refusal(
                "discretionary cancellations require the consuming dispatch capability",
            )));
        }
        self.sign_common(cleared, nonce, expires_after, clock, authorize, None, None)
            .map(|(request, clearance, _, _, _)| (request, clearance))
    }

    /// Signs only a discretionary cancellation, with observational, nonblocking
    /// caller checks after ledger/state waits. The initial guard remains held;
    /// a cancellation after the last pre-crypto observation may race with signing.
    pub fn sign_discretionary_cancel_authorized<G>(
        &self,
        cleared: Cleared,
        nonce: u64,
        expires_after: Option<u64>,
        clock: impl Fn() -> u64,
        authorize: impl Fn() -> Result<G, Refusal>,
    ) -> Result<SignedCancellation, SignClearedError> {
        if !matches!(
            cleared.clearance.kind,
            ClearedKind::DiscretionaryCancel { .. }
        ) {
            return Err(SignClearedError::Refused(submission_refusal(
                "discretionary cancellation clearance required",
            )));
        }
        let final_authorize = || authorize().map(drop);
        let deadline = cleared.approval_deadline_ms;
        let (request, clearance, _, wallet, signer) = self.sign_common(
            cleared,
            nonce,
            expires_after,
            clock,
            &authorize,
            None,
            Some(&final_authorize),
        )?;
        Ok(SignedCancellation {
            owner: self.submission_owner.clone(),
            request,
            clearance,
            deadline,
            wallet,
            signer,
        })
    }

    /// Signs and publishes digest-only evidence before granting one dispatch.
    ///
    /// `authorize` must be observational, nonblocking, perform no I/O, and not
    /// depend on invocation count. Its initial guard is retained; repeated
    /// checks after ledger/state/publication waits discard only their new guard.
    /// Cancellation observed at the final pre-crypto check prevents crypto.
    /// Cancellation after that observation can race with crypto; a refusal at
    /// the post-publication check withholds dispatch capability, not the already
    /// produced signature. The dispatch boundary checks again separately.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_submission_authorized<G>(
        &self,
        cleared: Cleared,
        journal: &SubmissionJournal,
        receipt: &SubmissionReceipt,
        nonce: u64,
        expires_after: Option<u64>,
        clock: impl Fn() -> u64,
        authorize: impl Fn() -> Result<G, Refusal>,
    ) -> Result<SignedSubmission, SignClearedError> {
        let feed_stamp = cleared.feed_stamp.clone();
        let deadline = cleared.approval_deadline_ms;
        let final_authorize = || authorize().map(drop);
        let (request, clearance, signed, wallet, signer) = self.sign_common(
            cleared,
            nonce,
            expires_after,
            clock,
            &authorize,
            Some((journal, receipt)),
            Some(&final_authorize),
        )?;
        let signed = signed.ok_or_else(|| {
            SignClearedError::Refused(submission_refusal("missing signing publication"))
        })?;
        Ok(SignedSubmission {
            feed_stamp,
            owner: self.submission_owner.clone(),
            journal: journal.clone(),
            receipt: receipt.clone(),
            signed,
            request,
            clearance,
            deadline,
            wallet,
            signer,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_common<G>(
        &self,
        cleared: Cleared,
        nonce: u64,
        expires_after: Option<u64>,
        clock: impl Fn() -> u64,
        authorize: impl FnOnce() -> Result<G, Refusal>,
        submission: Option<(&SubmissionJournal, &SubmissionReceipt)>,
        final_authorize: Option<&dyn Fn() -> Result<(), Refusal>>,
    ) -> Result<SignedParts, SignClearedError> {
        if let Some((journal, receipt)) = submission {
            let check = self
                .submissions()
                .map_err(submission_refusal)
                .and_then(|own| {
                    if !own.same_authority(journal) {
                        return Err(submission_refusal(
                            "submission journal differs from engine authority",
                        ));
                    }
                    journal
                        .verify_reserved(receipt, cleared.clearance())
                        .map_err(submission_refusal)
                });
            if let Err(refusal) = check {
                self.record_pre_sign_refusal(&cleared.clearance.agent, clock(), &refusal);
                return Err(SignClearedError::Refused(refusal));
            }
        }
        let (action, clearance, approval_deadline_ms, feed_stamp) = cleared.into_parts();
        let (key, wallet): (AgentKey, AgentWallet) =
            self.keys.load_agent_key_with_wallet(&clearance.agent)?;
        let caller_authority = authorize().map_err(|refusal| {
            self.record_pre_sign_refusal(&clearance.agent, clock(), &refusal);
            SignClearedError::Refused(refusal)
        })?;
        let actual_signer = key.address();
        let gate = PreSignGate {
            feed_stamp: feed_stamp.as_ref(),
            held_feed: std::cell::RefCell::new(None),
            engine: self,
            clearance: &clearance,
            approval_deadline_ms,
            clock: &clock,
            final_authorize,
            observed_at_ms: std::cell::Cell::new(None),
            actual_signer,
            wallet: wallet.clone(),
            held_state: std::cell::RefCell::new(None),
            held_authority: std::cell::RefCell::new(None),
        };
        let signed = ExchangeRequest::sign_checked(
            &key,
            action,
            nonce,
            clearance.vault_address,
            expires_after,
            clearance.network,
            &gate,
        );
        let mut publication = None;
        let signed = signed.and_then(|request| {
            if let Some((journal, receipt)) = submission {
                let signed_at_ms = gate.observed_at_ms.get().ok_or_else(|| {
                    SignError::Refused(submission_refusal("signing clock missing"))
                })?;
                gate.held_feed.borrow_mut().take();
                gate.held_state.borrow_mut().take();
                let evidence = GuardedSignature {
                    journal,
                    receipt,
                    request: &request,
                    clearance: &clearance,
                    wallet: &wallet,
                    signer: actual_signer,
                    signed_at_ms,
                };
                let mut permit = gate.held_authority.borrow_mut();
                publication = Some(
                    permit
                        .as_mut()
                        .ok_or_else(|| {
                            SignError::Refused(submission_refusal("signing authority missing"))
                        })?
                        .publish_submission(&evidence)
                        .map_err(SignError::Refused)?,
                );
                let state = self.state();
                gate.check_state(&state, request.action())
                    .map_err(SignError::Refused)?;
                *gate.held_state.borrow_mut() = Some(state);
            }
            Ok(request)
        });
        // Release durable authority only after signing, and before refusal
        // auditing (which needs the same ledger lock).
        let observed_at_ms = gate.observed_at_ms.get();
        drop(gate);
        drop(caller_authority);
        let request = signed.map_err(|e| match e {
            SignError::Refused(refusal) => {
                if matches!(
                    &refusal,
                    Refusal::Unevaluable(
                        Unevaluable::PolicyAuthority { .. } | Unevaluable::PolicyChanged
                    )
                ) {
                    self.state().inhibit();
                }
                self.record_pre_sign_refusal(
                    &clearance.agent,
                    observed_at_ms.unwrap_or_else(&clock),
                    &refusal,
                );
                SignClearedError::Refused(refusal)
            }
            SignError::Signing(e) => SignClearedError::Signing(e),
        })?;
        Ok((request, clearance, publication, wallet, actual_signer))
    }

    /// Consumes a private request once. First polling is local dispatch admission,
    /// not proof that any bytes reached a socket or that the venue accepted them.
    /// `authorize` has the same observational/nonblocking contract as
    /// [`Self::sign_submission_authorized`]. The final observation follows all
    /// ledger/state/evidence waits, before the clock sample and first HTTP poll.
    /// Cancellation after that observation may race with local dispatch.
    pub async fn post_submission_authorized<G, C, A>(
        &self,
        signed: SignedSubmission,
        exchange: &oppen_hl::exchange::ExchangeClient,
        clock: C,
        authorize: A,
    ) -> Result<oppen_hl::exchange::ExchangeResponse, SubmissionPostError>
    where
        C: Fn() -> u64 + Send + Sync,
        A: Fn() -> Result<G, Refusal> + Send,
    {
        let response = self
            .post_authorized(
                Dispatch {
                    owner: &signed.owner,
                    request: &signed.request,
                    clearance: &signed.clearance,
                    deadline: signed.deadline,
                    wallet: &signed.wallet,
                    signer: signed.signer,
                    submission: Some(&signed),
                },
                exchange,
                &clock,
                authorize,
            )
            .await;
        signed
            .journal
            .record_post_result(
                &signed.receipt,
                &signed.signed,
                &signed.request,
                &signed.clearance,
                &response,
                clock(),
            )
            .map_err(SubmissionPostError::JournalUncertain)?;
        response
    }

    /// Consumes a cancellation after rechecking route, policy, ownership, TTL and
    /// observational caller authority. Run on the retained blocking order worker;
    /// first polling is local admission, not proof of socket or venue delivery.
    pub async fn post_cancellation_authorized<G, C, A>(
        &self,
        signed: SignedCancellation,
        exchange: &oppen_hl::exchange::ExchangeClient,
        clock: C,
        authorize: A,
    ) -> Result<oppen_hl::exchange::ExchangeResponse, SubmissionPostError>
    where
        C: Fn() -> u64 + Send + Sync,
        A: Fn() -> Result<G, Refusal> + Send,
    {
        self.post_authorized(
            Dispatch {
                owner: &signed.owner,
                request: &signed.request,
                clearance: &signed.clearance,
                deadline: signed.deadline,
                wallet: &signed.wallet,
                signer: signed.signer,
                submission: None,
            },
            exchange,
            &clock,
            authorize,
        )
        .await
    }

    async fn post_authorized<G, C, A>(
        &self,
        signed: Dispatch<'_>,
        exchange: &oppen_hl::exchange::ExchangeClient,
        clock: &C,
        authorize: A,
    ) -> Result<oppen_hl::exchange::ExchangeResponse, SubmissionPostError>
    where
        C: Fn() -> u64 + Send + Sync,
        A: Fn() -> Result<G, Refusal> + Send,
    {
        use std::future::Future;
        let mut authorize = Some(authorize);
        let mut post = std::pin::pin!(exchange.post(signed.request));
        std::future::poll_fn(|cx| {
            if let Some(authorize) = authorize.take() {
                let mut admit = || -> Result<_, Refusal> {
                    if !Arc::ptr_eq(&self.submission_owner, signed.owner)
                        || exchange.network() != signed.clearance.network
                    {
                        return Err(submission_refusal("dispatch network or authority mismatch"));
                    }
                    if let Some(submission) = signed.submission
                        && !self
                            .submissions()
                            .map_err(submission_refusal)?
                            .same_authority(&submission.journal)
                    {
                        return Err(submission_refusal("dispatch submission authority mismatch"));
                    }
                    let caller = authorize()?;
                    let final_authorize = || authorize().map(drop);
                    let gate = PreSignGate {
                        feed_stamp: signed
                            .submission
                            .and_then(|submission| submission.feed_stamp.as_ref()),
                        held_feed: std::cell::RefCell::new(None),
                        engine: self,
                        clearance: signed.clearance,
                        approval_deadline_ms: signed.deadline,
                        clock,
                        final_authorize: Some(&final_authorize),
                        observed_at_ms: std::cell::Cell::new(None),
                        actual_signer: signed.signer,
                        wallet: signed.wallet.clone(),
                        held_state: std::cell::RefCell::new(None),
                        held_authority: std::cell::RefCell::new(None),
                    };
                    gate.check(PreSign {
                        action: signed.request.action(),
                        nonce: signed.request.nonce(),
                        vault_address: signed.request.vault_address(),
                        expires_after: signed.request.expires_after(),
                        network: signed.clearance.network,
                    })?;
                    gate.held_feed.borrow_mut().take();
                    if let Some(submission) = signed.submission {
                        gate.held_authority
                            .borrow()
                            .as_ref()
                            .ok_or_else(|| submission_refusal("dispatch authority missing"))?
                            .validate_submission(submission)?;
                    }
                    // Evidence verification may block; sample the same predicates again.
                    gate.check_state(
                        gate.held_state
                            .borrow()
                            .as_ref()
                            .ok_or_else(|| submission_refusal("dispatch state missing"))?,
                        signed.request.action(),
                    )?;
                    let polled = post.as_mut().poll(cx);
                    drop(gate);
                    drop(caller);
                    Ok(polled)
                };
                match admit() {
                    Ok(polled) => polled.map_err(SubmissionPostError::Transport),
                    Err(refusal) => {
                        self.record_pre_sign_refusal(&signed.clearance.agent, clock(), &refusal);
                        std::task::Poll::Ready(Err(SubmissionPostError::NotSent(refusal)))
                    }
                }
            } else {
                post.as_mut()
                    .poll(cx)
                    .map_err(SubmissionPostError::Transport)
            }
        })
        .await
    }

    /// Writes the ledger row for a refusal that happened at the signer rather
    /// than at [`GuardrailEngine::evaluate`].
    ///
    /// `evaluate` has already written a *clearance* row by the time this gate
    /// runs, so without this an audit export would show an order cleared and
    /// never explain why no fill followed — which is the question D6 built
    /// the chain to answer, and item 18 names guardrail trips as events.
    ///
    /// A failed write is logged, never a reason to sign. This is the same
    /// reading as [`GuardrailEngine::record`]'s refusal branch: losing the
    /// row is bad, but the safe outcome has already happened.
    fn record_pre_sign_refusal(&self, agent: &AgentId, now_ms: u64, refusal: &Refusal) {
        // A committed evidence row may still need head-anchor publication.
        // Do not turn that recoverable one-row window into a second append.
        if matches!(
            refusal,
            Refusal::Unevaluable(Unevaluable::SubmissionAuthority { .. })
        ) {
            return;
        }
        let entry = AuditEntry {
            agent: Some(agent),
            at_ms: now_ms,
            reason: "pre-sign gate",
            outcome: AuditOutcome::Refused(refusal),
        };
        if let Err(e) = self.sink.record(&entry) {
            tracing::warn!(
                agent = %agent,
                error = %e,
                "a pre-sign refusal was not recorded in the ledger; the refusal still stands"
            );
        }
    }
}

/// The pre-sign gate: one engine bound to the one clearance it is signing.
///
/// **Private, and constructed only by the shared guarded signing boundary.**
/// That is the compile-time half of `AGENTS.md` invariant 1, and it is the
/// reason `GuardrailEngine` itself deliberately does *not* implement
/// [`PreSignCheck`]. While it did, the engine was public, `sign_checked` is
/// public, and any caller could write
///
/// ```text
/// ExchangeRequest::sign_checked(&key, any_action_at_all, nonce, …, &engine)
/// ```
///
/// and be handed a signature. The gate only ever saw the assembled wire
/// request, so it could not tell an action the engine had evaluated from one
/// the caller invented: the engine rubber-stamped its own bypass. There is
/// now no value of the checker type outside this file, so that call does not
/// fail at run time — it fails to compile.
///
/// The gate deliberately does **not** re-run the full order evaluation. It
/// cannot: a [`PreSign`] carries the wire action, not the market tick or the
/// account snapshot the notional, slippage and leverage caps were measured
/// against, and re-fetching those here would be evaluating an order against a
/// different world than the one that cleared it. What it re-checks is
/// everything knowable from the engine's own state at this instant, and that
/// is exactly the set of things that can change *after* an evaluation and
/// *before* a signature:
///
/// - **The network (R4).** A clearance minted by the testnet engine cannot be
///   signed through the mainnet one, and vice versa.
/// - **The agent (D1).** Taken from [`Clearance::agent`], never re-derived
///   from `vaultAddress`. Under the revised D1 a container is a venue
///   account, which on Hyperliquid is a *top-level* account that sends no
///   `vaultAddress` at all — so an absent one no longer identifies anything,
///   and reading it as "the master account" let an agent-scoped kill switch
///   be signed straight through. The agent must still be one this engine
///   knows, or it has never measured a limit against that capital.
/// - **What the clearance's own age makes untrue**, which is per kind and so
///   is a `match` on [`ClearedKind`] rather than a single check. An order is
///   a verdict about the market at [`Clearance::evaluated_at_ms`] and expires
///   with the agent's market-data budget. An *arm* of the dead-man's switch
///   is a verdict about the clock: item 7's queue eats into the lead the
///   venue requires, and `deadman.rs` says silently failing to arm is the
///   worst outcome there, so a lead that has fallen inside the minimum is
///   refused here and re-armed fresh rather than sent to be rejected. A
///   cancel and a *disarm* were priced off neither and never go stale.
/// - **The kill switch (spec item 26), read fresh.** An operator pressing the
///   switch, or another agent's loss breaker tripping `Global`, between
///   `evaluate` and `sign_cleared` stops this order too.
///
/// Cancels and `scheduleCancel` are exempt from the switch — item 26 makes
/// cancelling resting orders part of what engaging the switch *does*, and
/// item 10 requires headroom for risk-reducing actions.
struct PreSignGate<'a> {
    feed_stamp: Option<&'a crate::feed::FeedStamp>,
    // Reverse lock order on release: feed, engine state, ledger authority.
    held_feed: std::cell::RefCell<Option<crate::feed::AdmissionGuard<'a>>>,
    engine: &'a GuardrailEngine,
    /// The evaluation that authorises this signature, and the only place the
    /// gate reads an identity from.
    clearance: &'a Clearance,
    approval_deadline_ms: Option<u64>,
    clock: &'a dyn Fn() -> u64,
    final_authorize: Option<&'a dyn Fn() -> Result<(), Refusal>>,
    observed_at_ms: std::cell::Cell<Option<u64>>,
    actual_signer: Address,
    wallet: AgentWallet,
    // Declaration order releases state before the enclosing ledger authority.
    held_state: std::cell::RefCell<Option<MutexGuard<'a, EngineState>>>,
    held_authority: std::cell::RefCell<Option<Box<dyn SigningPermit + 'a>>>,
}

impl PreSignGate<'_> {
    fn check_state(&self, state: &EngineState, action: &Action) -> Result<(), Refusal> {
        self.held_feed.borrow_mut().take();
        if matches!(self.clearance.kind, ClearedKind::Order { .. }) {
            *self.held_feed.borrow_mut() = Some(self.engine.feed.admit(
                self.feed_stamp,
                self.clearance.network,
                self.clearance.route.binding.container,
            )?);
        }
        if let Some(authorize) = self.final_authorize {
            authorize()?;
        }
        // Sample only after every potentially blocking admission dependency.
        let now_ms = (self.clock)();
        self.observed_at_ms.set(Some(now_ms));
        if let Some(expires_at_ms) = self.approval_deadline_ms
            && now_ms >= expires_at_ms
        {
            return Err(Unevaluable::ApprovalExpired {
                expires_at_ms,
                now_ms,
            }
            .into());
        }
        // Exhaustive on purpose, and `ClearedKind` is `#[non_exhaustive]`
        // only outside this crate: a new kind cannot be added without an
        // answer here to "what does this clearance's age make untrue?".
        match &self.clearance.kind {
            ClearedKind::Order { .. } => {
                check_order_approval_window(&self.clearance.route, now_ms)?;
                if self.clearance.policy_revision != state.policy_revision {
                    return Err(Unevaluable::PolicyChanged.into());
                }
                state.check_scoped_acknowledgment(
                    &self.clearance.route,
                    self.engine.supervised_alpha,
                )?;
                let config = state.guardrails.get(&self.clearance.agent).ok_or_else(|| {
                    Unevaluable::UnknownAgent {
                        agent: self.clearance.agent.clone(),
                    }
                })?;
                let evaluated_at_ms = self.clearance.evaluated_at_ms;
                if now_ms < evaluated_at_ms {
                    return Err(Unevaluable::ClockWentBackwards {
                        now_ms,
                        last_ms: evaluated_at_ms,
                    }
                    .into());
                }
                let age_ms = now_ms.saturating_sub(evaluated_at_ms);
                let max_age_ms = config.freshness.max_market_age_ms;
                if age_ms > max_age_ms {
                    return Err(Unevaluable::StaleClearance { age_ms, max_age_ms }.into());
                }
            }
            ClearedKind::DiscretionaryCancel { observed_at_ms, .. } => {
                if self.clearance.policy_revision != state.policy_revision {
                    return Err(Unevaluable::PolicyChanged.into());
                }
                let config = state.guardrails.get(&self.clearance.agent).ok_or_else(|| {
                    Unevaluable::UnknownAgent {
                        agent: self.clearance.agent.clone(),
                    }
                })?;
                let last_ms = self.clearance.evaluated_at_ms.max(*observed_at_ms);
                if now_ms < last_ms {
                    return Err(Unevaluable::ClockWentBackwards { now_ms, last_ms }.into());
                }
                let age_ms = now_ms - observed_at_ms;
                let max_age_ms = config.freshness.max_account_age_ms;
                if age_ms > max_age_ms {
                    return Err(Unevaluable::StaleClearance { age_ms, max_age_ms }.into());
                }
            }
            ClearedKind::ScheduleCancel {
                cancel_at_ms: Some(at),
            } => {
                let earliest_ms = now_ms.saturating_add(DEAD_MAN_MIN_LEAD_MS);
                if *at < earliest_ms {
                    return Err(VenueRule::ScheduleCancelTooSoon {
                        cancel_at_ms: *at,
                        earliest_ms,
                    }
                    .into());
                }
            }
            ClearedKind::Cancel { .. } | ClearedKind::ScheduleCancel { cancel_at_ms: None } => {}
        }
        if !is_risk_reducing(action)
            && let Some((scope, engagement)) =
                state.effective_kill().blocking(&self.clearance.agent)
        {
            return Err(Refusal::TradingPaused {
                scope,
                since_ms: engagement.engaged_at_ms,
                reason: engagement.reason.clone(),
            });
        }
        Ok(())
    }
}

impl PreSignCheck for PreSignGate<'_> {
    /// The engine's own taxonomy, not a string (`AGENTS.md` invariant 8), so
    /// a refusal from the signer reads the same as one from `evaluate` and
    /// `oppen-mcp` maps it with the same code.
    type Refusal = Refusal;

    fn check(&self, request: PreSign<'_>) -> Result<(), Refusal> {
        let engine = self.engine;
        if request.network != engine.network {
            return Err(Unevaluable::WrongNetwork {
                expected: engine.network,
                supplied: request.network,
            }
            .into());
        }
        let agent = &self.clearance.agent;
        validate_route(&self.clearance.route, agent, self.clearance.network)?;
        if self.clearance.vault_address != self.clearance.route.binding.vault_address
            || request.vault_address != self.clearance.vault_address
            || self.wallet != self.clearance.route.binding.wallet
            || self.actual_signer != self.clearance.route.binding.wallet.address
        {
            return Err(route_refusal(
                "clearance, wallet or signer does not match route authority",
            ));
        }
        *self.held_authority.borrow_mut() = Some(engine.sink.before_sign(
            self.clearance,
            &self.wallet,
            self.actual_signer,
        )?);
        let state = engine.state();
        self.check_state(&state, request.action)?;
        *self.held_state.borrow_mut() = Some(state);
        Ok(())
    }
}

/// Whether the action removes exposure rather than adding it.
///
/// Only these three clear while the kill switch is engaged (spec item 26,
/// item 10). Everything else — orders, and the leverage, margin and
/// sub-account actions — is gated, because a paused account changing its
/// leverage is not risk-reducing and the fail-closed reading is the one this
/// module takes everywhere else.
fn is_risk_reducing(action: &Action) -> bool {
    matches!(
        action,
        Action::Cancel { .. } | Action::CancelByCloid { .. } | Action::ScheduleCancel { .. }
    )
}

/// Why a cleared action was not signed.
///
/// Distinct from [`Refusal`] only in that it also carries a signing failure;
/// the refusal it wraps is the engine's ordinary typed one, so a caller
/// renders both the same way.
#[derive(Debug, thiserror::Error)]
pub enum SignClearedError {
    /// The pre-sign gate refused. Reachable in normal operation: the kill
    /// switch can engage between the evaluation and the signature.
    #[error("refused at the signer: {0}")]
    Refused(#[source] Refusal),
    /// The agent's wallet could not be loaded, so nothing was signed.
    ///
    /// Distinct from [`SignClearedError::Refused`] in what it asks of the
    /// operator: a missing, corrupt or address-mismatched wallet is a
    /// provisioning failure to fix in the console, not a guardrail the agent
    /// can adapt to and retry against.
    #[error("the agent wallet could not be loaded: {0}")]
    Key(#[from] KeyStoreError),
    /// The gate passed and signing itself failed.
    #[error(transparent)]
    Signing(#[from] oppen_hl::Error),
}

/// Whether this evaluation is an agent's first attempt or an operator
/// approving a proposal the engine minted (spec item 28).
///
/// The only two things `Approved` changes are the order-rate token, which was
/// spent when the proposal was minted, and the approval requirement itself.
/// Every other predicate runs again against fresh state.
///
/// `Preflight` changes only what an answer must not cost (spec item 20 says
/// "without executing"): it spends no order-rate token, draws no global
/// request, and mints no proposal. Every predicate still runs, against the
/// same state, in the same order — a preflight that evaluated a *restatement*
/// of the guardrails would be a second copy of them to keep in step, and
/// `AGENTS.md` invariant 1 exists to stop exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode<'a> {
    Fresh,
    Approved(&'a AuthorizedRoute),
    Preflight,
}

impl EngineState {
    /// Mints an approval proposal and returns its id.
    ///
    /// The id is a receipt for a value the engine holds, not a credential: it
    /// only names which stored intent to re-evaluate, and only
    /// [`GuardrailEngine::operator_approve_proposal`] accepts it. The
    /// sequence number makes two proposals minted in the same millisecond
    /// distinct.
    fn mint_proposal(
        &mut self,
        agent: &AgentId,
        intent: &OrderIntent,
        route: &AuthorizedRoute,
        now_ms: u64,
    ) -> String {
        self.sweep_proposals(now_ms);
        self.proposal_seq = self.proposal_seq.saturating_add(1);
        let id = format!("{}-{now_ms}-{}", agent.as_str(), self.proposal_seq);
        self.proposals.insert(
            id.clone(),
            Proposal {
                id: id.clone(),
                agent: agent.clone(),
                intent: ProposalIntent::Order(intent.clone()),
                cancel_provenance: None,
                route: route.clone(),
                expires_at_ms: now_ms.saturating_add(APPROVAL_TTL_MS),
            },
        );
        id
    }
}

impl Utilization {
    fn none_with_global(global_tokens_remaining: Decimal) -> Self {
        Utilization {
            order_notional_pct: None,
            position_notional_pct: None,
            daily_loss_pct: None,
            drawdown_pct: None,
            vol_scaled_position_pct: None,
            leverage: Decimal::ZERO,
            order_tokens_remaining: Decimal::ZERO,
            global_tokens_remaining,
        }
    }
}

/// Spends one request from the address-wide budget for an order, refusing
/// once the remainder would fall to or below the reserve item 10 keeps for
/// risk-reducing actions. Returns what is left.
fn spend_global(
    bucket: &mut TokenBucket,
    budget: GlobalRateBudget,
    mode: Mode<'_>,
    now_ms: u64,
) -> Result<Decimal, Refusal> {
    let reserve = Decimal::from(budget.reserve);
    bucket.refill(now_ms).map_err(|e| match e {
        BucketError::ClockWentBackwards { last_ms } => {
            Unevaluable::ClockWentBackwards { now_ms, last_ms }.into()
        }
        _ => Refusal::from(Unevaluable::InvalidGuardrailConfig {
            field: "global_rate.rate.per_ms".to_owned(),
            detail: "must be positive".to_owned(),
        }),
    })?;
    // An approved proposal already spent its request when it was minted, and a
    // preflight sends none at all.
    if matches!(mode, Mode::Approved(_) | Mode::Preflight) {
        return Ok(bucket.tokens());
    }
    // The reserve is checked before the take, so a refused order never draws
    // on the headroom item 10 keeps for cancels.
    if bucket.tokens() <= reserve || bucket.try_take(now_ms).is_err() {
        return Err(Refusal::GlobalRateBudget {
            tokens_available: bucket.tokens(),
            reserve: budget.reserve,
            retry_after_ms: bucket.retry_after_ms(),
        });
    }
    Ok(bucket.tokens())
}

/// Charges the address-wide budget for a risk-reducing request without ever
/// refusing it (item 10). Saturates at zero.
fn draw_global_reserve(bucket: &mut TokenBucket, now_ms: u64) -> Decimal {
    // A backwards clock or a zero window means the budget cannot be metered.
    // That is not a reason to stop a cancel, so the draw is simply skipped.
    let _ = bucket.try_take(now_ms);
    bucket.tokens()
}

/// Spec item 19 requires a reason; item 30 makes it untrusted text and D-e
/// keeps it forever. Bounded and control-character-checked here, at
/// ingestion — `AGENTS.md` invariant 9 covers only rendering, and on the
/// character grid U1 chose, an ANSI escape in a stored string is not inert.
fn check_reason(reason: &str) -> Result<(), Refusal> {
    if reason.trim().is_empty() {
        return Err(Refusal::MissingReason);
    }
    if reason.len() > MAX_REASON_BYTES {
        return Err(Refusal::ReasonTooLong {
            len_bytes: reason.len(),
            max_bytes: MAX_REASON_BYTES,
        });
    }
    for (at_byte, ch) in reason.char_indices() {
        if ch.is_control() && ch != '\n' && ch != '\t' {
            return Err(Refusal::ReasonControlCharacter {
                at_byte,
                codepoint: ch as u32,
            });
        }
    }
    Ok(())
}

fn route_refusal(detail: &str) -> Refusal {
    Unevaluable::RouteAuthority {
        detail: detail.to_owned(),
    }
    .into()
}

fn approval_refusal(error: impl std::fmt::Display) -> Refusal {
    Unevaluable::ApprovalAuthority {
        detail: error.to_string(),
    }
    .into()
}

// Admission only: automatic expiry latching and cancellation delivery remain
// runtime activation work. Cleanup actions still require live route authority.
fn check_order_approval_window(route: &AuthorizedRoute, now_ms: u64) -> Result<(), Refusal> {
    let wallet = &route.binding.wallet;
    if now_ms < wallet.approved_at_ms || now_ms >= wallet.valid_until_ms {
        return Err(route_refusal(
            "order is outside the approved wallet validity window",
        ));
    }
    Ok(())
}

fn validate_route(
    route: &AuthorizedRoute,
    agent: &AgentId,
    network: Network,
) -> Result<(), Refusal> {
    if route.network != network
        || route.binding.agent != *agent
        || route.binding_seq == 0
        || route
            .binding
            .vault_address
            .is_some_and(|vault| vault != route.binding.container)
    {
        return Err(route_refusal("registry route identity is inconsistent"));
    }
    Ok(())
}

fn cancel_targets(state: &EngineState, scope: &KillScope) -> BTreeSet<AgentId> {
    match scope {
        KillScope::Global => state.guardrails.keys().cloned().collect(),
        KillScope::Agent { agent } => BTreeSet::from([agent.clone()]),
    }
}

fn expect_same(field: &str, supplied: &str, expected: &str) -> Result<(), Refusal> {
    if supplied == expected {
        return Ok(());
    }
    Err(Unevaluable::InputMismatch {
        field: field.to_owned(),
        expected: expected.to_owned(),
        supplied: supplied.to_owned(),
    }
    .into())
}

/// Everything that has to be true of an account snapshot before a limit can
/// be measured against it.
fn check_account(
    account: &AccountSnapshot,
    config: &AgentGuardrails,
    now_ms: u64,
) -> Result<(), Refusal> {
    if now_ms < account.as_of_ms {
        return Err(Unevaluable::ClockWentBackwards {
            now_ms,
            last_ms: account.as_of_ms,
        }
        .into());
    }
    if !account.reconciled {
        return Err(Unevaluable::UnreconciledAccount {
            as_of_ms: account.as_of_ms,
        }
        .into());
    }
    let age_ms = now_ms - account.as_of_ms;
    if age_ms > config.freshness.max_account_age_ms {
        return Err(Unevaluable::StaleAccountState {
            age_ms,
            max_age_ms: config.freshness.max_account_age_ms,
        }
        .into());
    }
    if account.equity_usd <= Decimal::ZERO {
        return Err(Unevaluable::NonPositiveEquity {
            equity_usd: account.equity_usd,
        }
        .into());
    }
    if !account.covers_day_of(now_ms) {
        return Err(Unevaluable::LossWindowMismatch {
            day_start_ms: account.day_start_ms,
            now_ms,
        }
        .into());
    }
    Ok(())
}

/// Everything that has to be true of a market tick, returning the reference
/// price once it is established.
fn check_market(
    market: &MarketRef,
    config: &AgentGuardrails,
    now_ms: u64,
) -> Result<Decimal, Refusal> {
    if now_ms < market.as_of_ms {
        return Err(Unevaluable::ClockWentBackwards {
            now_ms,
            last_ms: market.as_of_ms,
        }
        .into());
    }
    if !market.quality.is_ok() {
        return Err(Unevaluable::DegradedFeed {
            symbol: market.symbol.clone(),
            quality: market.quality,
        }
        .into());
    }
    let age_ms = now_ms - market.as_of_ms;
    if age_ms > config.freshness.max_market_age_ms {
        return Err(Unevaluable::StaleMarketData {
            symbol: market.symbol.clone(),
            age_ms,
            max_age_ms: config.freshness.max_market_age_ms,
        }
        .into());
    }
    // A sustained divergence between the reconstructed mark and the venue's
    // is a hard stop before signing (`docs/specs/fair-value.md` §3, §7): mark
    // is what the chain margins and liquidates with. An *instantaneous*
    // divergence is not — §14.2 measured containment failing 37–39% of the
    // time, so refusing on every tick outside tolerance would refuse
    // everything. Without a start time there is no duration, so the window
    // has not elapsed.
    if let Some(observed_bps) = market.mark_divergence_bps
        && observed_bps > config.max_mark_divergence_bps
        && let Some(since_ms) = market.mark_divergent_since_ms
    {
        if now_ms < since_ms {
            return Err(Unevaluable::ClockWentBackwards {
                now_ms,
                last_ms: since_ms,
            }
            .into());
        }
        let sustained_ms = now_ms - since_ms;
        if sustained_ms >= config.mark_divergence_window_ms {
            return Err(Unevaluable::MarkDivergence {
                symbol: market.symbol.clone(),
                observed_bps,
                limit_bps: config.max_mark_divergence_bps,
                sustained_ms,
                window_ms: config.mark_divergence_window_ms,
            }
            .into());
        }
    }
    match market.reference_px {
        Some(px) if px > Decimal::ZERO => Ok(px),
        _ => Err(Unevaluable::MissingReferencePrice {
            symbol: market.symbol.clone(),
        }
        .into()),
    }
}

/// Reduce-only mode (spec item 24): the order must be flagged reduce-only at
/// the venue *and* actually reduce the open position. The flag alone is not
/// enough — the venue's flag prevents an increase, but a mode an operator
/// switched on to wind an agent down should also refuse an order that was
/// never going to reduce anything.
fn check_reduce_only(
    intent: &OrderIntent,
    position_szi: Decimal,
    signed_sz: Decimal,
    sz: Decimal,
) -> Result<(), Refusal> {
    let breach = if !intent.reduce_only {
        Some(ReduceOnlyBreach::NotFlagged)
    } else if position_szi.is_zero() {
        Some(ReduceOnlyBreach::NoPosition)
    } else if position_szi.is_sign_negative() == signed_sz.is_sign_negative() {
        Some(ReduceOnlyBreach::SameSide)
    } else if sz > position_szi.abs() {
        Some(ReduceOnlyBreach::Oversized)
    } else {
        None
    };
    match breach {
        None => Ok(()),
        Some(detail) => Err(Refusal::ReduceOnly {
            symbol: intent.symbol.clone(),
            position_szi,
            signed_sz,
            flagged_reduce_only: intent.reduce_only,
            detail,
        }),
    }
}

/// Slippage in the direction that costs money, in bps. A passive order priced
/// away from the reference has no slippage, so a resting bid below the mid is
/// not refused for being far from it.
///
/// The reference is positive by the time this runs: a market one comes from
/// [`check_market`], and a trigger one has been through `OrderSpec::to_wire`,
/// which rejects a non-positive price.
///
/// `None` when the arithmetic overflows, which the caller turns into a
/// refusal — the fail-closed reading of "this number is not representable".
fn adverse_slippage_bps(is_buy: bool, px: Decimal, reference_px: Decimal) -> Option<Decimal> {
    let adverse = if is_buy {
        px.checked_sub(reference_px)
    } else {
        reference_px.checked_sub(px)
    }?;
    if adverse <= Decimal::ZERO {
        return Some(Decimal::ZERO);
    }
    adverse.checked_div(reference_px)?.checked_mul(BPS)
}

/// A computed vol-scaled cap and the two numbers it came from.
///
/// The inputs travel with the result so the refusal is built from the values
/// the arithmetic actually used, rather than re-read from the config and the
/// tick at the point of failure. Re-reading would need an `unwrap` on each —
/// both are `Some` by construction here — and the fallback would print a
/// refusal claiming a $0 budget at 0% volatility, which is a message that
/// lies about why the order was refused.
#[derive(Debug, Clone, Copy)]
struct VolScaledCap {
    effective_cap_usd: Decimal,
    risk_budget_usd: Decimal,
    sigma_day: Decimal,
    vol_scale: Decimal,
}

/// Spec F's `effective_cap = risk_budget / (2 * sigma_day)`, or `None` when
/// no vol-scaled cap is configured.
///
/// The doubling is the spec's, and it is what makes the number mean
/// something an operator can hold in their head: at the cap, an ordinary
/// two-sigma day moves the position by `max_risk_usd` and no more. Halve the
/// volatility and the same budget buys twice the size.
///
/// The sigma divided by is [`MarketRef::sigma_day`] multiplied by
/// [`vol_scale`] — the day's volatility corrected by what the last hour is
/// actually doing, because a twenty-four-bar statistic cannot notice an hour
/// old regime on its own.
///
/// **Fails closed on a missing or non-positive sigma.** A cap configured and
/// not computable is a cap not enforced, which is the failure the whole
/// module exists to prevent — and a zero sigma would divide to an infinite
/// cap, so the one input that must never be defaulted is the denominator.
/// The scale is not that input: it multiplies a denominator that already
/// exists, so an unmeasured one leaves the cap computable and merely
/// untightened.
fn vol_scaled_cap(
    config: &AgentGuardrails,
    market: &MarketRef,
    symbol: &str,
) -> Result<Option<VolScaledCap>, Refusal> {
    let Some(risk_budget_usd) = config.risk.max_risk_usd else {
        return Ok(None);
    };
    let sigma_day = market
        .sigma_day
        .filter(|sigma| *sigma > Decimal::ZERO)
        .ok_or_else(|| {
            Refusal::from(Unevaluable::MissingVolatility {
                symbol: symbol.to_owned(),
            })
        })?;
    let vol_scale = vol_scale(market.vol_ratio);
    let cap = sigma_day
        .checked_mul(vol_scale)
        .and_then(|sigma| TWO.checked_mul(sigma))
        .and_then(|denominator| risk_budget_usd.checked_div(denominator));
    Ok(Some(VolScaledCap {
        effective_cap_usd: checked(cap, "vol-scaled cap")?,
        risk_budget_usd,
        sigma_day,
        vol_scale,
    }))
}

/// How much the last hour tightens the day's volatility: `max(1, vol_ratio)`.
///
/// **One is the floor, and that is the whole of the design.** A ratio below
/// one says the last hour was quieter than the day, and honouring it would
/// *widen* a guardrail on the strength of a sixty-bar sample — the direction
/// in which being wrong costs money. An absent ratio lands on the same floor
/// for the same reason it is not a refusal: the cap still has its
/// denominator, so the fail-closed reading [`Unevaluable::MissingVolatility`]
/// gets does not apply, and the result is exactly the cap this predicate
/// computed before the correction existed.
fn vol_scale(vol_ratio: Option<Decimal>) -> Decimal {
    vol_ratio.unwrap_or(Decimal::ONE).max(Decimal::ONE)
}

/// `None` when the limit is zero (the ratio is undefined) or the arithmetic
/// overflows. Utilization is a display number, so an unrepresentable one is
/// simply absent rather than a refusal.
pub(super) fn ratio_pct(observed: Decimal, limit: Decimal) -> Option<Decimal> {
    if limit.is_zero() {
        return None;
    }
    observed.checked_div(limit)?.checked_mul(HUNDRED)
}

/// Turns an overflowed `Decimal` operation into a fail-closed refusal.
/// `rust_decimal`'s operators panic on overflow and an agent chooses the
/// price and the size, so nothing on this path uses a bare `*` or `+`.
fn checked(value: Option<Decimal>, field: &str) -> Result<Decimal, Refusal> {
    value.ok_or_else(|| {
        Unevaluable::ArithmeticOverflow {
            field: field.to_owned(),
        }
        .into()
    })
}

fn bucket_refusal(error: BucketError, rate: OrderRate, now_ms: u64) -> Refusal {
    match error {
        BucketError::Empty {
            tokens_available_micro,
            retry_after_ms,
        } => Refusal::OrderRate {
            limit: rate.count,
            window_ms: rate.per_ms,
            tokens_available: Decimal::from(tokens_available_micro) / Decimal::from(1_000_000u64),
            retry_after_ms,
        },
        BucketError::ClockWentBackwards { last_ms } => {
            Unevaluable::ClockWentBackwards { now_ms, last_ms }.into()
        }
        BucketError::InvalidRate => Unevaluable::InvalidGuardrailConfig {
            field: "order_rate.per_ms".to_owned(),
            detail: "must be positive".to_owned(),
        }
        .into(),
    }
}

#[cfg(test)]
mod stop_generation_tests {
    use super::*;

    struct AuditOnly;

    impl AuditSink for AuditOnly {
        fn record(&self, _: &AuditEntry<'_>) -> Result<(), AuditError> {
            Ok(())
        }

        fn route_for_agent(&self, _: &AgentId) -> Result<AuthorizedRoute, Refusal> {
            Err(route_refusal("no signing authority in this fixture"))
        }

        fn before_sign(
            &self,
            _: &Clearance,
            _: &AgentWallet,
            _: Address,
        ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
            Err(route_refusal("no signing authority in this fixture"))
        }
    }

    #[test]
    fn saturated_stop_generation_never_wraps_or_accepts_acknowledgment() {
        let engine = GuardrailEngine::from_parts(
            Arc::new(super::super::store::MemoryStore::new()),
            Arc::new(AuditOnly),
            Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet)),
            Network::Testnet,
            crate::feed::test_session(oppen_hl::Address::from_bytes([1; 20])),
        )
        .unwrap();
        engine.state().stop_generation = u64::MAX - 1;
        let reviewed = engine.policy_observation().unwrap();
        engine.operator_acknowledge_policy(reviewed, 1).unwrap();
        assert!(!engine.policy_status().admission_inhibited);

        {
            let mut state = engine.state();
            state.inhibit();
            assert_eq!(state.stop_generation, u64::MAX);
            assert!(state.acknowledged.is_none());
            state.inhibit();
            assert_eq!(state.stop_generation, u64::MAX);
        }

        let matching = engine.policy_observation().unwrap();
        assert_eq!(matching.revision, reviewed.revision);
        assert_eq!(matching.stop_generation, u64::MAX);
        assert!(matches!(
            engine.operator_acknowledge_policy(matching, 2),
            Err(GuardrailError::Policy { .. })
        ));
        let status = engine.policy_status();
        assert_eq!(status.stop_generation, u64::MAX);
        assert!(status.acknowledgment.is_none());
        assert!(status.admission_inhibited);
        assert!(engine.cancellation_needed(&AgentId::new("alpha")));
    }
}
