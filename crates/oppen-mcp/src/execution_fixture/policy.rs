//! Policy admission is separate from registry-authenticated cleanup.

use super::*;

#[tokio::test]
async fn contended_refresh_still_attempts_known_stop_cleanup_and_reports_retry() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    runtime.activate_orders().await;
    let placed = runtime
        .call(
            "place",
            place(Cloid::from_bytes([122; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(placed["status"], "resting", "{placed}");
    runtime
        .gateway
        .inner
        .engine
        .operator_engage_kill(
            KillScope::Global,
            oppen_core::guardrail::KillReason::Operator,
            now_ms(),
        )
        .unwrap();
    let permit = runtime
        .gateway
        .inner
        .decision_worker
        .clone()
        .try_acquire_owned()
        .unwrap();
    let bound = Binding {
        agent: AgentId::new("fixture-agent"),
        account: runtime.account,
    };
    let reads = venue.info_count();
    assert!(matches!(
        runtime
            .gateway
            .enforce_pauses(std::slice::from_ref(&bound), runtime.tracker())
            .await,
        Err(ToolError::Unavailable {
            what: "decision worker busy",
            ..
        })
    ));
    assert!(venue.info_count() > reads, "known-stop cleanup was skipped");
    assert_eq!(venue.submissions().len(), 1);
    drop(permit);
    runtime
        .gateway
        .enforce_pauses(&[bound], runtime.tracker())
        .await
        .unwrap();
    assert_eq!(venue.submissions().len(), 2);
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn idle_server_refreshes_independent_kill_and_redacted_policy_before_cleanup() {
    use oppen_core::guardrail::KillReason;
    use oppen_core::ledger::RegistryJournal;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{sleep, timeout};
    use tokio_util::sync::CancellationToken;

    for redact in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        let placed = runtime
            .call(
                "place",
                place(Cloid::from_bytes([120; 16]).as_str(), "0.12"),
            )
            .await;
        assert_eq!(placed["status"], "resting", "{placed}");
        assert!(
            !runtime
                .gateway
                .inner
                .engine
                .policy_status()
                .admission_inhibited
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(crate::server::serve(
            addr.port(),
            runtime.gateway.clone(),
            runtime.pairings.clone(),
            shutdown.clone(),
        ));
        timeout(Duration::from_secs(2), async {
            while TcpStream::connect(addr).await.is_err() {
                assert!(!server.is_finished());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        // Independent operator handle; never refresh the serving engine here.
        let ledger = runtime.ledger.clone();
        let operator_keys = keys.clone();
        tokio::task::spawn_blocking(move || {
            let journal = PolicyJournal::new(Arc::new(
                RegistryJournal::open(
                    ledger.clone(),
                    Arc::new(oppen_core::keys::HmacKey::from_bytes([77; 32])),
                )
                .unwrap(),
            ));
            let current = journal.current().unwrap();
            if redact {
                ledger
                    .redact(
                        current.revision,
                        "synthetic policy unavailable",
                        now_ms() as i64,
                    )
                    .unwrap();
            } else {
                let operator = GuardrailEngine::new(
                    Arc::new(journal),
                    operator_keys,
                    Arc::new(FeedSession::new()),
                )
                .unwrap();
                operator
                    .operator_engage_kill(KillScope::Global, KillReason::Operator, now_ms())
                    .unwrap();
            }
            ledger.verify().unwrap();
        })
        .await
        .unwrap();

        let canceled = timeout(Duration::from_secs(8), async {
            while venue.submissions().len() < 2 {
                assert!(!server.is_finished());
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        shutdown.cancel();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        canceled.expect("idle supervision did not observe independent policy change");
        assert_eq!(venue.submissions().len(), 2);
        assert_eq!(venue.submissions()[1]["action"]["type"], "cancel");
        assert!(
            runtime
                .gateway
                .inner
                .engine
                .policy_status()
                .admission_inhibited
        );
        runtime
            .registry
            .route_for_agent(&AgentId::new("fixture-agent"))
            .unwrap();
        let reads = keys.read_heads.lock().unwrap().len();
        let denied = runtime
            .call(
                "place",
                place(Cloid::from_bytes([121; 16]).as_str(), "0.12"),
            )
            .await;
        assert_eq!(denied["status"], "rejected", "{denied}");
        assert_eq!(venue.submissions().len(), 2);
        assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn missing_policy_keeps_registry_cleanup_available_without_default_policy() {
    let seed_dir = tempfile::tempdir().unwrap();
    let cleanup_dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let seed = Runtime::open(seed_dir.path(), venue.port(), keys.clone()).await;
    seed.activate_orders().await;
    let cloid = Cloid::from_bytes([101; 16]);
    assert_eq!(
        seed.call("place", place(cloid.as_str(), "0.12")).await["status"],
        "resting"
    );
    assert_eq!(
        seed.call(
            "place",
            place(Cloid::from_bytes([106; 16]).as_str(), "0.12")
        )
        .await["status"],
        "resting"
    );
    seed.shutdown().await;

    let runtime =
        Runtime::open_policy_fixture(cleanup_dir.path(), venue.port(), keys.clone(), None, false)
            .await;
    let bound = Binding {
        agent: AgentId::new("fixture-agent"),
        account: runtime.account,
    };
    assert!(
        runtime
            .gateway
            .inner
            .engine
            .guardrails(&bound.agent)
            .is_none()
    );
    assert!(runtime.gateway.inner.engine.policy_observation().is_err());
    assert!(
        runtime
            .gateway
            .runtime_cancellation_needed(&bound)
            .await
            .unwrap()
    );
    runtime.reconcile().await;
    let reads = keys.read_heads.lock().unwrap().len();
    let denied = runtime
        .call(
            "place",
            place(Cloid::from_bytes([102; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(denied["status"], "rejected", "{denied}");
    assert_eq!(
        denied["refusal"]["unevaluable"], "policy_authority",
        "{denied}"
    );
    assert_eq!(venue.submissions().len(), 2);
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    let account = runtime.call("get_state", json!({})).await;
    assert_eq!(
        account["policy_status"]["admission_inhibited"], true,
        "{account}"
    );
    assert_eq!(account["policy_status"]["cached_revision"], Value::Null);
    assert_eq!(account["policy_status"]["acknowledgment"], Value::Null);
    assert!(account.get("balances").is_some(), "{account}");
    assert!(account.get("loss_budget").is_none(), "{account}");
    let visible = runtime.call("get_order_status", json!({"oid":1})).await;
    assert_eq!(visible["known"], true, "{visible}");
    let canceled = runtime
        .call(
            "cancel",
            json!({"cloid":cloid.as_str(), "reason":"policy unavailable cleanup"}),
        )
        .await;
    assert_eq!(canceled["status"], "rejected", "{canceled}");
    assert_eq!(
        canceled["refusal"]["unevaluable"], "policy_authority",
        "{canceled}"
    );
    let cancel_all = runtime
        .call("cancel_all", json!({"reason":"policy unavailable cleanup"}))
        .await;
    assert_eq!(cancel_all["status"], "rejected", "{cancel_all}");
    assert_eq!(
        cancel_all["refusal"]["unevaluable"], "policy_authority",
        "{cancel_all}"
    );
    assert_eq!(venue.submissions().len(), 2);
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    runtime
        .gateway
        .enforce_pauses(&[bound], runtime.tracker())
        .await
        .unwrap();
    assert_eq!(venue.submissions().len(), 3);
    assert_eq!(venue.submissions()[2]["action"]["type"], "cancel");
    assert_eq!(
        venue.submissions()[2]["action"]["cancels"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for oid in [1, 2] {
        let status = runtime.call("get_order_status", json!({"oid":oid})).await;
        assert_eq!(status["status"], "canceled", "{status}");
    }
    assert!(runtime.gateway.inner.engine.policy_observation().is_err());
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn restart_and_stale_operator_observation_cannot_enable_orders() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let agent = AgentId::new("fixture-agent");
    let engine = &runtime.gateway.inner.engine;
    let observed = engine.policy_observation().unwrap();
    let mut config = engine.guardrails(&agent).unwrap();
    config.max_order_usd = Decimal::from(14);
    engine
        .operator_set_guardrails(&agent, config, now_ms())
        .unwrap();
    assert!(
        engine
            .operator_acknowledge_policy(observed, now_ms())
            .is_err()
    );
    let denied = runtime
        .call(
            "place",
            place(Cloid::from_bytes([103; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(denied["status"], "rejected", "{denied}");
    assert_eq!(
        denied["refusal"]["unevaluable"], "policy_authority",
        "{denied}"
    );
    assert!(venue.submissions().is_empty());
    assert!(keys.read_heads.lock().unwrap().is_empty());
    runtime.acknowledge_policy();
    runtime.shutdown().await;

    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.reconcile().await;
    let account = runtime.call("get_state", json!({})).await;
    assert_eq!(
        account["policy_status"]["admission_inhibited"], true,
        "{account}"
    );
    assert!(account["policy_status"]["cached_revision"].is_u64());
    assert_eq!(account["policy_status"]["acknowledgment"], Value::Null);
    assert!(account.get("loss_budget").is_none());
    let denied = runtime
        .call(
            "place",
            place(Cloid::from_bytes([104; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(denied["status"], "rejected", "{denied}");
    assert_eq!(
        denied["refusal"]["unevaluable"], "policy_authority",
        "{denied}"
    );
    assert!(venue.submissions().is_empty());
    assert!(keys.read_heads.lock().unwrap().is_empty());
    runtime.acknowledge_policy();
    let allowed = runtime
        .call(
            "place",
            place(Cloid::from_bytes([105; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(allowed["status"], "resting", "{allowed}");
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn corrupt_policy_preserves_account_reads_and_cleanup_after_refresh_failure_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let agent = AgentId::new("fixture-agent");
    let engine = &runtime.gateway.inner.engine;
    let mut config = engine.guardrails(&agent).unwrap();
    config.loss.max_daily_loss_usd = Some(Decimal::from(10));
    engine
        .operator_set_guardrails(&agent, config, now_ms())
        .unwrap();
    runtime.acknowledge_policy();
    let before = runtime.call("get_state", json!({})).await;
    assert_eq!(
        before["policy_status"]["admission_inhibited"], false,
        "{before}"
    );
    assert!(
        before["loss_budget"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "{before}"
    );
    let cached_revision = before["policy_status"]["cached_revision"].clone();
    for byte in [110, 111] {
        let placed = runtime
            .call(
                "place",
                place(Cloid::from_bytes([byte; 16]).as_str(), "0.12"),
            )
            .await;
        assert_eq!(placed["status"], "resting", "{placed}");
    }

    // Only policy replay metadata is damaged. The hash chain and registry
    // authority must remain valid so this is not a whole-ledger failure.
    let db = rusqlite::Connection::open(dir.path().join("ledger.db")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE events SET idem_key = NULL WHERE kind = 'policy_initialized'",
            []
        )
        .unwrap(),
        1
    );
    drop(db);
    runtime.ledger.verify().unwrap();
    runtime.registry.route_for_agent(&agent).unwrap();
    let reads = keys.read_heads.lock().unwrap().len();
    let denied = runtime
        .call(
            "place",
            place(Cloid::from_bytes([112; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(denied["status"], "rejected", "{denied}");
    assert_eq!(
        denied["refusal"]["unevaluable"], "policy_authority",
        "{denied}"
    );
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    assert_eq!(venue.submissions().len(), 2);
    let account = runtime.call("get_state", json!({})).await;
    assert_eq!(
        account["policy_status"]["admission_inhibited"], true,
        "{account}"
    );
    assert_eq!(account["policy_status"]["cached_revision"], cached_revision);
    assert_eq!(account["policy_status"]["acknowledgment"], Value::Null);
    assert!(account.get("balances").is_some());
    assert!(
        account.get("loss_budget").is_none(),
        "stale budget: {account}"
    );
    let canceled = runtime
        .call(
            "cancel",
            json!({"oid":1, "reason":"corrupt policy cleanup"}),
        )
        .await;
    assert_eq!(canceled["status"], "rejected", "{canceled}");
    assert_eq!(
        canceled["refusal"]["unevaluable"], "policy_authority",
        "{canceled}"
    );
    let cancel_all = runtime
        .call("cancel_all", json!({"reason":"corrupt policy cleanup"}))
        .await;
    assert_eq!(cancel_all["status"], "rejected", "{cancel_all}");
    assert_eq!(
        cancel_all["refusal"]["unevaluable"], "policy_authority",
        "{cancel_all}"
    );
    assert_eq!(venue.submissions().len(), 2);
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    // Runtime cleanup still signs and posts without usable policy. A venue
    // rejection leaves both orders resting so restart must retry the work.
    venue.next_response(Behavior::Rejected);
    assert!(
        runtime
            .gateway
            .enforce_pauses(
                &[Binding {
                    agent: agent.clone(),
                    account: runtime.account,
                }],
                runtime.tracker()
            )
            .await
            .is_err()
    );
    assert_eq!(venue.submissions().len(), 3);
    assert_eq!(venue.submissions()[2]["action"]["type"], "cancel");
    runtime.shutdown().await;

    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    assert!(runtime.gateway.inner.engine.policy_observation().is_err());
    assert!(runtime.gateway.inner.engine.guardrails(&agent).is_none());
    let account = runtime.call("get_state", json!({})).await;
    assert_eq!(
        account["policy_status"]["cached_revision"],
        Value::Null,
        "{account}"
    );
    assert_eq!(account["policy_status"]["admission_inhibited"], true);
    assert!(account.get("balances").is_some());
    let reads = keys.read_heads.lock().unwrap().len();
    let cancel_all = runtime
        .call("cancel_all", json!({"reason":"restarted policy cleanup"}))
        .await;
    assert_eq!(cancel_all["status"], "rejected", "{cancel_all}");
    assert_eq!(
        cancel_all["refusal"]["unevaluable"], "policy_authority",
        "{cancel_all}"
    );
    assert_eq!(venue.submissions().len(), 3);
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    runtime
        .gateway
        .enforce_pauses(
            &[Binding {
                agent,
                account: runtime.account,
            }],
            runtime.tracker(),
        )
        .await
        .unwrap();
    assert_eq!(venue.submissions().len(), 4);
    assert_eq!(venue.submissions()[3]["action"]["type"], "cancel");
    assert_eq!(
        venue.submissions()[3]["action"]["cancels"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for oid in [1, 2] {
        let status = runtime.call("get_order_status", json!({"oid":oid})).await;
        assert_eq!(status["status"], "canceled", "{status}");
    }
    runtime.shutdown().await;
    venue.shutdown().await;
}
