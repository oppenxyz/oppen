use super::*;
use crate::guardrail::{ApprovalReviewDisplay, CancelContext, CancelIntent, CancelTarget};
use oppen_hl::wire::CancelWire;

fn owned_fixture() -> DurableFixture {
    owned_fixture_for(cancellation().targets.remove(0))
}

fn owned_fixture_for(target: CancelTarget) -> DurableFixture {
    std::thread::spawn(move || {
        let f = DurableFixture::new();
        let mut order = approval_order();
        order.is_buy = target.is_buy;
        order.px = target.limit_px;
        order.sz = target.orig_sz;
        order.cloid = target.cloid;
        order.reduce_only = target.reduce_only;
        order.grouping = if target.is_position_tpsl { oppen_hl::wire::Grouping::PositionTpsl } else { oppen_hl::wire::Grouping::Na };
        order.kind = if target.is_trigger { oppen_hl::OrderKind::Trigger {
            is_market: target.order_type.ends_with("Market"), trigger_px: target.trigger_px.unwrap(),
            tpsl: if target.order_type.starts_with("Take Profit") { oppen_hl::wire::Tpsl::Tp } else { oppen_hl::wire::Tpsl::Sl },
        } } else { oppen_hl::OrderKind::Limit { tif: target.tif.unwrap() } };
        let mut state = exposure(d("100000"));
        state.agent.positions.insert("BTC".into(), PositionSnapshot { szi: if target.is_buy { -target.orig_sz } else { target.orig_sz } });
        state.agent.total_position_notional_usd = d("200");
        let mut btc = asset("BTC", 2, 40);
        btc.index = target.asset_index;
        let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
        let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.engine.evaluate(&AgentId::new("alpha"), &order, &btc, &market, &state, NOW_MS) else { panic!("owned fixture order must require real approval"); };
        let cleared = f.engine.operator_approve_proposal(&approval_id, &btc, &market, &state, NOW_MS).unwrap();
        let journal = f.engine.submissions().unwrap();
        let receipt = journal.begin(vault(), cleared.clearance(), journal.state(vault()).unwrap().revision, NOW_MS).unwrap();
        let signed = f.engine.sign_submission_authorized(cleared, &journal, &receipt, NOW_MS, None, || NOW_MS, || Ok(())).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (client, listener) = super::submission_evidence_tests::transport().await;
            let body = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":701}}]}}}"#;
            let server = tokio::spawn(super::submission_evidence_tests::reply(listener, body));
            f.engine.post_submission_authorized(signed, &client, || NOW_MS, || Ok(())).await.unwrap();
            server.await.unwrap();
        });
        f
    }).join().unwrap()
}

fn cancellation() -> CancelIntent {
    CancelIntent {
        reason: "explicitly cancel protective order".into(),
        targets: vec![CancelTarget {
            symbol: "BTC".into(),
            asset_index: 2,
            oid: 701,
            cloid: Some(Cloid::parse("0x00000000000000000000000000000701").unwrap()),
            is_buy: false,
            limit_px: d("94.95"),
            sz: d("1"),
            orig_sz: d("2"),
            timestamp: NOW_MS - 10,
            order_type: "Stop Market".into(),
            tif: None,
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
    let f = owned_fixture();
    let intent = cancellation();
    let p = propose(&f, &intent, NOW_MS);
    let f = f.reopen();
    assert!(f.engine.policy_status().admission_inhibited);
    assert_eq!(
        f.engine.pending_proposals(NOW_MS + 1).unwrap(),
        vec![p.clone()]
    );
    assert_eq!(propose(&f, &intent, NOW_MS + 1), p);
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 2);
    assert!(f.events(EventKind::OrderIntent).len() == 1);
    assert!(
        f.engine.policy_status().admission_inhibited,
        "queue read/retry must not activate orders"
    );
}

#[test]
fn cancellation_terminal_proposal_allows_only_new_explicit_proposal_and_review() {
    for terminal in ["rejected", "expired", "claimed"] {
        let f = owned_fixture();
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
    let f = owned_fixture();
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
    assert_eq!(f.events(EventKind::ApprovalClaimed).len(), 2);
    let disposed = f.events(EventKind::ApprovalDisposed);
    assert_eq!(disposed.len(), 2);
    assert_eq!(
        disposed[1]
            .payload
            .as_ref()
            .unwrap()
            .pointer("/envelope/operation/outcome/intent/seq"),
        Some(&receipt.seq.into())
    );
    assert!(confirm(&f, &p, second, NOW_MS + 1).is_err());
    assert!(f.events(EventKind::OrderIntent).len() == 1);
    assert!(
        f.engine
            .sign_discretionary_cancel_authorized(
                cleared,
                NOW_MS + 1,
                None,
                || NOW_MS + 1,
                || Ok(())
            )
            .is_ok()
    );
    let f = f.reopen();
    assert!(f.engine.pending_proposals(NOW_MS + 1).unwrap().is_empty());
}

#[test]
fn cancellation_snapshot_changes_are_terminal_refusals_never_filtered_actions() {
    for mutation in 0..17 {
        let f = owned_fixture();
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
            7 => fresh.targets[0].sz *= d("2"),
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
        assert_eq!(f.events(EventKind::ApprovalDisposed).len(), 2);
        assert!(f.events(EventKind::AgentDecision).is_empty());
    }
}

#[test]
fn cancellation_snapshot_scoped_rows_do_not_expand_to_new_orders() {
    let f = owned_fixture();
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
        let f = owned_fixture();
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
        let result = f
            .engine
            .sign_discretionary_cancel_authorized(cleared, now, None, || now, || Ok(()))
            .map(|_| ());
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
    let f = owned_fixture();
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
    assert_eq!(
        f.events(EventKind::ApprovalProposed).len(),
        1,
        "only the owned-order setup proposal exists"
    );
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails.get_mut(&agent).unwrap().approval_required = true;
    f.policy.replace(current.revision, next, NOW_MS).unwrap();
    assert!(matches!(
        f.engine
            .sign_discretionary_cancel_authorized(cleared, NOW_MS, None, || NOW_MS, || Ok(())),
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
    let f = owned_fixture();
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
    assert_eq!(f.events(EventKind::ApprovalDisposed).len(), 2);
}

#[test]
fn cancellation_claim_and_disposition_publication_failures_remain_consumed() {
    for offset in [1, 3] {
        let f = owned_fixture();
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
        assert_eq!(f.events(EventKind::ApprovalClaimed).len(), 2);
    }
}

#[test]
fn cancellation_mint_publication_failure_preserves_original_pending_without_extra_audit() {
    let f = owned_fixture();
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
    assert!(f.events(EventKind::Refusal).len() == 1);
    f.fail_seq.store(0, Ordering::SeqCst);
    let original = f.engine.pending_proposals(NOW_MS).unwrap().remove(0);
    assert_eq!(propose(&f, &intent, NOW_MS + 1), original);
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 2);
}

#[test]
fn cancellation_invalid_approval_evidence_never_blocks_runtime_cleanup() {
    let f = owned_fixture();
    let p = propose(&f, &cancellation(), NOW_MS);
    let row = f.events(EventKind::ApprovalProposed).pop().unwrap();
    assert!(
        row.payload
            .as_ref()
            .unwrap()
            .pointer("/envelope/operation/proposal/intent/targets")
            .is_some(),
        "redact the cancellation proposal, not the seeded order proposal"
    );
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
        let mut f = owned_fixture();
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
            crate::feed::test_session(vault()),
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
                    .send(
                        engine
                            .sign_discretionary_cancel_authorized(
                                cleared,
                                at,
                                None,
                                || {
                                    samples.fetch_add(1, Ordering::SeqCst);
                                    clock.load(Ordering::SeqCst)
                                },
                                || Ok(()),
                            )
                            .map(|_| ()),
                    )
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

fn approval_off(f: &DurableFixture) {
    let current = f.policy.current().unwrap();
    let mut state = current.state;
    state
        .guardrails
        .get_mut(&AgentId::new("alpha"))
        .unwrap()
        .approval_required = false;
    f.policy.replace(current.revision, state, NOW_MS).unwrap();
}

#[test]
fn cancellation_unknown_and_changed_signed_identity_never_mint_a_proposal() {
    let unowned = DurableFixture::new();
    let intent = cancellation();
    assert!(matches!(
        unowned.engine.evaluate_cancel(
            &AgentId::new("alpha"),
            &intent,
            &context(&intent, NOW_MS),
            NOW_MS
        ),
        Err(Refusal::Unevaluable(
            Unevaluable::SubmissionAuthority { .. }
        ))
    ));
    assert!(unowned.events(EventKind::ApprovalProposed).is_empty());
    let f = owned_fixture();
    for mutation in 0..12 {
        let mut intent = cancellation();
        let target = &mut intent.targets[0];
        match mutation {
            0 => target.oid += 1,
            1 => target.cloid = None,
            2 => target.symbol = "ETH".into(),
            3 => target.asset_index += 1,
            4 => target.is_buy = true,
            5 => target.limit_px += Decimal::ONE,
            6 => target.orig_sz += Decimal::ONE,
            7 => target.reduce_only = false,
            8 => target.order_type = "Take Profit Market".into(),
            9 => target.trigger_condition = Some("Price above 95".into()),
            10 => target.is_position_tpsl = false,
            _ => target.tif = Some(oppen_hl::wire::Tif::Gtc),
        }
        assert!(
            matches!(
                f.engine.evaluate_cancel(
                    &AgentId::new("alpha"),
                    &intent,
                    &context(&intent, NOW_MS),
                    NOW_MS
                ),
                Err(Refusal::Unevaluable(
                    Unevaluable::SubmissionAuthority { .. }
                ))
            ),
            "mutation {mutation}"
        );
    }
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 1);
    let mut mixed = cancellation();
    let mut manual = mixed.targets[0].clone();
    manual.oid += 1;
    mixed.targets.push(manual);
    assert!(
        f.engine
            .evaluate_cancel(
                &AgentId::new("alpha"),
                &mixed,
                &context(&mixed, NOW_MS),
                NOW_MS
            )
            .is_err()
    );
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 1);
}

#[test]
fn cancellation_limit_requires_observed_matching_tif_and_trigger_condition_is_numeric() {
    let mut target = cancellation().targets.remove(0);
    target.limit_px = d("100");
    target.order_type = "Limit".into();
    target.tif = Some(oppen_hl::wire::Tif::Gtc);
    target.is_trigger = false;
    target.trigger_px = Some(Decimal::ZERO);
    target.trigger_condition = Some("N/A".into());
    target.is_position_tpsl = false;
    let f = owned_fixture_for(target.clone());
    approval_off(&f);
    let mut intent = CancelIntent {
        reason: "cancel owned limit".into(),
        targets: vec![target],
    };
    assert!(
        f.engine
            .evaluate_cancel(
                &AgentId::new("alpha"),
                &intent,
                &context(&intent, NOW_MS),
                NOW_MS
            )
            .is_ok()
    );
    for tif in [
        None,
        Some(oppen_hl::wire::Tif::Ioc),
        Some(oppen_hl::wire::Tif::Alo),
    ] {
        intent.targets[0].tif = tif;
        assert!(matches!(
            f.engine.evaluate_cancel(
                &AgentId::new("alpha"),
                &intent,
                &context(&intent, NOW_MS),
                NOW_MS
            ),
            Err(Refusal::Unevaluable(
                Unevaluable::SubmissionAuthority { .. }
            ))
        ));
    }
    let f = owned_fixture();
    approval_off(&f);
    let mut intent = cancellation();
    intent.targets[0].trigger_condition = Some("Price below 95.0000".into());
    assert!(
        f.engine
            .evaluate_cancel(
                &AgentId::new("alpha"),
                &intent,
                &context(&intent, NOW_MS),
                NOW_MS
            )
            .is_ok()
    );
    for condition in [
        "Price below 95 trailing",
        "Price below 96",
        "Price below +95",
        "Price Below 95",
    ] {
        intent.targets[0].trigger_condition = Some(condition.into());
        assert!(
            f.engine
                .evaluate_cancel(
                    &AgentId::new("alpha"),
                    &intent,
                    &context(&intent, NOW_MS),
                    NOW_MS
                )
                .is_err()
        );
    }
}

#[tokio::test]
async fn cancellation_decreasing_remaining_size_keeps_review_bytes_and_posts_exact_oids() {
    let f = owned_fixture();
    let intent = cancellation();
    let p = propose(&f, &intent, NOW_MS);
    let mut fresh = context(&intent, NOW_MS);
    fresh.targets[0].sz = d("0.75");
    let review = f
        .engine
        .operator_prepare_cancel_proposal(p.id(), &fresh, NOW_MS)
        .unwrap();
    let display = serde_json::to_value(review.display()).unwrap();
    assert_eq!(
        display["targets"],
        serde_json::to_value(&intent.targets).unwrap()
    );
    fresh.targets[0].sz = d("0.5");
    let cleared = f
        .engine
        .operator_confirm_cancel_review(review, &fresh, NOW_MS)
        .unwrap();
    let clearance = serde_json::to_value(cleared.clearance()).unwrap();
    assert_eq!(
        clearance["kind"]["targets"],
        serde_json::to_value(&intent.targets).unwrap()
    );
    let provenance = &clearance["kind"]["provenance"]["links"][0];
    assert_eq!(
        provenance["signed_seq"],
        f.events(EventKind::SubmissionSigned)[0].seq
    );
    assert_eq!(
        provenance["accepted_seq"],
        f.events(EventKind::SubmissionAccepted)[0].seq
    );
    let signed = f
        .engine
        .sign_discretionary_cancel_authorized(cleared, NOW_MS, None, || NOW_MS, || Ok(()))
        .unwrap();
    let (client, listener) = super::submission_evidence_tests::transport().await;
    let server = tokio::spawn(super::submission_evidence_tests::reply(
        listener,
        r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#,
    ));
    f.engine
        .post_cancellation_authorized(signed, &client, || NOW_MS, || Ok(()))
        .await
        .unwrap();
    let posted = server.await.unwrap();
    assert_eq!(
        posted["action"],
        serde_json::json!({"type":"cancel","cancels":[{"a":2,"o":701}]})
    );
    assert_eq!(f.events(EventKind::SubmissionAccepted).len(), 1);
    assert!(f.events(EventKind::SubmissionResolved).is_empty());
    assert!(
        f.engine
            .submissions()
            .unwrap()
            .state(vault())
            .unwrap()
            .pending
            .is_some()
    );
}

#[tokio::test]
async fn cancellation_dispatch_rechecks_policy_route_deadline_provenance_and_owner() {
    for change in [
        "policy",
        "route",
        "deadline",
        "signed_redacted",
        "accepted_redacted",
        "owner",
        "session",
    ] {
        let f = owned_fixture();
        let p = propose(&f, &cancellation(), NOW_MS);
        let cleared = confirm(&f, &p, prepare(&f, &p, NOW_MS), NOW_MS).unwrap();
        let signed = f
            .engine
            .sign_discretionary_cancel_authorized(cleared, NOW_MS, None, || NOW_MS, || Ok(()))
            .unwrap();
        match change {
            "policy" => approval_off(&f),
            "route" => {
                let registry = RegistryJournal::open(f.ledger.clone(), authority_key()).unwrap();
                registry
                    .retire(&registry.route_for_agent(p.agent()).unwrap(), NOW_MS)
                    .unwrap();
            }
            "signed_redacted" | "accepted_redacted" => {
                let kind = if change == "signed_redacted" {
                    EventKind::SubmissionSigned
                } else {
                    EventKind::SubmissionAccepted
                };
                f.ledger
                    .redact(
                        f.events(kind)[0].seq,
                        "synthetic unavailable ownership",
                        NOW_MS as i64,
                    )
                    .unwrap();
            }
            _ => {}
        }
        let another = GuardrailEngine::new(
            f.policy.clone(),
            f.keys.clone(),
            crate::feed::test_session(vault()),
        )
        .unwrap();
        let engine = if change == "owner" {
            &another
        } else {
            &f.engine
        };
        let now = if change == "deadline" {
            p.expires_at_ms()
        } else {
            NOW_MS
        };
        let (client, listener) = super::submission_evidence_tests::transport().await;
        let outcome = engine
            .post_cancellation_authorized(
                signed,
                &client,
                || now,
                || {
                    if change == "session" {
                        Err(Unevaluable::RouteAuthority {
                            detail: "synthetic cancelled session".into(),
                        }
                        .into())
                    } else {
                        Ok(())
                    }
                },
            )
            .await;
        assert!(
            matches!(
                outcome,
                Err(crate::guardrail::SubmissionPostError::NotSent(_))
            ),
            "{change}: {outcome:?}"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        assert_eq!(f.events(EventKind::SubmissionAccepted).len(), 1);
        assert!(f.events(EventKind::SubmissionResolved).is_empty());
    }
}

#[test]
fn cancellation_final_sign_requires_ownership_but_runtime_cleanup_does_not() {
    for kind in [EventKind::SubmissionSigned, EventKind::SubmissionAccepted] {
        let f = owned_fixture();
        let p = propose(&f, &cancellation(), NOW_MS);
        let cleared = confirm(&f, &p, prepare(&f, &p, NOW_MS), NOW_MS).unwrap();
        f.ledger
            .redact(
                f.events(kind)[0].seq,
                "synthetic unavailable evidence",
                NOW_MS as i64,
            )
            .unwrap();
        assert!(matches!(
            f.engine.sign_discretionary_cancel_authorized(
                cleared,
                NOW_MS,
                None,
                || NOW_MS,
                || Ok(())
            ),
            Err(SignClearedError::Refused(Refusal::Unevaluable(
                Unevaluable::SubmissionAuthority { .. }
            )))
        ));
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
}

#[test]
fn cancellation_ownership_does_not_transfer_to_a_new_assignment_of_the_same_agent() {
    let f = owned_fixture();
    let registry = RegistryJournal::open(f.ledger.clone(), authority_key()).unwrap();
    let old = registry.route_for_agent(&AgentId::new("alpha")).unwrap();
    registry.retire(&old, NOW_MS).unwrap();
    assert!(registry.grant(old.binding.clone(), NOW_MS).is_err());
    let wallet = f
        .keys
        .rotate_agent_key(
            &old.binding.agent,
            SecretText::new(format!("{:064x}", 2)),
            NOW_MS + 86_400_000,
            NOW_MS,
        )
        .unwrap();
    let mut binding = old.binding.clone();
    binding.wallet = wallet;
    binding.container = oppen_hl::Address::from_bytes([5; 20]);
    binding.vault_address = Some(binding.container);
    let new = registry.grant(binding, NOW_MS).unwrap();
    assert_ne!(old.binding_seq, new.binding_seq);
    let intent = cancellation();
    let mut fresh = context(&intent, NOW_MS);
    fresh.account = new.binding.container;
    assert!(matches!(
        f.engine
            .evaluate_cancel(&AgentId::new("alpha"), &intent, &fresh, NOW_MS),
        Err(Refusal::Unevaluable(
            Unevaluable::SubmissionAuthority { .. }
        ))
    ));
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 1);
}

#[test]
fn cancellation_finish_refuses_missing_wrong_digest_or_changed_ownership_receipt() {
    use crate::ledger::approval::{ApprovalJournal, ReviewCommitment};
    for tamper in [
        "missing_digest",
        "wrong_digest",
        "missing_provenance",
        "wrong_provenance",
    ] {
        let f = owned_fixture();
        let intent = cancellation();
        let p = propose(&f, &intent, NOW_MS);
        let journal = ApprovalJournal::new(f.policy.clone());
        let head = f.ledger.chain_head().unwrap();
        assert!(journal.claim(p.id(), NOW_MS).is_err());
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        approval_off(&f);
        let evaluated = f
            .engine
            .evaluate_cancel(p.agent(), &intent, &context(&intent, NOW_MS), NOW_MS)
            .unwrap();
        let evidence = journal.prepare(p.id(), NOW_MS).unwrap().unwrap();
        let review = ReviewCommitment::new_cancel(
            &evidence,
            &intent,
            evaluated.action().clone(),
            evaluated.clearance(),
            NOW_MS,
            NOW_MS,
        )
        .unwrap();
        let digest = review.digest().unwrap();
        let claim = journal
            .claim_review(evidence, review, NOW_MS)
            .unwrap()
            .unwrap();
        let cleared = f
            .engine
            .evaluate_cancel(p.agent(), &intent, &context(&intent, NOW_MS), NOW_MS)
            .unwrap();
        let mut payload = serde_json::to_value(cleared.clearance()).unwrap();
        payload["reason"] = intent.reason.clone().into();
        payload["approval_review_digest"] = digest.into();
        match tamper {
            "missing_digest" => {
                payload
                    .as_object_mut()
                    .unwrap()
                    .remove("approval_review_digest");
            }
            "wrong_digest" => payload["approval_review_digest"] = "00".repeat(32).into(),
            "missing_provenance" => {
                payload["kind"]
                    .as_object_mut()
                    .unwrap()
                    .remove("provenance");
            }
            _ => payload["kind"]["provenance"]["links"][0]["accepted_seq"] = 1.into(),
        }
        let receipt = f
            .ledger
            .append(&crate::ledger::NewEvent {
                kind: EventKind::AgentDecision,
                ts_ms: NOW_MS as i64,
                agent_id: Some(p.agent().as_str()),
                payload: &payload,
                snapshot: None,
            })
            .unwrap();
        let head = f.ledger.chain_head().unwrap();
        assert!(
            matches!(journal.finish(claim, &Ok(cleared), Some(&receipt), NOW_MS), Err(crate::ledger::approval::ApprovalError::Unavailable { detail }) if detail.contains("reviewed commitment")),
            "{tamper}"
        );
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        let f = f.reopen();
        assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
    }
}

#[test]
fn cancellation_legacy_raw_signers_refuse_before_key_loading() {
    use std::sync::mpsc;
    for authorized in [false, true] {
        let mut f = owned_fixture();
        approval_off(&f);
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
        let (entered, observed) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        drop(release); // Any attempted key read fails immediately in the fixture.
        f.engine = GuardrailEngine::new(
            f.policy.clone(),
            Arc::new(WaitingKeys {
                inner: f.keys.clone(),
                wait: Some((entered, Mutex::new(wait))),
            }),
            crate::feed::test_session(vault()),
        )
        .unwrap();
        let outcome = if authorized {
            f.engine.sign_cleared_authorized(
                cleared,
                NOW_MS,
                None,
                || NOW_MS,
                || -> Result<(), Refusal> { panic!("legacy caller authority must not be reached") },
            )
        } else {
            f.engine.sign_cleared(cleared, NOW_MS, None, || NOW_MS)
        };
        assert!(matches!(
            outcome,
            Err(SignClearedError::Refused(Refusal::Unevaluable(
                Unevaluable::SubmissionAuthority { .. }
            )))
        ));
        assert!(matches!(
            observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }
}
