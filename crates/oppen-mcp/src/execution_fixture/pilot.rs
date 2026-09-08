//! Persistent pilot budgets through MCP, signing, HTTP and fill reconciliation.

use super::*;
use oppen_core::guardrail::PilotMetric;
use oppen_core::ledger::{PilotJournal, PilotState, PilotStop};

async fn authorize(runtime: &Runtime) {
    runtime.activate_orders().await;
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
    let state = PilotJournal::new(runtime.registry.clone())
        .authorize(AgentId::new("fixture-agent"), runtime.account, now_ms())
        .expect("authorize reconciled flat fixture");
    assert_eq!(state.executed_usd, Decimal::ZERO);
    assert_eq!(state.reserved_usd, Decimal::ZERO);
    assert_eq!(state.net_realized_pnl_usd, Decimal::ZERO);
    assert!(state.halt.is_none());
}

fn state(runtime: &Runtime) -> PilotState {
    PilotJournal::new(runtime.registry.clone())
        .state(runtime.account)
        .expect("pilot replay")
        .expect("persisted authority")
}

fn binding(runtime: &Runtime) -> Binding {
    Binding {
        agent: AgentId::new("fixture-agent"),
        account: runtime.account,
    }
}

#[tokio::test]
async fn pilot_stop_retries_resting_cancels_for_revoked_binding_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let mut runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    authorize(&runtime).await;
    let cloid = Cloid::from_bytes([90; 16]);
    assert_eq!(
        runtime.call("place", place(cloid.as_str(), "0.15")).await["status"],
        "resting"
    );
    venue.fill_with_fee(cloid.as_str(), Decimal::new(5, 2), Decimal::from(5));
    runtime.reconcile().await;
    assert!(
        runtime
            .gateway
            .runtime_cancellation_needed(&binding(&runtime))
            .await
            .unwrap()
    );
    let revoked = {
        let mut pairings = runtime.pairings.write().unwrap();
        let original = pairings.authenticate(&runtime.token).unwrap().id;
        assert!(pairings.revoke(original).unwrap());
        let token = pairings.issue(binding(&runtime)).unwrap();
        assert!(pairings.revoke(token.id).unwrap());
        // DELETE still needs a live bearer, but not one for the stopped account.
        runtime.token = pairings
            .issue(Binding {
                account: Address::from_bytes([8; 20]),
                ..binding(&runtime)
            })
            .unwrap()
            .reveal()
            .to_owned();
        token
    };
    runtime.shutdown().await;

    let mut runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let bound = binding(&runtime);
    {
        let pairings = runtime.pairings.read().unwrap();
        assert!(matches!(
            pairings.authenticate(revoked.reveal()),
            Err(crate::auth::AuthError::Revoked)
        ));
        assert!(pairings.bindings().contains(&bound));
    }
    let other = Binding {
        account: Address::from_bytes([8; 20]),
        ..bound.clone()
    };
    assert!(
        !runtime
            .gateway
            .runtime_cancellation_needed(&other)
            .await
            .unwrap()
    );
    {
        let mut pairings = runtime.pairings.write().unwrap();
        let current = pairings.authenticate(&runtime.token).unwrap().id;
        assert!(pairings.revoke(current).unwrap());
        runtime.token = pairings.issue(other).unwrap().reveal().to_owned();
        assert!(
            pairings.bindings().contains(&bound),
            "only revoked pairings name the stopped account"
        );
    }
    let wrong_agent = Binding {
        agent: AgentId::new("other-agent"),
        ..bound.clone()
    };
    assert!(matches!(
        runtime
            .gateway
            .runtime_cancellation_needed(&wrong_agent)
            .await,
        Err(ToolError::Unavailable { .. })
    ));
    let bindings = runtime.pairings.read().unwrap().bindings();
    assert!(runtime.gateway.inner.engine.paused_agents().is_empty());
    venue.next_response(Behavior::Rejected);
    assert!(
        runtime
            .gateway
            .enforce_pauses(&bindings, runtime.tracker())
            .await
            .is_err()
    );
    assert_eq!(venue.submissions().len(), 2);
    assert_eq!(
        runtime
            .gateway
            .inner
            .info
            .frontend_open_orders(runtime.account)
            .await
            .unwrap()
            .len(),
        1
    );
    runtime
        .gateway
        .enforce_pauses(&bindings, runtime.tracker())
        .await
        .unwrap();
    assert_eq!(
        venue.submissions().len(),
        3,
        "duplicate bindings must not duplicate cancellation"
    );
    assert_eq!(venue.submissions()[2]["action"]["type"], "cancel");
    assert!(
        runtime
            .gateway
            .inner
            .info
            .frontend_open_orders(runtime.account)
            .await
            .unwrap()
            .is_empty()
    );
    runtime.reconcile().await;
    let budget = state(&runtime);
    assert_eq!(budget.executed_usd, Decimal::from(5));
    assert_eq!(budget.reserved_usd, Decimal::from(10));
    assert_stop(
        &budget,
        PilotMetric::RealizedLoss,
        Decimal::from(5),
        Decimal::from(5),
    );
    runtime
        .gateway
        .enforce_pauses(&bindings, runtime.tracker())
        .await
        .unwrap();
    assert_eq!(
        venue.submissions().len(),
        3,
        "empty book needs no signed cancel"
    );
    assert_eq!(
        runtime
            .gateway
            .inner
            .info
            .clearinghouse_state(runtime.account)
            .await
            .unwrap()
            .asset_positions
            .len(),
        1,
        "supervision must not flatten"
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn pilot_awaiting_order_linkage_does_not_cancel_resting_orders() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys).await;
    authorize(&runtime).await;
    let cloid = Cloid::from_bytes([91; 16]);
    venue.next_response(Behavior::AppliedMalformed);
    let reply = runtime.call("place", place(cloid.as_str(), "0.15")).await;
    assert_eq!(
        reply["protocol_error"]["data"]["code"], "timeout_unknown_outcome",
        "{reply}"
    );
    venue.fill(cloid.as_str(), Decimal::new(5, 2));
    let mut fills = runtime
        .gateway
        .inner
        .info
        .user_fills_by_time(runtime.account, 0, None)
        .await
        .unwrap();
    // Account fill streams may carry only the OID, before the ambiguous
    // submission has acquired its authoritative OID binding.
    for fill in &mut fills {
        fill.cloid = None;
    }
    runtime
        .gateway
        .inner
        .feed
        .apply(
            &runtime.ledger,
            &runtime.account.to_string(),
            &oppen_hl::ws::WsEvent::UserFills {
                user: runtime.account,
                is_snapshot: false,
                fills,
            },
            now_ms(),
        )
        .unwrap();
    assert_eq!(
        state(&runtime).halt,
        Some(PilotStop::AwaitingReconciliation)
    );
    runtime
        .gateway
        .enforce_pauses(&[binding(&runtime)], runtime.tracker())
        .await
        .unwrap();
    assert_eq!(venue.submissions().len(), 1);
    // Resolve the pending submission before a duplicate REST fill supplies
    // its cloid. Unproven duplicate enrichment is deliberately a hard stop.
    drop(
        runtime
            .gateway
            .reserve_submission(&binding(&runtime))
            .await
            .unwrap(),
    );
    runtime.reconcile().await;
    assert!(state(&runtime).halt.is_none());
    runtime
        .gateway
        .enforce_pauses(&[binding(&runtime)], runtime.tracker())
        .await
        .unwrap();
    assert_eq!(venue.submissions().len(), 1);
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn ordinary_resume_during_cancel_reads_cannot_clear_a_pilot_stop() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    authorize(&runtime).await;
    let cloid = Cloid::from_bytes([92; 16]);
    assert_eq!(
        runtime.call("place", place(cloid.as_str(), "0.15")).await["status"],
        "resting"
    );
    venue.fill_with_fee(cloid.as_str(), Decimal::new(5, 2), Decimal::from(5));
    runtime.reconcile().await;
    let bound = binding(&runtime);
    let scope = oppen_core::guardrail::KillScope::Agent {
        agent: bound.agent.clone(),
    };
    runtime
        .gateway
        .inner
        .engine
        .operator_engage_kill(
            scope.clone(),
            oppen_core::guardrail::KillReason::Operator,
            now_ms(),
        )
        .unwrap();
    let queue = runtime.gateway.execution_queue(bound.account);
    {
        let held = queue.lock().await;
        let params = SymbolActionParams {
            symbol: None,
            reason: "pilot sweep".into(),
        };
        let mut cancel = std::pin::pin!(runtime.gateway.cancel_all_with(
            &bound,
            &params,
            true,
            runtime.tracker(),
            async {
                runtime
                    .gateway
                    .inner
                    .engine
                    .operator_release_kill(&scope, now_ms())
                    .unwrap();
                runtime.acknowledge_policy();
                Ok((
                    runtime
                        .gateway
                        .inner
                        .info
                        .frontend_open_orders(bound.account)
                        .await
                        .unwrap(),
                    Universe::from_meta(&runtime.gateway.inner.info.meta().await.unwrap()).unwrap(),
                ))
            },
            |cleared| async {
                assert!(queue.try_lock().is_err());
                runtime
                    .gateway
                    .submit(cleared, None, &bound, None, None)
                    .await
            },
        ));
        assert!(matches!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(cancel.as_mut().poll(cx))).await,
            std::task::Poll::Pending
        ));
        runtime
            .gateway
            .inner
            .engine
            .operator_release_kill(&scope, now_ms())
            .unwrap();
        runtime.acknowledge_policy();
        drop(held);
        assert!(cancel.as_mut().await.unwrap().complete);
        assert_eq!(venue.submissions().len(), 2);
        assert!(runtime.gateway.inner.engine.paused_agents().is_empty());
        assert!(
            runtime
                .gateway
                .runtime_cancellation_needed(&bound)
                .await
                .unwrap()
        );
    }
    runtime.shutdown().await;
    venue.shutdown().await;
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
    restarted.activate_orders().await;
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
    restarted.activate_orders().await;
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
    restarted.activate_orders().await;
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
