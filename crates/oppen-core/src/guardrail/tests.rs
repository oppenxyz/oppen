//! Tests for the guardrail engine.
//!
//! This module is a sibling of `engine`, so it cannot call `Cleared::new` —
//! that constructor is private to `engine.rs`. Every [`Cleared`] below came
//! out of [`GuardrailEngine::evaluate`], [`GuardrailEngine::clear_cancel`] or
//! [`GuardrailEngine::clear_schedule_cancel`], because there is no other way
//! to obtain one. That the file compiles is itself part of the proof.
//!
//! The centrepiece is [`no_input_produces_a_signable_value_without_passing_every_predicate`]:
//! twenty thousand pseudo-random cases, each re-checked against every
//! predicate independently of the engine that produced it, and against the
//! bytes of the action that would actually be signed rather than the request
//! that was made.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;

use oppen_hl::meta::Asset;
use oppen_hl::order::OrderKind;
use oppen_hl::types::AssetInfo;
use oppen_hl::wire::{CancelByCloidWire, CancelWire, Cloid, Grouping, OrderWire, Tif, Tpsl};
use oppen_hl::{Action, Network};

use super::config::{
    APPROVAL_TTL_MS, DEFAULT_DAILY_LOSS_USD, DEFAULT_MARK_DIVERGENCE_BPS,
    DEFAULT_MARK_DIVERGENCE_WINDOW_MS, DEFAULT_MAX_ORDER_USD, DEFAULT_MAX_POSITION_USD,
    DEFAULT_ORDER_RATE, MAX_REASON_BYTES,
};
use super::deadman::DEAD_MAN_MIN_LEAD_MS;
use super::engine::NullAuditSink;
use super::store::MemoryStore;
use super::*;

// ---- fixtures -----------------------------------------------------------

/// 2026-09-03T00:00:00Z, an exact UTC midnight.
const MIDNIGHT_MS: u64 = 1_788_998_400_000;
/// Noon that day, the `now` of most tests.
const NOW_MS: u64 = MIDNIGHT_MS + 12 * 3_600_000;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("decimal literal")
}

/// The sub-account D1 pairs `alpha` with.
fn vault() -> oppen_hl::Address {
    oppen_hl::Address::parse("0x0d1d9635d0640821d15e323ac8adadfa9c111414").expect("address")
}

fn cloid() -> Cloid {
    Cloid::parse("0x00000000000000000000000000000001").expect("cloid")
}

fn asset(name: &str, sz_decimals: u32, max_leverage: u32) -> Asset {
    Asset {
        index: 7,
        info: AssetInfo {
            name: name.to_owned(),
            sz_decimals,
            max_leverage,
            margin_table_id: 0,
            is_delisted: false,
            only_isolated: false,
        },
    }
}

fn account(equity: Decimal) -> AccountSnapshot {
    AccountSnapshot {
        as_of_ms: NOW_MS,
        reconciled: true,
        equity_usd: equity,
        peak_equity_usd: equity,
        realized_pnl_today_usd: Decimal::ZERO,
        unrealized_pnl_usd: Decimal::ZERO,
        day_start_ms: MIDNIGHT_MS,
        total_position_notional_usd: Decimal::ZERO,
        positions: BTreeMap::new(),
        resting: Some(RestingExposure::none()),
    }
}

/// An exposure whose working book holds `szi` of `symbol` at `px`.
fn with_resting(mut exposure: Exposure, symbol: &str, szi: Decimal, px: Decimal) -> Exposure {
    let mut book = RestingExposure::none();
    book.szi.insert(symbol.to_owned(), szi);
    book.notional_usd = szi.abs() * px;
    exposure.agent.resting = Some(book);
    exposure
}

fn exposure(equity: Decimal) -> Exposure {
    Exposure {
        agent: account(equity),
        fleet: None,
    }
}

fn intent(symbol: &str, is_buy: bool, px: Decimal, sz: Decimal) -> OrderIntent {
    OrderIntent {
        symbol: symbol.to_owned(),
        is_buy,
        px,
        sz,
        kind: OrderKind::Limit { tif: Tif::Gtc },
        reduce_only: false,
        cloid: None,
        grouping: Grouping::Na,
        builder: None,
        max_slippage_bps: None,
        reason: "test".to_owned(),
    }
}

/// D-c's defaults, loosened only where a test needs room. Approval mode goes
/// off because item 28's queue is not built yet and every test that is not
/// about approval would otherwise stop there.
fn permissive(symbols: &[&str]) -> AgentGuardrails {
    AgentGuardrails {
        symbols: symbols.iter().map(|s| (*s).to_owned()).collect(),
        max_order_usd: d("1000000"),
        max_position_usd: d("1000000"),
        max_slippage_bps: d("10000"),
        order_rate: OrderRate {
            count: 1_000,
            per_ms: 1_000,
        },
        reduce_only: false,
        risk: RiskSettings {
            max_leverage: 50,
            margin_mode: MarginMode::Cross,
        },
        loss: LossLimits::UNSET,
        approval_required: false,
        freshness: Freshness::default(),
        max_mark_divergence_bps: DEFAULT_MARK_DIVERGENCE_BPS,
        mark_divergence_window_ms: DEFAULT_MARK_DIVERGENCE_WINDOW_MS,
    }
}

#[derive(Debug, Default)]
struct CountingSink {
    cleared: AtomicUsize,
    refused: AtomicUsize,
    operator: Mutex<Vec<OperatorAction>>,
}

impl AuditSink for CountingSink {
    fn record(&self, entry: &AuditEntry<'_>) -> Result<(), AuditError> {
        match entry.outcome {
            AuditOutcome::Cleared(_) => {
                self.cleared.fetch_add(1, Ordering::Relaxed);
            }
            AuditOutcome::Refused(_) => {
                self.refused.fetch_add(1, Ordering::Relaxed);
            }
            AuditOutcome::Operator(action) => self
                .operator
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(action.clone()),
        }
        Ok(())
    }
}

impl CountingSink {
    fn operator_actions(&self) -> Vec<OperatorAction> {
        self.operator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[derive(Debug)]
struct FailingSink;

impl AuditSink for FailingSink {
    fn record(&self, _entry: &AuditEntry<'_>) -> Result<(), AuditError> {
        Err(AuditError {
            detail: "the ledger disk is full".to_owned(),
        })
    }
}

/// A store whose writes fail, for the fail-closed path where a kill-switch
/// engagement cannot be persisted.
#[derive(Debug)]
struct FailingStore;

impl GuardrailStore for FailingStore {
    fn load(&self) -> Result<PersistedState, StoreError> {
        Ok(PersistedState::default())
    }
    fn save_guardrails(&self, _a: &AgentId, _c: &AgentGuardrails) -> Result<(), StoreError> {
        Ok(())
    }
    fn save_kill_switch(&self, _k: &KillSwitch) -> Result<(), StoreError> {
        Err(StoreError::Poisoned)
    }
    fn save_account_limits(&self, _l: &LossLimits) -> Result<(), StoreError> {
        Ok(())
    }
    fn save_vault(&self, _a: &AgentId, _v: &oppen_hl::Address) -> Result<(), StoreError> {
        Ok(())
    }
}

struct Fixture {
    engine: GuardrailEngine,
    agent: AgentId,
}

impl Fixture {
    fn new(config: AgentGuardrails) -> Self {
        Self::with(
            config,
            Arc::new(MemoryStore::new()),
            Arc::new(NullAuditSink),
        )
    }

    fn with(
        config: AgentGuardrails,
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
    ) -> Self {
        let engine = GuardrailEngine::new(store, sink, Network::Testnet).expect("engine");
        let agent = AgentId::new("alpha");
        engine
            .register_agent(&agent, Some(vault()), NOW_MS)
            .expect("register");
        engine
            .operator_set_guardrails(&agent, config, NOW_MS)
            .expect("set guardrails");
        Fixture { engine, agent }
    }

    fn evaluate(
        &self,
        intent: &OrderIntent,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
    ) -> Result<Cleared, Refusal> {
        self.engine
            .evaluate(&self.agent, intent, asset, market, exposure, NOW_MS)
    }
}

/// The order the engine actually built, read back off the wire.
fn wire_of(cleared: &Cleared) -> OrderWire {
    match cleared.action() {
        Action::Order { orders, .. } => {
            assert_eq!(orders.len(), 1, "one intent must produce one order");
            orders[0].clone()
        }
        other => panic!("expected an order action, got {other:?}"),
    }
}

fn wire_px(wire: &OrderWire) -> Decimal {
    Decimal::from_str(wire.p.as_str()).expect("wire price parses")
}

fn wire_sz(wire: &OrderWire) -> Decimal {
    Decimal::from_str(wire.s.as_str()).expect("wire size parses")
}

// ---- D-c: the refusal is the onboarding ---------------------------------

#[test]
fn a_freshly_paired_agent_is_refused_and_told_which_limit_to_raise() {
    let store: Arc<dyn GuardrailStore> = Arc::new(MemoryStore::new());
    let engine =
        GuardrailEngine::new(store, Arc::new(NullAuditSink), Network::Testnet).expect("engine");
    let agent = AgentId::new("alpha");
    let config = engine
        .register_agent(&agent, Some(vault()), NOW_MS)
        .expect("register");

    // D-c verbatim.
    assert!(config.symbols.is_empty());
    assert_eq!(config.max_order_usd, DEFAULT_MAX_ORDER_USD);
    assert_eq!(config.max_position_usd, DEFAULT_MAX_POSITION_USD);
    assert_eq!(config.loss.max_daily_loss_usd, Some(DEFAULT_DAILY_LOSS_USD));
    assert_eq!(config.order_rate, DEFAULT_ORDER_RATE);
    assert!(config.approval_required);

    let btc = asset("BTC", 5, 40);
    let refusal = engine
        .evaluate(
            &agent,
            &intent("BTC", true, d("100"), d("0.2")),
            &btc,
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("1000")),
            NOW_MS,
        )
        .expect_err("a fresh agent's first order must be refused");
    match refusal {
        Refusal::SymbolNotAllowed { symbol, allowed } => {
            assert_eq!(symbol, "BTC");
            assert!(allowed.is_empty(), "the refusal names the empty allowlist");
        }
        other => panic!("expected the allowlist refusal, got {other}"),
    }
}

// ---- boundaries: exactly at the limit, and one unit past -----------------

#[test]
fn the_symbol_allowlist_is_exact() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let eth = asset("ETH", 2, 40);
    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .is_ok()
    );
    assert!(matches!(
        f.evaluate(
            &intent("ETH", true, d("100"), d("1")),
            &eth,
            &MarketRef::fresh("ETH", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::SymbolNotAllowed { .. })
    ));
}

#[test]
fn the_order_notional_cap_allows_the_limit_and_refuses_a_cent_past() {
    let mut config = permissive(&["BTC"]);
    config.max_order_usd = d("25");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("25"), NOW_MS);

    let at = f
        .evaluate(
            &intent("BTC", true, d("25"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect("exactly at the cap clears");
    assert_eq!(wire_px(&wire_of(&at)) * wire_sz(&wire_of(&at)), d("25"));

    let past = f
        .evaluate(
            &intent("BTC", true, d("25.01"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("a cent past the cap is refused");
    match past {
        Refusal::OrderNotional {
            observed_usd,
            limit_usd,
            ..
        } => {
            assert_eq!(observed_usd, d("25.01"));
            assert_eq!(limit_usd, d("25"));
        }
        other => panic!("expected the notional refusal, got {other}"),
    }
}

#[test]
fn the_position_cap_is_measured_after_the_fill() {
    let mut config = permissive(&["BTC"]);
    config.max_position_usd = d("100");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok(),
        "a fill landing exactly on the cap clears"
    );
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1.01")),
            &btc,
            &market,
            &exposure(d("100000")),
        ),
        Err(Refusal::PositionNotional { .. })
    ));

    // An existing position counts, and a sell against a long reduces it.
    let mut long = exposure(d("100000"));
    long.agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("1") });
    long.agent.total_position_notional_usd = d("100");
    assert!(
        matches!(
            f.evaluate(
                &intent("BTC", true, d("100"), d("0.1")),
                &btc,
                &market,
                &long
            ),
            Err(Refusal::PositionNotional { .. })
        ),
        "adding to a position already at the cap is refused"
    );
    assert!(
        f.evaluate(
            &intent("BTC", false, d("100"), d("0.5")),
            &btc,
            &market,
            &long
        )
        .is_ok(),
        "halving the same position clears"
    );
}

#[test]
fn the_leverage_cap_uses_the_tighter_of_the_operator_and_the_venue() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_leverage = 2;
    let f = Fixture::new(config);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let btc = asset("BTC", 2, 40);
    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("2")),
            &btc,
            &market,
            &exposure(d("100")),
        )
        .is_ok(),
        "$200 of notional on $100 of equity is exactly 2x"
    );
    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("2.01")),
            &btc,
            &market,
            &exposure(d("100")),
        )
        .expect_err("2.01x is past the cap")
    {
        Refusal::Leverage {
            observed, limit, ..
        } => {
            assert_eq!(observed, d("2.01"));
            assert_eq!(limit, 2);
        }
        other => panic!("expected the leverage refusal, got {other}"),
    }

    // The venue's own maximum is the harder bound of the two.
    let thin = asset("BTC", 2, 1);
    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("2")),
            &thin,
            &market,
            &exposure(d("100")),
        )
        .expect_err("the venue caps this asset at 1x")
    {
        Refusal::Leverage { limit, .. } => assert_eq!(limit, 1),
        other => panic!("expected the leverage refusal, got {other}"),
    }
}

#[test]
fn slippage_is_measured_only_in_the_direction_that_costs_money() {
    let mut config = permissive(&["BTC"]);
    config.max_slippage_bps = d("10");
    let f = Fixture::new(config.clone());
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // 10 bps above the mid on a buy: exactly at the cap.
    let at = f
        .evaluate(
            &intent("BTC", true, d("100.1"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect("exactly at the cap clears");
    match &at.clearance().kind {
        ClearedKind::Order { slippage_bps, .. } => assert_eq!(*slippage_bps, d("10")),
        other => panic!("expected an order clearance, got {other:?}"),
    }

    match f
        .evaluate(
            &intent("BTC", true, d("100.2"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("20 bps is past a 10 bps cap")
    {
        Refusal::Slippage {
            observed_bps,
            limit_bps,
            ..
        } => {
            assert_eq!(observed_bps, d("20"));
            assert_eq!(limit_bps, d("10"));
        }
        other => panic!("expected the slippage refusal, got {other}"),
    }

    // A passive bid far below the mid is not slippage, and must not be
    // refused for being a long way from the reference.
    assert!(
        f.evaluate(
            &intent("BTC", true, d("50"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok()
    );
    // An agent may bind itself tighter than its guardrail, never looser.
    let mut tight = intent("BTC", true, d("100.1"), d("1"));
    tight.max_slippage_bps = Some(d("5"));
    assert!(matches!(
        f.evaluate(&tight, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::Slippage { limit_bps, .. }) if limit_bps == d("5")
    ));
    let mut loose = intent("BTC", true, d("100.2"), d("1"));
    loose.max_slippage_bps = Some(d("100000"));
    assert!(matches!(
        f.evaluate(&loose, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::Slippage { limit_bps, .. }) if limit_bps == d("10")
    ));
}

#[test]
fn a_protective_stop_is_priced_against_its_own_trigger_not_the_mid() {
    let mut config = permissive(&["BTC"]);
    config.max_slippage_bps = d("50");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // A stop-market sell 10% below the mid: enormous distance from the mid,
    // but the limit price sits 10 bps under its own trigger.
    let mut stop = intent("BTC", false, d("89.91"), d("1"));
    stop.kind = OrderKind::Trigger {
        is_market: true,
        trigger_px: d("90"),
        tpsl: Tpsl::Sl,
    };
    let cleared = f
        .evaluate(&stop, &btc, &market, &exposure(d("100000")))
        .expect("a protective stop is not a slippage breach");
    match &cleared.clearance().kind {
        ClearedKind::Order {
            slippage_bps,
            slippage_reference_px,
            ..
        } => {
            assert_eq!(
                *slippage_reference_px,
                d("90"),
                "the trigger is the reference"
            );
            assert_eq!(*slippage_bps, d("10"));
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }

    // The same stop with a limit far under its trigger is a real breach.
    let mut wide = stop.clone();
    wide.px = d("80");
    assert!(matches!(
        f.evaluate(&wide, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::Slippage { .. })
    ));
}

#[test]
fn reduce_only_mode_requires_the_flag_and_an_actual_reduction() {
    let mut config = permissive(&["BTC"]);
    config.reduce_only = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let mut long = exposure(d("100000"));
    long.agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("2") });
    long.agent.total_position_notional_usd = d("200");

    let reduce = |sz: &str, is_buy: bool, flagged: bool| {
        let mut i = intent("BTC", is_buy, d("100"), d(sz));
        i.reduce_only = flagged;
        i
    };

    assert!(matches!(
        f.evaluate(&reduce("1", false, false), &btc, &market, &long),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::NotFlagged,
            ..
        })
    ));
    assert!(matches!(
        f.evaluate(&reduce("1", true, true), &btc, &market, &long),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::SameSide,
            ..
        })
    ));
    assert!(matches!(
        f.evaluate(&reduce("2.01", false, true), &btc, &market, &long),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::Oversized,
            ..
        })
    ));
    assert!(matches!(
        f.evaluate(
            &reduce("1", false, true),
            &btc,
            &market,
            &exposure(d("100000"))
        ),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::NoPosition,
            ..
        })
    ));
    // Exactly closing the position is the boundary and must clear.
    assert!(
        f.evaluate(&reduce("2", false, true), &btc, &market, &long)
            .is_ok()
    );
}

#[test]
fn the_order_rate_cap_holds_at_the_engine_and_refills_over_time() {
    let mut config = permissive(&["BTC"]);
    config.order_rate = DEFAULT_ORDER_RATE; // 5 per 5 minutes (D-c)
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    for i in 0..5 {
        assert!(
            f.evaluate(&order, &btc, &market, &exposure(d("100000")))
                .is_ok(),
            "order {i} is inside the budget"
        );
    }
    match f
        .evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect_err("the sixth order exhausts the budget")
    {
        Refusal::OrderRate {
            limit,
            window_ms,
            retry_after_ms,
            ..
        } => {
            assert_eq!(limit, 5);
            assert_eq!(window_ms, 300_000);
            assert_eq!(retry_after_ms, 60_000);
        }
        other => panic!("expected the rate refusal, got {other}"),
    }

    // One token exactly 60s later, and not a millisecond earlier.
    let later = |ms: u64| {
        let mut market = market.clone();
        market.as_of_ms = NOW_MS + ms;
        let mut exposure = exposure(d("100000"));
        exposure.agent.as_of_ms = NOW_MS + ms;
        (market, exposure)
    };
    let (m, e) = later(59_999);
    assert!(
        f.engine
            .evaluate(&f.agent, &order, &btc, &m, &e, NOW_MS + 59_999)
            .is_err()
    );
    let (m, e) = later(60_000);
    assert!(
        f.engine
            .evaluate(&f.agent, &order, &btc, &m, &e, NOW_MS + 60_000)
            .is_ok()
    );
}

#[test]
fn a_refused_order_does_not_spend_a_rate_token() {
    let mut config = permissive(&["BTC"]);
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 300_000,
    };
    config.max_order_usd = d("25");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    for _ in 0..10 {
        assert!(matches!(
            f.evaluate(
                &intent("BTC", true, d("100"), d("1")),
                &btc,
                &market,
                &exposure(d("100000")),
            ),
            Err(Refusal::OrderNotional { .. })
        ));
    }
    assert!(
        f.evaluate(
            &intent("BTC", true, d("25"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok(),
        "the single token survived ten refusals"
    );
}

// ---- item 25: the loss circuit breaker ----------------------------------

#[test]
fn the_daily_loss_breaker_trips_the_kill_switch_and_the_next_order_is_paused() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-25");
    losing.agent.peak_equity_usd = d("1000");

    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &losing,
        )
        .expect_err("the budget is exhausted")
    {
        Refusal::LossLimit {
            scope,
            kind,
            observed_usd,
            limit_usd,
        } => {
            assert_eq!(scope, KillScope::agent("alpha"));
            assert_eq!(kind, LossKind::Daily);
            assert_eq!(observed_usd, d("25"));
            assert_eq!(limit_usd, d("25"));
        }
        other => panic!("expected the loss refusal, got {other}"),
    }

    // The switch is now engaged, so a healthy snapshot does not un-stop it.
    assert!(
        f.engine
            .kill_switch()
            .is_engaged(&KillScope::agent("alpha"))
    );
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        ),
        Err(Refusal::TradingPaused { .. })
    ));
}

#[test]
fn an_account_wide_breach_stops_every_agent() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_set_account_limits(
            LossLimits {
                max_daily_loss_usd: Some(d("400")),
                max_drawdown_usd: None,
            },
            NOW_MS,
        )
        .expect("set account limits");
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let mut fleet = account(d("4600"));
    fleet.realized_pnl_today_usd = d("-400");
    fleet.peak_equity_usd = d("5000");
    let with_fleet = Exposure {
        agent: account(d("1000")),
        fleet: Some(fleet),
    };

    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &with_fleet
        ),
        Err(Refusal::LossLimit {
            scope: KillScope::Global,
            ..
        })
    ));
    assert!(f.engine.kill_switch().is_engaged(&KillScope::Global));
}

/// Item 24's position cap is a cap on exposure, and a working order is
/// exposure. Five orders each landing exactly on the cap used to clear
/// one after another, because each was measured against a flat book.
#[test]
fn the_position_cap_counts_working_orders_so_it_cannot_be_split() {
    let mut config = permissive(&["BTC"]);
    config.max_position_usd = d("100");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    // The first order fills the cap on its own.
    let flat = exposure(d("100000"));
    assert!(f.evaluate(&order, &btc, &market, &flat).is_ok());

    // Once it is resting, every further one is refused — including the four
    // that the old engine admitted, which together held 5x the cap.
    let mut book = flat.clone();
    for n in 1..5 {
        book = with_resting(book, "BTC", Decimal::from(n), d("100"));
        match f
            .evaluate(&order, &btc, &market, &book)
            .expect_err("order {n} is past the cap once the book is counted")
        {
            Refusal::PositionNotional {
                observed_usd,
                resting_usd,
                limit_usd,
                ..
            } => {
                assert_eq!(observed_usd, Decimal::from(n + 1) * d("100"));
                assert_eq!(resting_usd, Decimal::from(n) * d("100"));
                assert_eq!(limit_usd, d("100"));
            }
            other => panic!("expected the position refusal, got {other}"),
        }
    }
}

/// Working orders on other symbols still consume the account's leverage.
#[test]
fn the_leverage_cap_counts_working_orders_on_other_symbols() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_leverage = 2;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    // $100 of new BTC exposure on $100 of equity is 1x on its own.
    assert!(
        f.evaluate(&order, &btc, &market, &exposure(d("100")))
            .is_ok()
    );

    // With $150 of ETH already working, the same order is 2.5x.
    let mut with_eth = exposure(d("100"));
    with_eth.agent.resting = Some(RestingExposure {
        szi: BTreeMap::from([("ETH".to_owned(), d("1.5"))]),
        notional_usd: d("150"),
    });
    assert!(matches!(
        f.evaluate(&order, &btc, &market, &with_eth),
        Err(Refusal::Leverage { observed, .. }) if observed == d("2.5")
    ));
}

// ---- item 26: the kill switch -------------------------------------------

#[test]
fn the_kill_switch_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("testnet.db");
    let agent = AgentId::new("alpha");

    {
        let store: Arc<dyn GuardrailStore> =
            Arc::new(SqliteGuardrailStore::open(&path).expect("open"));
        let engine =
            GuardrailEngine::new(store, Arc::new(NullAuditSink), Network::Testnet).expect("engine");
        engine
            .register_agent(&agent, Some(vault()), NOW_MS)
            .expect("register");
        engine
            .operator_set_guardrails(&agent, permissive(&["BTC"]), NOW_MS)
            .expect("set guardrails");
        let effect = engine
            .operator_engage_kill(KillScope::agent(agent.clone()), KillReason::Operator, 7)
            .expect("engage");
        assert!(effect.newly_engaged);
        assert!(effect.cancel_for.contains(&agent));
    }

    // A whole new process would see exactly this.
    let store: Arc<dyn GuardrailStore> =
        Arc::new(SqliteGuardrailStore::open(&path).expect("reopen"));
    let engine =
        GuardrailEngine::new(store, Arc::new(NullAuditSink), Network::Testnet).expect("engine");
    assert!(
        engine
            .kill_switch()
            .is_engaged(&KillScope::agent(agent.clone()))
    );
    let refusal = engine
        .evaluate(
            &agent,
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
        .expect_err("still paused after the restart");
    assert!(matches!(
        refusal,
        Refusal::TradingPaused {
            reason: KillReason::Operator,
            since_ms: 7,
            ..
        }
    ));

    // And the guardrails came back too, not D-c defaults.
    assert_eq!(
        engine.guardrails(&agent).map(|g| g.max_order_usd),
        Some(d("1000000"))
    );
}

#[test]
fn a_global_engagement_pauses_an_agent_with_no_engagement_of_its_own() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::FeedFailure, 1)
        .expect("engage");
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::TradingPaused {
            scope: KillScope::Global,
            ..
        })
    ));
    f.engine
        .operator_release_kill(&KillScope::Global, NOW_MS)
        .expect("release");
    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .is_ok()
    );
}

#[test]
fn cancelling_still_clears_while_the_kill_switch_is_engaged() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, 1)
        .expect("engage");
    let cleared = f
        .engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 42 }],
            "kill switch",
            NOW_MS,
        )
        .expect("a cancel is risk-reducing and must clear while paused");
    assert!(matches!(
        cleared.action(),
        Action::Cancel { cancels } if cancels.len() == 1
    ));
    assert_eq!(cleared.clearance().kind, ClearedKind::Cancel { count: 1 });
}

/// Item 26 makes cancelling resting orders part of what engaging the switch
/// *does*, and item 25's whole point is stopping an agent that is grinding
/// the account down overnight. A trip that pauses new orders but leaves the
/// working ones live has not stopped anything.
#[test]
fn a_breaker_trip_queues_the_same_cancels_an_operator_engagement_would() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::new(config);
    assert!(f.engine.take_pending_kill_effects().is_empty());

    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-25");
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &losing,
        ),
        Err(Refusal::LossLimit { .. })
    ));

    let effects = f.engine.take_pending_kill_effects();
    assert_eq!(
        effects.len(),
        1,
        "the trip must name whose orders to cancel"
    );
    assert!(effects[0].newly_engaged);
    assert_eq!(effects[0].scope, KillScope::agent("alpha"));
    assert!(effects[0].cancel_for.contains(&f.agent));

    // Identical to what the operator pressing the same scope would produce.
    let manual = f
        .engine
        .operator_engage_kill(KillScope::agent("alpha"), KillReason::Operator, NOW_MS)
        .expect("engage");
    assert_eq!(effects[0].cancel_for, manual.cancel_for);

    // Drained, so a caller cannot double-issue the cancels.
    assert!(f.engine.take_pending_kill_effects().is_empty());
}

/// A `Global` trip has to be actionable without an agent id in hand.
#[test]
fn a_global_breaker_trip_names_every_agent() {
    let f = Fixture::new(permissive(&["BTC"]));
    let beta = AgentId::new("beta");
    f.engine
        .register_agent(&beta, None, NOW_MS)
        .expect("register");
    f.engine
        .operator_set_account_limits(
            LossLimits {
                max_daily_loss_usd: Some(d("400")),
                max_drawdown_usd: None,
            },
            NOW_MS,
        )
        .expect("set account limits");

    let mut fleet = account(d("4600"));
    fleet.realized_pnl_today_usd = d("-400");
    let with_fleet = Exposure {
        agent: account(d("1000")),
        fleet: Some(fleet),
    };
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &with_fleet,
        ),
        Err(Refusal::LossLimit {
            scope: KillScope::Global,
            ..
        })
    ));
    let effects = f.engine.take_pending_kill_effects();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].scope, KillScope::Global);
    assert_eq!(effects[0].cancel_for, f.engine.agents());
    assert_eq!(
        effects[0].cancel_for,
        BTreeSet::from([f.agent.clone(), beta])
    );
}

/// A kill-switch write failure refuses the order, but must still hand the
/// caller the cancels: staying stopped and leaving the book live is the
/// worst of both.
#[test]
fn a_trip_whose_state_write_fails_still_queues_the_cancels() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::with(config, Arc::new(FailingStore), Arc::new(NullAuditSink));
    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-30");
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &losing,
        ),
        Err(Refusal::Unevaluable(Unevaluable::StateWriteFailed { .. }))
    ));
    let effects = f.engine.take_pending_kill_effects();
    assert_eq!(effects.len(), 1);
    assert!(effects[0].cancel_for.contains(&f.agent));
}

// ---- item 27: the dead-man's switch -------------------------------------

#[test]
fn the_dead_man_switch_arms_while_an_agent_is_active() {
    let f = Fixture::new(permissive(&["BTC"]));
    assert_eq!(f.engine.dead_man_intent(NOW_MS, None), DeadManIntent::Hold);

    f.engine.set_agent_active(&f.agent, true);
    assert_eq!(f.engine.active_agents(), 1);
    let intent = f.engine.dead_man_intent(NOW_MS, None);
    let DeadManIntent::Arm { cancel_at_ms } = intent else {
        panic!("expected an arm, got {intent:?}");
    };
    let cleared = f
        .engine
        .clear_dead_man(intent, NOW_MS)
        .expect("arming clears")
        .expect("an arm produces an action");
    assert_eq!(
        cleared.action(),
        &Action::ScheduleCancel {
            time: Some(cancel_at_ms)
        }
    );

    f.engine.set_agent_active(&f.agent, false);
    assert_eq!(
        f.engine.dead_man_intent(NOW_MS, Some(cancel_at_ms)),
        DeadManIntent::Disarm
    );
    let disarm = f
        .engine
        .clear_dead_man(DeadManIntent::Disarm, NOW_MS)
        .expect("disarming clears")
        .expect("a disarm produces an action");
    assert_eq!(disarm.action(), &Action::ScheduleCancel { time: None });
    assert!(
        f.engine
            .clear_dead_man(DeadManIntent::Hold, NOW_MS)
            .expect("hold clears")
            .is_none()
    );
}

#[test]
fn a_schedule_cancel_inside_the_venue_minimum_is_refused() {
    let f = Fixture::new(permissive(&["BTC"]));
    assert!(matches!(
        f.engine
            .clear_schedule_cancel(Some(NOW_MS + DEAD_MAN_MIN_LEAD_MS - 1), NOW_MS),
        Err(Refusal::VenueRule(VenueRule::ScheduleCancelTooSoon { .. }))
    ));
    assert!(
        f.engine
            .clear_schedule_cancel(Some(NOW_MS + DEAD_MAN_MIN_LEAD_MS), NOW_MS)
            .is_ok()
    );
}

// ---- fail-closed --------------------------------------------------------

/// Every way the engine can be blind, and the refusal each one produces.
/// A new unevaluable input belongs in this table before it belongs in the
/// engine.
#[test]
fn every_unevaluable_input_refuses() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let good_market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let good_exposure = exposure(d("100000"));
    let order = intent("BTC", true, d("100"), d("1"));

    // Sanity: the baseline clears, so every refusal below is caused by the
    // one thing that was changed.
    assert!(
        f.evaluate(&order, &btc, &good_market, &good_exposure)
            .is_ok()
    );

    let stale_market = MarketRef {
        as_of_ms: NOW_MS - 2_001,
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &stale_market, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::StaleMarketData { .. }))
    ));

    let no_price = MarketRef {
        reference_px: None,
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &no_price, &good_exposure),
        Err(Refusal::Unevaluable(
            Unevaluable::MissingReferencePrice { .. }
        ))
    ));

    let zero_price = MarketRef {
        reference_px: Some(Decimal::ZERO),
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &zero_price, &good_exposure),
        Err(Refusal::Unevaluable(
            Unevaluable::MissingReferencePrice { .. }
        ))
    ));

    for quality in [
        FeedQuality::Warmup,
        FeedQuality::Degraded,
        FeedQuality::Unusable,
    ] {
        let degraded = MarketRef {
            quality,
            ..good_market.clone()
        };
        assert!(
            matches!(
                f.evaluate(&order, &btc, &degraded, &good_exposure),
                Err(Refusal::Unevaluable(Unevaluable::DegradedFeed { .. }))
            ),
            "{quality} must refuse"
        );
    }

    let diverged = MarketRef {
        mark_divergence_bps: Some(d("6")),
        mark_divergent_since_ms: Some(NOW_MS - 30_000),
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &diverged, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::MarkDivergence { .. }))
    ));
    // Inside tolerance, or not yet sustained, is not a refusal: §14.2
    // measured containment failing 37-39% of the time.
    let brief = MarketRef {
        mark_divergent_since_ms: Some(NOW_MS - 29_999),
        ..diverged.clone()
    };
    assert!(f.evaluate(&order, &btc, &brief, &good_exposure).is_ok());
    let small = MarketRef {
        mark_divergence_bps: Some(d("5")),
        ..diverged.clone()
    };
    assert!(f.evaluate(&order, &btc, &small, &good_exposure).is_ok());

    let backwards = MarketRef {
        as_of_ms: NOW_MS + 1,
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &backwards, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::ClockWentBackwards { .. }))
    ));

    let mut unreconciled = good_exposure.clone();
    unreconciled.agent.reconciled = false;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &unreconciled),
        Err(Refusal::Unevaluable(
            Unevaluable::UnreconciledAccount { .. }
        ))
    ));

    let mut no_book = good_exposure.clone();
    no_book.agent.resting = None;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &no_book),
        Err(Refusal::Unevaluable(Unevaluable::MissingRestingOrders))
    ));

    let mut stale_account = good_exposure.clone();
    stale_account.agent.as_of_ms = NOW_MS - 5_001;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &stale_account),
        Err(Refusal::Unevaluable(Unevaluable::StaleAccountState { .. }))
    ));

    let mut broke = good_exposure.clone();
    broke.agent.equity_usd = Decimal::ZERO;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &broke),
        Err(Refusal::Unevaluable(Unevaluable::NonPositiveEquity { .. }))
    ));

    let mut yesterday = good_exposure.clone();
    yesterday.agent.day_start_ms = MIDNIGHT_MS - 86_400_000;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &yesterday),
        Err(Refusal::Unevaluable(Unevaluable::LossWindowMismatch { .. }))
    ));

    let wrong_market = MarketRef::fresh("ETH", d("100"), NOW_MS);
    assert!(matches!(
        f.evaluate(&order, &btc, &wrong_market, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::InputMismatch { .. }))
    ));
    assert!(matches!(
        f.evaluate(&order, &asset("ETH", 2, 40), &good_market, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::InputMismatch { .. }))
    ));

    let mut unnamed = order.clone();
    unnamed.reason = "   ".to_owned();
    assert!(matches!(
        f.evaluate(&unnamed, &btc, &good_market, &good_exposure),
        Err(Refusal::MissingReason)
    ));

    let unknown = AgentId::new("nobody");
    assert!(matches!(
        f.engine
            .evaluate(&unknown, &order, &btc, &good_market, &good_exposure, NOW_MS),
        Err(Refusal::Unevaluable(Unevaluable::UnknownAgent { .. }))
    ));
}

#[test]
fn account_wide_limits_with_no_fleet_snapshot_refuse() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_set_account_limits(
            LossLimits {
                max_daily_loss_usd: Some(d("500")),
                max_drawdown_usd: None,
            },
            NOW_MS,
        )
        .expect("set");
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::Unevaluable(Unevaluable::MissingFleetState))
    ));
}

/// `rust_decimal`'s operators panic on overflow, and an agent picks the price
/// and the size. An absurd order must be refused, not fatal.
#[test]
fn an_order_whose_arithmetic_overflows_is_refused_not_fatal() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let huge = Decimal::MAX.round_dp(0);
    let market = MarketRef::fresh("BTC", huge, NOW_MS);
    let refusal = f
        .evaluate(
            &intent("BTC", true, huge, huge),
            &btc,
            &market,
            &exposure(d("100")),
        )
        .expect_err("an unrepresentable order is refused");
    assert!(refusal.is_unevaluable() || matches!(refusal, Refusal::VenueRule(_)));

    // And a position snapshot large enough to overflow the post-fill maths.
    let mut vast = exposure(d("100"));
    vast.agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: huge });
    vast.agent.total_position_notional_usd = huge;
    let refusal = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &MarketRef::fresh("BTC", huge, NOW_MS),
            &vast,
        )
        .expect_err("an unrepresentable exposure is refused");
    assert!(refusal.is_unevaluable(), "got {refusal}");
}

#[test]
fn a_ledger_write_failure_refuses_the_order() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(MemoryStore::new()),
        Arc::new(FailingSink),
    );
    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect_err("an unexplainable order does not happen")
    {
        Refusal::Unevaluable(Unevaluable::AuditWriteFailed { detail }) => {
            assert!(detail.contains("disk"));
        }
        other => panic!("expected the audit refusal, got {other}"),
    }
}

/// Fail-closed exists to stop new exposure. Applying it to the actions that
/// *remove* exposure inverts the property: a full ledger disk would take away
/// the operator's ability to stop trading, take the kill switch's own cancels
/// down with it, and stop `scheduleCancel` from being disarmed — all while
/// the orders it cannot cancel keep working.
#[test]
fn a_ledger_write_failure_never_blocks_a_cancel_or_the_dead_man_switch() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(MemoryStore::new()),
        Arc::new(FailingSink),
    );

    f.engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 42 }],
            "the disk is full and the orders still have to go",
            NOW_MS,
        )
        .expect("a cancel is risk-reducing and must clear");

    f.engine
        .clear_cancel_by_cloid(
            &f.agent,
            vec![CancelByCloidWire {
                asset: 7,
                cloid: cloid(),
            }],
            "cancel by cloid after a timeout",
            NOW_MS,
        )
        .expect("cancel-by-cloid is the only safe move after a timeout");

    let disarm = f
        .engine
        .clear_dead_man(DeadManIntent::Disarm, NOW_MS)
        .expect("disarming clears")
        .expect("a disarm produces an action");
    assert_eq!(disarm.action(), &Action::ScheduleCancel { time: None });

    f.engine
        .clear_schedule_cancel(Some(NOW_MS + DEAD_MAN_MIN_LEAD_MS), NOW_MS)
        .expect("re-arming clears");

    // The order path is unchanged: an unexplainable *order* still does not
    // happen.
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::Unevaluable(Unevaluable::AuditWriteFailed { .. }))
    ));
}

/// Item 18 names guardrail trips, approval decisions and kill-switch changes
/// as ledger events. Without these rows an export cannot say why an order
/// refused yesterday cleared today.
#[test]
fn operator_actions_reach_the_ledger() {
    let sink = Arc::new(CountingSink::default());
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(MemoryStore::new()),
        sink.clone() as Arc<dyn AuditSink>,
    );
    f.engine
        .operator_set_account_limits(LossLimits::UNSET, NOW_MS)
        .expect("set");
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        .expect("engage");
    f.engine
        .operator_release_kill(&KillScope::Global, NOW_MS)
        .expect("release");
    f.engine
        .operator_set_global_rate_budget(GlobalRateBudget::default(), NOW_MS)
        .expect("set budget");

    let actions = sink.operator_actions();
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OperatorAction::AgentRegistered { .. })),
        "{actions:?}"
    );
    // The guardrail row carries both sides, so the change is readable.
    let changed = actions
        .iter()
        .find_map(|a| match a {
            OperatorAction::GuardrailsChanged { before, after } => Some((before, after)),
            _ => None,
        })
        .expect("the fixture's own set_guardrails is recorded");
    assert_eq!(
        changed.0.as_deref().map(|c| c.max_order_usd),
        Some(DEFAULT_MAX_ORDER_USD)
    );
    assert_eq!(changed.1.max_order_usd, d("1000000"));
    assert!(actions.iter().any(|a| matches!(
        a,
        OperatorAction::KillEngaged {
            newly_engaged: true,
            ..
        }
    )));
    assert!(actions.iter().any(|a| matches!(
        a,
        OperatorAction::KillReleased {
            was_engaged: true,
            ..
        }
    )));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OperatorAction::AccountLimitsChanged { .. }))
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OperatorAction::GlobalRateBudgetChanged { .. }))
    );
}

/// An operator action that has already happened must not be reported as
/// failed because its row could not be written.
#[test]
fn an_operator_action_still_happens_when_its_ledger_row_fails() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(MemoryStore::new()),
        Arc::new(FailingSink),
    );
    let effect = f
        .engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        .expect("the switch engages even when the ledger cannot record it");
    assert!(effect.newly_engaged);
    assert!(f.engine.kill_switch().is_engaged(&KillScope::Global));
}

// ---- item 10: the address-wide request budget ---------------------------

/// Item 10's budget is metered per address, so no per-agent cap bounds it.
/// An order may not draw it below the reserve; a cancel may, because item 10
/// says to always reserve headroom for risk-reducing actions and a budget
/// that refuses a cancel has spent that headroom on the wrong thing.
#[test]
fn the_global_budget_refuses_orders_at_the_reserve_but_never_a_cancel() {
    let f = Fixture::new(permissive(&["BTC"]));
    // Four requests, one of them reserved.
    f.engine
        .operator_set_global_rate_budget(
            GlobalRateBudget {
                rate: OrderRate {
                    count: 4,
                    per_ms: 4_000_000,
                },
                reserve: 1,
            },
            NOW_MS,
        )
        .expect("set budget");

    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));
    for i in 0..3 {
        let cleared = f
            .evaluate(&order, &btc, &market, &exposure(d("100000")))
            .unwrap_or_else(|e| panic!("order {i} is inside the budget: {e}"));
        assert_eq!(
            cleared.clearance().utilization.global_tokens_remaining,
            Decimal::from(3 - i)
        );
    }
    match f
        .evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect_err("the fourth order would eat the reserve")
    {
        Refusal::GlobalRateBudget {
            tokens_available,
            reserve,
            ..
        } => {
            assert_eq!(tokens_available, Decimal::ONE);
            assert_eq!(reserve, 1);
        }
        other => panic!("expected the global budget refusal, got {other}"),
    }

    // The reserve is what the cancel path is for, and once it is gone the
    // cancel still clears rather than being throttled.
    for _ in 0..3 {
        f.engine
            .clear_cancel(
                &f.agent,
                vec![CancelWire { a: 7, o: 42 }],
                "winding down",
                NOW_MS,
            )
            .expect("a cancel is never refused by the request budget");
    }
}

// ---- item 30: the reason is untrusted text ------------------------------

#[test]
fn an_unbounded_or_control_laden_reason_is_refused_at_both_boundaries() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let with_reason = |reason: String| {
        let mut i = intent("BTC", true, d("100"), d("1"));
        i.reason = reason;
        i
    };

    assert!(
        f.evaluate(
            &with_reason("a".repeat(MAX_REASON_BYTES)),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok(),
        "exactly at the limit is allowed"
    );
    match f
        .evaluate(
            &with_reason("a".repeat(MAX_REASON_BYTES + 1)),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("one byte past the limit is refused")
    {
        Refusal::ReasonTooLong {
            len_bytes,
            max_bytes,
        } => {
            assert_eq!(len_bytes, MAX_REASON_BYTES + 1);
            assert_eq!(max_bytes, MAX_REASON_BYTES);
        }
        other => panic!("expected the length refusal, got {other}"),
    }

    // Newline and tab are the only control characters a sentence needs.
    assert!(
        f.evaluate(
            &with_reason("funding\nflipped\tnegative".to_owned()),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok()
    );
    match f
        .evaluate(
            &with_reason("\u{1b}[2J fake price row".to_owned()),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("an ANSI escape is not inert on a character grid")
    {
        Refusal::ReasonControlCharacter { at_byte, codepoint } => {
            assert_eq!(at_byte, 0);
            assert_eq!(codepoint, 0x1b);
        }
        other => panic!("expected the control-character refusal, got {other}"),
    }

    // The cancel path takes the same reason and must check it identically.
    assert!(matches!(
        f.engine.clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 42 }],
            &"a".repeat(MAX_REASON_BYTES + 1),
            NOW_MS,
        ),
        Err(Refusal::ReasonTooLong { .. })
    ));
}

#[test]
fn a_kill_switch_write_failure_refuses_rather_than_forgetting_the_trip() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::with(config, Arc::new(FailingStore), Arc::new(NullAuditSink));
    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-30");

    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &losing,
        ),
        Err(Refusal::Unevaluable(Unevaluable::StateWriteFailed { .. }))
    ));
    // The engagement still stands in memory: the conservative direction.
    assert!(
        f.engine
            .kill_switch()
            .is_engaged(&KillScope::agent("alpha"))
    );
}

#[test]
fn refusals_are_recorded_too() {
    let sink = Arc::new(CountingSink::default());
    let f = Fixture::with(
        permissive(&[]),
        Arc::new(MemoryStore::new()),
        sink.clone() as Arc<dyn AuditSink>,
    );
    let _ = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", d("100"), NOW_MS),
        &exposure(d("100000")),
    );
    assert_eq!(sink.refused.load(Ordering::Relaxed), 1);
    assert_eq!(sink.cleared.load(Ordering::Relaxed), 0);
}

// ---- item 28: approval mode ---------------------------------------------

#[test]
fn approval_is_the_last_check_and_is_not_charged_twice() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.max_order_usd = d("25");
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 300_000,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // An order that would be refused anyway is refused, not queued.
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        ),
        Err(Refusal::OrderNotional { .. })
    ));
    assert!(f.engine.pending_proposals(NOW_MS).is_empty());

    // A good order becomes a proposal, and spends the single rate token.
    let order = intent("BTC", true, d("25"), d("1"));
    let Err(Refusal::ApprovalRequired {
        approval_id,
        expires_at_ms,
        ..
    }) = f.evaluate(&order, &btc, &market, &exposure(d("100000")))
    else {
        panic!("a good order under approval mode becomes a proposal");
    };
    assert_eq!(expires_at_ms, NOW_MS + APPROVAL_TTL_MS);
    let pending = f.engine.pending_proposals(NOW_MS);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id(), approval_id);
    assert_eq!(pending[0].intent(), &order);

    // The operator approves; the re-evaluation must not need a second token.
    assert!(
        f.engine
            .operator_approve_proposal(&approval_id, &btc, &market, &exposure(d("100000")), NOW_MS)
            .is_ok(),
        "an approved proposal does not pay the rate token twice"
    );
    // And it is spent: one approval, one evaluation.
    assert!(f.engine.pending_proposals(NOW_MS).is_empty());
    assert!(matches!(
        f.engine.operator_approve_proposal(
            &approval_id,
            &btc,
            &market,
            &exposure(d("100000")),
            NOW_MS
        ),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
}

/// The whole of finding 2: there must be no value a caller can build that
/// asserts it was approved. `OrderIntent` has no approval field, so the only
/// way to reach the approved path is an id the engine minted — and the engine
/// re-evaluates *its own* stored intent, never one the caller re-supplies.
#[test]
fn an_approval_cannot_be_asserted_by_the_caller_and_cannot_be_swapped() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 300_000,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // A made-up id is not an approval.
    assert!(matches!(
        f.engine
            .operator_approve_proposal("a-1", &btc, &market, &exposure(d("100000")), NOW_MS),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));

    // A small order is proposed; approving it signs the small order, and
    // there is no parameter by which a larger one could be substituted.
    let small = intent("BTC", true, d("100"), d("1"));
    let Err(Refusal::ApprovalRequired { approval_id, .. }) =
        f.evaluate(&small, &btc, &market, &exposure(d("100000")))
    else {
        panic!("expected a proposal");
    };
    let cleared = f
        .engine
        .operator_approve_proposal(&approval_id, &btc, &market, &exposure(d("100000")), NOW_MS)
        .expect("the operator approves the proposal it was shown");
    match &cleared.clearance().kind {
        ClearedKind::Order { sz, .. } => assert_eq!(*sz, d("1")),
        other => panic!("expected an order clearance, got {other:?}"),
    }

    // The rate token was spent when the proposal was minted, so a second
    // fresh order is refused by the cap the old bypass skipped entirely.
    assert!(matches!(
        f.evaluate(&small, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::OrderRate { .. })
    ));
}

/// Item 28: proposals carry a TTL and auto-expire.
#[test]
fn a_proposal_expires_and_cannot_be_approved_afterwards() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &btc,
        &market,
        &exposure(d("100000")),
    ) else {
        panic!("expected a proposal");
    };

    let expiry = NOW_MS + APPROVAL_TTL_MS;
    assert_eq!(f.engine.pending_proposals(expiry - 1).len(), 1);
    assert!(f.engine.pending_proposals(expiry).is_empty());
    let mut late_market = market.clone();
    late_market.as_of_ms = expiry;
    let mut late = exposure(d("100000"));
    late.agent.as_of_ms = expiry;
    assert!(matches!(
        f.engine
            .operator_approve_proposal(&approval_id, &btc, &late_market, &late, expiry),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
}

/// An approval is not a waiver. Every hard predicate runs again against the
/// state at approval time, which is what item 28's "re-priced at approval
/// time" means for the guardrails.
#[test]
fn an_approved_proposal_is_still_refused_if_the_world_moved() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.max_position_usd = d("200");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &btc,
        &market,
        &exposure(d("100000")),
    ) else {
        panic!("expected a proposal");
    };

    // By the time the operator looks, the agent already holds the cap.
    let mut loaded = exposure(d("100000"));
    loaded
        .agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("2") });
    loaded.agent.total_position_notional_usd = d("200");
    assert!(matches!(
        f.engine
            .operator_approve_proposal(&approval_id, &btc, &market, &loaded, NOW_MS),
        Err(Refusal::PositionNotional { .. })
    ));
}

#[test]
fn a_rejected_proposal_is_gone() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &btc,
        &market,
        &exposure(d("100000")),
    ) else {
        panic!("expected a proposal");
    };
    assert!(f.engine.operator_reject_proposal(&approval_id, NOW_MS));
    assert!(!f.engine.operator_reject_proposal(&approval_id, NOW_MS));
    assert!(f.engine.pending_proposals(NOW_MS).is_empty());
}

// ---- what gets signed is what was checked -------------------------------

#[test]
fn the_cleared_action_carries_the_rounded_values_that_were_checked() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 5, 40);
    let market = MarketRef::fresh("BTC", d("41505.123"), NOW_MS);
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("41505.123"), d("0.0012345678")),
            &btc,
            &market,
            &exposure(d("1000000")),
        )
        .expect("clears");
    let wire = wire_of(&cleared);
    assert_eq!(wire.a, btc.index);
    assert_eq!(wire.p.as_str(), "41505");
    assert_eq!(wire.s.as_str(), "0.00123");
    match &cleared.clearance().kind {
        ClearedKind::Order {
            px,
            sz,
            notional_usd,
            ..
        } => {
            assert_eq!(*px, d("41505"));
            assert_eq!(*sz, d("0.00123"));
            assert_eq!(*notional_usd, d("41505") * d("0.00123"));
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }
}

/// Item 19 makes query-by-cloid the only safe move after a
/// `timeout_unknown_outcome` and item 9 reconciles by it, so the row that
/// says why an order was allowed has to carry the same cloid the wire order
/// does — otherwise the decision cannot be joined to the fill. R6 wants the
/// same of the decision-time book snapshot.
#[test]
fn the_clearance_carries_the_cloid_and_the_snapshot_it_was_decided_against() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let mut market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    market.snapshot = Some(MarketSnapshotRef {
        id: "snap-1".to_owned(),
        hash: "0xfeed".to_owned(),
    });
    let mut order = intent("BTC", true, d("100"), d("1"));
    order.cloid = Some(cloid());

    let cleared = f
        .evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect("clears");
    let wire = wire_of(&cleared);
    match &cleared.clearance().kind {
        ClearedKind::Order {
            cloid: recorded,
            snapshot_id,
            snapshot_hash,
            ..
        } => {
            assert_eq!(recorded.as_ref(), Some(&cloid()));
            assert_eq!(
                recorded.as_ref().map(|c| c.as_str()),
                wire.c.as_ref().map(|c| c.as_str()),
                "the ledger row and the wire order must name the same cloid"
            );
            assert_eq!(snapshot_id.as_deref(), Some("snap-1"));
            assert_eq!(snapshot_hash.as_deref(), Some("0xfeed"));
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }

    // Both are nullable today, and absence is absence rather than a
    // placeholder that could be mistaken for a real id.
    let bare = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    match &bare.clearance().kind {
        ClearedKind::Order {
            cloid,
            snapshot_id,
            snapshot_hash,
            ..
        } => {
            assert!(cloid.is_none());
            assert!(snapshot_id.is_none());
            assert!(snapshot_hash.is_none());
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }
}

#[test]
fn a_venue_rule_breach_is_a_typed_subtype_not_a_string() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    // $9 is under the venue's $10 minimum.
    match f
        .evaluate(
            &intent("BTC", true, d("9"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("the venue would reject this")
    {
        Refusal::VenueRule(VenueRule::MinNotional {
            notional_usd,
            minimum_usd,
        }) => {
            assert_eq!(notional_usd, d("9"));
            assert_eq!(minimum_usd, d("10"));
        }
        other => panic!("expected the min-notional subtype, got {other}"),
    }
}

/// R4 calls a mainnet number that is actually a testnet number the worst bug
/// this product can ship, and D1 makes each agent's sub-account the unit of
/// capital segregation. Neither may be a signing parameter: the clearance
/// carries what it was evaluated against, and `sign_cleared` has no argument
/// by which a caller could change either.
#[test]
fn a_clearance_is_signed_for_the_network_and_sub_account_it_was_evaluated_for() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    assert_eq!(cleared.clearance().network, Network::Testnet);
    assert_eq!(cleared.clearance().vault_address, Some(vault()));

    let key = oppen_hl::AgentKey::from_hex(
        "0123456789012345678901234567890123456789012345678901234567890123",
    )
    .expect("key");
    let (request, _) = f
        .engine
        .sign_cleared(&key, cleared, 1, None, NOW_MS)
        .expect("signs");
    assert_eq!(request.vault_address(), Some(vault()));

    // The same order under a mainnet engine signs for mainnet — the network
    // travels with the engine, not with the call.
    let mainnet = GuardrailEngine::new(
        Arc::new(MemoryStore::new()),
        Arc::new(NullAuditSink),
        Network::Mainnet,
    )
    .expect("engine");
    let other = AgentId::new("beta");
    let other_vault =
        oppen_hl::Address::parse("0x000000000000000000000000000000000000beef").expect("address");
    mainnet
        .register_agent(&other, Some(other_vault), NOW_MS)
        .expect("register");
    mainnet
        .operator_set_guardrails(&other, permissive(&["BTC"]), NOW_MS)
        .expect("set guardrails");
    let cleared = mainnet
        .evaluate(
            &other,
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
        .expect("clears");
    assert_eq!(cleared.clearance().network, Network::Mainnet);
    assert_eq!(cleared.clearance().vault_address, Some(other_vault));
}

/// The binding has to survive a restart, or the second launch signs a
/// sub-account's orders against the master account.
#[test]
fn the_sub_account_binding_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("testnet.db");
    let agent = AgentId::new("alpha");
    {
        let store: Arc<dyn GuardrailStore> =
            Arc::new(SqliteGuardrailStore::open(&path).expect("open"));
        let engine =
            GuardrailEngine::new(store, Arc::new(NullAuditSink), Network::Testnet).expect("engine");
        engine
            .register_agent(&agent, Some(vault()), NOW_MS)
            .expect("register");
        engine
            .operator_set_guardrails(&agent, permissive(&["BTC"]), NOW_MS)
            .expect("set guardrails");
    }
    let store: Arc<dyn GuardrailStore> =
        Arc::new(SqliteGuardrailStore::open(&path).expect("reopen"));
    let engine =
        GuardrailEngine::new(store, Arc::new(NullAuditSink), Network::Testnet).expect("engine");
    assert_eq!(engine.vault_address(&agent), Some(vault()));
    let cleared = engine
        .evaluate(
            &agent,
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
        .expect("clears");
    assert_eq!(cleared.clearance().vault_address, Some(vault()));
}

/// Internally-tagged enums with newtype variants fail at *serialization*
/// time, not compile time, if the inner value is not a map. `Refusal`
/// wraps `VenueRule` and `Unevaluable` that way, so the taxonomy is
/// serialized here to prove the MCP layer can actually render it, with the
/// tag first and the fields in declaration order (`AGENTS.md` invariants 6
/// and 8).
#[test]
fn every_refusal_shape_serializes_deterministically() {
    let cases = vec![
        Refusal::MissingReason,
        Refusal::SymbolNotAllowed {
            symbol: "BTC".to_owned(),
            allowed: vec![],
        },
        Refusal::OrderNotional {
            symbol: "BTC".to_owned(),
            observed_usd: d("25.01"),
            limit_usd: d("25"),
        },
        Refusal::OrderRate {
            limit: 5,
            window_ms: 300_000,
            tokens_available: d("0.5"),
            retry_after_ms: 60_000,
        },
        Refusal::TradingPaused {
            scope: KillScope::agent("alpha"),
            since_ms: 1,
            reason: KillReason::LossLimit {
                kind: LossKind::Daily,
                observed_usd: d("25"),
                limit_usd: d("25"),
            },
        },
        Refusal::VenueRule(VenueRule::MinNotional {
            notional_usd: d("9"),
            minimum_usd: d("10"),
        }),
        Refusal::Unevaluable(Unevaluable::StaleMarketData {
            symbol: "BTC".to_owned(),
            age_ms: 3_000,
            max_age_ms: 2_000,
        }),
        Refusal::PositionNotional {
            symbol: "BTC".to_owned(),
            observed_usd: d("500"),
            resting_usd: d("400"),
            limit_usd: d("100"),
        },
        Refusal::GlobalRateBudget {
            tokens_available: d("100"),
            reserve: 100,
            retry_after_ms: 10_000,
        },
        Refusal::ApprovalRequired {
            symbol: "BTC".to_owned(),
            notional_usd: d("25"),
            approval_id: "alpha-1-1".to_owned(),
            expires_at_ms: 120_000,
        },
        Refusal::ReasonTooLong {
            len_bytes: 4_096,
            max_bytes: MAX_REASON_BYTES,
        },
        Refusal::ReasonControlCharacter {
            at_byte: 0,
            codepoint: 0x1b,
        },
        Refusal::Unevaluable(Unevaluable::MissingRestingOrders),
        Refusal::Unevaluable(Unevaluable::UnknownProposal {
            approval_id: "alpha-1-1".to_owned(),
        }),
    ];
    for refusal in &cases {
        let json = serde_json::to_string(refusal)
            .unwrap_or_else(|e| panic!("{refusal:?} does not serialize: {e}"));
        assert!(json.starts_with(r#"{"refusal":"#), "{json}");
        // Same value, same bytes, every time.
        assert_eq!(json, serde_json::to_string(refusal).expect("re-serialize"));
        // And the message a human reads is never empty.
        assert!(!refusal.to_string().is_empty());
    }
    assert_eq!(
        serde_json::to_string(&cases[2]).expect("serialize"),
        r#"{"refusal":"order_notional","symbol":"BTC","observed_usd":"25.01","limit_usd":"25"}"#
    );
}

#[test]
fn a_clearance_serializes_for_the_ledger() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    let json = serde_json::to_string(cleared.clearance()).expect("serializes");
    assert!(json.contains(r#""agent":"alpha""#), "{json}");
    assert!(json.contains(r#""cleared":"order""#), "{json}");
}

// ---- the property: no bypass --------------------------------------------

/// xorshift64*, so the case stream is identical on every machine and a
/// failure can be replayed from the seed alone.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        let i = self.below(xs.len() as u64) as usize;
        &xs[i]
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

/// The whole invariant, stated as a property.
///
/// For twenty thousand pseudo-random combinations of configuration, market
/// state, account state, clock and intent: if the engine produced a
/// [`Cleared`], then every predicate holds when re-derived here from the
/// action that would actually be signed. Nothing in this function calls back
/// into the engine's own arithmetic, so an error in the engine cannot hide by
/// being made twice.
///
/// The case stream is checked for balance at the end. A property test that
/// only ever produces refusals proves nothing, and one that only ever
/// produces clearances is not testing the guardrails.
#[test]
fn no_input_produces_a_signable_value_without_passing_every_predicate() {
    const CASES: usize = 20_000;
    let symbols = ["BTC", "ETH", "SOL"];
    let sizes = ["0.25", "0.5", "1", "2", "5"];
    // Order prices are derived from the market reference, so the generated
    // slippage spans "passive" through "wildly aggressive" instead of being
    // dominated by unrelated price levels.
    let price_factors = ["0.5", "0.99", "1", "1.001", "1.002", "1.02", "1.5"];
    let caps = ["25", "100", "1000", "100000", "1000000"];
    let leverages = [2u32, 5, 40, 50];

    let mut rng = Rng(0x0DDB_1A5E_5BAD_5EED);
    let mut cleared_count = 0usize;
    let mut refused_count = 0usize;
    let mut unevaluable_count = 0usize;

    for case in 0..CASES {
        let symbol = *rng.pick(&symbols);
        let allowed: BTreeSet<String> = symbols
            .iter()
            .filter(|_| !rng.chance(4))
            .map(|s| (*s).to_owned())
            .collect();
        let config = AgentGuardrails {
            symbols: allowed,
            max_order_usd: if rng.chance(20) {
                Decimal::ZERO
            } else {
                d(rng.pick(&caps))
            },
            max_position_usd: d(rng.pick(&caps)),
            max_slippage_bps: d(rng.pick(&["0", "5", "10", "20", "200", "10000", "10000"])),
            order_rate: OrderRate {
                count: 1 + rng.below(4) as u32,
                per_ms: 60_000,
            },
            reduce_only: rng.chance(6),
            risk: RiskSettings {
                max_leverage: *rng.pick(&leverages),
                margin_mode: MarginMode::Cross,
            },
            loss: LossLimits {
                max_daily_loss_usd: rng.chance(2).then(|| d(rng.pick(&["25", "100", "1000"]))),
                max_drawdown_usd: rng.chance(3).then(|| d(rng.pick(&["200", "2000"]))),
            },
            approval_required: rng.chance(6),
            freshness: Freshness::default(),
            max_mark_divergence_bps: d("5"),
            mark_divergence_window_ms: 30_000,
        };

        let f = Fixture::new(config.clone());
        let sz_decimals = rng.below(4) as u32;
        let asset = asset(symbol, sz_decimals, *rng.pick(&leverages));

        let now_ms = NOW_MS + rng.below(120_000);
        let base_px = d(rng.pick(&["50", "100", "150"]));
        let market = MarketRef {
            symbol: symbol.to_owned(),
            reference_px: if rng.chance(16) { None } else { Some(base_px) },
            as_of_ms: now_ms.saturating_sub(rng.below(2_200)),
            quality: *rng.pick(&[
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Warmup,
                FeedQuality::Degraded,
                FeedQuality::Unusable,
            ]),
            mark_divergence_bps: rng.chance(4).then(|| d(rng.pick(&["1", "6", "40"]))),
            mark_divergent_since_ms: rng
                .chance(2)
                .then(|| now_ms.saturating_sub(rng.below(60_000))),
            snapshot: None,
        };

        let mut positions = BTreeMap::new();
        if rng.chance(2) {
            positions.insert(
                symbol.to_owned(),
                PositionSnapshot {
                    szi: d(rng.pick(&["-2", "-0.5", "0.5", "2"])),
                },
            );
        }
        let equity = if rng.chance(30) {
            Decimal::ZERO
        } else {
            d(rng.pick(&["100", "1000", "100000"]))
        };
        let agent_account = AccountSnapshot {
            as_of_ms: now_ms.saturating_sub(rng.below(5_400)),
            reconciled: !rng.chance(16),
            equity_usd: equity,
            peak_equity_usd: equity + d(rng.pick(&["0", "0", "100", "1000"])),
            realized_pnl_today_usd: d(rng.pick(&["-100", "-25", "0", "0", "0", "50"])),
            unrealized_pnl_usd: d(rng.pick(&["-50", "0", "0", "0", "10"])),
            day_start_ms: if rng.chance(16) {
                MIDNIGHT_MS - 86_400_000
            } else {
                MIDNIGHT_MS
            },
            total_position_notional_usd: d(rng.pick(&["0", "50", "500"])),
            positions,
            resting: if rng.chance(16) {
                None
            } else {
                let mut book = RestingExposure::none();
                if rng.chance(2) {
                    let szi = d(rng.pick(&["-2", "-0.5", "0.5", "2"]));
                    book.notional_usd = szi.abs() * base_px;
                    book.szi.insert(symbol.to_owned(), szi);
                }
                Some(book)
            },
        };
        let exposure = Exposure {
            agent: agent_account,
            fleet: None,
        };

        let mut order = intent(
            symbol,
            rng.chance(2),
            base_px * d(rng.pick(&price_factors)),
            d(rng.pick(&sizes)),
        );
        order.reduce_only = rng.chance(3);
        if rng.chance(5) {
            order.kind = OrderKind::Trigger {
                is_market: true,
                trigger_px: base_px * d(rng.pick(&price_factors)),
                tpsl: Tpsl::Sl,
            };
        }
        if rng.chance(25) {
            order.reason = String::new();
        }

        match f
            .engine
            .evaluate(&f.agent, &order, &asset, &market, &exposure, now_ms)
        {
            Err(refusal) => {
                refused_count += 1;
                if refusal.is_unevaluable() {
                    unevaluable_count += 1;
                }
            }
            Ok(cleared) => {
                cleared_count += 1;
                verify_every_predicate(
                    case, &cleared, &config, &order, &asset, &market, &exposure, now_ms,
                );
            }
        }
    }

    assert!(
        cleared_count > 1_000,
        "only {cleared_count} of {CASES} cases cleared; the property would be vacuous"
    );
    assert!(
        refused_count > 500,
        "only {refused_count} of {CASES} cases were refused; the generator is too permissive"
    );
    assert!(
        unevaluable_count > 100,
        "only {unevaluable_count} fail-closed refusals; the generator is not exercising them"
    );
}

/// Re-derives every guardrail from the action that would be signed and the
/// inputs that were supplied, independently of the engine.
#[allow(clippy::too_many_arguments)]
fn verify_every_predicate(
    case: usize,
    cleared: &Cleared,
    config: &AgentGuardrails,
    order: &OrderIntent,
    asset: &Asset,
    market: &MarketRef,
    exposure: &Exposure,
    now_ms: u64,
) {
    let ctx = format!("case {case}");
    let wire = wire_of(cleared);
    let px = wire_px(&wire);
    let sz = wire_sz(&wire);
    let account = &exposure.agent;

    // The action is the intent, rounded — not something else entirely.
    assert_eq!(wire.a, asset.index, "{ctx}: wrong asset id");
    assert_eq!(wire.b, order.is_buy, "{ctx}: wrong side");
    assert_eq!(px, asset.round_price(order.px), "{ctx}: price drifted");
    assert_eq!(sz, asset.round_size(order.sz), "{ctx}: size drifted");
    assert!(
        asset.validate_order(px, sz).is_ok(),
        "{ctx}: the venue would reject this"
    );

    // Reason, agent, approval.
    assert!(!order.reason.trim().is_empty(), "{ctx}: no reason");
    assert!(
        !config.approval_required,
        "{ctx}: approval mode bypassed — `evaluate` never clears under it"
    );

    // Inputs the engine must have established before measuring anything.
    assert!(market.quality.is_ok(), "{ctx}: cleared off a degraded feed");
    assert!(now_ms >= market.as_of_ms, "{ctx}: market from the future");
    assert!(
        now_ms - market.as_of_ms <= config.freshness.max_market_age_ms,
        "{ctx}: stale market"
    );
    assert!(account.reconciled, "{ctx}: unreconciled account");
    assert!(now_ms >= account.as_of_ms, "{ctx}: account from the future");
    assert!(
        now_ms - account.as_of_ms <= config.freshness.max_account_age_ms,
        "{ctx}: stale account"
    );
    assert!(account.equity_usd > Decimal::ZERO, "{ctx}: no equity");
    assert!(account.covers_day_of(now_ms), "{ctx}: wrong PnL window");
    let reference_px = market.reference_px.expect("a cleared order has a price");
    assert!(
        reference_px > Decimal::ZERO,
        "{ctx}: non-positive reference"
    );
    if let (Some(bps), Some(since)) = (market.mark_divergence_bps, market.mark_divergent_since_ms)
        && bps > config.max_mark_divergence_bps
    {
        assert!(
            now_ms < since || now_ms - since < config.mark_divergence_window_ms,
            "{ctx}: cleared through a sustained mark divergence"
        );
    }

    // The loss budgets.
    let loss = -account.day_pnl_usd();
    if let Some(limit) = config.loss.max_daily_loss_usd {
        assert!(loss < limit, "{ctx}: cleared past the daily budget");
    }
    if let Some(limit) = config.loss.max_drawdown_usd {
        let drawdown = (account.peak_equity_usd - account.equity_usd).max(Decimal::ZERO);
        assert!(drawdown < limit, "{ctx}: cleared past the drawdown budget");
    }

    // Item 24, one predicate at a time.
    assert!(
        config.symbols.contains(&order.symbol),
        "{ctx}: symbol off the allowlist"
    );
    assert!(
        px * sz <= config.max_order_usd,
        "{ctx}: order notional past the cap"
    );
    let position_szi = account.position_szi(&order.symbol);
    let resting = account
        .resting
        .as_ref()
        .expect("a cleared order has a resting book");
    let resting_szi = resting.szi_of(&order.symbol);
    let signed_sz = if order.is_buy { sz } else { -sz };
    let after = position_szi + resting_szi + signed_sz;
    assert!(
        after.abs() * reference_px <= config.max_position_usd,
        "{ctx}: position notional past the cap"
    );
    let symbol_before = position_szi.abs() * reference_px + resting_szi.abs() * reference_px;
    let total_after = (account.total_position_notional_usd + resting.notional_usd - symbol_before
        + after.abs() * reference_px)
        .max(Decimal::ZERO);
    assert!(
        total_after / account.equity_usd
            <= Decimal::from(config.risk.max_leverage.min(asset.info.max_leverage)),
        "{ctx}: leverage past the cap"
    );
    if config.reduce_only {
        assert!(order.reduce_only, "{ctx}: reduce-only flag missing");
        assert!(!position_szi.is_zero(), "{ctx}: nothing to reduce");
        assert_ne!(
            position_szi.is_sign_negative(),
            signed_sz.is_sign_negative(),
            "{ctx}: reduce-only order on the same side"
        );
        assert!(
            sz <= position_szi.abs(),
            "{ctx}: reduce-only order oversized"
        );
    }
    let slippage_reference = match &wire.t {
        oppen_hl::wire::OrderType::Trigger { trigger_px, .. } => {
            Decimal::from_str(trigger_px.as_str()).expect("trigger price parses")
        }
        oppen_hl::wire::OrderType::Limit { .. } => reference_px,
    };
    let adverse = if order.is_buy {
        px - slippage_reference
    } else {
        slippage_reference - px
    };
    let slippage_bps = if adverse <= Decimal::ZERO {
        Decimal::ZERO
    } else {
        adverse / slippage_reference * Decimal::from(10_000)
    };
    let slippage_limit = match order.max_slippage_bps {
        Some(agent_limit) => config.max_slippage_bps.min(agent_limit),
        None => config.max_slippage_bps,
    };
    assert!(
        slippage_bps <= slippage_limit,
        "{ctx}: slippage {slippage_bps} past the {slippage_limit} bps cap"
    );

    // The clearance describes what was checked.
    match &cleared.clearance().kind {
        ClearedKind::Order {
            symbol,
            px: cpx,
            sz: csz,
            notional_usd,
            ..
        } => {
            assert_eq!(symbol, &order.symbol, "{ctx}");
            assert_eq!(*cpx, px, "{ctx}");
            assert_eq!(*csz, sz, "{ctx}");
            assert_eq!(*notional_usd, px * sz, "{ctx}");
        }
        other => panic!("{ctx}: expected an order clearance, got {other:?}"),
    }
    assert_eq!(
        cleared.clearance().agent.as_ref(),
        Some(&AgentId::new("alpha")),
        "{ctx}"
    );
}

/// The second property, and the one the first is structurally blind to.
///
/// [`no_input_produces_a_signable_value_without_passing_every_predicate`]
/// builds a fresh [`Fixture`] inside its loop, so every case starts with a
/// full token bucket and a clear kill switch: the order-rate cap and item
/// 26's pause are never exercised *across* cases, which is exactly where the
/// bucket over-refill and the approval bypass lived. This run hoists one
/// engine out of the loop and drives a sequence of intents against it on a
/// monotonically advancing clock.
///
/// The bucket is re-derived here from the carry arithmetic directly, not by
/// asking the engine, so an error in the token bucket cannot hide by being
/// made twice. Item 25's breaker and item 26's pause are re-derived the same
/// way: once anything has stopped this agent, nothing may clear again.
#[test]
fn a_sequence_against_one_engine_never_outruns_the_rate_cap_or_the_pause() {
    const STEPS: usize = 20_000;
    /// 3 orders per 30s. Small enough that the cap binds constantly, and
    /// `capacity_micro / per_ms` does not divide evenly — 3,000,000 / 30,000
    /// is exact, so use a window that is not a multiple: 29,999 ms.
    const RATE: OrderRate = OrderRate {
        count: 3,
        per_ms: 29_999,
    };
    const CAPACITY_MICRO: u128 = 3_000_000;

    let mut config = permissive(&["BTC"]);
    config.order_rate = RATE;
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("100")),
        max_drawdown_usd: None,
    };
    let f = Fixture::new(config.clone());
    // Item 10's budget must not be what refuses anything here.
    f.engine
        .operator_set_global_rate_budget(
            GlobalRateBudget {
                rate: OrderRate {
                    count: 1_000_000,
                    per_ms: 1,
                },
                reserve: 0,
            },
            NOW_MS,
        )
        .expect("set budget");

    let btc = asset("BTC", 2, 40);
    let mut rng = Rng(0x5EED_5EED_0BAD_F00D);

    // An independent model of the bucket: exact numerator carry, no
    // borrowing of the engine's arithmetic.
    let mut tokens_micro: u128 = CAPACITY_MICRO;
    let mut carry: u128 = 0;
    let mut last_ms = NOW_MS;
    let mut stopped_at_step: Option<usize> = None;
    let mut trips = 0usize;
    let mut releases = 0usize;

    let mut now_ms = NOW_MS;
    let mut cleared_count = 0usize;
    let mut rate_refusals = 0usize;
    let mut paused_refusals = 0usize;

    for step in 0..STEPS {
        now_ms += rng.below(3_000);
        // The operator looks at the trip after a while and releases it, so
        // the sequence keeps exercising the rate cap afterwards.
        if let Some(at) = stopped_at_step
            && step - at > 20
        {
            assert!(
                f.engine
                    .operator_release_kill(&KillScope::agent("alpha"), now_ms)
                    .expect("release"),
                "step {step}: the switch should still have been engaged"
            );
            stopped_at_step = None;
            releases += 1;
        }
        // Stay inside the UTC day the snapshot's `day_start_ms` names.
        if now_ms >= MIDNIGHT_MS + 86_400_000 {
            break;
        }
        let losing = stopped_at_step.is_none() && rng.chance(400);
        let mut exposure = exposure(d("100000"));
        exposure.agent.as_of_ms = now_ms;
        if losing {
            exposure.agent.realized_pnl_today_usd = d("-100");
        }
        let market = MarketRef::fresh("BTC", d("100"), now_ms);
        let order = intent("BTC", true, d("100"), d("1"));

        let outcome = f
            .engine
            .evaluate(&f.agent, &order, &btc, &market, &exposure, now_ms);

        // Advance the model's bucket to `now_ms` before judging the verdict.
        let numerator = u128::from(now_ms - last_ms) * CAPACITY_MICRO + carry;
        let per = u128::from(RATE.per_ms);
        let gain = numerator / per;
        carry = numerator % per;
        last_ms = now_ms;
        tokens_micro += gain;
        if tokens_micro >= CAPACITY_MICRO {
            tokens_micro = CAPACITY_MICRO;
            carry = 0;
        }

        match outcome {
            Ok(cleared) => {
                assert!(
                    stopped_at_step.is_none(),
                    "step {step}: cleared after the agent was stopped at {now_ms}ms"
                );
                assert!(
                    tokens_micro >= 1_000_000,
                    "step {step}: cleared with {tokens_micro} micro-tokens at {now_ms}ms — \
                     the rate cap was outrun"
                );
                tokens_micro -= 1_000_000;
                assert_eq!(
                    cleared.clearance().utilization.order_tokens_remaining,
                    Decimal::from(tokens_micro) / Decimal::from(1_000_000u64),
                    "step {step}: the engine and the model disagree about the budget"
                );
                cleared_count += 1;
            }
            Err(Refusal::OrderRate { .. }) => {
                assert!(
                    tokens_micro < 1_000_000,
                    "step {step}: refused with {tokens_micro} micro-tokens — a token was lost"
                );
                rate_refusals += 1;
            }
            Err(Refusal::LossLimit { .. }) => {
                assert!(losing, "step {step}: the breaker tripped on a healthy day");
                // Item 25: the trip engages the switch, and item 26 says
                // whose orders to cancel.
                let effects = f.engine.take_pending_kill_effects();
                assert_eq!(effects.len(), 1, "step {step}: no cancels queued");
                assert!(effects[0].cancel_for.contains(&f.agent));
                stopped_at_step = Some(step);
                trips += 1;
                // The refusal costs no token, so the model does not spend one.
            }
            Err(Refusal::TradingPaused { .. }) => {
                assert!(
                    stopped_at_step.is_some(),
                    "step {step}: paused without ever being stopped"
                );
                paused_refusals += 1;
            }
            Err(other) => panic!("step {step}: unexpected refusal {other}"),
        }
    }

    assert!(
        cleared_count > 100,
        "only {cleared_count} clearances; the sequence is not exercising the engine"
    );
    assert!(
        rate_refusals > 100,
        "only {rate_refusals} rate refusals; the cap is never binding"
    );
    assert!(
        paused_refusals > 10,
        "only {paused_refusals} paused refusals; item 26 is never exercised across cases"
    );
    assert!(
        trips > 5 && releases > 5,
        "{trips} trips and {releases} releases; item 25 is barely exercised"
    );
}

// ---- the P3 gate: no signer path without a guardrail check ---------------

/// A throwaway agent key. The signature is never posted anywhere: these
/// tests care about *whether* one was produced, not what it says. Signing
/// correctness is pinned by the official SDK vectors in `oppen-hl`.
fn agent_key() -> oppen_hl::AgentKey {
    oppen_hl::AgentKey::from_hex("0123456789012345678901234567890123456789012345678901234567890123")
        .expect("test key")
}

fn account_at(equity: Decimal, now_ms: u64) -> AccountSnapshot {
    AccountSnapshot {
        as_of_ms: now_ms,
        ..account(equity)
    }
}

/// Spec item 7 puts a submit queue between the decision and the wire, so
/// "the switch was clear when we evaluated" is not the same statement as
/// "the switch is clear now". The engine is the [`PreSignCheck`], so it
/// re-reads the switch inside the signer and this order dies there.
#[test]
fn a_kill_switch_engaged_after_the_clearance_still_stops_the_signature() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears while the switch is open");

    f.engine
        .operator_engage_kill(
            KillScope::agent(f.agent.clone()),
            KillReason::Operator,
            NOW_MS + 1,
        )
        .expect("engage");

    match f
        .engine
        .sign_cleared(&agent_key(), cleared, 1, None, NOW_MS + 2)
    {
        Err(SignClearedError::Refused(Refusal::TradingPaused {
            scope,
            since_ms,
            reason,
        })) => {
            assert_eq!(scope, KillScope::agent(f.agent.clone()));
            assert_eq!(since_ms, NOW_MS + 1);
            assert_eq!(reason, KillReason::Operator);
        }
        other => panic!("a paused agent must not get a signature, got {other:?}"),
    }
}

/// D6 makes the chain the single record of why an order happened. A refusal
/// at the signer arrives *after* `evaluate` already wrote the clearance row,
/// so without its own row the export would show an order cleared and never
/// say why no fill followed — and item 18 names guardrail trips as events.
#[test]
fn a_refusal_at_the_signer_reaches_the_ledger() {
    let sink = Arc::new(CountingSink::default());
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(MemoryStore::new()),
        sink.clone() as Arc<dyn AuditSink>,
    );
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    assert_eq!(sink.cleared.load(Ordering::Relaxed), 1);
    assert_eq!(sink.refused.load(Ordering::Relaxed), 0);

    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS + 1)
        .expect("engage");
    let err = f
        .engine
        .sign_cleared(&agent_key(), cleared, 1, None, NOW_MS + 2)
        .expect_err("the signer refuses");
    assert!(matches!(
        err,
        SignClearedError::Refused(Refusal::TradingPaused { .. })
    ));

    // The clearance row still stands — it was true when it was written — and
    // the refusal that overtook it is now beside it.
    assert_eq!(sink.cleared.load(Ordering::Relaxed), 1);
    assert_eq!(sink.refused.load(Ordering::Relaxed), 1);
}

/// A ledger that cannot be written must not turn a refusal into a signature.
/// Same reading as the refusal branch of `record`: losing the row is bad, the
/// safe outcome already happened.
#[test]
fn a_failed_ledger_write_never_turns_a_signer_refusal_into_a_signature() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(MemoryStore::new()),
        Arc::new(NullAuditSink),
    );
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");

    // A second engine over the same store, whose sink fails every write, and
    // which has never paired this sub-account.
    let broken = GuardrailEngine::new(
        Arc::new(MemoryStore::new()),
        Arc::new(FailingSink),
        Network::Testnet,
    )
    .expect("engine");
    let err = broken
        .sign_cleared(&agent_key(), cleared, 1, None, NOW_MS)
        .expect_err("still refuses with the ledger down");
    assert!(matches!(
        err,
        SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::UnknownSubAccount { .. }))
    ));
}

/// The exemption, and only the exemption. Item 26 makes cancelling resting
/// orders part of what engaging the switch *does*, so a cancel that clears
/// while paused must also *sign* while paused — otherwise the switch stops
/// the cure along with the disease.
#[test]
fn a_cancel_still_signs_while_the_kill_switch_is_engaged() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        .expect("engage");

    let cleared = f
        .engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 991 }],
            "stopping",
            NOW_MS,
        )
        .expect("a cancel clears while paused");
    let (request, _) = f
        .engine
        .sign_cleared(&agent_key(), cleared, 1, None, NOW_MS)
        .expect("and it signs while paused");
    assert!(matches!(request.action(), Action::Cancel { .. }));

    // The dead-man's switch is the other risk-reducing action, and it is
    // operator-scoped so it carries no sub-account at all.
    let cleared = f
        .engine
        .clear_schedule_cancel(Some(NOW_MS + 60_000), NOW_MS)
        .expect("the dead-man's switch clears while paused");
    let (request, _) = f
        .engine
        .sign_cleared(&agent_key(), cleared, 2, None, NOW_MS)
        .expect("and it signs");
    assert!(matches!(request.action(), Action::ScheduleCancel { .. }));
    assert_eq!(request.vault_address(), None);
}

/// R4: one engine, one network, one database file. A clearance is a verdict
/// about testnet limits measured against testnet positions; signing it for
/// mainnet is the bug R4 calls the worst this product can ship. The gate is
/// the engine itself, so the wrong engine cannot sign it even though the
/// value type is the same.
#[test]
fn a_clearance_cannot_be_signed_through_another_networks_engine() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears on testnet");

    let mainnet = GuardrailEngine::new(
        Arc::new(MemoryStore::new()),
        Arc::new(NullAuditSink),
        Network::Mainnet,
    )
    .expect("engine");
    // Same agent, same sub-account: only the network differs.
    mainnet
        .register_agent(&AgentId::new("alpha"), Some(vault()), NOW_MS)
        .expect("register");

    match mainnet.sign_cleared(&agent_key(), cleared, 1, None, NOW_MS) {
        Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::WrongNetwork {
            expected,
            supplied,
        }))) => {
            assert_eq!(expected, Network::Mainnet);
            assert_eq!(supplied, Network::Testnet);
        }
        other => panic!("a testnet clearance must not sign for mainnet, got {other:?}"),
    }
}

/// D1 maps the agent roster 1:1 onto sub-accounts. An engine that has never
/// paired this address has never measured a limit against that capital, so
/// it refuses rather than signing on someone else's behalf.
#[test]
fn a_clearance_cannot_be_signed_through_an_engine_that_does_not_know_its_sub_account() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");

    let stranger = GuardrailEngine::new(
        Arc::new(MemoryStore::new()),
        Arc::new(NullAuditSink),
        Network::Testnet,
    )
    .expect("engine");

    match stranger.sign_cleared(&agent_key(), cleared, 1, None, NOW_MS) {
        Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::UnknownSubAccount {
            vault_address,
        }))) => assert_eq!(vault_address, vault()),
        other => panic!("an unknown sub-account must not be signed for, got {other:?}"),
    }
}

/// **The P3 gate stated as a property.**
///
/// Thousands of generated intents driven at one engine with an advancing
/// clock, with the kill switch pressed and released *between* each
/// evaluation and its signature so the two do not always see the same world.
/// Three things are asserted on every case:
///
/// 1. A refusal produces no [`Cleared`], so `sign_cleared` cannot be called.
///    That half is enforced by the compiler, not by this loop — the `Err`
///    arm below has no signing call in it because none can be written.
/// 2. A signature is produced only when the gate passed, and a pause between
///    the evaluation and the signature refuses at the signer.
/// 3. Signatures never outnumber the clearances the ledger recorded.
///
/// **What this proves.** No input in the generated space reaches the signer
/// through `oppen-core` without exactly one evaluation, and a signature
/// always has a ledger row behind it.
///
/// **What it cannot prove.** That nothing else signs. `sign_unchecked` is
/// `pub` for spec item 33's manual path and a foreign crate can implement a
/// no-op [`PreSignCheck`]; see
/// [`no_call_site_in_oppen_core_reaches_the_signer_unchecked`] for the
/// grep-shaped half of the argument, and the module doc for the honest
/// statement of the residual.
#[test]
fn no_generated_intent_reaches_the_signer_without_an_evaluation() {
    const CASES: u64 = 3_000;
    let sink = Arc::new(CountingSink::default());
    let mut config = permissive(&["BTC", "ETH"]);
    config.max_order_usd = d("5000");
    config.max_position_usd = d("50000");
    config.max_slippage_bps = d("500");
    // Loose enough that most cases clear, tight enough that the cap still
    // bites: one token per 750 ms against a case every 900 ms.
    config.order_rate = OrderRate {
        count: 4,
        per_ms: 3_000,
    };
    let f = Fixture::with(
        config,
        Arc::new(MemoryStore::new()),
        sink.clone() as Arc<dyn AuditSink>,
    );
    let key = agent_key();

    let mut rng = Rng(0x0000_0000_5161_1ED0);
    let mut signed = 0u64;
    let mut refused_at_signer = 0u64;
    let mut refused_before_signer = 0u64;
    let mut nonce = 1u64;

    for case in 0..CASES {
        // 45 minutes in total, so `MIDNIGHT_MS` stays the correct UTC day
        // start and the loss window never mismatches.
        let now_ms = NOW_MS + case * 900;
        if rng.chance(3) {
            f.engine
                .operator_release_kill(&KillScope::Global, now_ms)
                .expect("release");
        }

        // "DOGE" is off the allowlist and the wide sizes and prices push
        // past the notional and slippage caps, so both branches are fed.
        let symbol = *rng.pick(&["BTC", "BTC", "ETH", "DOGE"]);
        let px = d(rng.pick(&["95", "100", "101", "180"]));
        let sz = d(rng.pick(&["0.1", "1", "9", "900"]));
        let mut order = intent(symbol, rng.chance(2), px, sz);
        order.reason = format!("case {case}");
        let market = MarketRef::fresh(symbol, d("100"), now_ms);
        let instrument = asset(symbol, 2, 40);
        let exposure = Exposure {
            agent: account_at(d("1000000"), now_ms),
            fleet: None,
        };

        let cleared =
            match f
                .engine
                .evaluate(&f.agent, &order, &instrument, &market, &exposure, now_ms)
            {
                Ok(cleared) => cleared,
                Err(_) => {
                    // No `Cleared` exists, so there is nothing to sign. This arm
                    // deliberately contains no signing call: `sign_cleared` takes
                    // a `Cleared` by value and there is no public constructor,
                    // so the bypass is not something this test has to police —
                    // it is something the compiler refuses to compile.
                    refused_before_signer += 1;
                    continue;
                }
            };

        // The gap spec item 7's submit queue lives in.
        if rng.chance(8) {
            f.engine
                .operator_engage_kill(KillScope::Global, KillReason::Operator, now_ms)
                .expect("engage");
        }
        let paused = f.engine.kill_switch().blocking(&f.agent).is_some();

        match f.engine.sign_cleared(&key, cleared, nonce, None, now_ms) {
            Ok((request, clearance)) => {
                assert!(!paused, "case {case}: a paused agent got a signature");
                assert_eq!(request.nonce(), nonce, "case {case}");
                assert_eq!(request.vault_address(), Some(vault()), "case {case}");
                assert_eq!(clearance.network, Network::Testnet, "case {case}");
                assert!(
                    matches!(clearance.kind, ClearedKind::Order { .. }),
                    "case {case}"
                );
                signed += 1;
            }
            Err(SignClearedError::Refused(Refusal::TradingPaused { .. })) => {
                assert!(
                    paused,
                    "case {case}: refused at the signer with the switch open"
                );
                refused_at_signer += 1;
            }
            Err(other) => panic!("case {case}: unexpected {other}"),
        }
        nonce += 1;
    }

    let recorded_clearances = sink.cleared.load(Ordering::Relaxed) as u64;
    let recorded_refusals = sink.refused.load(Ordering::Relaxed) as u64;

    // The invariant: a signature always has exactly one recorded evaluation
    // behind it, and some evaluations end without one.
    assert_eq!(
        recorded_clearances,
        signed + refused_at_signer,
        "every clearance was either signed or refused at the signer"
    );
    // Item 18 and D6: a refusal at the signer lands in the ledger too, or an
    // export would show a clearance with no fill and no explanation.
    assert_eq!(
        recorded_refusals,
        refused_before_signer + refused_at_signer,
        "every refusal, wherever it happened, has a row"
    );
    assert_eq!(signed + refused_at_signer + refused_before_signer, CASES);
    assert!(
        signed < recorded_clearances,
        "the signer gate never refused anything; the property is untested"
    );

    // Both branches have to be genuinely populated or the assertions above
    // are vacuous.
    assert!(signed > 500, "only {signed} signatures");
    assert!(
        refused_before_signer > 500,
        "only {refused_before_signer} refusals before the signer"
    );
    assert!(
        refused_at_signer > 20,
        "only {refused_at_signer} refusals at the signer"
    );
}

/// The other half of `AGENTS.md` invariant 1, which no type can express:
/// **`oppen-core` must not call the ungated signer at all.**
///
/// `oppen_hl::ExchangeRequest::sign_unchecked` and
/// `AgentKey::sign_l1_action` stay `pub` because spec item 33's manual
/// escape hatch needs them from `oppen-hl`'s own example. Nothing in this
/// crate may use them: every signature here goes through
/// [`GuardrailEngine::sign_cleared`], which passes the engine as the gate.
///
/// The needles are built at runtime so this file does not match itself, and
/// comment lines are skipped so the doc comments that name the hazard are
/// not mistaken for call sites. The check is a grep, and a grep is exactly
/// as strong as the claim being made: a bypass cannot happen by accident,
/// and a deliberate one is visible.
#[test]
fn no_call_site_in_oppen_core_reaches_the_signer_unchecked() {
    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .expect("the crate's own src directory is readable")
            .map(|e| e.expect("directory entry").path())
            .collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    let needles = [
        format!("sign_{}", "unchecked"),
        format!("sign_{}", "l1_action"),
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(files.len() > 5, "the source walk found almost nothing");

    let mut offenders = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("source file is utf-8");
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for needle in &needles {
                if code.contains(needle.as_str()) {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, code));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "oppen-core must reach the signer only through GuardrailEngine::sign_cleared \
         (AGENTS.md invariant 1). Ungated call sites:\n{}",
        offenders.join("\n")
    );
}
