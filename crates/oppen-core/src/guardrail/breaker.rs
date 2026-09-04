//! The loss circuit breaker (spec item 25).
//!
//! Size caps bound one order. They do not stop an agent placing a thousand
//! correctly-sized orders and grinding the account to zero overnight, which
//! is why this exists as a separate predicate over PnL rather than over
//! order size. It runs per agent and account-wide, and a breach trips the
//! kill switch rather than merely refusing the order in hand — a budget that
//! is exhausted is exhausted until an operator looks at it.

use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::AgentId;
use super::config::LossLimits;
use super::kill::KillScope;
use super::snapshot::AccountSnapshot;

/// Which budget was exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LossKind {
    /// Mark-to-market PnL since the UTC day boundary.
    Daily,
    /// Peak equity minus current equity, all-time for the account.
    Drawdown,
}

impl fmt::Display for LossKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LossKind::Daily => f.write_str("daily"),
            LossKind::Drawdown => f.write_str("drawdown"),
        }
    }
}

/// An exhausted loss budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breach {
    pub scope: KillScope,
    pub kind: LossKind,
    /// The loss, as a positive number of dollars.
    pub observed_usd: Decimal,
    pub limit_usd: Decimal,
}

/// Checks one snapshot against one set of limits.
///
/// A budget is **exhausted at the limit, not past it**: a $25 daily budget
/// trips on a $25.00 loss. That is deliberately different from the size caps,
/// which allow a value exactly equal to the cap. A cap bounds a number the
/// agent chooses and may legitimately hit exactly; a budget is a quantity
/// being consumed, and reaching zero left is the event. The tests pin both
/// readings so neither can drift.
///
/// Returns the first breach found, daily before drawdown, because the daily
/// budget is the tighter and more actionable of the two.
pub fn check(scope: &KillScope, limits: &LossLimits, account: &AccountSnapshot) -> Option<Breach> {
    if let Some(limit) = limits.max_daily_loss_usd {
        let loss = Decimal::ZERO.saturating_sub(account.day_pnl_usd());
        if loss >= limit {
            return Some(Breach {
                scope: scope.clone(),
                kind: LossKind::Daily,
                observed_usd: loss,
                limit_usd: limit,
            });
        }
    }
    if let Some(limit) = limits.max_drawdown_usd {
        let drawdown = account
            .peak_equity_usd
            .saturating_sub(account.equity_usd)
            .max(Decimal::ZERO);
        if drawdown >= limit {
            return Some(Breach {
                scope: scope.clone(),
                kind: LossKind::Drawdown,
                observed_usd: drawdown,
                limit_usd: limit,
            });
        }
    }
    None
}

/// Runs the per-agent budget and then the account-wide one.
///
/// Order matters for the refusal an agent sees: its own budget is the one it
/// can reason about, so it is reported first. The account-wide breach carries
/// [`KillScope::Global`] and therefore stops every agent, not just this one.
pub fn check_all(
    agent: &AgentId,
    agent_limits: &LossLimits,
    agent_account: &AccountSnapshot,
    account_limits: &LossLimits,
    fleet: Option<&AccountSnapshot>,
) -> Option<Breach> {
    if let Some(breach) = check(
        &KillScope::agent(agent.clone()),
        agent_limits,
        agent_account,
    ) {
        return Some(breach);
    }
    let fleet = fleet?;
    check(&KillScope::Global, account_limits, fleet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::snapshot::AccountSnapshot;

    fn d(n: i64) -> Decimal {
        Decimal::from(n)
    }

    fn account(realized: Decimal, unrealized: Decimal, equity: i64, peak: i64) -> AccountSnapshot {
        AccountSnapshot {
            as_of_ms: 0,
            reconciled: true,
            equity_usd: d(equity),
            peak_equity_usd: d(peak),
            realized_pnl_today_usd: realized,
            unrealized_pnl_usd: unrealized,
            day_start_ms: 0,
            total_position_notional_usd: Decimal::ZERO,
            positions: Default::default(),
        }
    }

    fn daily(limit: i64) -> LossLimits {
        LossLimits {
            max_daily_loss_usd: Some(d(limit)),
            max_drawdown_usd: None,
        }
    }

    /// D-c's $25 daily budget, at the limit and one cent either side.
    #[test]
    fn a_daily_budget_is_exhausted_at_the_limit() {
        let scope = KillScope::agent("alpha");
        let cent = Decimal::from_parts(1, 0, 0, false, 2);
        let under = account(-d(25) + cent, Decimal::ZERO, 100, 125);
        assert!(check(&scope, &daily(25), &under).is_none());
        let at = account(-d(25), Decimal::ZERO, 100, 125);
        let breach = check(&scope, &daily(25), &at).expect("exactly at the budget trips");
        assert_eq!(breach.kind, LossKind::Daily);
        assert_eq!(breach.observed_usd, d(25));
        let past = account(-d(25) - cent, Decimal::ZERO, 100, 125);
        assert!(check(&scope, &daily(25), &past).is_some());
    }

    /// Unrealized losses count. An agent holding a losing position all night
    /// has spent the budget whether or not it has closed anything.
    #[test]
    fn unrealized_pnl_counts_against_the_daily_budget() {
        let scope = KillScope::agent("alpha");
        let a = account(d(5), -d(30), 100, 125);
        let breach = check(&scope, &daily(25), &a).expect("mark-to-market loss trips");
        assert_eq!(breach.observed_usd, d(25));
    }

    #[test]
    fn a_profitable_day_never_trips() {
        let scope = KillScope::agent("alpha");
        let a = account(d(1_000), d(500), 2_000, 2_000);
        assert!(check(&scope, &daily(25), &a).is_none());
    }

    #[test]
    fn drawdown_measures_peak_to_current_equity() {
        let scope = KillScope::agent("alpha");
        let limits = LossLimits {
            max_daily_loss_usd: None,
            max_drawdown_usd: Some(d(200)),
        };
        let under = account(Decimal::ZERO, Decimal::ZERO, 801, 1_000);
        assert!(check(&scope, &limits, &under).is_none());
        let at = account(Decimal::ZERO, Decimal::ZERO, 800, 1_000);
        assert_eq!(
            check(&scope, &limits, &at).map(|b| b.kind),
            Some(LossKind::Drawdown)
        );
        // Equity above the recorded peak is a zero drawdown, not a negative one.
        let above = account(Decimal::ZERO, Decimal::ZERO, 1_200, 1_000);
        assert!(check(&scope, &limits, &above).is_none());
    }

    #[test]
    fn unset_limits_never_trip() {
        let scope = KillScope::Global;
        let broke = account(-d(1_000_000), Decimal::ZERO, 1, 1_000_000);
        assert!(check(&scope, &LossLimits::UNSET, &broke).is_none());
    }

    #[test]
    fn the_account_wide_breach_is_global_scoped() {
        let agent = AgentId::new("alpha");
        let agent_account = account(Decimal::ZERO, Decimal::ZERO, 100, 100);
        let fleet = account(-d(500), Decimal::ZERO, 5_000, 5_500);
        let breach = check_all(
            &agent,
            &LossLimits::UNSET,
            &agent_account,
            &daily(400),
            Some(&fleet),
        )
        .expect("the fleet budget trips");
        assert_eq!(breach.scope, KillScope::Global);
    }

    #[test]
    fn the_agents_own_budget_is_reported_before_the_fleets() {
        let agent = AgentId::new("alpha");
        let agent_account = account(-d(30), Decimal::ZERO, 70, 100);
        let fleet = account(-d(500), Decimal::ZERO, 5_000, 5_500);
        let breach = check_all(
            &agent,
            &daily(25),
            &agent_account,
            &daily(400),
            Some(&fleet),
        )
        .expect("a breach");
        assert_eq!(breach.scope, KillScope::agent("alpha"));
    }
}
