use super::*;
use crate::guardrail::{ApprovalReviewDisplay, CancelContext, CancelIntent, CancelTarget};
use oppen_hl::wire::CancelWire;

fn cancellation() -> CancelIntent {
    CancelIntent {
        reason: "explicitly cancel protective order".into(),
        targets: vec![CancelTarget {
            symbol: "BTC".into(),
            asset_index: 2,
            oid: 701,
            cloid: Some(Cloid::parse("0x00000000000000000000000000000701").unwrap()),
            is_buy: false,
            limit_px: d("90"),
            sz: d("1"),
            orig_sz: d("2"),
            timestamp: NOW_MS - 10,
            order_type: "Stop Market".into(),
            reduce_only: true,
            is_trigger: true,
            trigger_px: Some(d("95")),
            trigger_condition: Some("Price below 95".into()),
            is_position_tpsl: true,
        }],
    }
}

fn context(intent: &CancelIntent, at_ms: u64) -> CancelContext {
    CancelContext {
        account: vault(),
        observed_at_ms: at_ms,
        targets: intent.targets.clone(),
    }
}

fn propose(f: &DurableFixture, intent: &CancelIntent, at_ms: u64) -> Proposal {
    let result = f.engine.evaluate_cancel(
        &AgentId::new("alpha"),
        intent,
        &context(intent, at_ms),
        at_ms,
    );
    let Err(Refusal::CancellationApprovalRequired {
        approval_id,
        expires_at_ms,
        targets,
    }) = result
    else {
        panic!("expected typed cancellation proposal: {result:?}");
    };
    assert_eq!(targets, intent.targets);
    let proposal = f
        .engine
        .pending_proposals(at_ms)
        .unwrap()
        .into_iter()
        .find(|p| p.id() == approval_id)
        .unwrap();
    assert_eq!(proposal.expires_at_ms(), expires_at_ms);
    assert_eq!(proposal.cancel_intent(), Some(intent));
    assert!(proposal.order_intent().is_none());
    proposal
}

fn prepare(f: &DurableFixture, p: &Proposal, at_ms: u64) -> ApprovalReview {
    f.engine
        .operator_prepare_cancel_proposal(
            p.id(),
            &context(p.cancel_intent().unwrap(), at_ms),
            at_ms,
        )
        .unwrap()
}

fn confirm(
    f: &DurableFixture,
    p: &Proposal,
    review: ApprovalReview,
    at_ms: u64,
) -> Result<Cleared, Refusal> {
    f.engine.operator_confirm_cancel_review(
        review,
        &context(p.cancel_intent().unwrap(), at_ms),
        at_ms,
    )
}

#[test]
fn cancellation_pending_reopens_with_same_targets_id_and_original_ttl() {
    let f = DurableFixture::new();
    let intent = cancellation();
    let p = propose(&f, &intent, NOW_MS);
    let f = f.reopen();
    assert!(f.engine.policy_status().admission_inhibited);
    assert_eq!(
        f.engine.pending_proposals(NOW_MS + 1).unwrap(),
        vec![p.clone()]
    );
    assert_eq!(propose(&f, &intent, NOW_MS + 1), p);
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 1);
    assert!(f.events(EventKind::OrderIntent).is_empty());
    assert!(
        f.engine.policy_status().admission_inhibited,
        "queue read/retry must not activate orders"
    );
}

#[test]
fn cancellation_terminal_proposal_allows_only_new_explicit_proposal_and_review() {
    for terminal in ["rejected", "expired", "claimed"] {
        let f = DurableFixture::new();
        let intent = cancellation();
        let old = propose(&f, &intent, NOW_MS);
        let stale_review = prepare(&f, &old, NOW_MS);
        let at = match terminal {
            "rejected" => {
                assert!(f.engine.operator_reject_proposal(old.id(), NOW_MS).unwrap());
                NOW_MS
            }
            "expired" => old.expires_at_ms(),
            _ => {
                let review = prepare(&f, &old, NOW_MS);
                let head = f.ledger.chain_head().unwrap();
                f.fail_seq.store(head.seq + 1, Ordering::SeqCst);
                assert_approval_authority(confirm(&f, &old, review, NOW_MS));
                f.fail_seq.store(0, Ordering::SeqCst);
                NOW_MS
            }
        };
        let new = propose(&f, &intent, at);
        assert_ne!(new.id(), old.id());
        assert_eq!(new.expires_at_ms(), at + APPROVAL_TTL_MS);
        assert!(confirm(&f, &old, stale_review, at).is_err());
        assert_eq!(f.engine.pending_proposals(at).unwrap(), vec![new.clone()]);
        let review = prepare(&f, &new, at);
        assert!(confirm(&f, &new, review, at).is_ok());
        assert!(f.engine.pending_proposals(at).unwrap().is_empty());
    }
}

#[test]
fn cancellation_review_is_read_only_and_receipt_binds_exact_action() {
    let f = DurableFixture::new();
    let intent = cancellation();
    let p = propose(&f, &intent, NOW_MS);
    let head = f.ledger.chain_head().unwrap();
    let review = prepare(&f, &p, NOW_MS);
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert_eq!(f.engine.pending_proposals(NOW_MS).unwrap(), vec![p.clone()]);
    assert_eq!(review.symbol(), None);
    let ApprovalReviewDisplay::Cancel(display) = review.display() else {
        panic!("typed cancellation display")
    };
    assert_eq!(display.targets, intent.targets);
    let json = serde_json::to_value(review.display()).unwrap();
    assert_eq!(json["kind"], "cancel");
    assert!(json.get("notional_usd").is_none());
    let second = prepare(&f, &p, NOW_MS);
    let cleared = confirm(&f, &p, review, NOW_MS + 1).unwrap();
    assert_eq!(
        cleared.action(),
        &oppen_hl::Action::Cancel {
            cancels: vec![CancelWire { a: 2, o: 701 }]
        }
    );
    let receipt = f
        .events(EventKind::AgentDecision)
        .into_iter()
        .find(|row| {
            row.payload
                .as_ref()
                .unwrap()
                .pointer("/kind/cleared")
                .and_then(|v| v.as_str())
                == Some("discretionary_cancel")
        })
        .unwrap();
    assert_eq!(
        receipt.payload.as_ref().unwrap()["approval_review_digest"],
        serde_json::to_value(cleared.clearance().approval_review_digest.as_ref().unwrap()).unwrap()
    );
    assert_eq!(
        receipt.payload.as_ref().unwrap()["kind"]["targets"],
        serde_json::to_value(&intent.targets).unwrap()
    );
    assert_eq!(f.events(EventKind::ApprovalClaimed).len(), 1);
    let disposed = f.events(EventKind::ApprovalDisposed);
    assert_eq!(disposed.len(), 1);
    assert_eq!(
        disposed[0]
            .payload
            .as_ref()
            .unwrap()
            .pointer("/envelope/operation/outcome/intent/seq"),
        Some(&receipt.seq.into())
    );
    assert!(confirm(&f, &p, second, NOW_MS + 1).is_err());
    assert!(f.events(EventKind::OrderIntent).is_empty());
    assert!(
        f.engine
            .sign_cleared(cleared, NOW_MS + 1, None, || NOW_MS + 1)
            .is_ok()
    );
    let f = f.reopen();
    assert!(f.engine.pending_proposals(NOW_MS + 1).unwrap().is_empty());
}

#[test]
fn cancellation_snapshot_changes_are_terminal_refusals_never_filtered_actions() {
    for mutation in 0..17 {
        let f = DurableFixture::new();
        let p = propose(&f, &cancellation(), NOW_MS);
        let review = prepare(&f, &p, NOW_MS);
        let mut fresh = context(p.cancel_intent().unwrap(), NOW_MS);
        match mutation {
            0 => fresh.targets.clear(),
            1 => fresh.targets[0].symbol = "ETH".into(),
            2 => fresh.targets[0].asset_index += 1,
            3 => fresh.targets[0].oid += 1,
            4 => fresh.targets[0].cloid = None,
            5 => fresh.targets[0].is_buy = true,
            6 => fresh.targets[0].limit_px += Decimal::ONE,
            7 => fresh.targets[0].sz /= d("2"),
            8 => fresh.targets[0].orig_sz += Decimal::ONE,
            9 => fresh.targets[0].timestamp += 1,
            10 => fresh.targets[0].order_type = "Limit".into(),
            11 => fresh.targets[0].reduce_only = false,
            12 => fresh.targets[0].is_trigger = false,
            13 => fresh.targets[0].trigger_px = None,
            14 => fresh.targets[0].trigger_condition = None,
            15 => fresh.targets[0].is_position_tpsl = false,
            _ => fresh.targets.push(fresh.targets[0].clone()),
        }
        let result = f
            .engine
            .operator_confirm_cancel_review(review, &fresh, NOW_MS);
        assert!(
            matches!(
                result,
                Err(Refusal::Unevaluable(
                    Unevaluable::ApprovalReviewChanged { .. }
                ))
            ),
            "{mutation}: {result:?}"
        );
        assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
        assert_eq!(f.events(EventKind::ApprovalDisposed).len(), 1);
        assert!(f.events(EventKind::AgentDecision).is_empty());
    }
}

#[test]
fn cancellation_snapshot_scoped_rows_do_not_expand_to_new_orders() {
    let f = DurableFixture::new();
    let p = propose(&f, &cancellation(), NOW_MS);
    let review = prepare(&f, &p, NOW_MS);
    let mut fresh = context(p.cancel_intent().unwrap(), NOW_MS);
    let mut other = fresh.targets[0].clone();
    other.oid += 1;
    other.symbol = "UNSUPPORTED".into();
    fresh.targets.push(other);
    let cleared = f
        .engine
        .operator_confirm_cancel_review(review, &fresh, NOW_MS)
        .unwrap();
    assert_eq!(
        cleared.action(),
        &oppen_hl::Action::Cancel {
            cancels: vec![CancelWire { a: 2, o: 701 }]
        }
    );
}

#[test]
fn cancellation_snapshot_freshness_and_clock_rollback_are_checked_at_final_sign() {
    for (delta, backwards) in [(0, true), (60_001, false)] {
        let f = DurableFixture::new();
        let current = f.policy.current().unwrap();
        let mut next = current.state;
        let config = next.guardrails.get_mut(&AgentId::new("alpha")).unwrap();
        config.approval_required = false;
        config.freshness.max_account_age_ms = 60_000;
        f.policy.replace(current.revision, next, NOW_MS).unwrap();
        let intent = cancellation();
        let cleared = f
            .engine
            .evaluate_cancel(
                &AgentId::new("alpha"),
                &intent,
                &context(&intent, NOW_MS),
                NOW_MS,
            )
            .unwrap();
        let now = if backwards {
            NOW_MS - 1
        } else {
            NOW_MS + delta
        };
        let result = f.engine.sign_cleared(cleared, now, None, || now);
        assert!(
            matches!(
                result,
                Err(SignClearedError::Refused(Refusal::Unevaluable(
                    Unevaluable::ClockWentBackwards { .. } | Unevaluable::StaleClearance { .. }
                )))
            ),
            "{result:?}"
        );
    }
}

#[test]
fn cancellation_approval_off_to_on_policy_change_blocks_final_signature() {
    let f = DurableFixture::new();
    let agent = AgentId::new("alpha");
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails.get_mut(&agent).unwrap().approval_required = false;
    f.policy.replace(current.revision, next, NOW_MS).unwrap();
    let intent = cancellation();
    let cleared = f
        .engine
        .evaluate_cancel(&agent, &intent, &context(&intent, NOW_MS), NOW_MS)
        .unwrap();
    assert!(cleared.clearance().policy_revision > 0);
    assert!(f.events(EventKind::ApprovalProposed).is_empty());
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails.get_mut(&agent).unwrap().approval_required = true;
    f.policy.replace(current.revision, next, NOW_MS).unwrap();
    assert!(matches!(
        f.engine.sign_cleared(cleared, NOW_MS, None, || NOW_MS),
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::PolicyChanged
        )))
    ));
    let cleanup = f
        .engine
        .clear_cancel(
            &agent,
            vec![CancelWire { a: 2, o: 701 }],
            "runtime cleanup",
            NOW_MS,
        )
        .unwrap();
    assert!(
        f.engine
            .sign_cleared(cleanup, NOW_MS, None, || NOW_MS)
            .is_ok()
    );
}

#[test]
fn cancellation_review_retirement_cannot_retarget_and_refusal_is_terminal() {
    let f = DurableFixture::new();
    let p = propose(&f, &cancellation(), NOW_MS);
    let review = prepare(&f, &p, NOW_MS);
    let registry = RegistryJournal::open(f.ledger.clone(), authority_key()).unwrap();
    let route = registry.route_for_agent(p.agent()).unwrap();
    registry.retire(&route, NOW_MS).unwrap();
    let result = confirm(&f, &p, review, NOW_MS);
    assert!(
        matches!(
            result,
            Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
        ),
        "{result:?}"
    );
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
    assert_eq!(f.events(EventKind::ApprovalDisposed).len(), 1);
}

#[test]
fn cancellation_claim_and_disposition_publication_failures_remain_consumed() {
    for offset in [1, 3] {
        let f = DurableFixture::new();
        let p = propose(&f, &cancellation(), NOW_MS);
        let review = prepare(&f, &p, NOW_MS);
        let head = f.ledger.chain_head().unwrap();
        f.fail_seq.store(head.seq + offset, Ordering::SeqCst);
        assert_approval_authority(confirm(&f, &p, review, NOW_MS));
        assert_eq!(f.ledger.chain_head().unwrap().seq, head.seq + offset);
        f.fail_seq.store(0, Ordering::SeqCst);
        let f = f.reopen();
        assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
        assert!(
            f.engine
                .operator_prepare_cancel_proposal(p.id(), &context(&cancellation(), NOW_MS), NOW_MS)
                .is_err()
        );
        assert_eq!(f.events(EventKind::ApprovalClaimed).len(), 1);
    }
}

#[test]
fn cancellation_mint_publication_failure_preserves_original_pending_without_extra_audit() {
    let f = DurableFixture::new();
    let intent = cancellation();
    let head = f.ledger.chain_head().unwrap();
    f.fail_seq.store(head.seq + 1, Ordering::SeqCst);
    assert_approval_authority(f.engine.evaluate_cancel(
        &AgentId::new("alpha"),
        &intent,
        &context(&intent, NOW_MS),
        NOW_MS,
    ));
    assert_eq!(f.ledger.chain_head().unwrap().seq, head.seq + 1);
    assert!(f.events(EventKind::Refusal).is_empty());
    f.fail_seq.store(0, Ordering::SeqCst);
    let original = f.engine.pending_proposals(NOW_MS).unwrap().remove(0);
    assert_eq!(propose(&f, &intent, NOW_MS + 1), original);
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 1);
}

#[test]
fn cancellation_invalid_approval_evidence_never_blocks_runtime_cleanup() {
    let f = DurableFixture::new();
    let p = propose(&f, &cancellation(), NOW_MS);
    let row = f.events(EventKind::ApprovalProposed).remove(0);
    f.ledger
        .redact(
            row.seq,
            "synthetic unavailable approval evidence",
            NOW_MS as i64,
        )
        .unwrap();
    assert!(f.engine.pending_proposals(NOW_MS).is_err());
    assert!(
        f.engine
            .operator_prepare_cancel_proposal(p.id(), &context(&cancellation(), NOW_MS), NOW_MS)
            .is_err()
    );
    let cleanup = f
        .engine
        .clear_cancel(
            p.agent(),
            vec![CancelWire { a: 2, o: 701 }],
            "runtime cleanup",
            NOW_MS,
        )
        .unwrap();
    assert!(
        f.engine
            .sign_cleared(cleanup, NOW_MS, None, || NOW_MS)
            .is_ok()
    );
}

#[test]
fn cancellation_review_deadline_is_checked_after_keys_and_authority_waits() {
    use std::sync::mpsc;
    use std::time::Duration;
    for authority_wait in [false, true] {
        let mut f = DurableFixture::new();
        let p = propose(&f, &cancellation(), NOW_MS);
        let expires = p.expires_at_ms();
        let at = expires - 1;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        f.engine = GuardrailEngine::new(
            f.policy.clone(),
            Arc::new(WaitingKeys {
                inner: f.keys.clone(),
                wait: Some((entered_tx, Mutex::new(release_rx))),
            }),
        )
        .unwrap();
        let cleared = confirm(&f, &p, prepare(&f, &p, at), at).unwrap();
        let clock = AtomicU64::new(at);
        let samples = AtomicUsize::new(0);
        let result = std::thread::scope(|scope| {
            let coordination = authority_wait.then(|| {
                let file = std::fs::File::options()
                    .read(true)
                    .write(true)
                    .open(f.dir.path().join("approval.db.lock"))
                    .unwrap();
                file.lock().unwrap();
                file
            });
            let (done_tx, done_rx) = mpsc::channel();
            let engine = &f.engine;
            let clock = &clock;
            let samples = &samples;
            scope.spawn(move || {
                done_tx
                    .send(engine.sign_cleared(cleared, at, None, || {
                        samples.fetch_add(1, Ordering::SeqCst);
                        clock.load(Ordering::SeqCst)
                    }))
                    .unwrap();
            });
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if authority_wait {
                release_tx.send(()).unwrap();
            }
            let waiting = done_rx.recv_timeout(Duration::from_millis(50));
            let sampled = samples.load(Ordering::SeqCst);
            clock.store(expires, Ordering::SeqCst);
            drop(coordination);
            if !authority_wait {
                release_tx.send(()).unwrap();
            }
            assert!(matches!(waiting, Err(mpsc::RecvTimeoutError::Timeout)));
            assert_eq!(sampled, 0);
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap()
        });
        assert!(
            matches!(result, Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::ApprovalExpired { expires_at_ms, now_ms }))) if expires_at_ms == expires && now_ms == expires),
            "{result:?}"
        );
        assert_eq!(samples.load(Ordering::SeqCst), 1);
        assert!(f.engine.pending_proposals(expires).unwrap().is_empty());
    }
}
