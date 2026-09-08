//! Reuse the real authenticated, physically reopenable activation authority.
use super::*;
use crate::guardrail::{KillReleaseError, KillReleaseResolution};

fn engage(f: &Fixture, scope: KillScope) {
    f.engine
        .operator_engage_kill(scope, KillReason::Operator, NOW)
        .unwrap();
}

#[test]
fn exact_scope_receipt_and_original_pilot_survive_physical_reopen() {
    for scope in [KillScope::Global, KillScope::agent("activation-agent")] {
        let f = Fixture::new(true, false);
        let original = f.pilot.state(f.account).unwrap().unwrap();
        let order = f.historical_order(1, true);
        f.observe_order(&order, 1, "canceled");
        f.historical_fill(1, 1, true, "0.05", "0", "0.1");
        let accounted = f.pilot.state(f.account).unwrap().unwrap();
        assert_eq!(accounted.executed_usd, Decimal::from(5));
        assert_eq!(accounted.reserved_usd, Decimal::from(10));
        assert_eq!(accounted.net_realized_pnl_usd, Decimal::new(-1, 1));
        assert_eq!(accounted.baseline, original.baseline);
        assert_eq!(accounted.authorized_at_ms, original.authorized_at_ms);
        engage(&f, KillScope::Global);
        engage(&f, KillScope::agent(f.agent.clone()));
        let pilot = f.pilot.state(f.account).unwrap();
        let consent_count = f.consent_count();
        let before = f.policy.current().unwrap();
        let before_head = f.ledger.chain_head().unwrap();
        let review = f
            .engine
            .review_kill_release(scope.clone(), &|| NOW)
            .unwrap();
        let display = review.display().clone();
        assert_eq!(display.network, Network::Testnet);
        assert_eq!(display.affected.len(), 1);
        assert_eq!(display.affected[0].pilot, accounted);
        let receipt = f
            .engine
            .confirm_kill_release(review, &|| NOW, &|| Ok(()))
            .unwrap();
        assert_eq!(receipt.reviewed_stop_generation, display.stop_generation);
        assert_eq!(receipt.operation_id, display.operation_id);
        let mut expected = before.state;
        expected.kill.release(&scope);
        assert_eq!(f.policy.current().unwrap().state, expected);
        assert!(f.engine.policy_status().acknowledgment.is_none());
        let release_events = f.ledger.get_events(before_head.seq, 100).unwrap().events;
        assert_eq!(release_events.len(), 1);
        assert_eq!(
            release_events[0].kind,
            crate::ledger::EventKind::PolicyReplaced
        );
        let f = f.reopen();
        let head = f.ledger.chain_head().unwrap();
        match f.engine.reconcile_kill_release(&receipt.operation_id) {
            KillReleaseResolution::Committed {
                receipt: found,
                current_persisted_kill,
                ..
            } => {
                assert_eq!(*found, receipt);
                assert_eq!(current_persisted_kill, expected.kill);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        assert_eq!(f.pilot.state(f.account).unwrap(), pilot);
        assert_eq!(f.consent_count(), consent_count);
        assert!(f.engine.policy_status().acknowledgment.is_none());
        assert!(f.ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn repeated_same_engagement_invalidates_review_without_changing_fields() {
    let f = Fixture::new(true, false);
    engage(&f, KillScope::Global);
    let review = f
        .engine
        .review_kill_release(KillScope::Global, &|| NOW)
        .unwrap();
    let original = review.display().local_engagement.clone();
    let pending = f
        .engine
        .begin_operator_kill(KillScope::Global, KillReason::Operator, NOW);
    assert_eq!(
        f.engine.state().emergency.get(&KillScope::Global),
        original.as_ref()
    );
    assert!(matches!(
        f.engine.confirm_kill_release(review, &|| NOW, &|| Ok(())),
        Err(KillReleaseError::Refused { .. })
    ));
    f.engine.persist_operator_kill(pending).unwrap();
}

#[test]
fn pending_halt_rejects_release_reengage_aba_but_not_unrelated_scope() {
    let f = Fixture::new(true, false);
    let scope = KillScope::agent(f.agent.clone());
    let old = f
        .engine
        .begin_operator_kill(scope.clone(), KillReason::Operator, NOW);
    let review = f
        .engine
        .review_kill_release(scope.clone(), &|| NOW)
        .unwrap();
    f.engine
        .confirm_kill_release(review, &|| NOW, &|| Ok(()))
        .unwrap();
    let new = f
        .engine
        .begin_operator_kill(scope.clone(), KillReason::Operator, NOW);
    let head = f.ledger.chain_head().unwrap();
    assert!(f.engine.persist_operator_kill(old).is_err());
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    let global = f
        .engine
        .begin_operator_kill(KillScope::Global, KillReason::Operator, NOW);
    f.engine.persist_operator_kill(new).unwrap();
    f.engine.persist_operator_kill(global).unwrap();
    assert!(
        f.policy
            .current()
            .unwrap()
            .state
            .kill
            .engagement(&scope)
            .is_some()
    );
    assert!(f.policy.current().unwrap().state.kill.global().is_some());
}

#[test]
fn operator_request_reason_is_not_replaced_by_existing_feed_failure() {
    let f = Fixture::new(true, false);
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::FeedFailure, NOW)
        .unwrap();
    let before = f.ledger.chain_head().unwrap();
    let pending = f
        .engine
        .begin_operator_kill(KillScope::Global, KillReason::Operator, NOW);
    assert_eq!(
        f.ledger.chain_head().unwrap(),
        before,
        "begin must do no disk I/O"
    );
    f.engine.persist_operator_kill(pending).unwrap();
    assert_eq!(
        f.policy
            .current()
            .unwrap()
            .state
            .kill
            .global()
            .unwrap()
            .reason,
        KillReason::FeedFailure
    );
    let events = f.ledger.get_events(before.seq, 100).unwrap().events;
    let actions: Vec<_> = events
        .iter()
        .filter(|e| e.kind == crate::ledger::EventKind::OperatorAction)
        .collect();
    assert_eq!(actions.len(), 1);
    let payload = actions[0].payload.as_ref().unwrap();
    assert!(payload.to_string().contains("\"operator\""), "{payload}");
    assert!(!payload.to_string().contains("feed_failure"), "{payload}");
}

#[test]
fn actual_halt_during_publication_wins_for_both_scopes() {
    for scope in [KillScope::Global, KillScope::agent("activation-agent")] {
        let f = Fixture::new(true, false);
        engage(&f, scope.clone());
        let review = f
            .engine
            .review_kill_release(scope.clone(), &|| NOW)
            .unwrap();
        let display = review.display().clone();
        let pending = Arc::new(Mutex::new(None));
        let capture = pending.clone();
        let engine = f.engine.clone();
        let target = scope.clone();
        f.at_next_audit(move || {
            *capture.lock().unwrap() =
                Some(engine.begin_operator_kill(target, KillReason::Operator, NOW));
        });
        assert!(matches!(
            f.engine.confirm_kill_release(review, &|| NOW, &|| Ok(())),
            Err(KillReleaseError::Uncertain { .. })
        ));
        let head = f.ledger.chain_head().unwrap();
        match f.engine.reconcile_kill_release(&display.operation_id) {
            KillReleaseResolution::Committed {
                receipt,
                current_persisted_kill,
                current_effective_kill,
                current_stop_generation,
                ..
            } => {
                assert_eq!(receipt.reviewed_stop_generation, display.stop_generation);
                assert!(current_persisted_kill.engagement(&scope).is_none());
                assert!(current_effective_kill.engagement(&scope).is_some());
                assert!(current_stop_generation > display.stop_generation);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(head, f.ledger.chain_head().unwrap());
        assert!(f.engine.policy_status().acknowledgment.is_none());
        f.engine
            .persist_operator_kill(pending.lock().unwrap().take().unwrap())
            .unwrap();
        assert!(
            f.policy
                .current()
                .unwrap()
                .state
                .kill
                .engagement(&scope)
                .is_some()
        );
        assert_no_order_evidence(&f);
    }
}

#[test]
fn publication_failure_remains_unknown_across_reopen_without_publishing_on_read() {
    let f = Fixture::new(true, false);
    engage(&f, KillScope::Global);
    let review = f
        .engine
        .review_kill_release(KillScope::Global, &|| NOW)
        .unwrap();
    let id = review.display().operation_id.clone();
    let fail = f.fail.clone();
    f.at_next_audit(move || fail.store(true, Ordering::SeqCst));
    assert!(matches!(
        f.engine.confirm_kill_release(review, &|| NOW, &|| Ok(())),
        Err(KillReleaseError::Uncertain { .. })
    ));
    assert!(matches!(
        f.engine.reconcile_kill_release(&id),
        KillReleaseResolution::Unknown { .. }
    ));
    assert!(f.engine.state().emergency.contains_key(&KillScope::Global));
    f.fail.store(false, Ordering::SeqCst);
    let f = f.reopen();
    let head = f.ledger.chain_head().unwrap();
    // Opening does not repair an existing anchor. A read-only outcome query
    // must not silently publish the uncertain transition either.
    assert!(matches!(
        f.engine.reconcile_kill_release(&id),
        KillReleaseResolution::Unknown { .. }
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert!(f.engine.policy_status().acknowledgment.is_none());
}

#[test]
fn absent_is_only_snapshot_absence_and_reading_it_does_not_consume_review() {
    let f = Fixture::new(true, false);
    engage(&f, KillScope::Global);
    let review = f
        .engine
        .review_kill_release(KillScope::Global, &|| NOW)
        .unwrap();
    let id = review.display().operation_id.clone();
    let head = f.ledger.chain_head().unwrap();
    assert!(matches!(
        f.engine.reconcile_kill_release(&id),
        KillReleaseResolution::Absent { .. }
    ));
    assert_eq!(head, f.ledger.chain_head().unwrap());
    f.engine
        .confirm_kill_release(review, &|| NOW, &|| Ok(()))
        .unwrap();
    assert!(matches!(
        f.engine.reconcile_kill_release(&id),
        KillReleaseResolution::Committed { .. }
    ));
}

#[test]
fn clock_expiry_rollback_and_foreign_engine_refuse_before_transition() {
    for at in [NOW - 1, NOW + 60_000] {
        let f = Fixture::new(true, false);
        engage(&f, KillScope::Global);
        let review = f
            .engine
            .review_kill_release(KillScope::Global, &|| NOW)
            .unwrap();
        let head = f.ledger.chain_head().unwrap();
        assert!(matches!(
            f.engine.confirm_kill_release(review, &|| at, &|| Ok(())),
            Err(KillReleaseError::Refused { .. })
        ));
        assert_eq!(head, f.ledger.chain_head().unwrap());
    }
    let f = Fixture::new(true, false);
    let other = Fixture::new(true, false);
    engage(&f, KillScope::Global);
    let review = f
        .engine
        .review_kill_release(KillScope::Global, &|| NOW)
        .unwrap();
    assert!(matches!(
        other
            .engine
            .confirm_kill_release(review, &|| NOW, &|| Ok(())),
        Err(KillReleaseError::Refused { .. })
    ));
}

#[test]
fn missing_pilot_and_unresolved_global_membership_refuse() {
    let f = Fixture::new(false, false);
    engage(&f, KillScope::Global);
    assert!(
        f.engine
            .review_kill_release(KillScope::Global, &|| NOW)
            .is_err()
    );
    let f = Fixture::new(true, false);
    engage(&f, KillScope::Global);
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails.insert(
        AgentId::from("unresolved"),
        next.guardrails[&f.agent].clone(),
    );
    f.policy.replace(current.revision, next, NOW).unwrap();
    assert!(
        f.engine
            .review_kill_release(KillScope::Global, &|| NOW)
            .is_err()
    );
}

#[test]
fn executed_and_loss_exhaustion_refuse_both_release_scopes_after_reopen() {
    for loss in [false, true] {
        let f = Fixture::new(true, false);
        let original = f.pilot.state(f.account).unwrap().unwrap();
        if loss {
            let order = f.historical_order(1, true);
            f.observe_order(&order, 1, "filled");
            f.historical_fill(1, 1, true, "0.05", "0", "5");
            // Later profit does not erase the original loss exhaustion.
            f.historical_fill(1, 2, true, "0.10", "10", "0");
        } else {
            for id in 1..=10 {
                let is_buy = id % 2 != 0;
                let order = f.historical_order(id, is_buy);
                f.observe_order(&order, id, "filled");
                f.historical_fill(id, u64::from(id), is_buy, "0.15", "0", "0");
            }
        }
        let pilot = f.pilot.state(f.account).unwrap().unwrap();
        let metric = if loss {
            crate::guardrail::PilotMetric::RealizedLoss
        } else {
            crate::guardrail::PilotMetric::ExecutedNotional
        };
        assert!(matches!(&pilot.halt,
            Some(crate::ledger::PilotStop::Exhausted { metric: observed, .. }) if *observed == metric));
        assert_eq!(pilot.baseline, original.baseline);
        assert_eq!(pilot.authorized_at_ms, original.authorized_at_ms);
        engage(&f, KillScope::Global);
        engage(&f, KillScope::agent(f.agent.clone()));
        let f = f.reopen();
        let head = f.ledger.chain_head().unwrap();
        let policy = f.policy.current().unwrap();
        for scope in [KillScope::Global, KillScope::agent(f.agent.clone())] {
            assert!(matches!(
                f.engine.review_kill_release(scope, &|| NOW),
                Err(KillReleaseError::Refused { .. })
            ));
            assert_eq!(f.ledger.chain_head().unwrap(), head);
            assert_eq!(f.policy.current().unwrap(), policy);
            assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), pilot);
            assert_eq!(f.consent_count(), 1);
            assert!(f.engine.policy_status().admission_inhibited);
        }
    }
}

#[test]
fn active_grant_without_policy_and_multiagent_pilot_roster_refuse_global_release() {
    let f = Fixture::new(true, false);
    engage(&f, KillScope::Global);
    let agent = AgentId::from("second-agent");
    let account = Address::from_bytes([22; 20]);
    let wallet = f
        .keys
        .create_agent_key(
            &agent,
            SecretText::new(format!("{:064x}", 2)),
            NOW + 86_400_000,
            NOW,
        )
        .unwrap();
    f.registry
        .grant(
            RegistryBinding {
                agent: agent.clone(),
                container: account,
                vault_address: None,
                wallet,
            },
            NOW,
        )
        .unwrap();
    let head = f.ledger.chain_head().unwrap();
    assert!(matches!(
        f.engine.review_kill_release(KillScope::Global, &|| NOW),
        Err(KillReleaseError::Refused { .. })
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);

    // Resolve policy membership legitimately. Global release must still refuse:
    // this ledger's original immutable pilot does not authorize the second route.
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails
        .insert(agent.clone(), next.guardrails[&f.agent].clone());
    f.policy.replace(current.revision, next, NOW).unwrap();
    let head = f.ledger.chain_head().unwrap();
    let original = f.pilot.state(f.account).unwrap();
    assert!(f.pilot.authorize(agent, account, NOW).is_err());
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert!(matches!(
        f.engine.review_kill_release(KillScope::Global, &|| NOW),
        Err(KillReleaseError::Refused { .. })
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert_eq!(f.pilot.state(f.account).unwrap(), original);
    assert_eq!(f.consent_count(), 1);
}

#[test]
fn full_head_policy_route_and_pilot_changes_invalidate_review() {
    for change in 0..4 {
        let f = Fixture::new(true, false);
        engage(&f, KillScope::Global);
        let review = f
            .engine
            .review_kill_release(KillScope::Global, &|| NOW)
            .unwrap();
        match change {
            0 => {
                ordinary_row(&f);
            }
            1 => {
                let current = f.policy.current().unwrap();
                let mut next = current.state;
                next.guardrails.get_mut(&f.agent).unwrap().max_order_usd = Decimal::from(14);
                f.policy.replace(current.revision, next, NOW).unwrap();
            }
            2 => {
                f.registry
                    .retire(&f.registry.route_for_agent(&f.agent).unwrap(), NOW)
                    .unwrap();
            }
            _ => {
                f.historical_order(9, true);
            }
        }
        let head = f.ledger.chain_head().unwrap();
        assert!(matches!(
            f.engine.confirm_kill_release(review, &|| NOW, &|| Ok(())),
            Err(KillReleaseError::Refused { .. })
        ));
        assert_eq!(f.ledger.chain_head().unwrap(), head);
    }
}

#[test]
fn deadline_is_resampled_after_write_acquisition_before_any_durable_release() {
    let f = Fixture::new(true, false);
    engage(&f, KillScope::Global);
    let review = f
        .engine
        .review_kill_release(KillScope::Global, &|| NOW)
        .unwrap();
    let before = f.policy.current().unwrap();
    let head = f.ledger.chain_head().unwrap();
    let path = f._dir.path().join("activation.db");
    let now = Arc::new(AtomicU64::new(NOW));
    let advance = now.clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked_tx.send(()).unwrap();
        release_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        advance.store(NOW + 60_000, Ordering::SeqCst);
        connection.execute_batch("ROLLBACK").unwrap();
    });
    locked_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let first = AtomicBool::new(true);
    // This proves post-acquisition resampling, not a measured lock wait: the
    // writer may release before confirmation attempts BEGIN IMMEDIATE.
    let result = f.engine.confirm_kill_release(
        review,
        &|| {
            if first.swap(false, Ordering::SeqCst) {
                release_tx.send(()).unwrap();
                NOW
            } else {
                now.load(Ordering::SeqCst)
            }
        },
        &|| Ok(()),
    );
    writer.join().unwrap();
    assert!(
        matches!(result, Err(KillReleaseError::Refused { .. })),
        "{result:?}"
    );
    assert_eq!(f.policy.current().unwrap(), before);
    assert_eq!(f.ledger.chain_head().unwrap(), head);
}
