//! The dead-man's switch (spec item 27).
//!
//! Hyperliquid's `scheduleCancel` cancels every resting order of an account
//! at a stated time unless the time is pushed forward first. oppen keeps it
//! armed while the agent that owns that account is active, so a crashed
//! process, a closed laptop or a killed daemon does not leave orders working
//! with nothing watching them.
//!
//! **The decision is per container.** Item 27: "armed per container, N times,
//! not once" — `scheduleCancel` is per address, so N containers are N
//! independent arming duties and N separate ten-trigger daily budgets, and no
//! container is covered by another's arming. A fleet-wide answer would arm one
//! address and leave every other agent's orders unwatched while the console
//! reported the roster covered.
//!
//! This module only decides. It holds no timer and issues no request: it
//! answers "given the clock, who is active and what is currently armed, what
//! should the arm state be", and the caller acts. That keeps the decision
//! pure and testable, and keeps `oppen-core` free of a scheduler (R1).

/// The venue requires a scheduled cancel to be at least five seconds in the
/// future. Sourced from the Hyperliquid exchange-endpoint documentation for
/// `scheduleCancel`; not yet confirmed against the live API, because
/// `scheduleCancel` is a signed action and there is no funded key.
pub(super) const DEAD_MAN_MIN_LEAD_MS: u64 = 5_000;

/// How far ahead of now the cancel is scheduled. Twelve times the venue's
/// [`DEAD_MAN_MIN_LEAD_MS`], so the arm request is never one the venue would
/// reject — silently not arming is the worst outcome here.
const LEAD_MS: u64 = 60_000;
const _: () = assert!(
    LEAD_MS > DEAD_MAN_MIN_LEAD_MS,
    "the scheduled lead must clear the venue minimum, or arming silently fails"
);

/// Re-arm once the armed deadline is nearer than this. A third of the lead,
/// so two consecutive refreshes can fail before the switch actually fires.
const REFRESH_AT_REMAINING_MS: u64 = 20_000;

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
/// Arms when this container's agent is active and nothing is armed, or when
/// the armed deadline is closer than the refresh threshold. Disarms when the
/// agent is gone and something is armed — otherwise an operator's own resting
/// orders on that account would be cancelled by an agent's absence.
///
/// A deadline already in the past is treated as not armed: the venue has
/// fired it, and arming again is the correct move while the agent is active.
pub(super) fn evaluate(now_ms: u64, active: bool, armed_until_ms: Option<u64>) -> DeadManIntent {
    let armed = armed_until_ms.filter(|until| *until > now_ms);
    if !active {
        return if armed.is_some() {
            DeadManIntent::Disarm
        } else {
            DeadManIntent::Hold
        };
    }
    let cancel_at_ms = now_ms.saturating_add(LEAD_MS);
    match armed {
        None => DeadManIntent::Arm { cancel_at_ms },
        Some(until) if until - now_ms <= REFRESH_AT_REMAINING_MS => {
            DeadManIntent::Arm { cancel_at_ms }
        }
        Some(_) => DeadManIntent::Hold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arms_when_the_containers_agent_is_active_and_disarms_when_it_is_not() {
        assert_eq!(evaluate(1_000, false, None), DeadManIntent::Hold);
        assert_eq!(
            evaluate(1_000, true, None),
            DeadManIntent::Arm {
                cancel_at_ms: 61_000
            }
        );
        assert_eq!(evaluate(1_000, false, Some(61_000)), DeadManIntent::Disarm);
    }

    #[test]
    fn refreshes_only_once_the_deadline_is_close() {
        assert_eq!(evaluate(1_000, true, Some(41_000)), DeadManIntent::Hold);
        assert_eq!(
            evaluate(1_000, true, Some(21_000)),
            DeadManIntent::Arm {
                cancel_at_ms: 61_000
            }
        );
    }

    #[test]
    fn an_expired_deadline_counts_as_unarmed() {
        assert_eq!(
            evaluate(100_000, true, Some(99_999)),
            DeadManIntent::Arm {
                cancel_at_ms: 160_000
            }
        );
        assert_eq!(evaluate(100_000, false, Some(99_999)), DeadManIntent::Hold);
    }
}
