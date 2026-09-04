//! The guardrail engine: the single evaluation every order passes through
//! immediately before it is signed.
//!
//! `AGENTS.md` invariant 1 says there must be exactly one code path to the
//! signer and it must run the guardrail check. This module makes part of
//! that a type rather than a convention. [`Cleared`] has private fields and
//! a private constructor reachable only from the success branch of
//! [`GuardrailEngine::evaluate`]; [`sign_cleared`] is the only signing entry
//! point oppen-core exposes, and it consumes a [`Cleared`] by value.
//!
//! **What the compiler actually enforces, and what it does not.** A
//! `Cleared` cannot be constructed except by an evaluation, so a value of
//! that type is proof one ran; and because `sign_cleared` takes it by value
//! and `Cleared` is not `Clone`, one evaluation authorises one signature.
//! That is the whole guarantee. It is *not* the guarantee that nothing else
//! can sign: `oppen_hl::ExchangeRequest::sign_unchecked` and
//! `oppen_hl::AgentKey::sign_l1_action` are both `pub`, so any module can
//! build and sign an arbitrary action without ever producing a `Cleared`.
//! Those are deliberately named to be greppable rather than hidden, and this
//! file may not modify `oppen-hl`. Closing them means a workspace
//! `clippy.toml` with `disallowed-methods` for both, so a bypass fails
//! `cargo clippy -D warnings` everywhere except [`sign_cleared`]. Until that
//! lands, single-path is a convention backed by review, and the claim in
//! this doc is the narrower one above.
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

use oppen_hl::{AgentKey, ExchangeRequest, Network, exchange::PreSign, exchange::PreSignCheck};
use serde::{Deserialize, Serialize};

pub use breaker::{Breach, LossKind};
pub use bucket::TokenBucket;
pub use config::{
    APPROVAL_TTL_MS, AgentGuardrails, DEFAULT_DAILY_LOSS_USD, DEFAULT_MARK_DIVERGENCE_BPS,
    DEFAULT_MARK_DIVERGENCE_WINDOW_MS, DEFAULT_MAX_LEVERAGE, DEFAULT_MAX_ORDER_USD,
    DEFAULT_MAX_POSITION_USD, DEFAULT_MAX_SLIPPAGE_BPS, DEFAULT_ORDER_RATE, Freshness,
    GlobalRateBudget, LossLimits, MAX_REASON_BYTES, MarginMode, OrderRate, RiskSettings,
};
pub use deadman::{DEAD_MAN_MIN_LEAD_MS, DeadManIntent, DeadManPolicy};
pub use engine::{
    AuditEntry, AuditError, AuditOutcome, AuditSink, Clearance, Cleared, ClearedKind,
    GuardrailEngine, GuardrailError, NullAuditSink, OperatorAction, OrderIntent, Proposal,
    Utilization,
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
/// **The sub-account and the network are not parameters.** They come out of
/// the clearance, which took them from the engine's own per-agent registry
/// and its constructor-time network. When a caller supplied them, a clearance
/// evaluated against agent X's positions and caps could be signed with agent
/// Y's `vaultAddress`, and one evaluated by the testnet engine against
/// testnet limits could be signed for mainnet — which R4 calls the worst bug
/// this product can ship and which R4's per-network file boundary was meant
/// to make unrepresentable. The nonce and `expires_after` stay parameters:
/// they belong to the submit queue (spec item 7), not to the risk decision.
///
/// It signs through `oppen_hl::ExchangeRequest::sign_checked`, whose gate is
/// [`ClearedGate`] below. `oppen-hl` also exposes `sign_unchecked` and
/// `AgentKey::sign_l1_action`, so this is the *supported* path rather than
/// the only possible one; see the module doc for the `clippy.toml` change
/// that would close them.
pub fn sign_cleared(
    key: &AgentKey,
    cleared: Cleared,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<(ExchangeRequest, Clearance), SignClearedError> {
    let (action, clearance) = cleared.into_parts();
    let gate = ClearedGate {
        vault_address: clearance.vault_address,
        network: clearance.network,
    };
    let request = ExchangeRequest::sign_checked(
        key,
        action,
        nonce,
        clearance.vault_address,
        expires_after,
        clearance.network,
        &gate,
    )
    .map_err(|e| match e {
        oppen_hl::exchange::SignError::Signing(e) => SignClearedError::Signing(e),
        oppen_hl::exchange::SignError::Refused(detail) => {
            SignClearedError::ClearanceMismatch { detail }
        }
    })?;
    Ok((request, clearance))
}

/// Why a cleared action was not signed.
#[derive(Debug, thiserror::Error)]
pub enum SignClearedError {
    /// The request reaching the signer did not carry the sub-account or the
    /// network the clearance was evaluated for. Unreachable through
    /// [`sign_cleared`], which takes both from the clearance itself; it
    /// exists because this is the boundary where that binding is asserted
    /// rather than assumed.
    #[error("the request does not match its clearance: {detail}")]
    ClearanceMismatch { detail: String },
    #[error(transparent)]
    Signing(#[from] oppen_hl::Error),
}

/// Asserts, at the signer, that the request being signed carries the
/// sub-account and network the clearance was evaluated for.
struct ClearedGate {
    vault_address: Option<oppen_hl::Address>,
    network: Network,
}

impl PreSignCheck for ClearedGate {
    type Refusal = String;

    fn check(&self, request: PreSign<'_>) -> Result<(), String> {
        if request.network != self.network {
            return Err(format!(
                "clearance is for {:?} but the request is for {:?}",
                self.network, request.network
            ));
        }
        if request.vault_address != self.vault_address {
            return Err(format!(
                "clearance is for vault {:?} but the request is for {:?}",
                self.vault_address, request.vault_address
            ));
        }
        Ok(())
    }
}
