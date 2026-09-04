//! The guardrail engine: the single evaluation every order passes through
//! immediately before it is signed.
//!
//! `AGENTS.md` invariant 1 says there must be exactly one code path to the
//! signer and it must run the guardrail check. This module makes most of
//! that a type rather than a convention:
//!
//! - [`Cleared`] has private fields and a constructor private to `engine.rs`,
//!   reachable only from the success branch of an evaluation. A value of that
//!   type is proof one ran.
//! - [`GuardrailEngine::sign_cleared`] is the only signing entry point
//!   `oppen-core` exposes. It consumes a `Cleared` by value, and `Cleared` is
//!   not `Clone`, so one evaluation authorises exactly one signature.
//! - The engine implements [`oppen_hl::exchange::PreSignCheck`] and hands
//!   *itself* to `ExchangeRequest::sign_checked`. There is no checker
//!   parameter, so no caller can pass a permissive one, and the check runs
//!   inside the signer rather than beside it — which is what "immediately
//!   before signing" has to mean if it is to mean anything.
//!
//! **What that does not buy.** It is not the claim that nothing else can
//! sign. `oppen_hl::ExchangeRequest::sign_unchecked` and
//! `oppen_hl::AgentKey::sign_l1_action` are both `pub` — spec item 33's
//! manual escape hatch needs the former — so a module that wants to sign
//! without a clearance can, and a caller in another crate can implement
//! `PreSignCheck` as a no-op. Both are visible rather than hidden: a no-op
//! gate is a struct someone had to write, `sign_unchecked` is greppable by
//! name, and `tests::no_call_site_in_oppen_core_reaches_the_signer_unchecked`
//! fails this crate's suite if either name appears at a call site here.
//! Closing them workspace-wide means a `clippy.toml` with
//! `disallowed-methods` for both; that file does not exist yet.
//!
//! So the deliverable invariant is the narrower, true one: **a bypass cannot
//! happen by accident, and every deliberate one is one grep away.**
//!
//! What is enforced here, by spec item:
//!
//! | Item | Guardrail |
//! |---|---|
//! | 24 | symbol allowlist, max order notional, max position notional, order rate, reduce-only mode, max slippage, leverage cap |
//! | 25 | loss circuit breaker: max daily loss and max drawdown, per agent and account-wide, tripping the kill switch |
//! | 26 | kill switch per agent and global, persisted across restart, typed `trading_paused` |
//! | 27 | dead-man's switch: whether `scheduleCancel` should be armed |
//! | D3 | leverage and margin mode are operator-set, readable, never agent-writable |
//! | D-c | a newly paired agent starts near-zero and the first refusal names the limit to raise |
//!
//! Two rules run through all of it. **Fail closed**: if the engine cannot
//! evaluate — stale market data, a missing reference price, an unreconciled
//! account, a ledger write that failed — the order is refused, because
//! uncertainty about whether a limit is breached is treated as a breach.
//! **Structured refusal**: every rejection is a [`Refusal`] carrying the
//! failed predicate, the observed value and the configured limit, so an agent
//! can adapt and an operator can read it (`AGENTS.md` invariant 8).

mod breaker;
mod bucket;
mod config;
mod deadman;
mod engine;
mod kill;
mod refusal;
mod snapshot;
mod store;

#[cfg(test)]
mod tests;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use breaker::LossKind;
pub use config::{
    APPROVAL_TTL_MS, AgentGuardrails, DEFAULT_DAILY_LOSS_USD, DEFAULT_MARK_DIVERGENCE_BPS,
    DEFAULT_MARK_DIVERGENCE_WINDOW_MS, DEFAULT_MAX_LEVERAGE, DEFAULT_MAX_ORDER_USD,
    DEFAULT_MAX_POSITION_USD, DEFAULT_MAX_SLIPPAGE_BPS, DEFAULT_ORDER_RATE, Freshness,
    GlobalRateBudget, LossLimits, MAX_REASON_BYTES, MarginMode, OrderRate, RiskSettings,
};
pub use deadman::{DEAD_MAN_MIN_LEAD_MS, DeadManIntent};
pub use engine::{
    AuditEntry, AuditError, AuditOutcome, AuditSink, Clearance, Cleared, ClearedKind,
    GuardrailEngine, GuardrailError, NullAuditSink, OperatorAction, OrderIntent, Proposal,
    SignClearedError, Utilization,
};
pub use kill::{Engagement, KillEffect, KillReason, KillScope, KillSwitch};
pub use refusal::{ReduceOnlyBreach, Refusal, Unevaluable, VenueRule};
pub use snapshot::{
    AccountSnapshot, Exposure, FeedQuality, MarketRef, MarketSnapshotRef, PositionSnapshot,
    RestingExposure,
};
pub use store::{GuardrailStore, MemoryStore, PersistedState, SqliteGuardrailStore, StoreError};

/// Stable identity of one paired agent. D1 maps the roster 1:1 onto
/// sub-accounts, so an `AgentId` is also the identity of the sub-account
/// whose guardrails, kill switch and loss budget are being evaluated.
///
/// Ordered rather than hashed: every collection keyed by an agent is a
/// `BTreeMap`, so anything this module serializes or persists has one
/// deterministic byte sequence (`AGENTS.md` invariant 6).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(id: impl Into<String>) -> Self {
        AgentId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AgentId {
    fn from(s: &str) -> Self {
        AgentId(s.to_owned())
    }
}
