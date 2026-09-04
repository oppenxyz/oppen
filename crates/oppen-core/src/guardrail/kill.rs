//! The kill switch (spec item 26): per agent and global, persisted across
//! restart, and the thing the loss circuit breaker trips.
//!
//! Engaging pauses new orders **and** cancels resting ones. This type only
//! decides and records; issuing the cancels is the caller's job, and
//! [`KillEffect`] tells it whose orders to cancel. Flatten stays decoupled —
//! market-dumping every position is itself a destructive action, so it is
//! per position with a confirm and never a consequence of a trip.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::AgentId;
use super::breaker::LossKind;

/// Whose trading is paused.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum KillScope {
    /// Every agent, including ones paired after the switch was engaged.
    Global,
    Agent {
        agent: AgentId,
    },
}

impl KillScope {
    pub(super) fn agent(agent: impl Into<AgentId>) -> Self {
        KillScope::Agent {
            agent: agent.into(),
        }
    }
}

impl fmt::Display for KillScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KillScope::Global => f.write_str("global"),
            KillScope::Agent { agent } => write!(f, "agent {agent}"),
        }
    }
}

/// Why trading was paused. Carried into the refusal an agent receives, so
/// "you were stopped by your own daily loss budget" and "an operator stopped
/// you" are distinguishable without reading the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum KillReason {
    /// An operator pressed the switch in the console.
    Operator,
    /// Spec item 25's circuit breaker tripped.
    LossLimit {
        kind: LossKind,
        observed_usd: Decimal,
        limit_usd: Decimal,
    },
    /// D-b: the agent wallet reached its 90-day expiry, so oppen halts the
    /// agent and cancels resting orders while leaving positions open.
    AgentWalletExpired,
    /// Spec item 34: feeds went dark with exposure on.
    FeedFailure,
}

impl fmt::Display for KillReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KillReason::Operator => f.write_str("engaged by the operator"),
            KillReason::LossLimit {
                kind,
                observed_usd,
                limit_usd,
            } => write!(
                f,
                "{kind} loss ${observed_usd} against a ${limit_usd} limit"
            ),
            KillReason::AgentWalletExpired => f.write_str("the agent wallet expired"),
            KillReason::FeedFailure => f.write_str("market data failed"),
        }
    }
}

/// One engagement of the switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engagement {
    pub engaged_at_ms: u64,
    pub reason: KillReason,
}

/// What the caller must do now that the switch is engaged.
///
/// `newly_engaged` is false when the scope was already engaged, so a repeated
/// trip does not re-issue cancels. `cancel_for` lists the agents whose
/// resting orders must be cancelled: for a global engagement that is every
/// agent the engine knows about, since item 26 pauses all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillEffect {
    pub scope: KillScope,
    pub newly_engaged: bool,
    pub cancel_for: BTreeSet<AgentId>,
}

/// The persisted kill-switch state: one optional global engagement plus a
/// per-agent map.
///
/// `BTreeMap`, not `HashMap`: this value is serialized into SQLite and read
/// back on the next launch, and `AGENTS.md` invariant 6 wants one byte
/// sequence per state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSwitch {
    global: Option<Engagement>,
    agents: BTreeMap<AgentId, Engagement>,
}

impl KillSwitch {
    pub(super) fn new() -> Self {
        KillSwitch::default()
    }

    /// The engagement that blocks this agent, global first.
    ///
    /// Global is checked first because it is the broader statement: an
    /// operator who stopped everything should see that reason in the
    /// refusal, not whichever per-agent trip happened to be older.
    pub(super) fn blocking(&self, agent: &AgentId) -> Option<(KillScope, &Engagement)> {
        if let Some(engagement) = &self.global {
            return Some((KillScope::Global, engagement));
        }
        self.agents
            .get(agent)
            .map(|e| (KillScope::agent(agent.clone()), e))
    }

    /// Whether one scope is engaged, independently of any agent.
    /// Test-only: the engine asks [`KillSwitch::blocking`], and a console
    /// reads the serialized switch out of `get_state`.
    #[cfg(test)]
    pub(super) fn is_engaged(&self, scope: &KillScope) -> bool {
        match scope {
            KillScope::Global => self.global.is_some(),
            KillScope::Agent { agent } => self.agents.contains_key(agent),
        }
    }

    pub(super) fn global(&self) -> Option<&Engagement> {
        self.global.as_ref()
    }

    /// Engages the switch. Idempotent: re-engaging an already-engaged scope
    /// keeps the original timestamp and reason, because the first trip is
    /// the one that explains what happened.
    pub(super) fn engage(&mut self, scope: KillScope, engagement: Engagement) -> bool {
        match scope {
            KillScope::Global => {
                if self.global.is_some() {
                    return false;
                }
                self.global = Some(engagement);
                true
            }
            KillScope::Agent { agent } => {
                if self.agents.contains_key(&agent) {
                    return false;
                }
                self.agents.insert(agent, engagement);
                true
            }
        }
    }

    /// Releases the switch. Releasing global does not release per-agent
    /// engagements: an agent stopped by its own loss breaker stays stopped
    /// when the operator lifts the global pause.
    pub(super) fn release(&mut self, scope: &KillScope) -> bool {
        match scope {
            KillScope::Global => self.global.take().is_some(),
            KillScope::Agent { agent } => self.agents.remove(agent).is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engagement(at: u64) -> Engagement {
        Engagement {
            engaged_at_ms: at,
            reason: KillReason::Operator,
        }
    }

    #[test]
    fn global_engagement_blocks_every_agent_and_wins_the_reason() {
        let mut k = KillSwitch::new();
        let a = AgentId::new("alpha");
        let b = AgentId::new("beta");
        assert!(k.blocking(&a).is_none());
        k.engage(KillScope::agent(a.clone()), engagement(1));
        assert!(k.blocking(&a).is_some());
        assert!(k.blocking(&b).is_none());
        k.engage(KillScope::Global, engagement(2));
        assert_eq!(k.blocking(&b).map(|(s, _)| s), Some(KillScope::Global));
        assert_eq!(k.blocking(&a).map(|(s, _)| s), Some(KillScope::Global));
    }

    #[test]
    fn releasing_global_leaves_a_per_agent_trip_engaged() {
        let mut k = KillSwitch::new();
        let a = AgentId::new("alpha");
        k.engage(KillScope::agent(a.clone()), engagement(1));
        k.engage(KillScope::Global, engagement(2));
        assert!(k.release(&KillScope::Global));
        assert_eq!(
            k.blocking(&a).map(|(s, _)| s),
            Some(KillScope::agent(a.clone()))
        );
        assert!(k.release(&KillScope::agent(a.clone())));
        assert!(k.blocking(&a).is_none());
    }

    #[test]
    fn re_engaging_keeps_the_first_trip() {
        let mut k = KillSwitch::new();
        assert!(k.engage(KillScope::Global, engagement(1)));
        assert!(!k.engage(KillScope::Global, engagement(9)));
        assert_eq!(k.global().map(|e| e.engaged_at_ms), Some(1));
    }
}
