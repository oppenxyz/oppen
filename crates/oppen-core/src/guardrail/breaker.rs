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
pub(super) struct Breach {
    pub scope: KillScope,
    pub kind: LossKind,
    /// The loss, as a positive number of dollars.
    pub observed_usd: Decimal,
    pub limit_usd: Decimal,
}

/// How many dollars of one budget the account has consumed.
///
/// **The one place this arithmetic lives.** [`check`] fires on it and
/// [`gauge`] reports it, so "the gauge reads 96% while the breaker has
/// already tripped" is not a state this file can produce. It was two copies
/// of the daily half before the gauge existed — one here, one inlined in the
/// engine's `Utilization` — and two copies of a number that decides whether
/// an account keeps trading is the drift `AGENTS.md` invariant 1 is about.
///
/// Signed for the daily budget, because a profitable day consumes a negative
/// amount of it and clamping that to zero would make a zero-dollar budget
/// trip on a profit. Non-negative for drawdown, where the venue's peak is a
/// high-water mark and equity above it is a new peak, not a negative
/// drawdown.
fn consumed_usd(kind: LossKind, account: &AccountSnapshot) -> Decimal {
    match kind {
        LossKind::Daily => Decimal::ZERO.saturating_sub(account.day_pnl_usd()),
        LossKind::Drawdown => account
            .peak_equity_usd
            .saturating_sub(account.equity_usd)
            .max(Decimal::ZERO),
    }
}

/// A budget's utilization as a percentage, for the engine's `Utilization`
/// block and for [`gauge`] — one function so the two can never round
/// differently.
///
/// `None` when the budget is unset, or when the limit is zero and the ratio
/// is undefined. Clamped at the bottom: a profitable day consumes a negative
/// amount of the daily budget, and "utilization -48%" is not what a gauge
/// that runs to 100 means. Clamping only the low end leaves the trip end
/// exact — for every positive limit this reaches 100 precisely when [`check`]
/// fires.
pub(super) fn utilization_pct(
    kind: LossKind,
    limit: Option<Decimal>,
    account: &AccountSnapshot,
) -> Option<Decimal> {
    let limit = limit?;
    super::engine::ratio_pct(consumed_usd(kind, account).max(Decimal::ZERO), limit)
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
fn check(scope: &KillScope, limits: &LossLimits, account: &AccountSnapshot) -> Option<Breach> {
    for (kind, limit) in [
        (LossKind::Daily, limits.max_daily_loss_usd),
        (LossKind::Drawdown, limits.max_drawdown_usd),
    ] {
        let Some(limit) = limit else { continue };
        let observed_usd = consumed_usd(kind, account);
        if observed_usd >= limit {
            return Some(Breach {
                scope: scope.clone(),
                kind,
                observed_usd,
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
pub(super) fn check_all(
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

/// Whose budget a gauge row is measuring.
///
/// Not [`KillScope`], which carries an [`AgentId`] the caller already knows
/// and would serialize differently per agent — a gauge is read by the agent
/// it belongs to, and `"agent"` versus `"account"` is the whole distinction
/// it needs: one it can act on alone, one it shares with every other
/// container under the same operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    /// This agent's own limits, set per container.
    Agent,
    /// The account-wide limits of spec item 25, shared across the fleet. A
    /// breach here trips the switch for **every** agent, not just this one.
    Account,
}

/// One loss budget, continuously, before it fires (`docs/spec.md` spec F).
///
/// The breaker is a step function: nothing, nothing, nothing, killed. This is
/// the same predicate read as a dial, so an agent can slow down at 80% rather
/// than discover the limit by hitting it — and so it can see the *account*
/// budget it shares, which no refusal it has ever received would have
/// mentioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LossBudget {
    pub scope: BudgetScope,
    pub kind: LossKind,
    /// Dollars consumed, in [`check`]'s own sign convention: negative on a
    /// profitable day. Unclamped because [`LossBudget::tripped`] is this
    /// number against the limit, and a clamped copy would trip a zero-dollar
    /// budget on a profit while the breaker did not.
    pub consumed_usd: Decimal,
    pub limit_usd: Decimal,
    /// Consumption against the limit, as a percentage, floored at zero.
    ///
    /// **`None` only when the limit is zero**, where the ratio is undefined —
    /// a zero budget is not a gauge, it is a switch, and `tripped` is the
    /// field that answers for it.
    ///
    /// For every positive limit, `utilization_pct >= 100`,
    /// `remaining_usd == 0` and `tripped` are the same fact, because all
    /// three read [`consumed_usd`] against the same limit.
    /// `the_gauge_reads_a_hundred_percent_exactly_when_the_breaker_trips`
    /// pins it; without it the gauge could sit at 99% on an account that is
    /// already dead.
    pub utilization_pct: Option<Decimal>,
    /// Dollars of loss left before the trip, and the number an agent sizes
    /// its next stop against. Zero *is* the trip, since a budget is exhausted
    /// at its limit and not past it.
    pub remaining_usd: Decimal,
    /// What [`check`] would say right now.
    pub tripped: bool,
}

/// Reads one set of limits as gauge rows, one per **configured** budget.
///
/// An unset limit yields no row rather than a zero one: an operator who set
/// no drawdown budget has no drawdown gauge, and a row reading `limit: 0`
/// would say the opposite of what it means.
pub(super) fn gauge(
    scope: BudgetScope,
    limits: &LossLimits,
    account: &AccountSnapshot,
    now_ms: u64,
) -> Vec<LossBudget> {
    [
        (LossKind::Daily, limits.max_daily_loss_usd),
        (LossKind::Drawdown, limits.max_drawdown_usd),
    ]
    .into_iter()
    .filter_map(|(kind, limit)| {
        let limit_usd = limit?;
        // The same guard `check_account` puts in front of an order, for the
        // same reason: a PnL window that starts on another UTC day measures
        // some of yesterday and not all of today, and the error runs toward
        // *understating* the loss. The engine refuses an order on that
        // snapshot; a gauge cannot refuse, so it declines to report the
        // number rather than reporting a reassuring wrong one. Drawdown is a
        // high-water mark with no day window and is unaffected.
        if kind == LossKind::Daily && !account.covers_day_of(now_ms) {
            return None;
        }
        let consumed = consumed_usd(kind, account);
        Some(LossBudget {
            scope,
            kind,
            consumed_usd: consumed,
            limit_usd,
            utilization_pct: utilization_pct(kind, Some(limit_usd), account),
            // Against the clamped consumption, so a profitable day reports
            // the budget it has rather than more than the budget exists.
            remaining_usd: limit_usd
                .saturating_sub(consumed.max(Decimal::ZERO))
                .max(Decimal::ZERO),
            // Against the raw one, because this is [`check`]'s own comparison
            // and the two must agree on every input, the zero-dollar budget
            // included.
            tripped: consumed >= limit_usd,
        })
    })
    .collect()
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
            resting: Some(crate::guardrail::RestingExposure::none()),
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

    // ---- the gauge (spec F) ----------------------------------------------

    fn drawdown(limit: i64) -> LossLimits {
        LossLimits {
            max_daily_loss_usd: None,
            max_drawdown_usd: Some(d(limit)),
        }
    }

    /// The single row a one-budget gauge must produce, by value so a caller
    /// can inline the `gauge` call it came from.
    fn only(mut rows: Vec<LossBudget>) -> LossBudget {
        assert_eq!(rows.len(), 1, "expected one row, got {rows:?}");
        rows.remove(0)
    }

    /// The whole point of the gauge: it is the breaker's own predicate read
    /// as a dial, so there is no consumption at which one says "fine" and the
    /// other has already killed the account. Swept across the limit a cent at
    /// a time, because the interesting values are the two on either side of
    /// it and the one exactly on it.
    #[test]
    fn the_gauge_reads_a_hundred_percent_exactly_when_the_breaker_trips() {
        let scope = KillScope::agent("alpha");
        let limits = daily(25);
        let hundred = d(100);
        for cents in 2_480..=2_520 {
            let loss = Decimal::from(cents) / d(100);
            let snapshot = account(-loss, Decimal::ZERO, 100, 100);
            let row = only(gauge(BudgetScope::Agent, &limits, &snapshot, 0));
            let fired = check(&scope, &limits, &snapshot).is_some();
            let pct = row.utilization_pct.expect("a positive limit has a ratio");

            assert_eq!(fired, row.tripped, "at ${loss}: tripped disagrees");
            assert_eq!(
                fired,
                pct >= hundred,
                "at ${loss}: {pct}% disagrees with the breaker"
            );
            assert_eq!(
                fired,
                row.remaining_usd.is_zero(),
                "at ${loss}: remaining disagrees"
            );
        }
    }

    /// The drawdown half. It is the reason this exists as more than a rename:
    /// the breaker has fired on two budgets since it was written and the
    /// utilization block reported one, so an account a dollar from a drawdown
    /// kill saw nothing about drawdown anywhere.
    #[test]
    fn the_drawdown_budget_is_gauged_too() {
        let snapshot = account(Decimal::ZERO, Decimal::ZERO, 850, 1_000);
        let row = only(gauge(BudgetScope::Agent, &drawdown(200), &snapshot, 0));
        assert_eq!(row.kind, LossKind::Drawdown);
        assert_eq!(row.consumed_usd, d(150));
        assert_eq!(row.utilization_pct, Some(d(75)));
        assert_eq!(row.remaining_usd, d(50));
        assert!(!row.tripped);
    }

    #[test]
    fn an_unset_budget_has_no_row_rather_than_a_zero_one() {
        let snapshot = account(-d(10), Decimal::ZERO, 100, 100);
        assert!(gauge(BudgetScope::Agent, &LossLimits::UNSET, &snapshot, 0).is_empty());
        // And the configured half of a half-configured pair is the only row.
        let rows = gauge(BudgetScope::Agent, &daily(25), &snapshot, 0);
        assert_eq!(only(rows).kind, LossKind::Daily);
    }

    /// A PnL window that starts on another UTC day measures part of yesterday
    /// and not all of today, and the error runs toward *understating* the
    /// loss. `check_account` refuses an order on such a snapshot; the gauge
    /// cannot refuse, so it must report nothing rather than something
    /// reassuring. Drawdown has no day window and survives.
    #[test]
    fn a_stale_day_window_withdraws_the_daily_row_and_keeps_the_drawdown_one() {
        let both = LossLimits {
            max_daily_loss_usd: Some(d(25)),
            max_drawdown_usd: Some(d(200)),
        };
        let snapshot = account(-d(24), Decimal::ZERO, 900, 1_000);
        assert_eq!(snapshot.day_start_ms, 0);

        let fresh = gauge(BudgetScope::Agent, &both, &snapshot, 0);
        assert_eq!(fresh.len(), 2);

        // A day later, against a snapshot still carrying day zero.
        let stale = gauge(BudgetScope::Agent, &both, &snapshot, 86_400_000);
        assert_eq!(only(stale).kind, LossKind::Drawdown);
    }

    /// A good day consumes a negative amount of the daily budget. The
    /// percentage floors at zero because a gauge that runs to 100 does not
    /// run to -48; the dollars stay signed because `tripped` is that number
    /// against the limit, and a zero-dollar budget must not trip on a profit.
    #[test]
    fn a_profitable_day_reads_zero_percent_and_keeps_its_signed_dollars() {
        let up = account(d(12), Decimal::ZERO, 112, 112);
        let row = only(gauge(BudgetScope::Agent, &daily(25), &up, 0));
        assert_eq!(row.consumed_usd, -d(12));
        assert_eq!(row.utilization_pct, Some(Decimal::ZERO));
        assert_eq!(
            row.remaining_usd,
            d(25),
            "a good day does not buy extra budget"
        );
        assert!(!row.tripped);
    }

    /// The corner the clamping is written around: a zero-dollar budget is not
    /// a gauge, it is a switch. It has no ratio, and `tripped` has to be the
    /// breaker's own comparison or the two disagree on a profitable day.
    #[test]
    fn a_zero_dollar_budget_has_no_ratio_and_still_matches_the_breaker() {
        let scope = KillScope::agent("alpha");
        let zero = daily(0);
        for snapshot in [
            account(d(12), Decimal::ZERO, 112, 112),
            account(Decimal::ZERO, Decimal::ZERO, 100, 100),
            account(-d(1), Decimal::ZERO, 99, 100),
        ] {
            let row = only(gauge(BudgetScope::Agent, &zero, &snapshot, 0));
            assert_eq!(row.utilization_pct, None);
            assert_eq!(row.tripped, check(&scope, &zero, &snapshot).is_some());
        }
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
