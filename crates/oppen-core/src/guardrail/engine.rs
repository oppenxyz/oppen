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
use oppen_hl::wire::{BuilderInfo, CancelByCloidWire, CancelWire, Cloid, Grouping};
use oppen_hl::{Action, Address, AgentKey, ExchangeRequest, Network};

use crate::keys::{KeyStore, KeyStoreError};

use super::AgentId;
use super::breaker::{self, BudgetScope, LossBudget, LossKind};
use super::bucket::{BucketError, TokenBucket};
use super::config::{
    APPROVAL_TTL_MS, AgentGuardrails, GlobalRateBudget, LossLimits, MAX_REASON_BYTES, OrderRate,
};
use super::deadman::{DEAD_MAN_MIN_LEAD_MS, DeadManIntent};
use super::kill::{Engagement, KillEffect, KillReason, KillScope, KillSwitch};
use super::refusal::{ReduceOnlyBreach, Refusal, Unevaluable, VenueRule};
use super::snapshot::{AccountSnapshot, Exposure, MarketRef, MarketSnapshotRef};
use super::store::{GuardrailStore, StoreError};

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
    id: String,
    agent: AgentId,
    intent: OrderIntent,
    expires_at_ms: u64,
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

    /// What the agent asked for. Item 28 re-prices at approval time, so the
    /// operator console shows this against the current market to display the
    /// drift.
    pub fn intent(&self) -> &OrderIntent {
        &self.intent
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
    /// Whose container this was evaluated against. **Every** clearance names
    /// one, including the dead-man's switch: spec item 27 arms
    /// `scheduleCancel` *per container*, "N times, not once", and an
    /// agent-less clearance is a leftover of the pre-revision model where one
    /// master account spoke for the fleet.
    ///
    /// It is also the identity [`GuardrailEngine::sign_cleared`] loads the
    /// signing key from, which is why it is not an `Option`: under the revised
    /// D1 (V2) a Hyperliquid container is a *top-level* account carrying no
    /// `vaultAddress`, so the agent key **is** the container, and a clearance
    /// that cannot name its agent cannot name the key that may sign it.
    pub agent: AgentId,
    /// The container's `vaultAddress` where the venue granted a sub-account
    /// (D1), taken from the engine's own registry rather than from the caller.
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
    action: Action,
    clearance: Clearance,
    policy_revision: u64,
}

impl Cleared {
    /// Private on purpose. Moving this line, widening it to `pub(crate)`, or
    /// adding a second constructor breaks `AGENTS.md` invariant 1.
    fn new(action: Action, clearance: Clearance, policy_revision: u64) -> Self {
        Cleared {
            action,
            clearance,
            policy_revision,
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

    fn into_parts(self) -> (Action, Clearance, u64) {
        (self.action, self.clearance, self.policy_revision)
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
    /// A newly paired agent got D-c's near-zero defaults.
    AgentRegistered {
        config: Box<AgentGuardrails>,
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
}

/// Records nothing and always succeeds.
///
/// Test-only, and deliberately unreachable from outside this module:
/// shipping it would make every clearance unexplainable after the fact,
/// which D6 exists to prevent.
#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct NullAuditSink;

#[cfg(test)]
impl AuditSink for NullAuditSink {
    fn record(&self, _entry: &AuditEntry<'_>) -> Result<(), AuditError> {
        Ok(())
    }
}

/// Operator-side failures. Distinct from [`Refusal`], which is what an agent
/// receives: nothing here is ever handed to an agent.
#[derive(Debug, thiserror::Error)]
pub enum GuardrailError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("guardrail config field {field} is invalid: {detail}")]
    InvalidConfig { field: String, detail: String },
    /// D1: one container, one agent. The operator console names the agent
    /// already holding the address rather than silently re-pointing it, so
    /// the fix — a different container, or de-registering the other agent —
    /// is the operator's to make.
    #[error("{vault_address} is already the container bound to {bound_to}")]
    ContainerAlreadyBound {
        vault_address: Address,
        bound_to: AgentId,
    },
    /// D1 binds an agent to one container for its life, and pairing calls
    /// [`GuardrailEngine::register_agent`] on **every reconnect** — so an
    /// address that disagrees with the stored binding is a re-point, not a
    /// re-registration. Accepting it would move which capital an agent's
    /// caps, loss budget and ledger history describe, silently, from the
    /// reconnect path. `docs/decisions.md` V5 makes the container migration
    /// an explicit operator gesture that does not exist yet; until it does,
    /// this is refused and the operator de-registers the agent.
    ///
    /// `bound_to` is an `Option` because a top-level container — D1 V2's
    /// Hyperliquid default, and every container v1 provisions — is bound to
    /// `None`. Naming it takes a `None` binding as seriously as an address,
    /// which is the whole point: it is the shape the re-point guard used to
    /// read as "never bound".
    #[error("{agent} is already bound to container {bound_to:?}; {supplied:?} would move it")]
    ContainerChanged {
        agent: AgentId,
        bound_to: Option<Address>,
        supplied: Option<Address>,
    },
}

#[derive(Debug)]
struct EngineState {
    policy_revision: u64,
    guardrails: BTreeMap<AgentId, AgentGuardrails>,
    /// D1: each agent's sub-account, bound into every clearance it produces.
    vaults: BTreeMap<AgentId, Address>,
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
    store: Arc<dyn GuardrailStore>,
    sink: Arc<dyn AuditSink>,
    /// Where the agent wallets live. Held by the engine rather than passed to
    /// [`GuardrailEngine::sign_cleared`] for the same reason the network is:
    /// a key supplied per call is a key the caller can get wrong, and on a
    /// top-level container (D1 V2) the key *is* the account, so the wrong one
    /// signs a cleared order onto another agent's capital.
    keys: Arc<dyn KeyStore>,
    /// R4: one engine per network, and every clearance it produces carries
    /// this. Fixed at construction because there is no operator gesture that
    /// should move a running engine from testnet to mainnet — switching
    /// networks means a different database file and a different engine.
    network: Network,
    state: Mutex<EngineState>,
}

impl std::fmt::Debug for GuardrailEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardrailEngine")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl GuardrailEngine {
    /// Loads persisted guardrails, sub-account bindings and kill-switch state
    /// (spec item 26: the switch survives a restart).
    pub fn new(
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
        keys: Arc<dyn KeyStore>,
        network: Network,
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
        let persisted = store.load()?;
        check_containers_unique(&persisted.vaults)?;
        let global_budget = GlobalRateBudget::default();
        Ok(GuardrailEngine {
            store,
            sink,
            keys,
            network,
            state: Mutex::new(EngineState {
                policy_revision: 0,
                guardrails: persisted.guardrails,
                vaults: persisted.vaults,
                account_limits: persisted.account_limits,
                kill: persisted.kill,
                buckets: BTreeMap::new(),
                active: BTreeSet::new(),
                proposals: BTreeMap::new(),
                proposal_seq: 0,
                global_budget,
                global_bucket: TokenBucket::new(global_budget.rate, 0),
                pending_effects: Vec::new(),
            }),
        })
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

    /// Registers a newly paired agent with D-c's near-zero defaults if it has
    /// no stored configuration, and returns the configuration in force.
    ///
    /// `vault_address` is the container D1 pairs the agent with, when the
    /// venue grants a sub-account. It is recorded here and stamped onto every
    /// clearance the agent ever gets, so no later caller can supply a
    /// different one. `None` is the top-level container the revised D1 (V2)
    /// makes the Hyperliquid default — its own venue account, addressed by
    /// the agent key rather than by a `vaultAddress`.
    ///
    /// **A sub-account address binds to exactly one agent.** D1's isolation
    /// is one position book, one margin pool and one loss budget per agent;
    /// two agents on one address is two sets of caps measured against one
    /// pool of capital, and a roster that displays a segregation that does
    /// not exist. It is refused here, where the binding is made, so that no
    /// lookup downstream has to pick a winner among several. Two top-level
    /// containers do not collide with *each other* — `None` is the absence of
    /// a vault address on the wire, and the agent key is the account — but
    /// `None` is still a binding, and it binds as hard as an address does.
    ///
    /// **The registered agent is the bound agent, not the one with a vault
    /// row.** Reading the binding out of `vaults` alone treats a top-level
    /// container as "never bound", and under the revised D1 (V2) that is
    /// *every* container v1 provisions on Hyperliquid — so the re-point this
    /// refuses would have been reachable from the reconnect path on the only
    /// shape that actually ships, moving a funded agent onto a sub-account
    /// address the caller chose and writing no ledger row on the way.
    ///
    /// The defaults include an empty symbol allowlist, so the agent's first
    /// order is refused with [`Refusal::SymbolNotAllowed`] naming the limit
    /// to raise. That refusal is the onboarding.
    pub fn register_agent(
        &self,
        agent: &AgentId,
        vault_address: Option<Address>,
        now_ms: u64,
    ) -> Result<AgentGuardrails, GuardrailError> {
        let mut state = self.state();
        // An agent this engine has already registered is already bound, and
        // its container is whatever `vaults` says — including the `None` of a
        // top-level account. `vaults` is checked too, so a first registration
        // whose guardrail write failed after the vault write is still bound
        // on the retry rather than re-pointable.
        if state.guardrails.contains_key(agent) || state.vaults.contains_key(agent) {
            // Re-pairing with the same container is the ordinary reconnect and
            // is a no-op; anything else is a move.
            let bound_to = state.vaults.get(agent).copied();
            if vault_address != bound_to {
                return Err(GuardrailError::ContainerChanged {
                    agent: agent.clone(),
                    bound_to,
                    supplied: vault_address,
                });
            }
        } else if let Some(vault) = vault_address {
            if let Some((bound_to, _)) = state.vaults.iter().find(|(_, bound)| **bound == vault) {
                return Err(GuardrailError::ContainerAlreadyBound {
                    vault_address: vault,
                    bound_to: bound_to.clone(),
                });
            }
            self.store.save_vault(agent, &vault)?;
            state.vaults.insert(agent.clone(), vault);
        }
        if let Some(existing) = state.guardrails.get(agent) {
            return Ok(existing.clone());
        }
        let config = AgentGuardrails::default();
        self.store.save_guardrails(agent, &config)?;
        state.guardrails.insert(agent.clone(), config.clone());
        drop(state);
        self.record_operator(
            Some(agent),
            now_ms,
            &OperatorAction::AgentRegistered {
                config: Box::new(config.clone()),
                vault_address,
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
        let before = {
            let mut state = self.state();
            self.store.save_guardrails(agent, &config)?;
            state.policy_revision = state.policy_revision.wrapping_add(1);
            state.guardrails.insert(agent.clone(), config.clone())
        };
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
        let before = {
            let mut state = self.state();
            self.store.save_account_limits(&limits)?;
            state.policy_revision = state.policy_revision.wrapping_add(1);
            std::mem::replace(&mut state.account_limits, limits)
        };
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
        let effect = {
            let mut state = self.state();
            let newly_engaged = state.kill.engage(
                scope.clone(),
                Engagement {
                    engaged_at_ms: now_ms,
                    reason: reason.clone(),
                },
            );
            let effect = KillEffect {
                cancel_for: cancel_targets(&state, &scope),
                scope,
                newly_engaged,
            };
            if let Err(error) = self.store.save_kill_switch(&state.kill) {
                if newly_engaged {
                    state.pending_effects.push(effect);
                }
                return Err(error.into());
            }
            effect
        };
        self.record_operator(
            None,
            now_ms,
            &OperatorAction::KillEngaged {
                scope: effect.scope.clone(),
                reason,
                newly_engaged: effect.newly_engaged,
                cancel_for: effect.cancel_for.clone(),
            },
        );
        Ok(effect)
    }

    pub fn operator_release_kill(
        &self,
        scope: &KillScope,
        now_ms: u64,
    ) -> Result<bool, GuardrailError> {
        let released = {
            let mut state = self.state();
            let mut next = state.kill.clone();
            let released = next.release(scope);
            self.store.save_kill_switch(&next)?;
            state.kill = next;
            released
        };
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
    pub fn paused_agents(&self) -> BTreeSet<AgentId> {
        let state = self.state();
        state
            .guardrails
            .keys()
            .filter(|agent| state.kill.blocking(agent).is_some())
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

    /// The sub-account bound to an agent (D1).
    pub fn vault_address(&self, agent: &AgentId) -> Option<Address> {
        self.state().vaults.get(agent).copied()
    }

    /// Read back after a restart, so a limit the operator set is one the
    /// risk console can still display (item 25, item 32). The per-agent
    /// equivalent is [`GuardrailEngine::guardrails`].
    pub fn account_limits(&self) -> LossLimits {
        self.state().account_limits
    }

    pub fn kill_switch(&self) -> KillSwitch {
        self.state().kill.clone()
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
    pub fn pending_proposals(&self, now_ms: u64) -> Vec<Proposal> {
        let mut state = self.state();
        state.sweep_proposals(now_ms);
        state.proposals.values().cloned().collect()
    }

    /// Approves a proposal and re-evaluates it in full against fresh market
    /// and account state (spec item 28 re-prices at approval time).
    ///
    /// Operator-only, like everything else in this section (`AGENTS.md`
    /// invariant 3). The intent comes from the engine's own store, never from
    /// the caller, so approving proposal *A* cannot sign order *B*; and the
    /// proposal is consumed on success, so one approval authorises one
    /// evaluation. Every hard predicate runs again — an order that has since
    /// become a breach is refused rather than waved through because a human
    /// looked at it a minute ago. Only two things are skipped: the order-rate
    /// token, which was spent when the proposal was minted, and the approval
    /// requirement itself.
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
        let proposal = {
            let mut state = self.state();
            state.sweep_proposals(now_ms);
            state
                .proposals
                .remove(approval_id)
                .ok_or_else(|| Unevaluable::UnknownProposal {
                    approval_id: approval_id.to_owned(),
                })?
        };
        let outcome = self.decide(
            &proposal.agent,
            &proposal.intent,
            asset,
            market,
            exposure,
            now_ms,
            Mode::Approved,
        );
        self.record(
            Some(&proposal.agent),
            now_ms,
            &proposal.intent.reason,
            &outcome,
        )?;
        outcome
    }

    /// Rejects a proposal. Item 18 makes approval decisions ledger events, so
    /// this is recorded even though nothing is signed.
    pub fn operator_reject_proposal(&self, approval_id: &str, now_ms: u64) -> bool {
        let removed = self.state().proposals.remove(approval_id).is_some();
        if removed {
            self.record_operator(
                None,
                now_ms,
                &OperatorAction::ProposalRejected {
                    approval_id: approval_id.to_owned(),
                },
            );
        }
        removed
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
    ) -> Result<(), Refusal> {
        let entry = AuditEntry {
            agent,
            at_ms: now_ms,
            reason,
            outcome: match outcome {
                Ok(cleared) => AuditOutcome::Cleared(cleared.clearance()),
                Err(refusal) => AuditOutcome::Refused(refusal),
            },
        };
        let Err(e) = self.sink.record(&entry) else {
            return Ok(());
        };
        let blocks = match outcome {
            Ok(cleared) => matches!(cleared.clearance().kind, ClearedKind::Order { .. }),
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
        Ok(())
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
        mode: Mode,
    ) -> Result<Cleared, Refusal> {
        check_reason(&intent.reason)?;

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
        if let Some((scope, engagement)) = state.kill.blocking(agent) {
            return Err(Refusal::TradingPaused {
                scope,
                since_ms: engagement.engaged_at_ms,
                reason: engagement.reason.clone(),
            });
        }

        // The caller must supply the asset and the market tick for the symbol
        // it is asking about. A mismatch would measure the order against
        // another instrument's price, which is the worst silent failure in
        // this file.
        expect_same("asset", asset.name(), &intent.symbol)?;
        expect_same("market_ref", &market.symbol, &intent.symbol)?;

        let account = &exposure.agent;
        check_account(account, &config, now_ms)?;

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
            let newly_engaged = state.kill.engage(
                breach.scope.clone(),
                Engagement {
                    engaged_at_ms: now_ms,
                    reason: KillReason::LossLimit {
                        kind: breach.kind,
                        observed_usd: breach.observed_usd,
                        limit_usd: breach.limit_usd,
                    },
                },
            );
            // Item 26: engaging the switch cancels resting orders. Queued
            // before the store write, and regardless of whether that write
            // succeeds, because the cancels are the risk-reducing half of the
            // trip and a failed write must never be the reason an agent's
            // working orders stay live overnight.
            if newly_engaged {
                let effect = KillEffect {
                    cancel_for: cancel_targets(&state, &breach.scope),
                    scope: breach.scope.clone(),
                    newly_engaged,
                };
                state.pending_effects.push(effect);
            }
            // The in-memory engagement stands even if the write fails; the
            // conservative direction is to stay stopped.
            if let Err(e) = self.store.save_kill_switch(&state.kill) {
                return Err(Unevaluable::StateWriteFailed {
                    detail: e.to_string(),
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
        let wire = spec
            .to_wire(asset)
            .map_err(|e| Refusal::VenueRule(VenueRule::from(e)))?;

        let signed_sz = if intent.is_buy { sz } else { -sz };
        let position_szi = account.position_szi(&intent.symbol);
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
        let spend = match mode {
            // Refill without taking. Refilling is time-based and idempotent —
            // it only advances the bucket to the clock it would reach on the
            // next call either way — so a preflight leaves the agent's budget
            // exactly where it found it.
            Mode::Approved | Mode::Preflight => bucket.refill(now_ms),
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
            let approval_id = state.mint_proposal(agent, intent, now_ms);
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
            agent: agent.clone(),
            vault_address: state.vaults.get(agent).copied(),
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
        Ok(Cleared::new(action, clearance, state.policy_revision))
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
        if count == 0 {
            return Err(Unevaluable::InputMismatch {
                field: "cancels".to_owned(),
                expected: "at least one order".to_owned(),
                supplied: "none".to_owned(),
            }
            .into());
        }
        let mut state = self.state();
        if !state.guardrails.contains_key(agent) {
            return Err(Unevaluable::UnknownAgent {
                agent: agent.clone(),
            }
            .into());
        }
        // Charged, never refused: the account budget has to stay honest about
        // requests oppen actually sends, but a cancel is the one thing it may
        // not stop. Saturates at zero rather than going negative.
        let global_tokens_remaining = draw_global_reserve(&mut state.global_bucket, now_ms);
        Ok(Cleared::new(
            action,
            Clearance {
                agent: agent.clone(),
                vault_address: state.vaults.get(agent).copied(),
                network: self.network,
                evaluated_at_ms: now_ms,
                kind: ClearedKind::Cancel { count },
                utilization: Utilization::none_with_global(global_tokens_remaining),
            },
            state.policy_revision,
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
        if !state.guardrails.contains_key(agent) {
            return Err(Unevaluable::UnknownAgent {
                agent: agent.clone(),
            }
            .into());
        }
        let global_tokens_remaining = draw_global_reserve(&mut state.global_bucket, now_ms);
        Ok(Cleared::new(
            Action::ScheduleCancel { time: cancel_at_ms },
            Clearance {
                agent: agent.clone(),
                vault_address: state.vaults.get(agent).copied(),
                network: self.network,
                evaluated_at_ms: now_ms,
                kind: ClearedKind::ScheduleCancel { cancel_at_ms },
                utilization: Utilization::none_with_global(global_tokens_remaining),
            },
            state.policy_revision,
        ))
    }

    // ---- the signer ------------------------------------------------------

    /// Signs a cleared action, with this engine as the gate that runs inside
    /// the signer. **This is the only signing entry point `oppen-core`
    /// offers** (`AGENTS.md` invariant 1).
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
    /// stops the order — see [`PreSignGate`] below. `now_ms` is that gap
    /// measured: the gate refuses a clearance the queue held past the agent's
    /// own freshness budget, and the refusal lands in the ledger with that
    /// timestamp. `oppen-core` reads no clock of its own (R1), so it comes in
    /// from the caller exactly as it does for [`GuardrailEngine::evaluate`].
    pub fn sign_cleared(
        &self,
        cleared: Cleared,
        nonce: u64,
        expires_after: Option<u64>,
        now_ms: u64,
    ) -> Result<(ExchangeRequest, Clearance), SignClearedError> {
        let (action, clearance, policy_revision) = cleared.into_parts();
        let key: AgentKey = self.keys.load_agent_key(&clearance.agent)?;
        let gate = PreSignGate {
            engine: self,
            clearance: &clearance,
            policy_revision,
            now_ms,
        };
        let request = ExchangeRequest::sign_checked(
            &key,
            action,
            nonce,
            clearance.vault_address,
            expires_after,
            clearance.network,
            &gate,
        )
        .map_err(|e| match e {
            SignError::Refused(refusal) => {
                self.record_pre_sign_refusal(&clearance.agent, now_ms, &refusal);
                SignClearedError::Refused(refusal)
            }
            SignError::Signing(e) => SignClearedError::Signing(e),
        })?;
        Ok((request, clearance))
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
/// **Private, and constructed only by [`GuardrailEngine::sign_cleared`].**
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
    engine: &'a GuardrailEngine,
    /// The evaluation that authorises this signature, and the only place the
    /// gate reads an identity from.
    clearance: &'a Clearance,
    policy_revision: u64,
    now_ms: u64,
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
        let state = engine.state();
        let agent = &self.clearance.agent;
        let Some(config) = state.guardrails.get(agent) else {
            return Err(Unevaluable::UnknownAgent {
                agent: agent.clone(),
            }
            .into());
        };
        // Exhaustive on purpose, and `ClearedKind` is `#[non_exhaustive]`
        // only outside this crate: a new kind cannot be added without an
        // answer here to "what does this clearance's age make untrue?".
        match &self.clearance.kind {
            ClearedKind::Order { .. } => {
                if self.policy_revision != state.policy_revision {
                    return Err(Unevaluable::PolicyChanged.into());
                }
                let evaluated_at_ms = self.clearance.evaluated_at_ms;
                if self.now_ms < evaluated_at_ms {
                    return Err(Unevaluable::ClockWentBackwards {
                        now_ms: self.now_ms,
                        last_ms: evaluated_at_ms,
                    }
                    .into());
                }
                let age_ms = self.now_ms.saturating_sub(evaluated_at_ms);
                let max_age_ms = config.freshness.max_market_age_ms;
                if age_ms > max_age_ms {
                    return Err(Unevaluable::StaleClearance { age_ms, max_age_ms }.into());
                }
            }
            ClearedKind::ScheduleCancel {
                cancel_at_ms: Some(at),
            } => {
                let earliest_ms = self.now_ms.saturating_add(DEAD_MAN_MIN_LEAD_MS);
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
        if is_risk_reducing(request.action) {
            return Ok(());
        }
        if let Some((scope, engagement)) = state.kill.blocking(agent) {
            return Err(Refusal::TradingPaused {
                scope,
                since_ms: engagement.engaged_at_ms,
                reason: engagement.reason.clone(),
            });
        }
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
enum Mode {
    Fresh,
    Approved,
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
    fn mint_proposal(&mut self, agent: &AgentId, intent: &OrderIntent, now_ms: u64) -> String {
        self.sweep_proposals(now_ms);
        self.proposal_seq = self.proposal_seq.saturating_add(1);
        let id = format!("{}-{now_ms}-{}", agent.as_str(), self.proposal_seq);
        self.proposals.insert(
            id.clone(),
            Proposal {
                id: id.clone(),
                agent: agent.clone(),
                intent: intent.clone(),
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
    mode: Mode,
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
    if mode == Mode::Approved || mode == Mode::Preflight {
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

/// D1 is 1:1, and [`GuardrailEngine::register_agent`] is only one of the doors
/// into the registry. [`GuardrailStore`] is a public trait whose `save_vault`
/// any holder of the store can call, and a restart reads back whatever is on
/// disk — so a guard that lives only at registration is a guard on one door.
/// Two agents on one container is two sets of caps measured against one pool
/// of capital and a roster showing a segregation the venue does not enforce,
/// and no lookup downstream can pick a winner. The engine refuses to start
/// rather than start ambiguous.
///
/// Deterministic: `vaults` is a `BTreeMap`, so the agent named as the holder
/// is the same one on every run (`AGENTS.md` invariant 6).
fn check_containers_unique(vaults: &BTreeMap<AgentId, Address>) -> Result<(), GuardrailError> {
    let mut seen: Vec<(&Address, &AgentId)> = Vec::with_capacity(vaults.len());
    for (agent, vault) in vaults {
        if let Some((_, bound_to)) = seen.iter().find(|(held, _)| *held == vault) {
            return Err(GuardrailError::ContainerAlreadyBound {
                vault_address: *vault,
                bound_to: (*bound_to).clone(),
            });
        }
        seen.push((vault, agent));
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
