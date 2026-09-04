//! The engine, and the type that proves it ran.
//!
//! [`Cleared`] is the whole point of the module. It has private fields, no
//! `Clone`, no `Default`, and a constructor that is private to this file —
//! reachable only from the success branch of [`GuardrailEngine::decide`].
//! [`super::sign_cleared`] takes one by value.
//!
//! What that buys, exactly: **a `Cleared` cannot exist unless an evaluation
//! produced it**, and it authorises one call to `sign_cleared`. That much the
//! compiler checks. It is not the same claim as "there is no code path to the
//! signer without a guardrail evaluation" — `oppen_hl::ExchangeRequest`
//! exposes `sign_unchecked`, and `AgentKey::sign_l1_action` below it, so a
//! module that wants to sign without a clearance can. Those are named to be
//! greppable rather than hidden, and closing them is an `oppen-hl` change (a
//! workspace `clippy.toml` `disallowed-methods` entry); see
//! [`super::sign_cleared`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use rust_decimal::Decimal;
use serde::Serialize;

use oppen_hl::meta::Asset;
use oppen_hl::order::{OrderKind, OrderSpec};
use oppen_hl::wire::{BuilderInfo, CancelByCloidWire, CancelWire, Cloid, Grouping};
use oppen_hl::{Action, Address, Network};

use super::AgentId;
use super::breaker;
use super::bucket::{BucketError, TokenBucket};
use super::config::{
    APPROVAL_TTL_MS, AgentGuardrails, GlobalRateBudget, LossLimits, MAX_REASON_BYTES, OrderRate,
};
use super::deadman::{DEAD_MAN_MIN_LEAD_MS, DeadManIntent, DeadManPolicy};
use super::kill::{Engagement, KillEffect, KillReason, KillScope, KillSwitch};
use super::refusal::{ReduceOnlyBreach, Refusal, Unevaluable, VenueRule};
use super::snapshot::{AccountSnapshot, Exposure, MarketRef, MarketSnapshotRef};
use super::store::{GuardrailStore, StoreError};

/// One basis point is a ten-thousandth.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
const HUNDRED: Decimal = Decimal::from_parts(100, 0, 0, false, 0);

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
    issued_at_ms: u64,
    expires_at_ms: u64,
}

impl Proposal {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// What the agent asked for. Item 28 re-prices at approval time, so the
    /// operator console shows this against the current market to display the
    /// drift.
    pub fn intent(&self) -> &OrderIntent {
        &self.intent
    }

    pub fn issued_at_ms(&self) -> u64 {
        self.issued_at_ms
    }

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
    pub leverage: Decimal,
    pub order_tokens_remaining: Decimal,
    /// What is left of spec item 10's address-wide request budget, which
    /// every agent under the master shares. Item 16 puts it in `get_state`
    /// next to the per-agent number, because they throttle for different
    /// reasons and an agent that only sees its own cap cannot tell why it is
    /// being refused.
    pub global_tokens_remaining: Decimal,
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
    /// `None` for an operator-scoped action such as the dead-man's switch,
    /// which belongs to no agent. Not a placeholder id: a fabricated agent
    /// name in the ledger would be indistinguishable from a real one.
    pub agent: Option<AgentId>,
    /// The sub-account this was evaluated against (D1), taken from the
    /// engine's own registry rather than from the caller. A clearance
    /// measured against agent X's positions and caps must not be signable
    /// with agent Y's `vaultAddress`, and the only way to guarantee that is
    /// for the binding to travel with the clearance.
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
}

impl Cleared {
    /// Private on purpose. Moving this line, widening it to `pub(crate)`, or
    /// adding a second constructor breaks `AGENTS.md` invariant 1.
    fn new(action: Action, clearance: Clearance) -> Self {
        Cleared { action, clearance }
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

    pub(super) fn into_parts(self) -> (Action, Clearance) {
        (self.action, self.clearance)
    }
}

/// A ledger write failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct AuditError {
    pub detail: String,
}

impl AuditError {
    pub fn new(detail: impl Into<String>) -> Self {
        AuditError {
            detail: detail.into(),
        }
    }
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
    /// `None` for an operator-scoped action; see [`Clearance::agent`].
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
/// For tests, and for a headless run before the ledger lands. Shipping this
/// in the desktop app would make every clearance unexplainable after the
/// fact, which D6 exists to prevent.
#[derive(Debug, Default)]
pub struct NullAuditSink;

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
}

#[derive(Debug)]
struct EngineState {
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
    dead_man: DeadManPolicy,
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
            .field("dead_man", &self.dead_man)
            .finish_non_exhaustive()
    }
}

impl GuardrailEngine {
    /// Loads persisted guardrails, sub-account bindings and kill-switch state
    /// (spec item 26: the switch survives a restart).
    pub fn new(
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
        network: Network,
    ) -> Result<Self, GuardrailError> {
        Self::with_policy(store, sink, network, DeadManPolicy::default())
    }

    pub fn with_policy(
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
        network: Network,
        dead_man: DeadManPolicy,
    ) -> Result<Self, GuardrailError> {
        let persisted = store.load()?;
        let global_budget = GlobalRateBudget::default();
        Ok(GuardrailEngine {
            store,
            sink,
            dead_man,
            network,
            state: Mutex::new(EngineState {
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

    /// Which network every clearance from this engine is bound to (D4, R4).
    pub fn network(&self) -> Network {
        self.network
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
    /// `vault_address` is the sub-account D1 pairs the agent with. It is
    /// recorded here and stamped onto every clearance the agent ever gets, so
    /// no later caller can supply a different one. `None` means the agent
    /// trades the master account directly, which is only the manual escape
    /// hatch.
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
        if let Some(vault) = &vault_address
            && state.vaults.get(agent) != Some(vault)
        {
            self.store.save_vault(agent, vault)?;
            state.vaults.insert(agent.clone(), *vault);
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
        self.store.save_guardrails(agent, &config)?;
        let before = self
            .state()
            .guardrails
            .insert(agent.clone(), config.clone());
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
        self.store.save_account_limits(&limits)?;
        let before = std::mem::replace(&mut self.state().account_limits, limits);
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

    pub fn global_rate_budget(&self) -> GlobalRateBudget {
        self.state().global_budget
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
            self.store.save_kill_switch(&state.kill)?;
            KillEffect {
                cancel_for: cancel_targets(&state, &scope),
                scope,
                newly_engaged,
            }
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
            let released = state.kill.release(scope);
            self.store.save_kill_switch(&state.kill)?;
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

    pub fn account_limits(&self) -> LossLimits {
        self.state().account_limits
    }

    pub fn kill_switch(&self) -> KillSwitch {
        self.state().kill.clone()
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
        let proposal = {
            let mut state = self.state();
            state.sweep_proposals(now_ms);
            state.proposals.get(approval_id).cloned().ok_or_else(|| {
                Unevaluable::UnknownProposal {
                    approval_id: approval_id.to_owned(),
                }
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
        if outcome.is_ok() {
            self.state().proposals.remove(approval_id);
        }
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

    /// Whether `scheduleCancel` should be armed right now (spec item 27).
    pub fn dead_man_intent(&self, now_ms: u64, armed_until_ms: Option<u64>) -> DeadManIntent {
        super::deadman::evaluate(
            &self.dead_man,
            now_ms,
            self.state().active.len(),
            armed_until_ms,
        )
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
        let resting_szi = resting.szi_of(&intent.symbol);
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

        // Spec item 24, max position size, measured after this order and
        // every order already working on the symbol fills.
        let position_after = checked(
            position_szi
                .checked_add(resting_szi)
                .and_then(|n| n.checked_add(signed_sz)),
            "post-fill position",
        )?;
        let position_after_usd = checked(
            position_after.abs().checked_mul(reference_px),
            "post-fill position notional",
        )?;
        let resting_usd = checked(
            resting_szi.abs().checked_mul(reference_px),
            "resting notional",
        )?;
        if position_after_usd > config.max_position_usd {
            return Err(Refusal::PositionNotional {
                symbol: intent.symbol.clone(),
                observed_usd: position_after_usd,
                resting_usd,
                limit_usd: config.max_position_usd,
            });
        }

        // Spec item 24, leverage cap. D3: the operator sets it, and the
        // venue's own maximum for the asset is a hard bound below it.
        //
        // This symbol's current contribution — filled plus working — is
        // replaced by the post-fill number; every other symbol's positions
        // and working orders stay in, which is why the account total and the
        // resting total are added before the subtraction.
        let symbol_before_usd = checked(
            position_szi
                .abs()
                .checked_mul(reference_px)
                .and_then(|n| n.checked_add(resting_usd)),
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
        if leverage > Decimal::from(leverage_cap) {
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
            Mode::Approved => bucket.refill(now_ms),
            Mode::Fresh => bucket.try_take(now_ms),
        };
        spend.map_err(|e| bucket_refusal(e, rate, now_ms))?;
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
            agent: Some(agent.clone()),
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
                daily_loss_pct: config.loss.max_daily_loss_usd.and_then(|limit| {
                    ratio_pct(
                        Decimal::ZERO
                            .saturating_sub(account.day_pnl_usd())
                            .max(Decimal::ZERO),
                        limit,
                    )
                }),
                leverage,
                order_tokens_remaining: tokens_remaining,
                global_tokens_remaining,
            },
        };
        Ok(Cleared::new(action, clearance))
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
                agent: Some(agent.clone()),
                vault_address: state.vaults.get(agent).copied(),
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
        intent: DeadManIntent,
        now_ms: u64,
    ) -> Result<Option<Cleared>, Refusal> {
        match intent {
            DeadManIntent::Hold => Ok(None),
            DeadManIntent::Disarm => self.clear_schedule_cancel(None, now_ms).map(Some),
            DeadManIntent::Arm { cancel_at_ms } => self
                .clear_schedule_cancel(Some(cancel_at_ms), now_ms)
                .map(Some),
        }
    }

    /// Clears a `scheduleCancel`. `None` disarms.
    ///
    /// Operator-scoped and risk-reducing: the switch exists so a dead process
    /// does not leave orders working, so nothing about an agent's guardrails
    /// can block it.
    pub fn clear_schedule_cancel(
        &self,
        cancel_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        let outcome = self.decide_schedule_cancel(cancel_at_ms, now_ms);
        self.record(None, now_ms, "dead-man's switch", &outcome)?;
        outcome
    }

    fn decide_schedule_cancel(
        &self,
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
        let global_tokens_remaining = draw_global_reserve(&mut self.state().global_bucket, now_ms);
        Ok(Cleared::new(
            Action::ScheduleCancel { time: cancel_at_ms },
            Clearance {
                agent: None,
                // Operator-scoped: the dead-man's switch belongs to the
                // account, not to any one sub-account.
                vault_address: None,
                network: self.network,
                evaluated_at_ms: now_ms,
                kind: ClearedKind::ScheduleCancel { cancel_at_ms },
                utilization: Utilization::none_with_global(global_tokens_remaining),
            },
        ))
    }
}

/// Whether this evaluation is an agent's first attempt or an operator
/// approving a proposal the engine minted (spec item 28).
///
/// The only two things `Approved` changes are the order-rate token, which was
/// spent when the proposal was minted, and the approval requirement itself.
/// Every other predicate runs again against fresh state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fresh,
    Approved,
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
                issued_at_ms: now_ms,
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
    // An approved proposal already spent its request when it was minted.
    if mode == Mode::Approved {
        return Ok(bucket.tokens());
    }
    if bucket.tokens() <= reserve {
        return Err(Refusal::GlobalRateBudget {
            tokens_available: bucket.tokens(),
            reserve: budget.reserve,
            retry_after_ms: bucket.retry_after_ms(),
        });
    }
    match bucket.try_take(now_ms) {
        Ok(()) => Ok(bucket.tokens()),
        Err(_) => Err(Refusal::GlobalRateBudget {
            tokens_available: bucket.tokens(),
            reserve: budget.reserve,
            retry_after_ms: bucket.retry_after_ms(),
        }),
    }
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
/// `None` when the arithmetic overflows, which the caller turns into a
/// refusal — the fail-closed reading of "this number is not representable".
fn adverse_slippage_bps(is_buy: bool, px: Decimal, reference_px: Decimal) -> Option<Decimal> {
    if reference_px <= Decimal::ZERO {
        return Some(Decimal::ZERO);
    }
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

/// `None` when the limit is zero (the ratio is undefined) or the arithmetic
/// overflows. Utilization is a display number, so an unrepresentable one is
/// simply absent rather than a refusal.
fn ratio_pct(observed: Decimal, limit: Decimal) -> Option<Decimal> {
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
