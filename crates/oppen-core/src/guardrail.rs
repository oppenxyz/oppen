//! The guardrail engine: the single evaluation every order passes through
//! immediately before it is signed.
//!
//! `AGENTS.md` invariant 1 says there must be exactly one code path to the
//! signer and it must run the guardrail check. This module makes that a
//! type, not a convention. [`Cleared`] has private fields and a private
//! constructor reachable only from the success branch of
//! [`GuardrailEngine::evaluate`]; [`sign_cleared`] is the only signing entry
//! point oppen-core exposes, and it consumes a [`Cleared`] by value. No
//! `Cleared`, no signature; one `Cleared`, one signature.
//!
//! The seal is airtight inside this crate. It is not yet airtight across the
//! crate boundary, because `oppen_hl::ExchangeRequest::sign` is still `pub`
//! and this module may not modify `oppen-hl`. See the crate README task list
//! and the note on [`sign_cleared`] for the change oppen-hl needs.
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

use oppen_hl::{Address, AgentKey, ExchangeRequest, Network};
use serde::{Deserialize, Serialize};

pub use breaker::{Breach, LossKind};
pub use bucket::TokenBucket;
pub use config::{
    AgentGuardrails, DEFAULT_DAILY_LOSS_USD, DEFAULT_MARK_DIVERGENCE_BPS,
    DEFAULT_MARK_DIVERGENCE_WINDOW_MS, DEFAULT_MAX_LEVERAGE, DEFAULT_MAX_ORDER_USD,
    DEFAULT_MAX_POSITION_USD, DEFAULT_MAX_SLIPPAGE_BPS, DEFAULT_ORDER_RATE, Freshness, LossLimits,
    MarginMode, OrderRate, RiskSettings,
};
pub use deadman::{DEAD_MAN_MIN_LEAD_MS, DeadManIntent, DeadManPolicy};
pub use engine::{
    Approval, AuditEntry, AuditError, AuditOutcome, AuditSink, Clearance, Cleared, ClearedKind,
    GuardrailEngine, GuardrailError, NullAuditSink, OrderIntent, Utilization,
};
pub use kill::{Engagement, KillEffect, KillReason, KillScope, KillSwitch};
pub use refusal::{ReduceOnlyBreach, Refusal, Unevaluable, VenueRule};
pub use snapshot::{AccountSnapshot, Exposure, FeedQuality, MarketRef, PositionSnapshot};
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

/// Signs a cleared action. This is the only signing entry point oppen-core
/// offers, and the only one the MCP gateway and the desktop app are meant to
/// call (`AGENTS.md` invariant 1).
///
/// It takes [`Cleared`] **by value** on purpose: a clearance is spent by the
/// signature it authorises, so the same evaluation cannot be replayed into a
/// second order. The returned [`Clearance`] is the audit record of the
/// evaluation that authorised this exact request, for the hash-chained ledger
/// (D6).
///
/// `oppen-hl` still exposes `ExchangeRequest::sign`, so today this is the
/// *supported* path rather than the *only possible* path. Closing that is an
/// oppen-hl change: rename its constructor `sign_unchecked`, mark it
/// `#[doc(hidden)]`, and add a workspace `clippy.toml` with
/// `disallowed-methods = ["oppen_hl::ExchangeRequest::sign_unchecked"]` so a
/// bypass fails `cargo clippy -D warnings` in CI everywhere except this
/// function.
pub fn sign_cleared(
    key: &AgentKey,
    cleared: Cleared,
    nonce: u64,
    vault_address: Option<Address>,
    expires_after: Option<u64>,
    network: Network,
) -> Result<(ExchangeRequest, Clearance), oppen_hl::Error> {
    let (action, clearance) = cleared.into_parts();
    let request = ExchangeRequest::sign(key, action, nonce, vault_address, expires_after, network)?;
    Ok((request, clearance))
}
