//! Per-agent guardrail configuration and the near-zero defaults a newly
//! paired agent starts with (`docs/decisions.md` D-c).

use std::collections::BTreeSet;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// `$25` order cap for a new agent (D-c).
pub(super) const DEFAULT_MAX_ORDER_USD: Decimal = Decimal::from_parts(25, 0, 0, false, 0);
/// `$100` position cap for a new agent (D-c).
pub(super) const DEFAULT_MAX_POSITION_USD: Decimal = Decimal::from_parts(100, 0, 0, false, 0);
/// `$25` daily loss budget for a new agent (D-c).
pub(super) const DEFAULT_DAILY_LOSS_USD: Decimal = Decimal::from_parts(25, 0, 0, false, 0);
/// Five orders per five minutes for a new agent (D-c).
pub(super) const DEFAULT_ORDER_RATE: OrderRate = OrderRate {
    count: 5,
    per_ms: 300_000,
};
/// D-c does not name a slippage default, so this is the same posture applied
/// to the number D-c omits: tight enough that the first wide order is refused
/// and the refusal names the limit to raise. Spec item 12 makes max-slippage
/// mandatory on market orders, so there has to be a number here.
pub(super) const DEFAULT_MAX_SLIPPAGE_BPS: Decimal = Decimal::from_parts(10, 0, 0, false, 0);
/// D-c does not name a leverage default either. `1` is "no leverage", which
/// is what "default-deny all the way down" means for a multiplier.
pub(super) const DEFAULT_MAX_LEVERAGE: u32 = 1;
/// `docs/specs/fair-value.md` §9: `mark_divergence_bp`.
pub(super) const DEFAULT_MARK_DIVERGENCE_BPS: Decimal = Decimal::from_parts(5, 0, 0, false, 0);
/// `docs/specs/fair-value.md` §9: `mark_divergence_window_s`, in ms.
pub(super) const DEFAULT_MARK_DIVERGENCE_WINDOW_MS: u64 = 30_000;
/// How long an approval proposal stays valid (spec item 28: proposals carry
/// a TTL and auto-expire). Two minutes: long enough for an operator to look
/// at a queued order, short enough that approving one is still a decision
/// about roughly the current market rather than about a stale one. The order
/// is re-evaluated in full against fresh data at approval time anyway, so an
/// expiry is a convenience for the operator, not the safety property.
pub(super) const APPROVAL_TTL_MS: u64 = 120_000;
/// Longest agent `reason` string accepted, in bytes.
///
/// Refusal rows are kept forever (D-e) and a refusal costs no rate token, so
/// an unbounded reason is unlimited free writes into an append-only
/// hash-chained store. 2 KiB is far more than a sentence explaining a trade.
pub(super) const MAX_REASON_BYTES: usize = 2_048;

/// Token-bucket shape for spec item 24's order-rate cap: `count` orders per
/// `per_ms` milliseconds. `docs/specs/workflows.md` §7 writes it as
/// `order_rate: { count, per_s }`; milliseconds here because every clock the
/// engine sees is a millisecond clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRate {
    pub count: u32,
    pub per_ms: u64,
}

/// Spec item 10's address-level request budget, which item 24 calls "the
/// global budget" alongside the per-agent order-rate cap.
///
/// Hyperliquid meters requests per **address**, not per agent: a 10,000
/// request initial buffer, then one request per 1 USDC traded, and otherwise
/// one request per ten seconds. Every agent under the master shares it, so a
/// per-agent cap cannot bound it and a budget this engine does not model is a
/// budget nothing enforces before the venue rejects — and a venue rejection
/// has already consumed a nonce.
///
/// Not persisted. The live figure comes from `userRateLimit` on every
/// reconnect, so a stored copy would only ever be a stale one; the default
/// below is item 10's documented shape, used until the first live reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalRateBudget {
    /// `count` requests per `per_ms`. The default is item 10 verbatim: a
    /// 10,000 request buffer refilling at one per ten seconds.
    pub rate: OrderRate,
    /// Requests kept back for risk-reducing actions (item 10: "always reserve
    /// headroom for risk-reducing actions"). An order is refused once the
    /// remaining budget is at or below this; a cancel still goes through and
    /// is charged, because a budget must never be the reason a cancel does
    /// not happen.
    pub reserve: u32,
}

impl GlobalRateBudget {
    /// Rejects a budget that cannot be evaluated, in the same shape
    /// [`AgentGuardrails::validate`] uses.
    pub(super) fn validate(&self) -> Result<(), (&'static str, String)> {
        if self.rate.per_ms == 0 {
            return Err(("global_rate.rate.per_ms", "must be positive".to_owned()));
        }
        if self.reserve >= self.rate.count {
            return Err((
                "global_rate.reserve",
                "must be below the budget it reserves from".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for GlobalRateBudget {
    fn default() -> Self {
        GlobalRateBudget {
            rate: OrderRate {
                count: 10_000,
                per_ms: 100_000_000,
            },
            reserve: 100,
        }
    }
}

/// Margin mode, operator-set per symbol (D3). Present so an agent can read
/// it in `get_state`; there is no path in this module that lets an agent
/// write it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarginMode {
    #[default]
    Cross,
    Isolated,
}

/// The two risk parameters D3 reserves to the operator. They live in the
/// guardrail config so `get_state` can show them and the leverage cap can be
/// enforced pre-sign, and they are only ever written through
/// [`super::GuardrailEngine::operator_set_guardrails`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskSettings {
    /// Ceiling on account leverage implied by an order, in whole multiples.
    /// The effective cap is `min(this, asset.max_leverage)`: the venue's
    /// number is a hard bound and the operator's may only be tighter.
    pub max_leverage: u32,
    pub margin_mode: MarginMode,
}

impl Default for RiskSettings {
    fn default() -> Self {
        RiskSettings {
            max_leverage: DEFAULT_MAX_LEVERAGE,
            margin_mode: MarginMode::Cross,
        }
    }
}

/// Spec item 25's loss circuit breaker, expressed as budgets rather than
/// caps. Size caps bound one order; these bound a night of them.
///
/// `None` means "not configured", which is a real operator choice and not
/// the same as zero. D-c names a per-agent daily loss and no drawdown, so
/// that is exactly what [`LossLimits::default`] carries; the account-wide
/// pair defaults to unset because D-c is a per-agent decision and inventing
/// a fleet number here would be a limit nobody chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LossLimits {
    /// Trips when mark-to-market PnL since the UTC day boundary reaches
    /// `-max_daily_loss_usd`.
    pub max_daily_loss_usd: Option<Decimal>,
    /// Trips when `peak_equity - equity` reaches `max_drawdown_usd`.
    pub max_drawdown_usd: Option<Decimal>,
}

impl Default for LossLimits {
    fn default() -> Self {
        LossLimits {
            max_daily_loss_usd: Some(DEFAULT_DAILY_LOSS_USD),
            max_drawdown_usd: None,
        }
    }
}

impl LossLimits {
    /// The account-wide pair: unset until an operator sets it (see the type
    /// doc for why D-c does not supply one).
    pub(super) const UNSET: LossLimits = LossLimits {
        max_daily_loss_usd: None,
        max_drawdown_usd: None,
    };

    pub(super) fn is_unset(&self) -> bool {
        self.max_daily_loss_usd.is_none() && self.max_drawdown_usd.is_none()
    }
}

/// How old an input may be before the engine refuses to reason from it.
///
/// Spec item 34: execution tools fail closed during a disconnect. These are
/// the numbers that decide when "disconnected" starts. The market default is
/// the 2 s threshold `docs/specs/fair-value.md` §5.2 sets for book data —
/// note §14.4 correction 4, that only the `bbo` channel actually meets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Freshness {
    pub max_market_age_ms: u64,
    pub max_account_age_ms: u64,
}

impl Default for Freshness {
    fn default() -> Self {
        Freshness {
            max_market_age_ms: 2_000,
            max_account_age_ms: 5_000,
        }
    }
}

/// Everything spec item 24 lists, per agent, which by D1 is per sub-account.
///
/// [`AgentGuardrails::default`] is D-c verbatim: empty symbol allowlist, $25
/// orders, $100 positions, $25 daily loss, 5 orders per 5 minutes, approval
/// mode on. The empty allowlist is the load-bearing part — the first order a
/// fresh agent sends is refused with [`super::Refusal::SymbolNotAllowed`],
/// which carries the (empty) allowed list, so the refusal names the limit to
/// raise. The refusal is the onboarding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGuardrails {
    /// Ordered so the set has one serialization (`AGENTS.md` invariant 6)
    /// and so the refusal lists symbols in a stable order.
    pub symbols: BTreeSet<String>,
    pub max_order_usd: Decimal,
    pub max_position_usd: Decimal,
    pub max_slippage_bps: Decimal,
    pub order_rate: OrderRate,
    /// When on, every order must carry the venue's reduce-only flag *and*
    /// actually reduce the open position.
    pub reduce_only: bool,
    pub risk: RiskSettings,
    pub loss: LossLimits,
    /// Spec item 28, default ON for new agents. This module does not
    /// implement the approval queue (built last in v1); it refuses with
    /// [`super::Refusal::ApprovalRequired`] after every hard predicate has
    /// passed, which is the verdict `oppen-mcp` renders as
    /// `{status: pending_approval}`.
    pub approval_required: bool,
    pub freshness: Freshness,
    /// `docs/specs/fair-value.md` §7: reject when the reconstructed mark has
    /// diverged from `markPx` by more than this for longer than the window.
    pub max_mark_divergence_bps: Decimal,
    pub mark_divergence_window_ms: u64,
}

impl Default for AgentGuardrails {
    fn default() -> Self {
        AgentGuardrails {
            symbols: BTreeSet::new(),
            max_order_usd: DEFAULT_MAX_ORDER_USD,
            max_position_usd: DEFAULT_MAX_POSITION_USD,
            max_slippage_bps: DEFAULT_MAX_SLIPPAGE_BPS,
            order_rate: DEFAULT_ORDER_RATE,
            reduce_only: false,
            risk: RiskSettings::default(),
            loss: LossLimits::default(),
            approval_required: true,
            freshness: Freshness::default(),
            max_mark_divergence_bps: DEFAULT_MARK_DIVERGENCE_BPS,
            mark_divergence_window_ms: DEFAULT_MARK_DIVERGENCE_WINDOW_MS,
        }
    }
}

impl AgentGuardrails {
    /// Rejects a configuration that cannot be evaluated at all, so a bad
    /// value is caught when an operator types it rather than at the moment
    /// an order needs a verdict. Returns the offending field and why.
    pub(super) fn validate(&self) -> Result<(), (&'static str, String)> {
        if self.max_order_usd.is_sign_negative() {
            return Err(("max_order_usd", "must not be negative".to_owned()));
        }
        if self.max_position_usd.is_sign_negative() {
            return Err(("max_position_usd", "must not be negative".to_owned()));
        }
        if self.max_slippage_bps.is_sign_negative() {
            return Err(("max_slippage_bps", "must not be negative".to_owned()));
        }
        if self.max_mark_divergence_bps.is_sign_negative() {
            return Err(("max_mark_divergence_bps", "must not be negative".to_owned()));
        }
        if self.order_rate.per_ms == 0 {
            return Err(("order_rate.per_ms", "must be positive".to_owned()));
        }
        if self.risk.max_leverage == 0 {
            return Err(("risk.max_leverage", "must be at least 1".to_owned()));
        }
        for limit in [self.loss.max_daily_loss_usd, self.loss.max_drawdown_usd]
            .into_iter()
            .flatten()
        {
            if limit.is_sign_negative() {
                return Err(("loss", "limits must not be negative".to_owned()));
            }
        }
        Ok(())
    }
}
