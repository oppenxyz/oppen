//! Persistent pilot budgets through MCP, signing, HTTP and fill reconciliation.

use super::*;
use oppen_core::guardrail::PilotMetric;
use oppen_core::ledger::{PilotJournal, PilotState, PilotStop};

async fn authorize(runtime: &Runtime) {
    runtime.reconcile().await;
    let info = &runtime.gateway.inner.info;
    assert!(
        info.clearinghouse_state(runtime.account)
            .await
            .unwrap()
            .asset_positions
            .is_empty()
    );
    assert!(
        info.frontend_open_orders(runtime.account)
            .await
            .unwrap()
            .is_empty()
    );
    // This is operator authority over the exclusive loopback account only.
    // Keep no journal handle alive across Runtime::shutdown's physical drop.
    let state = PilotJournal::new(runtime.ledger.clone())
        .authorize(AgentId::new("fixture-agent"), runtime.account, now_ms())
        .expect("authorize reconciled flat fixture");
    assert_eq!(state.executed_usd, Decimal::ZERO);
    assert_eq!(state.reserved_usd, Decimal::ZERO);
    assert_eq!(state.net_realized_pnl_usd, Decimal::ZERO);
    assert!(state.halt.is_none());
}

fn state(runtime: &Runtime) -> PilotState {
    PilotJournal::new(runtime.ledger.clone())
        .state(runtime.account)
        .expect("pilot replay")
        .expect("persisted authority")
}

fn assert_stop(state: &PilotState, metric: PilotMetric, observed: Decimal, limit: Decimal) {
    assert_eq!(
        state.halt,
        Some(PilotStop::Exhausted {
            metric,
            observed_usd: observed,
            limit_usd: limit,
        })
    );
}

async fn assert_blocked(
    runtime: &Runtime,
    venue: &Venue,
    keys: &FixtureKeys,
    tool: &str,
    arguments: Value,
    metric: PilotMetric,
) {
    let posts = venue.submissions().len();
    let reads = keys.read_heads.lock().unwrap().len();
    let reply = runtime.call(tool, arguments).await;
    assert!(
        reply.get("protocol_error").is_none(),
        "budget refusal must be a normal Reply: {reply}"
    );
    assert_eq!(reply["status"], "rejected", "{reply}");
    assert_eq!(reply["code"], "guardrail_reject", "{reply}");
    assert_eq!(reply["retryable"], false, "{reply}");
    assert_eq!(reply["refusal"]["refusal"], "pilot_budget", "{reply}");
    assert_eq!(
        reply["refusal"]["metric"],
        serde_json::to_value(metric).unwrap(),
        "{reply}"
    );
    assert_eq!(
        venue.submissions().len(),
        posts,
        "refused order reached HTTP exchange"
    );
    assert_eq!(
        keys.read_heads.lock().unwrap().len(),
        reads,
        "refused order reached key access"
    );
}

#[tokio::test]
async fn five_round_trips_exhaust_executed_budget_across_physical_restart() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    authorize(&runtime).await;

    for cycle in 0..5u8 {
        let cloid = Cloid::from_bytes([40 + cycle; 16]);
        let placed = runtime.call("place", place(cloid.as_str(), "0.15")).await;
        assert_eq!(placed["status"], "resting", "cycle {cycle}: {placed}");
        venue.fill(cloid.as_str(), Decimal::new(15, 2));
        runtime.reconcile().await;
        let closed = runtime
            .call(
                "close_position",
                json!({"symbol":"TEST","reason":"pilot cycle close"}),
            )
            .await;
        assert_eq!(closed["status"], "filled", "cycle {cycle}: {closed}");
        runtime.reconcile().await;
        let budget = state(&runtime);
        assert_eq!(
            budget.executed_usd,
            Decimal::from(30 * (u32::from(cycle) + 1))
        );
        assert_eq!(budget.reserved_usd, Decimal::ZERO);
        assert_eq!(
            budget.net_realized_pnl_usd,
            -Decimal::new(2 * (i64::from(cycle) + 1), 2)
        );
        if cycle < 4 {
            assert!(budget.halt.is_none());
        }
    }
    let exhausted = state(&runtime);
    assert_stop(
        &exhausted,
        PilotMetric::ExecutedNotional,
        Decimal::from(150),
        Decimal::from(150),
    );
    assert_eq!(venue.submissions().len(), 10);
    runtime.shutdown().await;

    let restarted = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    restarted.reconcile().await;
    let recovered = state(&restarted);
    assert_eq!(recovered.executed_usd, Decimal::from(150));
    assert_eq!(recovered.reserved_usd, Decimal::ZERO);
    assert_eq!(recovered.net_realized_pnl_usd, -Decimal::new(10, 2));
    assert_eq!(recovered.halt, exhausted.halt);
    assert_blocked(
        &restarted,
        &venue,
        &keys,
        "place",
        place(Cloid::from_bytes([45; 16]).as_str(), "0.15"),
        PilotMetric::ExecutedNotional,
    )
    .await;
    restarted.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn canceled_orders_keep_committed_budget_across_physical_restart() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    authorize(&runtime).await;

    for cycle in 0..10u8 {
        let cloid = Cloid::from_bytes([60 + cycle; 16]);
        let placed = runtime.call("place", place(cloid.as_str(), "0.15")).await;
        assert_eq!(placed["status"], "resting", "cycle {cycle}: {placed}");
        let canceled = runtime
            .call(
                "cancel",
                json!({"cloid":cloid.as_str(),"reason":"pilot cancel"}),
            )
            .await;
        assert_eq!(canceled["status"], "canceled", "{canceled}");
        let visible = runtime.call("get_state", json!({})).await;
        assert_eq!(visible["orders"], json!([]), "{visible}");
        runtime.reconcile().await;
        let budget = state(&runtime);
        assert_eq!(budget.executed_usd, Decimal::ZERO);
        assert_eq!(
            budget.reserved_usd,
            Decimal::from(15 * (u32::from(cycle) + 1))
        );
        assert_eq!(budget.net_realized_pnl_usd, Decimal::ZERO);
    }
    assert_eq!(venue.submissions().len(), 20);
    runtime.shutdown().await;

    let restarted = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    restarted.reconcile().await;
    let recovered = state(&restarted);
    assert_eq!(recovered.executed_usd, Decimal::ZERO);
    assert_eq!(recovered.reserved_usd, Decimal::from(150));
    assert_eq!(recovered.net_realized_pnl_usd, Decimal::ZERO);
    assert_blocked(
        &restarted,
        &venue,
        &keys,
        "place",
        place(Cloid::from_bytes([70; 16]).as_str(), "0.15"),
        PilotMetric::CommittedNotional,
    )
    .await;
    restarted.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn five_dollars_of_fees_latches_loss_and_blocks_reduce_only_close_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    authorize(&runtime).await;
    let cloid = Cloid::from_bytes([80; 16]);
    let placed = runtime.call("place", place(cloid.as_str(), "0.15")).await;
    assert_eq!(placed["status"], "resting", "{placed}");
    venue.fill_with_fee(cloid.as_str(), Decimal::new(15, 2), Decimal::from(5));
    runtime.reconcile().await;
    let halted = state(&runtime);
    assert_eq!(halted.executed_usd, Decimal::from(15));
    assert_eq!(halted.reserved_usd, Decimal::ZERO);
    assert_eq!(halted.net_realized_pnl_usd, -Decimal::from(5));
    assert_stop(
        &halted,
        PilotMetric::RealizedLoss,
        Decimal::from(5),
        Decimal::from(5),
    );
    let close = json!({"symbol":"TEST","reason":"must not bypass pilot stop"});
    assert_blocked(
        &runtime,
        &venue,
        &keys,
        "close_position",
        close.clone(),
        PilotMetric::RealizedLoss,
    )
    .await;
    assert_eq!(venue.submissions().len(), 1);
    runtime.shutdown().await;

    let restarted = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    restarted.reconcile().await;
    let recovered = state(&restarted);
    assert_eq!(recovered.net_realized_pnl_usd, -Decimal::from(5));
    assert_eq!(recovered.halt, halted.halt);
    assert_blocked(
        &restarted,
        &venue,
        &keys,
        "close_position",
        close,
        PilotMetric::RealizedLoss,
    )
    .await;
    restarted.shutdown().await;
    venue.shutdown().await;
}
