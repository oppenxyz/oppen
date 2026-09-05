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
//! - **It takes no key.** The agent wallet is loaded from the engine's own
//!   key store, for the agent the clearance names. While the key was a
//!   parameter the seal had a hole exactly the size of the one it had already
//!   closed for `vaultAddress`, and a live one rather than a theoretical one:
//!   the revised D1 (V2) gives each agent a *top-level* Hyperliquid account
//!   that sends no `vaultAddress`, so the venue reads the account off the
//!   signature and the key **is** the container. Agent X's clearance signed
//!   with agent Y's key executed on Y's capital under X's caps, X's equity and
//!   X's kill switch — including while Y was paused.
//! - `sign_cleared` builds the pre-sign gate itself, over the engine and the
//!   clearance in hand. There is no checker parameter, so no caller can pass
//!   a permissive one, and the check runs inside the signer rather than
//!   beside it — which is what "immediately before signing" has to mean if
//!   it is to mean anything.
//! - The gate type is **private**. `GuardrailEngine` itself does not
//!   implement [`oppen_hl::exchange::PreSignCheck`], and that is load-bearing
//!   rather than tidy. While it did, both the engine and
//!   `ExchangeRequest::sign_checked` were public, so any caller could pass
//!   the real engine an action it had never evaluated and be handed a
//!   signature: the gate saw only the assembled wire request and had nothing
//!   to compare it against. The engine was its own rubber stamp. That call
//!   now fails to compile, because no value of the checker type exists
//!   outside `engine.rs`.
//!
//! **What that does not buy.** It is not the claim that nothing else can
//! sign. `oppen_hl::ExchangeRequest::sign_unchecked` and
//! `oppen_hl::AgentKey::sign_l1_action` are both `pub` — spec item 33's
//! manual escape hatch needs the former — so a module that wants to sign
//! without a clearance can, and a caller in another crate can still write
//! its *own* no-op `PreSignCheck`. That last one is now exactly as visible
//! as `sign_unchecked` and buys no more than it does: it is a struct someone
//! had to write, `sign_unchecked` is greppable by name, and
//! `tests::no_call_site_in_oppen_core_reaches_the_signer_unchecked` fails
//! this crate's suite if either name appears at a call site here. Closing
//! them workspace-wide means a `clippy.toml` with `disallowed-methods` for
//! both; that file does not exist yet.
//!
//! So the deliverable invariant is the narrower, true one: **a bypass cannot
//! happen by accident, and every deliberate one is one grep away.**
//!
//! One more thing the type does not buy, stated because it is the same
//! residual in a new place: the key store is an `Arc<dyn KeyStore>` fixed at
//! construction, so a caller that builds the engine with a store of its own
//! choosing chooses the keys. That is the constructor, not the signing path,
//! and it is the same trust the store and the audit sink already carry.
//!
//! What is enforced here, by spec item:
//!
//! | Item | Guardrail |
//! |---|---|
//! | 24 | symbol allowlist, max order notional, max position notional, order rate, reduce-only mode, max slippage, leverage cap |
//! | 25 | loss circuit breaker: max daily loss and max drawdown, per agent and account-wide, tripping the kill switch |
//! | 26 | kill switch per agent and global, persisted across restart, typed `trading_paused` |
//! | 27 | dead-man's switch: whether `scheduleCancel` should be armed, per container |
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
    AgentGuardrails, Freshness, GlobalRateBudget, LossLimits, MarginMode, OrderRate, RiskSettings,
};
pub use deadman::DeadManIntent;
pub use engine::{
    AuditEntry, AuditError, AuditOutcome, AuditSink, Clearance, Cleared, ClearedKind,
    GuardrailEngine, GuardrailError, OperatorAction, OrderIntent, Proposal, SignClearedError,
    Utilization, Verdict,
};
pub use kill::{Engagement, KillEffect, KillReason, KillScope, KillSwitch};
pub use refusal::{ReduceOnlyBreach, Refusal, Unevaluable, VenueRule};
pub use snapshot::{
    AccountSnapshot, Exposure, FeedQuality, MarketRef, MarketSnapshotRef, PositionSnapshot,
    RestingExposure,
};
pub use store::{GuardrailStore, PersistedState, SqliteGuardrailStore, StoreError};

/// Stable identity of one paired agent. D1 maps the roster 1:1 onto
/// **containers**, so an `AgentId` is also the identity of the venue account
/// whose guardrails, kill switch and loss budget are being evaluated.
///
/// Not "sub-account": the revised D1 (`docs/decisions.md` V1, V2) makes the
/// container a venue *account*, which on Hyperliquid v1 is a **top-level**
/// one carrying no `vaultAddress` at all. An `AgentId` that resolves to no
/// address is bound to a top-level container, not unbound — reading the two
/// as the same thing is what let the reconnect path re-point a live agent.
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
