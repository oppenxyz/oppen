//! The typed refusal taxonomy.
//!
//! `AGENTS.md` invariant 8: every rejection is typed, never a bare string.
//! Every variant here carries the predicate that failed, the value that was
//! observed and the limit that was configured, because the refusal has two
//! readers — an agent that has to adapt without guessing, and an operator who
//! has to decide whether the limit or the agent is wrong. A refusal that says
//! only "blocked" makes both of them guess.
//!
//! D-c leans on this: a newly paired agent has an empty allowlist and $25
//! caps, so its first order is refused and the refusal is what tells it which
//! limit to ask the operator to raise.

use rust_decimal::Decimal;
use serde::Serialize;

use oppen_hl::Network;
use oppen_hl::meta::ValidationError;
use oppen_hl::order::OrderError;
use oppen_hl::wire::WireError;

use super::AgentId;
use super::breaker::LossKind;
use super::kill::{KillReason, KillScope};
use super::snapshot::FeedQuality;

/// Why an order was not cleared for signing.
///
/// Serializes with the variant name in a `refusal` tag and fields in
/// declaration order, so one refusal has one byte sequence (`AGENTS.md`
/// invariant 6). `oppen-mcp` maps these onto its wire taxonomy: everything
/// here except [`Refusal::VenueRule`], [`Refusal::TradingPaused`],
/// [`Refusal::ApprovalRequired`] and the two rate refusals
/// ([`Refusal::OrderRate`], [`Refusal::GlobalRateBudget`], which become
/// `rate_limited`) is a `guardrail_reject`. The rate carve-out is
/// `docs/decisions.md` C2: they are the only refusals here that clear without
/// the agent changing anything, and they carry the wait that says when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Refusal {
    /// Spec item 19 requires a `reason` on every execution tool call. An
    /// unexplained order is refused before anything else is evaluated.
    #[error("the order carries no reason; spec item 19 requires one")]
    MissingReason,

    /// The reason is kept forever (D-e) and costs no rate token, so an
    /// unbounded one is unlimited free writes into the append-only chained
    /// ledger. Bounded at ingestion; invariant 9 covers only rendering.
    #[error("the reason is {len_bytes} bytes, past the {max_bytes}-byte limit")]
    ReasonTooLong { len_bytes: usize, max_bytes: usize },

    /// Item 30 renders the reason as inert plain text, but U1 and P4 put it
    /// on a character grid where an ANSI escape or a C0 byte is not inert at
    /// all. Newline and tab are the only control characters a sentence needs.
    #[error("the reason contains control character U+{codepoint:04X} at byte {at_byte}")]
    ReasonControlCharacter { at_byte: usize, codepoint: u32 },

    /// Spec item 24, symbol allowlist. `allowed` is empty for a freshly
    /// paired agent (D-c), which is the intended first refusal.
    #[error("{symbol} is not on this agent's allowlist ({})", fmt_list(.allowed))]
    SymbolNotAllowed {
        symbol: String,
        allowed: Vec<String>,
    },

    /// Spec item 24, notional cap. Measured on the rounded price and size
    /// that would actually be signed, not on what was asked for.
    #[error("order notional ${observed_usd} exceeds the ${limit_usd} cap on {symbol}")]
    OrderNotional {
        symbol: String,
        observed_usd: Decimal,
        limit_usd: Decimal,
    },

    /// Gross account positions and opening commitments, without netting.
    #[error("open exposure ${observed_usd} exceeds the ${limit_usd} account cap")]
    OpenExposure {
        observed_usd: Decimal,
        limit_usd: Decimal,
    },

    /// Spec item 24, max position size. `observed_usd` is the notional the
    /// position would hold *after* this order and every working order on the
    /// symbol fills, valued at the reference price. `resting_usd` is how much
    /// of it is working rather than filled, so an agent that is over the cap
    /// because of its own book is told to cancel rather than left guessing.
    #[error(
        "post-fill position ${observed_usd} on {symbol} (${resting_usd} of it resting) exceeds the ${limit_usd} cap"
    )]
    PositionNotional {
        symbol: String,
        observed_usd: Decimal,
        resting_usd: Decimal,
        limit_usd: Decimal,
    },

    /// Spec F's vol-scaled notional cap. Separate from
    /// [`Refusal::PositionNotional`] because the two name different knobs and
    /// an agent told only "too big" cannot tell which: one is raised by
    /// changing `max_position_usd`, the other by raising `max_risk_usd` or by
    /// trading something that moves less.
    ///
    /// Every input to the arithmetic is reported — the budget, the volatility
    /// it was divided by, the factor the last hour tightened that volatility
    /// by, and the cap that came out — because the cap is *derived* and a
    /// number an agent cannot reconstruct is one it will treat as arbitrary
    /// and retry against.
    ///
    /// `sigma_day_pct` stays the *measured* daily volatility and `vol_scale`
    /// is reported beside it rather than folded in, so an agent can tell a
    /// coin that simply moves a lot from a coin that started moving an hour
    /// ago. The two call for different responses: one is the asset, the other
    /// will pass.
    #[error(
        "post-fill position ${observed_usd} on {symbol} exceeds the ${effective_cap_usd} vol-scaled cap \
         (${risk_budget_usd} of risk at {sigma_day_pct}% daily volatility, scaled {vol_scale}x by the last hour)"
    )]
    VolScaledPositionNotional {
        symbol: String,
        observed_usd: Decimal,
        effective_cap_usd: Decimal,
        risk_budget_usd: Decimal,
        sigma_day_pct: Decimal,
        /// `max(1, vol_ratio)`. One means the last hour is ordinary for this
        /// day, or that nobody measured it — the cap is then what the day
        /// alone buys.
        vol_scale: Decimal,
    },

    /// Spec item 24, order-rate cap. `retry_after_ms` is how long until one
    /// token has refilled, so an agent can back off exactly rather than poll.
    #[error("order rate exhausted: {limit} per {window_ms}ms, retry in {retry_after_ms}ms")]
    OrderRate {
        limit: u32,
        window_ms: u64,
        tokens_available: Decimal,
        retry_after_ms: u64,
    },

    /// Spec item 10's address-wide request budget, which item 24 calls "the
    /// global budget". Distinct from [`Refusal::OrderRate`] because the fix
    /// is different: the agent's own cap is untouched and every agent under
    /// the master is sharing one venue budget. `reserve` is the headroom held
    /// back for risk-reducing actions, which is why an order is refused above
    /// zero.
    #[error(
        "the account request budget is down to {tokens_available} against a {reserve} reserve, retry in {retry_after_ms}ms"
    )]
    GlobalRateBudget {
        tokens_available: Decimal,
        reserve: u32,
        retry_after_ms: u64,
    },

    /// Spec item 24, reduce-only mode. Carries all three ways an order can
    /// fail it: not flagged reduce-only, no position to reduce, or a size or
    /// side that would increase exposure.
    #[error("reduce-only mode: {detail} (position {position_szi}, order {signed_sz} on {symbol})")]
    ReduceOnly {
        symbol: String,
        position_szi: Decimal,
        /// Signed order size: positive is a buy.
        signed_sz: Decimal,
        flagged_reduce_only: bool,
        detail: ReduceOnlyBreach,
    },

    /// Spec item 24, max slippage. For a limit order the reference is the
    /// market's; for a trigger order it is the order's own trigger price,
    /// because a stop fills when it triggers and not now.
    #[error("{observed_bps} bps of slippage on {symbol} exceeds the {limit_bps} bps cap")]
    Slippage {
        symbol: String,
        observed_bps: Decimal,
        limit_bps: Decimal,
        reference_px: Decimal,
    },

    /// Spec item 24, leverage cap. `limit` is `min(operator cap, venue max)`
    /// — D3 makes the operator's number the only one an agent cannot move,
    /// and the venue's is a hard bound below it.
    #[error("post-fill leverage {observed}x exceeds the {limit}x cap")]
    Leverage {
        observed: Decimal,
        limit: u32,
        equity_usd: Decimal,
        position_notional_usd: Decimal,
    },

    /// Spec item 26. `oppen-mcp` renders this as the typed `trading_paused`
    /// that item 19's taxonomy promises agents.
    #[error("trading is paused ({scope}) since {since_ms}: {reason}")]
    TradingPaused {
        scope: KillScope,
        since_ms: u64,
        reason: KillReason,
    },

    /// Spec item 25. Emitted with the kill switch already engaged: the
    /// breaker trips the switch and then refuses, so the next order is
    /// refused as [`Refusal::TradingPaused`] without needing fresh PnL.
    #[error("{kind} loss limit breached ({scope}): ${observed_usd} against ${limit_usd}")]
    LossLimit {
        scope: KillScope,
        kind: LossKind,
        observed_usd: Decimal,
        limit_usd: Decimal,
    },

    /// The venue's own order rules (spec item 8), evaluated here because an
    /// order that the venue would reject must never consume a nonce or a
    /// rate token. Maps onto item 19's `venue_reject` subtypes.
    #[error("venue rule: {0}")]
    VenueRule(VenueRule),

    /// Spec item 28. Not a denial: every hard predicate has already passed,
    /// the order-rate token has already been spent, and the engine has minted
    /// and is holding a proposal. `oppen-mcp` renders this verbatim as
    /// `{status: pending_approval, approval_id, expires_at}`.
    ///
    /// `approval_id` names a proposal the **engine** stores. It is a receipt,
    /// not a credential: presenting it back only tells the engine which of
    /// its own stored intents to re-evaluate, and only
    /// [`super::GuardrailEngine::operator_approve_proposal`] — an
    /// operator-only path, `AGENTS.md` invariant 3 — accepts it.
    #[error("approval required for a ${notional_usd} order on {symbol} (proposal {approval_id})")]
    ApprovalRequired {
        symbol: String,
        notional_usd: Decimal,
        approval_id: String,
        expires_at_ms: u64,
    },

    /// Fail-closed. The engine could not establish whether a limit was
    /// breached, so it treated the uncertainty as a breach.
    #[error("cannot evaluate: {0}")]
    Unevaluable(Unevaluable),
}

impl Refusal {
    /// True when the refusal is a fail-closed one rather than a breached
    /// limit. Test-only sugar over the public [`Refusal::Unevaluable`]
    /// variant, which anything else matches on directly.
    #[cfg(test)]
    pub(super) fn is_unevaluable(&self) -> bool {
        matches!(self, Refusal::Unevaluable(_))
    }
}

/// Which part of reduce-only mode the order failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ReduceOnlyBreach {
    #[error("the order is not flagged reduce-only")]
    NotFlagged,
    #[error("there is no open position to reduce")]
    NoPosition,
    #[error("the order is on the same side as the position")]
    SameSide,
    #[error("the order is larger than the position")]
    Oversized,
}

/// The venue-side order rules, as typed subtypes rather than a rendered
/// `ValidationError` string (`AGENTS.md` invariant 8, spec item 19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "venue_rule", rename_all = "snake_case")]
#[non_exhaustive]
pub enum VenueRule {
    #[error("unknown asset {symbol}")]
    UnknownAsset { symbol: String },
    #[error("{symbol} is delisted")]
    Delisted { symbol: String },
    #[error("price {px} is not positive")]
    NonPositivePrice { px: Decimal },
    #[error("size {sz} is not positive")]
    NonPositiveSize { sz: Decimal },
    #[error("price {px} has more than {max} significant figures")]
    PriceSignificantFigures { px: Decimal, max: u32 },
    #[error("price {px} has more than {max} decimals")]
    PriceDecimals { px: Decimal, max: u32 },
    #[error("size {sz} has more than {max} decimals")]
    SizeDecimals { sz: Decimal, max: u32 },
    #[error("notional ${notional_usd} is below the ${minimum_usd} venue minimum")]
    MinNotional {
        notional_usd: Decimal,
        minimum_usd: Decimal,
    },
    /// The venue requires a scheduled cancel to be at least five seconds
    /// ahead (spec item 27).
    #[error("scheduleCancel at {cancel_at_ms} is earlier than the venue's {earliest_ms}")]
    ScheduleCancelTooSoon { cancel_at_ms: u64, earliest_ms: u64 },
    /// A value that cannot be put on the wire at all: more than eight
    /// decimals, or a malformed cloid (`docs/hl-signing.md` §4).
    #[error("{detail}")]
    Unrepresentable { detail: String },
    /// The asset belongs to a builder-deployed (HIP-3) dex. v1 trades the
    /// validator dex only, and HIP-3 asset ids are numbered differently, so
    /// signing against a HIP-3 universe would name the wrong instrument
    /// (`docs/specs/fair-value.md` §14.4 correction 15).
    #[error("{symbol} is on a builder-deployed dex; v1 trades the validator dex only")]
    BuilderDeployedDex { symbol: String },
}

impl From<ValidationError> for VenueRule {
    fn from(e: ValidationError) -> Self {
        match e {
            ValidationError::UnknownAsset(symbol) => VenueRule::UnknownAsset { symbol },
            ValidationError::BuilderDeployedDex(symbol) => VenueRule::BuilderDeployedDex { symbol },
            ValidationError::Delisted(symbol) => VenueRule::Delisted { symbol },
            ValidationError::NonPositivePrice(px) => VenueRule::NonPositivePrice { px },
            ValidationError::NonPositiveSize(sz) => VenueRule::NonPositiveSize { sz },
            ValidationError::PriceSigFigs { px } => VenueRule::PriceSignificantFigures {
                px,
                max: oppen_hl::meta::PRICE_SIG_FIGS,
            },
            ValidationError::PriceDecimals { px, max } => VenueRule::PriceDecimals { px, max },
            ValidationError::SizeDecimals { sz, sz_decimals } => VenueRule::SizeDecimals {
                sz,
                max: sz_decimals,
            },
            ValidationError::MinNotional { notional } => VenueRule::MinNotional {
                notional_usd: notional,
                minimum_usd: oppen_hl::meta::MIN_NOTIONAL_USD,
            },
        }
    }
}

impl From<WireError> for VenueRule {
    fn from(e: WireError) -> Self {
        VenueRule::Unrepresentable {
            detail: e.to_string(),
        }
    }
}

impl From<OrderError> for VenueRule {
    fn from(e: OrderError) -> Self {
        match e {
            OrderError::Validation(v) => v.into(),
            OrderError::Wire(w) => w.into(),
        }
    }
}

/// Why the engine could not reach a verdict.
///
/// Every variant means the same thing operationally: **refuse**. They are
/// distinguished because an operator debugging a silent agent needs to know
/// whether the book went quiet, the account never reconciled, or the ledger
/// disk is full — and because spec item 34 wants per-feed staleness surfaced
/// rather than collapsed into one opaque outage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "unevaluable", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unevaluable {
    #[error("{agent} is not a paired agent")]
    UnknownAgent { agent: AgentId },

    /// The request arriving at the signer is for a different network than the
    /// engine gating it. `docs/decisions.md` R4 calls a mainnet number that
    /// is really a testnet number the worst bug this product can ship, and
    /// gives each network its own engine and database file; this is that
    /// boundary asserted at the last instant, in the pre-sign gate
    /// `super::GuardrailEngine::sign_cleared` builds, rather than assumed
    /// from how the caller was written.
    #[error("this engine is bound to {expected:?} but the request is for {supplied:?}")]
    WrongNetwork {
        expected: Network,
        supplied: Network,
    },

    /// Spec item 7 puts a submit queue between the decision and the wire, and
    /// a clearance is a verdict about the market it was evaluated against.
    /// Held past the agent's own market-data budget it describes a world that
    /// has moved, so the signer refuses it.
    ///
    /// Distinct from [`Unevaluable::StaleMarketData`] in what the reader does
    /// next: there the feed is down and the answer is to back off until it
    /// recovers; here the feed is fine and the answer is to evaluate again
    /// now.
    #[error("this clearance was evaluated {age_ms}ms ago, past the {max_age_ms}ms limit")]
    StaleClearance { age_ms: u64, max_age_ms: u64 },

    #[error("operator policy changed after evaluation; evaluate the order again")]
    PolicyChanged,

    /// The asset or the market tick the caller supplied is not the one the
    /// order names. Measuring an order against another instrument's price is
    /// the worst silent failure available to this module, so it is a refusal
    /// rather than a lookup.
    #[error("{field} is for {supplied}, but the order is for {expected}")]
    InputMismatch {
        field: String,
        expected: String,
        supplied: String,
    },

    /// Spec item 34: execution fails closed during a disconnect.
    #[error("market data for {symbol} is {age_ms}ms old, past the {max_age_ms}ms limit")]
    StaleMarketData {
        symbol: String,
        age_ms: u64,
        max_age_ms: u64,
    },

    /// `docs/specs/fair-value.md` §14.4 correction 2: `midPx` is `null`, not
    /// absent and not zero, on 56 of 233 mainnet assets, and correction 3
    /// forbids falling back to `allMids` — which returns a frozen `markPx`
    /// for exactly those assets. So a missing reference price is a refusal,
    /// never a substitution.
    #[error("no reference price for {symbol}")]
    MissingReferencePrice { symbol: String },

    /// `docs/specs/fair-value.md` §7: an order priced off a bar whose
    /// quality is not `Ok` is refused in the core, before signing.
    #[error("the {symbol} feed is {quality}, not ok")]
    DegradedFeed {
        symbol: String,
        quality: FeedQuality,
    },

    /// `docs/specs/fair-value.md` §3 and §9: a persistent divergence between
    /// the reconstructed mark and `markPx` is a hard stop before signing,
    /// because mark is the number the chain margins and liquidates with.
    #[error(
        "{symbol} mark has diverged {observed_bps} bps (limit {limit_bps}) for {sustained_ms}ms (window {window_ms}ms)"
    )]
    MarkDivergence {
        symbol: String,
        observed_bps: Decimal,
        limit_bps: Decimal,
        sustained_ms: u64,
        window_ms: u64,
    },

    /// Spec item 9: after a reconnect the ledger is reconciled against
    /// `userFillsByTime` and `frontendOpenOrders` before it is trusted.
    /// Until that finishes the position sizes the caps are measured against
    /// are unknown, so no order clears.
    #[error("account state has not been reconciled (snapshot at {as_of_ms})")]
    UnreconciledAccount { as_of_ms: u64 },

    #[error("account state is {age_ms}ms old, past the {max_age_ms}ms limit")]
    StaleAccountState { age_ms: u64, max_age_ms: u64 },

    /// The daily-loss budget is measured from a UTC day boundary. A snapshot
    /// carrying yesterday's boundary would understate today's loss, so it is
    /// refused rather than reinterpreted.
    #[error("PnL window starts at {day_start_ms} but now is {now_ms}, a different UTC day")]
    LossWindowMismatch { day_start_ms: u64, now_ms: u64 },

    /// Account-wide loss limits are configured but no fleet-level snapshot
    /// was supplied, so the account-wide half of item 25 cannot be checked.
    #[error("account-wide loss limits are set but no fleet snapshot was supplied")]
    MissingFleetState,

    /// A vol-scaled cap is configured but the market tick carried no daily
    /// volatility, so `max_risk_usd / (2 · sigma_day)` has no denominator.
    ///
    /// Same fail-closed reading as [`Unevaluable::MissingFleetState`]: a cap
    /// that cannot be computed is a cap that is not enforced, and the whole
    /// point of an operator setting one is that it binds. Also covers a
    /// non-positive sigma, which is not "an asset that cannot move" but a
    /// measurement that failed — the division would yield an infinite cap,
    /// which is the one number a cap must never be.
    #[error(
        "a vol-scaled cap is set for {symbol} but its daily volatility is unavailable, so the cap cannot be computed"
    )]
    MissingVolatility { symbol: String },

    /// No working-order book was supplied, so the position-notional and
    /// leverage caps would be measuring filled size only — which is
    /// bypassable by splitting one capped order into several resting ones.
    /// Same fail-closed reading as [`Unevaluable::MissingFleetState`]: a cap
    /// that cannot be measured is a cap that is not enforced.
    #[error("no resting-order exposure was supplied; the position caps cannot be measured")]
    MissingRestingOrders,

    #[error("account equity is ${equity_usd}; leverage is undefined")]
    NonPositiveEquity { equity_usd: Decimal },

    /// Every window in the engine — the token bucket, staleness, the
    /// divergence window — is measured against a monotonic-in-practice
    /// millisecond clock. A clock that moved backwards makes all of them
    /// wrong in the permissive direction, so it refuses.
    #[error("clock moved backwards: now {now_ms} is before {last_ms}")]
    ClockWentBackwards { now_ms: u64, last_ms: u64 },

    /// A quantity did not fit in a `Decimal`. `rust_decimal`'s operators
    /// panic on overflow, and an agent chooses the price and size, so every
    /// multiplication on the order path is checked and an absurd order is
    /// refused rather than taking the process down.
    #[error("{field} overflowed")]
    ArithmeticOverflow { field: String },

    /// A stored configuration that cannot be evaluated. Reachable only from
    /// a hand-edited or corrupted database row, since
    /// [`super::GuardrailEngine::operator_set_guardrails`] validates on the
    /// way in.
    #[error("guardrail config field {field} is invalid: {detail}")]
    InvalidGuardrailConfig { field: String, detail: String },

    /// The clearance could not be written to the audit ledger. D6 makes the
    /// ledger the single record of why an order happened; an order signed
    /// without one is unexplainable afterwards, so the write failing means
    /// the order does not happen.
    #[error("audit ledger write failed: {detail}")]
    AuditWriteFailed { detail: String },

    /// Persisting a kill-switch engagement failed. The engagement would not
    /// survive a restart (spec item 26), so the trip is treated as
    /// unevaluable rather than silently in-memory only.
    #[error("guardrail state write failed: {detail}")]
    StateWriteFailed { detail: String },

    /// An approval was presented for a proposal the engine is not holding:
    /// never issued, already consumed, already rejected, or swept as expired
    /// (spec item 28's TTL). The engine mints every proposal itself, so an
    /// unknown id is a clearance that was never authorised.
    ///
    /// Expiry is deliberately not a separate variant. Proposals are swept
    /// before every lookup, so an expired one is simply no longer held, and
    /// the caller's move — re-propose — is the same for all four causes.
    #[error("proposal {approval_id} is not pending")]
    UnknownProposal { approval_id: String },
}

impl From<Unevaluable> for Refusal {
    fn from(u: Unevaluable) -> Self {
        Refusal::Unevaluable(u)
    }
}

impl From<VenueRule> for Refusal {
    fn from(v: VenueRule) -> Self {
        Refusal::VenueRule(v)
    }
}

fn fmt_list(items: &[String]) -> String {
    if items.is_empty() {
        "empty".to_owned()
    } else {
        items.join(", ")
    }
}
