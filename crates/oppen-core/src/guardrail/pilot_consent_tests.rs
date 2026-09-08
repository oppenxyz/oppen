use super::*;
use crate::ledger::{
    PilotConsentAttestation, PilotConsentError, PilotConsentEvidence, PilotConsentOutcome,
};
use crate::reconcile::{ReconcileSource, Reconciler};
use oppen_hl::OrderRef;
use oppen_hl::types::{Fill, OpenOrder, OrderStatusResponse};

struct EmptyVenue;
impl ReconcileSource for EmptyVenue {
    fn network(&self) -> Network {
        Network::Testnet
    }
    async fn user_fills_by_time(
        &self,
        _: Address,
        _: u64,
        end: Option<u64>,
    ) -> Result<Vec<Fill>, oppen_hl::Error> {
        assert_eq!(end, None);
        Ok(vec![])
    }
    async fn frontend_open_orders(&self, _: Address) -> Result<Vec<OpenOrder>, oppen_hl::Error> {
        Ok(vec![])
    }
    async fn order_status(
        &self,
        _: Address,
        _: OrderRef,
    ) -> Result<OrderStatusResponse, oppen_hl::Error> {
        panic!("no pending orders in this initial account fixture")
    }
}

#[test]
fn standalone_historical_filled_order_cannot_receive_new_baseline() {
    let (f, now) = setup();
    f.ledger
        .append(&crate::ledger::NewEvent {
            kind: crate::ledger::EventKind::OrderStateChange,
            ts_ms: now as i64,
            agent_id: Some(f.agent.as_str()),
            payload: &serde_json::json!({"account":f.account,"oid":99,"status":"filled"}),
            snapshot: None,
        })
        .unwrap();
    let head = f.ledger.chain_head().unwrap();
    assert!(
        f.pilot
            .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
            .is_err()
    );
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert_eq!(f.consent_count(), 0);
}

fn setup() -> (Fixture, u64) {
    let f = Fixture::new(false, false);
    f.feed.bind_ledger(&f.ledger).unwrap();
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW)
        .unwrap();
    let now = NOW;
    let gap = f
        .ledger
        .open_gap(
            &format!("userFills:{}", f.account),
            now as i64,
            Some("initial consent fixture"),
        )
        .unwrap();
    f.ledger.close_gap(gap.gap_id, now as i64).unwrap();
    let stamp = f.feed.unreconciled();
    let outcomes = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            Reconciler::new(&f.ledger, EmptyVenue)
                .unwrap()
                .with_observation_clock(|| NOW as i64)
                .reconcile_all(&[])
                .await
                .unwrap()
        });
    let receipt = outcomes.into_iter().find_map(|o| o.fill_walk).unwrap();
    assert!(receipt.initial_window);
    assert_eq!(receipt.pages, 1);
    assert_eq!(receipt.requested_end_ms, None);
    assert_eq!(receipt.local_read_started_at_ms, NOW as i64);
    assert_eq!(receipt.local_read_completed_at_ms, NOW as i64);
    f.feed.record_fill_walk(&stamp, receipt);
    f.feed.reconciled(&stamp, now);
    (f, now)
}
fn evidence(f: &Fixture, now: u64) -> PilotConsentEvidence {
    PilotConsentEvidence {
        account: f.fresh_evidence(now),
    }
}
fn yes() -> PilotConsentAttestation {
    PilotConsentAttestation {
        dedicated_exclusive_account: true,
        never_used_for_trading: true,
    }
}

#[test]
fn initial_consent_only_appends_one_row_preserves_policy_and_reopens() {
    let (f, now) = setup();
    let policy = f.policy.current().unwrap();
    let observation = f
        .pilot
        .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
        .unwrap();
    let review = f
        .pilot
        .review_authorization(observation, evidence(&f, now), &|| now)
        .unwrap();
    assert!(
        review
            .display()
            .required_attestations
            .never_used_for_trading
    );
    assert_eq!(review.display().gross_exposure_limit_usd, Decimal::from(25));
    let correlation = review.display().correlation.clone();
    let before = f.ledger.chain_head().unwrap();
    assert!(matches!(
        f.pilot.authorization_outcome(&correlation).unwrap(),
        PilotConsentOutcome::Absent
    ));
    let receipt = f
        .pilot
        .confirm_authorization(review, evidence(&f, now), yes(), &|| now, &|| Ok(()))
        .unwrap();
    let events = f.ledger.get_events(before.seq, 100).unwrap().events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, crate::ledger::EventKind::PilotAuthorized);
    assert_eq!(events[0].hash, receipt.hash);
    assert_eq!(f.policy.current().unwrap(), policy);
    assert!(f.engine.policy_status().acknowledgment.is_none());
    let original = f.pilot.state(f.account).unwrap().unwrap();
    let f = f.reopen();
    let head = f.ledger.chain_head().unwrap();
    assert!(
        matches!(f.pilot.authorization_outcome(&correlation).unwrap(), PilotConsentOutcome::Committed { current_pilot, .. } if current_pilot == original)
    );
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert_eq!(f.policy.current().unwrap(), policy);
    assert!(
        f.pilot
            .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
            .is_err()
    );
}

#[test]
fn contradictory_known_execution_refuses_despite_never_used_attestation() {
    for before in [false, true] {
        let (f, now) = setup();
        let observation = f
            .pilot
            .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
            .unwrap();
        if before {
            f.historical_fill(55, 55, true, "0.05", "0", "0.1");
            assert!(
                f.pilot
                    .review_authorization(observation, evidence(&f, now), &|| now)
                    .is_err()
            );
        } else {
            let review = f
                .pilot
                .review_authorization(observation, evidence(&f, now), &|| now)
                .unwrap();
            f.historical_fill(55, 55, true, "0.05", "0", "0.1");
            assert!(matches!(
                f.pilot.confirm_authorization(
                    review,
                    evidence(&f, now),
                    yes(),
                    &|| now,
                    &|| Ok(())
                ),
                Err(PilotConsentError::Refused { .. })
            ));
        }
        let head = f.ledger.chain_head().unwrap();
        assert!(
            f.pilot
                .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
                .is_err()
        );
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        assert_eq!(f.consent_count(), 0);
    }
}

#[test]
fn missing_attestation_stale_evidence_changed_policy_and_disconnect_refuse() {
    for case in 0..4 {
        let (f, now) = setup();
        let observation = f
            .pilot
            .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
            .unwrap();
        let review = f
            .pilot
            .review_authorization(observation, evidence(&f, now), &|| now)
            .unwrap();
        let mut attestation = yes();
        let mut clock = now;
        match case {
            0 => attestation.never_used_for_trading = false,
            1 => clock += 60_000,
            2 => {
                f.engine
                    .operator_release_kill(&KillScope::Global, now)
                    .unwrap();
            }
            _ => {
                f.feed.unreconciled();
            }
        }
        let head = f.ledger.chain_head().unwrap();
        assert!(matches!(
            f.pilot.confirm_authorization(
                review,
                evidence(&f, now),
                attestation,
                &|| clock,
                &|| Ok(())
            ),
            Err(PilotConsentError::Refused { .. })
        ));
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        assert_eq!(f.consent_count(), 0);
    }
}

#[test]
fn failed_publication_stays_unknown_after_physical_reopen_and_read_does_not_repair() {
    let (f, now) = setup();
    let observation = f
        .pilot
        .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
        .unwrap();
    let review = f
        .pilot
        .review_authorization(observation, evidence(&f, now), &|| now)
        .unwrap();
    let correlation = review.display().correlation.clone();
    let fail = f.fail.clone();
    f.at_next_audit(move || fail.store(true, Ordering::SeqCst));
    assert!(matches!(
        f.pilot
            .confirm_authorization(review, evidence(&f, now), yes(), &|| now, &|| Ok(())),
        Err(PilotConsentError::Uncertain { .. })
    ));
    f.fail.store(false, Ordering::SeqCst);
    let f = f.reopen();
    let head = f.ledger.chain_head().unwrap();
    assert!(matches!(
        f.pilot.authorization_outcome(&correlation).unwrap(),
        PilotConsentOutcome::Unknown { .. }
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
}

#[test]
fn same_account_feed_from_another_ledger_is_not_consent_evidence() {
    let (first, now) = setup();
    let (second, _) = setup();
    assert_eq!(first.account, second.account);
    let before = first.ledger.chain_head().unwrap();
    assert!(
        first
            .pilot
            .begin_authorization_review(&first.agent, first.account, second.feed.clone(), &|| now)
            .is_err()
    );
    assert_eq!(first.ledger.chain_head().unwrap(), before);
    assert_eq!(first.consent_count(), 0);
}

#[test]
fn fully_filled_history_and_relevant_redaction_cannot_be_zeroed_by_new_consent() {
    for redacted in [false, true] {
        let (mut f, now) = setup();
        // Historical general-mode submissions are legitimate pre-pilot evidence.
        // The new reviewed consent must refuse, not retrofit a clean baseline.
        f.engine = Arc::new(
            GuardrailEngine::new(f.policy.clone(), f.keys.clone(), f.feed.clone()).unwrap(),
        );
        let receipt = f.historical_order(1, true);
        f.observe_order(&receipt, 1, "filled");
        f.historical_fill(1, 1, true, "0.15", "0", "0.1");
        if redacted {
            let fill = f
                .ledger
                .get_events(0, 100)
                .unwrap()
                .events
                .into_iter()
                .find(|e| e.kind == crate::ledger::EventKind::Fill)
                .unwrap();
            f.ledger
                .redact(fill.seq, "synthetic relevant retention", now as i64)
                .unwrap();
        }
        let head = f.ledger.chain_head().unwrap();
        assert!(
            f.pilot
                .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
                .is_err()
        );
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        assert_eq!(f.consent_count(), 0);
    }
}

#[test]
fn initial_consent_requires_paused_full_policy_caps() {
    for case in 0..4 {
        let (f, now) = setup();
        let current = f.policy.current().unwrap();
        let mut next = current.state;
        let config = next.guardrails.get_mut(&f.agent).unwrap();
        match case {
            0 => config.max_order_usd = Decimal::from(16),
            1 => config.risk.max_open_exposure_usd = None,
            2 => config.risk.max_leverage = 2,
            _ => {
                next.kill.release(&KillScope::Global);
            }
        }
        f.policy.replace(current.revision, next, now).unwrap();
        let head = f.ledger.chain_head().unwrap();
        assert!(
            f.pilot
                .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
                .is_err()
        );
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        assert_eq!(f.consent_count(), 0);
    }
}

#[test]
fn human_delay_accepts_fresh_reads_but_material_account_changes_refuse() {
    for changed in [false, true] {
        let (f, now) = setup();
        let observation = f
            .pilot
            .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
            .unwrap();
        let review = f
            .pilot
            .review_authorization(observation, evidence(&f, now), &|| now)
            .unwrap();
        let later = now + 10_000;
        f.feed.reconciled(&f.feed.stamp(), later);
        let mut fresh = evidence(&f, later);
        if changed {
            fresh.account.perps.margin_summary.total_raw_usd -= Decimal::ONE;
        }
        let result = f
            .pilot
            .confirm_authorization(review, fresh, yes(), &|| later, &|| Ok(()));
        if changed {
            assert!(matches!(result, Err(PilotConsentError::Refused { .. })));
            assert_eq!(f.consent_count(), 0);
        } else {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(f.consent_count(), 1);
        }
    }
}

#[test]
fn consent_resamples_clock_after_write_acquisition_and_before_append() {
    let (f, now) = setup();
    let observation = f
        .pilot
        .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
        .unwrap();
    let review = f
        .pilot
        .review_authorization(observation, evidence(&f, now), &|| now)
        .unwrap();
    let current = Arc::new(AtomicU64::new(now));
    let advance = current.clone();
    let path = f._dir.path().join("activation.db");
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked_tx.send(()).unwrap();
        release_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        advance.store(now + 60_000, Ordering::SeqCst);
        connection.execute_batch("ROLLBACK").unwrap();
    });
    locked_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let head = f.ledger.chain_head().unwrap();
    release_tx.send(()).unwrap();
    let result = f.pilot.confirm_authorization(
        review,
        evidence(&f, now),
        yes(),
        &|| current.load(Ordering::SeqCst),
        &|| Ok(()),
    );
    writer.join().unwrap();
    assert!(
        matches!(result, Err(PilotConsentError::Refused { .. })),
        "{result:?}"
    );
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert_eq!(f.consent_count(), 0);
}

#[test]
fn disconnect_during_anchor_publication_reports_uncertain_without_enabling_orders() {
    let (f, now) = setup();
    let observation = f
        .pilot
        .begin_authorization_review(&f.agent, f.account, f.feed.clone(), &|| now)
        .unwrap();
    let review = f
        .pilot
        .review_authorization(observation, evidence(&f, now), &|| now)
        .unwrap();
    let correlation = review.display().correlation.clone();
    let feed = f.feed.clone();
    f.at_next_audit(move || {
        feed.unreconciled();
    });
    assert!(matches!(
        f.pilot
            .confirm_authorization(review, evidence(&f, now), yes(), &|| now, &|| Ok(())),
        Err(PilotConsentError::Uncertain { .. })
    ));
    assert_eq!(f.consent_count(), 1);
    let head = f.ledger.chain_head().unwrap();
    assert!(matches!(
        f.pilot.authorization_outcome(&correlation).unwrap(),
        PilotConsentOutcome::Committed { .. }
    ));
    assert_eq!(head, f.ledger.chain_head().unwrap());
    assert!(f.engine.policy_status().acknowledgment.is_none());
    assert!(f.policy.current().unwrap().state.kill.global().is_some());
}
