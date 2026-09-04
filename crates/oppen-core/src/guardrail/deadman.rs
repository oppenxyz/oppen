//! The dead-man's switch (spec item 27).
//!
//! Hyperliquid's `scheduleCancel` cancels every resting order of an account
//! at a stated time unless the time is pushed forward first. oppen keeps it
//! armed while any agent is active, so a crashed process, a closed laptop or
//! a killed daemon does not leave orders working with nothing watching them.
//!
//! This module only decides. It holds no timer and issues no request: it
//! answers "given the clock, who is active and what is currently armed, what
//! should the arm state be", and the caller acts. That keeps the decision
//! pure and testable, and keeps `oppen-core` free of a scheduler (R1).

use serde::{Deserialize, Serialize};

/// The venue requires a scheduled cancel to be at least five seconds in the
/// future. Sourced from the Hyperliquid exchange-endpoint documentation for
/// `scheduleCancel`; not yet confirmed against the live API, because
/// `scheduleCancel` is a signed action and there is no funded key.
pub const DEAD_MAN_MIN_LEAD_MS: u64 = 5_000;

/// How far ahead to schedule and when to push the deadline forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadManPolicy {
    /// How far ahead of now the cancel is scheduled. Clamped up to
    /// [`DEAD_MAN_MIN_LEAD_MS`] — a shorter lead is a request the venue
    /// would reject, and silently not arming is the worst outcome here.
    pub lead_ms: u64,
    /// Re-arm once the armed deadline is nearer than this. A third of the
    /// lead by default, so two consecutive refreshes can fail before the
    /// switch actually fires.
    pub refresh_at_remaining_ms: u64,
}

impl Default for DeadManPolicy {
    fn default() -> Self {
        DeadManPolicy {
            lead_ms: 60_000,
            refresh_at_remaining_ms: 20_000,
        }
    }
}

impl DeadManPolicy {
    fn effective_lead_ms(&self) -> u64 {
        self.lead_ms.max(DEAD_MAN_MIN_LEAD_MS)
    }
}

/// What the caller should do with `scheduleCancel` right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadManIntent {
    /// Send `scheduleCancel { time: cancel_at_ms }`.
    Arm { cancel_at_ms: u64 },
    /// Send `scheduleCancel {}` with no time, which disarms.
    Disarm,
    /// Nothing to do.
    Hold,
}

/// Decides the arm state.
///
/// Arms when at least one agent is active and nothing is armed, or when the
/// armed deadline is closer than the refresh threshold. Disarms when no agent
/// is active and something is armed — otherwise an operator's own resting
/// orders would be cancelled by an agent's absence.
///
/// A deadline already in the past is treated as not armed: the venue has
/// fired it, and arming again is the correct move while agents are active.
pub fn evaluate(
    policy: &DeadManPolicy,
    now_ms: u64,
    active_agents: usize,
    armed_until_ms: Option<u64>,
) -> DeadManIntent {
    let armed = armed_until_ms.filter(|until| *until > now_ms);
    if active_agents == 0 {
        return if armed.is_some() {
            DeadManIntent::Disarm
        } else {
            DeadManIntent::Hold
        };
    }
    let cancel_at_ms = now_ms.saturating_add(policy.effective_lead_ms());
    match armed {
        None => DeadManIntent::Arm { cancel_at_ms },
        Some(until) if until - now_ms <= policy.refresh_at_remaining_ms => {
            DeadManIntent::Arm { cancel_at_ms }
        }
        Some(_) => DeadManIntent::Hold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arms_when_an_agent_becomes_active_and_disarms_when_none_are() {
        let p = DeadManPolicy::default();
        assert_eq!(evaluate(&p, 1_000, 0, None), DeadManIntent::Hold);
        assert_eq!(
            evaluate(&p, 1_000, 1, None),
            DeadManIntent::Arm {
                cancel_at_ms: 61_000
            }
        );
        assert_eq!(evaluate(&p, 1_000, 0, Some(61_000)), DeadManIntent::Disarm);
    }

    #[test]
    fn refreshes_only_once_the_deadline_is_close() {
        let p = DeadManPolicy::default();
        assert_eq!(evaluate(&p, 1_000, 2, Some(41_000)), DeadManIntent::Hold);
        assert_eq!(
            evaluate(&p, 1_000, 2, Some(21_000)),
            DeadManIntent::Arm {
                cancel_at_ms: 61_000
            }
        );
    }

    #[test]
    fn an_expired_deadline_counts_as_unarmed() {
        let p = DeadManPolicy::default();
        assert_eq!(
            evaluate(&p, 100_000, 1, Some(99_999)),
            DeadManIntent::Arm {
                cancel_at_ms: 160_000
            }
        );
        assert_eq!(evaluate(&p, 100_000, 0, Some(99_999)), DeadManIntent::Hold);
    }

    #[test]
    fn a_lead_shorter_than_the_venue_minimum_is_clamped_up() {
        let p = DeadManPolicy {
            lead_ms: 1_000,
            refresh_at_remaining_ms: 500,
        };
        assert_eq!(
            evaluate(&p, 0, 1, None),
            DeadManIntent::Arm {
                cancel_at_ms: DEAD_MAN_MIN_LEAD_MS
            }
        );
    }
}
