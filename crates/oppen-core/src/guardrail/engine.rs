//! The engine, and the type that proves it ran.
//!
//! [`Cleared`] is the whole point of the module. It has private fields, no
//! `Clone`, no `Default`, and a constructor that is private to this file —
//! reachable only from the success branch of [`GuardrailEngine::decide`].
//! [`super::sign_cleared`] takes one by value. So inside `oppen-core` the
//! statement "there is no code path to the signer without a guardrail
//! evaluation" is a fact the compiler checks, not a rule a reviewer has to
//! remember, and a clearance authorises exactly one signature.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use rust_decimal::Decimal;
use serde::Serialize;

use oppen_hl::Action;
use oppen_hl::meta::Asset;
use oppen_hl::order::{OrderKind, OrderSpec};
use oppen_hl::wire::{BuilderInfo, CancelByCloidWire, CancelWire, Cloid, Grouping};

use super::AgentId;
use super::breaker;
use super::bucket::{BucketError, TokenBucket};
use super::config::{AgentGuardrails, LossLimits, OrderRate};
use super::deadman::{DEAD_MAN_MIN_LEAD_MS, DeadManIntent, DeadManPolicy};
use super::kill::{Engagement, KillEffect, KillReason, KillScope, KillSwitch};
use super::refusal::{ReduceOnlyBreach, Refusal, Unevaluable, VenueRule};
use super::snapshot::{AccountSnapshot, Exposure, MarketRef};
use super::store::{GuardrailStore, StoreError};

/// One basis point is a ten-thousandth.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
const HUNDRED: Decimal = Decimal::from_parts(100, 0, 0, false, 0);

/// Whether an operator has already approved this order (spec item 28).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// The agent placed the order directly. If the agent's guardrails have
    /// approval mode on, the evaluation ends in
    /// [`Refusal::ApprovalRequired`] after every hard predicate has passed.
    NotSupplied,
    /// An operator approved the proposal this order came from. The order is
    /// re-evaluated in full — item 28 re-prices at approval time — but the
    /// order-rate token is not charged twice, because it was already spent
    /// when the proposal was accepted.
    Granted { approval_id: String },
}

impl Approval {
    fn is_granted(&self) -> bool {
        matches!(self, Approval::Granted { .. })
    }
}

/// What an agent is asking to do, in decimals, before anything is rounded.
///
/// This is a request, not an order: nothing here reaches the wire until the
/// engine has rounded it to the asset's rules and every predicate has passed.
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
    /// interpreted here.
    pub reason: String,
    pub approval: Approval,
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
    pub fn action(&self) -> &Action {
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
pub enum AuditOutcome<'a> {
    Cleared(&'a Clearance),
    Refused(&'a Refusal),
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

#[derive(Debug, Default)]
struct EngineState {
    guardrails: BTreeMap<AgentId, AgentGuardrails>,
    account_limits: LossLimits,
    kill: KillSwitch,
    buckets: BTreeMap<AgentId, TokenBucket>,
    active: BTreeSet<AgentId>,
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
    state: Mutex<EngineState>,
}

impl std::fmt::Debug for GuardrailEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardrailEngine")
            .field("dead_man", &self.dead_man)
            .finish_non_exhaustive()
    }
}

impl GuardrailEngine {
    /// Loads persisted guardrails and kill-switch state (spec item 26: the
    /// switch survives a restart).
    pub fn new(
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
    ) -> Result<Self, GuardrailError> {
        Self::with_policy(store, sink, DeadManPolicy::default())
    }

    pub fn with_policy(
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
        dead_man: DeadManPolicy,
    ) -> Result<Self, GuardrailError> {
        let persisted = store.load()?;
        Ok(GuardrailEngine {
            store,
            sink,
            dead_man,
            state: Mutex::new(EngineState {
                guardrails: persisted.guardrails,
                account_limits: persisted.account_limits,
                kill: persisted.kill,
                buckets: BTreeMap::new(),
                active: BTreeSet::new(),
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
    /// The defaults include an empty symbol allowlist, so the agent's first
    /// order is refused with [`Refusal::SymbolNotAllowed`] naming the limit
    /// to raise. That refusal is the onboarding.
    pub fn register_agent(&self, agent: &AgentId) -> Result<AgentGuardrails, GuardrailError> {
        let mut state = self.state();
        if let Some(existing) = state.guardrails.get(agent) {
            return Ok(existing.clone());
        }
        let config = AgentGuardrails::default();
        self.store.save_guardrails(agent, &config)?;
        state.guardrails.insert(agent.clone(), config.clone());
        Ok(config)
    }

    /// Replaces one agent's guardrails. Validates first, so a value that
    /// could not be evaluated is rejected when it is typed rather than when
    /// an order needs a verdict.
    pub fn operator_set_guardrails(
        &self,
        agent: &AgentId,
        config: AgentGuardrails,
    ) -> Result<(), GuardrailError> {
        if let Err((field, detail)) = config.validate() {
            return Err(GuardrailError::InvalidConfig {
                field: field.to_owned(),
                detail,
            });
        }
        self.store.save_guardrails(agent, &config)?;
        self.state().guardrails.insert(agent.clone(), config);
        Ok(())
    }

    pub fn operator_set_account_limits(&self, limits: LossLimits) -> Result<(), GuardrailError> {
        self.store.save_account_limits(&limits)?;
        self.state().account_limits = limits;
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
        let mut state = self.state();
        let newly_engaged = state.kill.engage(
            scope.clone(),
            Engagement {
                engaged_at_ms: now_ms,
                reason,
            },
        );
        self.store.save_kill_switch(&state.kill)?;
        Ok(KillEffect {
            cancel_for: cancel_targets(&state, &scope),
            scope,
            newly_engaged,
        })
    }

    pub fn operator_release_kill(&self, scope: &KillScope) -> Result<bool, GuardrailError> {
        let mut state = self.state();
        let released = state.kill.release(scope);
        self.store.save_kill_switch(&state.kill)?;
        Ok(released)
    }

    pub fn guardrails(&self, agent: &AgentId) -> Option<AgentGuardrails> {
        self.state().guardrails.get(agent).cloned()
    }

    pub fn account_limits(&self) -> LossLimits {
        self.state().account_limits
    }

    pub fn kill_switch(&self) -> KillSwitch {
        self.state().kill.clone()
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
        let outcome = self.decide(agent, intent, asset, market, exposure, now_ms);
        self.record(Some(agent), now_ms, &intent.reason, &outcome)?;
        outcome
    }

    /// Writes the verdict to the ledger.
    ///
    /// A clearance that cannot be recorded is downgraded to a refusal: D6
    /// makes the ledger the single record of why an order happened, and an
    /// order signed without one is unexplainable afterwards. A *refusal*
    /// that cannot be recorded is still a refusal — losing the row is bad,
    /// but the safe outcome already happened.
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
        match (self.sink.record(&entry), outcome) {
            (Err(e), Ok(_)) => Err(Unevaluable::AuditWriteFailed {
                detail: e.to_string(),
            }
            .into()),
            (Err(e), Err(_)) => {
                tracing::warn!(agent = ?agent, error = %e, "refusal not recorded in the ledger");
                Ok(())
            }
            (Ok(()), _) => Ok(()),
        }
    }

    fn decide(
        &self,
        agent: &AgentId,
        intent: &OrderIntent,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
        now_ms: u64,
    ) -> Result<Cleared, Refusal> {
        if intent.reason.trim().is_empty() {
            return Err(Refusal::MissingReason);
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
            state.kill.engage(
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

        // Spec item 24, max position size, measured after this order fills.
        let position_after = checked(position_szi.checked_add(signed_sz), "post-fill position")?;
        let position_after_usd = checked(
            position_after.abs().checked_mul(reference_px),
            "post-fill position notional",
        )?;
        if position_after_usd > config.max_position_usd {
            return Err(Refusal::PositionNotional {
                symbol: intent.symbol.clone(),
                observed_usd: position_after_usd,
                limit_usd: config.max_position_usd,
            });
        }

        // Spec item 24, leverage cap. D3: the operator sets it, and the
        // venue's own maximum for the asset is a hard bound below it.
        let position_before_usd = checked(
            position_szi.abs().checked_mul(reference_px),
            "current position notional",
        )?;
        let total_after_usd = checked(
            account
                .total_position_notional_usd
                .checked_sub(position_before_usd)
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
        // costs the agent budget. An already-approved proposal does not pay
        // twice: the token was spent when the proposal was accepted.
        let rate = config.order_rate;
        let bucket = state.bucket_mut(agent, rate, now_ms);
        let spend = if intent.approval.is_granted() {
            bucket.refill(now_ms)
        } else {
            bucket.try_take(now_ms)
        };
        spend.map_err(|e| bucket_refusal(e, rate, now_ms))?;
        let tokens_remaining = bucket.tokens();

        // Spec item 28, checked last: a proposal that would have been refused
        // is refused rather than queued for a human to approve.
        if config.approval_required && !intent.approval.is_granted() {
            return Err(Refusal::ApprovalRequired {
                symbol: intent.symbol.clone(),
                notional_usd,
            });
        }

        let action = Action::Order {
            orders: vec![wire],
            grouping: intent.grouping,
            builder: intent.builder.clone(),
        };
        let clearance = Clearance {
            agent: Some(agent.clone()),
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
            },
        };
        Ok(Cleared::new(action, clearance))
    }

    /// Clears a cancel by order id.
    ///
    /// Risk-reducing, so it clears while the kill switch is engaged — item 26
    /// makes cancelling resting orders part of what the switch *does*, and
    /// item 10 says to reserve headroom for risk-reducing actions, so a
    /// cancel spends no rate token either.
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
        if reason.trim().is_empty() {
            return Err(Refusal::MissingReason);
        }
        if count == 0 {
            return Err(Unevaluable::InputMismatch {
                field: "cancels".to_owned(),
                expected: "at least one order".to_owned(),
                supplied: "none".to_owned(),
            }
            .into());
        }
        if !self.state().guardrails.contains_key(agent) {
            return Err(Unevaluable::UnknownAgent {
                agent: agent.clone(),
            }
            .into());
        }
        Ok(Cleared::new(
            action,
            Clearance {
                agent: Some(agent.clone()),
                evaluated_at_ms: now_ms,
                kind: ClearedKind::Cancel { count },
                utilization: Utilization::none(),
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
        Ok(Cleared::new(
            Action::ScheduleCancel { time: cancel_at_ms },
            Clearance {
                agent: None,
                evaluated_at_ms: now_ms,
                kind: ClearedKind::ScheduleCancel { cancel_at_ms },
                utilization: Utilization::none(),
            },
        ))
    }
}

impl Utilization {
    fn none() -> Self {
        Utilization {
            order_notional_pct: None,
            position_notional_pct: None,
            daily_loss_pct: None,
            leverage: Decimal::ZERO,
            order_tokens_remaining: Decimal::ZERO,
        }
    }
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
