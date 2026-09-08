use super::*;
use crate::guardrail::{ApprovalReview, OriginalRequest, RequestedOrderKind};
use oppen_hl::wire::{OrderType, Tif, Tpsl};

fn prepare(f: &DurableFixture, id: &str, reference: Decimal) -> ApprovalReview {
    f.engine
        .operator_prepare_proposal(
            id,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", reference, NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
        .unwrap()
}

fn confirm(f: &DurableFixture, review: ApprovalReview) -> Result<Cleared, Refusal> {
    let reference = review.display().order().unwrap().reference_px;
    f.engine.operator_confirm_review(
        review,
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", reference, NOW_MS),
        &exposure(d("100000")),
        NOW_MS,
    )
}

fn propose_order(f: &DurableFixture, order: &OrderIntent, loaded: &Exposure) -> String {
    match f.engine.evaluate(
        &AgentId::new("alpha"),
        order,
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", d("100"), NOW_MS),
        loaded,
        NOW_MS,
    ) {
        Err(Refusal::ApprovalRequired { approval_id, .. }) => approval_id,
        other => panic!("expected durable proposal: {other:?}"),
    }
}

#[test]
fn review_prepare_is_nonconsuming_preserves_root_ttl_and_does_not_spend_rate_tokens() {
    let f = DurableFixture::new();
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails
        .get_mut(&AgentId::new("alpha"))
        .unwrap()
        .order_rate = OrderRate {
        count: 2,
        per_ms: 60_000,
    };
    f.policy.replace(current.revision, next, NOW_MS).unwrap();
    acknowledge(&f.engine);
    let proposal = f.propose();
    let head = f.ledger.chain_head().unwrap();
    for _ in 0..100 {
        let review = prepare(&f, proposal.id(), d("100"));
        assert_eq!(review.proposal_id(), proposal.id());
        assert_eq!(review.agent(), proposal.agent());
        assert_eq!(review.account(), proposal.account());
        assert_eq!(review.symbol(), Some("BTC"));
        assert_eq!(
            review.display().order().unwrap().expires_at_ms,
            proposal.expires_at_ms()
        );
        assert!(review.display().order().unwrap().original.is_none());
        assert!(review.display().order().unwrap().drift_bps.is_none());
        let display = serde_json::to_value(review.display().order().unwrap()).unwrap();
        assert_eq!(display["px"], "100");
        assert_eq!(display["sz"], "1");
        assert_eq!(display["notional_usd"], "100");
    }
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert_eq!(
        f.engine.pending_proposals(NOW_MS).unwrap(),
        vec![proposal.clone()]
    );
    let mut second = approval_order();
    second.cloid = Some(Cloid::from_bytes([2; 16]));
    assert!(matches!(
        f.evaluate(&second),
        Err(Refusal::ApprovalRequired { .. })
    ));
    let f = f.reopen();
    assert!(f.engine.policy_status().admission_inhibited);
    assert!(
        f.engine
            .operator_prepare_proposal(
                proposal.id(),
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &exposure(d("100000")),
                NOW_MS,
            )
            .is_err(),
        "queue reopen cannot restore activation"
    );
}

#[test]
fn reviewed_confirmation_commits_actual_receipt_once_and_reopens_consumed() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let review = prepare(&f, proposal.id(), d("100"));
    let competing = prepare(&f, proposal.id(), d("100"));
    let display = review.display().order().unwrap().clone();
    let cleared = confirm(&f, review).unwrap();
    assert_eq!(cleared.clearance().route, display.route);
    assert_eq!(cleared.clearance().policy_revision, display.policy_revision);
    assert!(matches!(
        confirm(&f, competing),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
    let claims = f.events(EventKind::ApprovalClaimed);
    let receipts = f.events(EventKind::OrderIntent);
    let disposed = f.events(EventKind::ApprovalDisposed);
    assert_eq!((claims.len(), receipts.len(), disposed.len()), (1, 1, 1));
    let commitment = &claims[0].payload.as_ref().unwrap()["envelope"]["operation"]["review"];
    assert_eq!(commitment["policy"]["hash"], display.policy_hash);
    assert_eq!(
        commitment["action"],
        serde_json::to_value(cleared.action()).unwrap()
    );
    let outcome = &disposed[0].payload.as_ref().unwrap()["envelope"]["operation"]["outcome"];
    assert_eq!(outcome["intent"]["seq"], receipts[0].seq);
    assert_eq!(outcome["intent"]["hash"], receipts[0].hash);
    assert!(
        outcome["review_receipt"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    );
    drop(cleared);
    let f = f.reopen();
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
    assert_eq!(f.events(EventKind::ApprovalClaimed), claims);
    assert_eq!(f.events(EventKind::OrderIntent), receipts);
    assert_eq!(f.events(EventKind::ApprovalDisposed), disposed);
}

#[test]
fn review_reprices_market_but_never_infers_market_from_explicit_ioc_or_absent_original() {
    for source in ["market", "ioc", "unknown"] {
        let f = DurableFixture::new();
        let mut order = approval_order();
        order.px = d("101");
        order.kind = OrderKind::Limit { tif: Tif::Ioc };
        order.original = match source {
            "market" => Some(OriginalRequest {
                kind: RequestedOrderKind::Market {
                    slippage_bps: d("100"),
                },
                reference_px: Some(d("100")),
                reference_at_ms: NOW_MS,
            }),
            "ioc" => Some(OriginalRequest {
                kind: RequestedOrderKind::Limit {
                    limit_px: d("101"),
                    tif: Tif::Ioc,
                },
                reference_px: Some(d("100")),
                reference_at_ms: NOW_MS,
            }),
            _ => None,
        };
        let id = propose_order(&f, &order, &exposure(d("100000")));
        let review = prepare(&f, &id, d("110"));
        let display = review.display().order().unwrap();
        assert_eq!(display.original, order.original);
        assert_eq!(display.original_px, d("101"));
        assert_eq!(
            display.px,
            if source == "market" {
                d("111.1")
            } else {
                d("101")
            }
        );
        assert_eq!(
            display.drift_bps,
            (source != "unknown").then_some(d("1000"))
        );
        assert!(matches!(
            display.order_type,
            OrderType::Limit { tif: Tif::Ioc }
        ));
        assert!(confirm(&f, review).is_ok());
        assert_eq!(
            f.events(EventKind::ApprovalProposed)[0]
                .payload
                .as_ref()
                .unwrap()["envelope"]["operation"]["proposal"]["intent"]["px"],
            "101"
        );
    }
}

#[test]
fn review_keeps_stop_trigger_tpsl_and_bound_price_fixed() {
    let f = DurableFixture::new();
    let mut order = approval_order();
    order.px = d("101");
    order.kind = OrderKind::Trigger {
        is_market: true,
        trigger_px: d("100"),
        tpsl: Tpsl::Sl,
    };
    order.original = Some(OriginalRequest {
        kind: RequestedOrderKind::StopMarket {
            trigger_px: d("100"),
            tpsl: Tpsl::Sl,
            slippage_bps: d("100"),
        },
        reference_px: Some(d("100")),
        reference_at_ms: NOW_MS,
    });
    let id = propose_order(&f, &order, &exposure(d("100000")));
    let review = prepare(&f, &id, d("110"));
    assert_eq!(review.display().order().unwrap().px, d("101"));
    assert!(
        matches!(&review.display().order().unwrap().order_type, OrderType::Trigger { is_market: true, trigger_px, tpsl: Tpsl::Sl } if trigger_px.as_str() == "100")
    );
    assert!(confirm(&f, review).is_ok());
}

#[test]
fn review_close_preserves_signed_size_and_refuses_position_changes_before_claim() {
    for changed_size in [d("0"), d("0.5"), d("2"), d("-1")] {
        let f = DurableFixture::new();
        let mut loaded = exposure(d("100000"));
        loaded
            .agent
            .positions
            .insert("BTC".into(), PositionSnapshot { szi: d("1") });
        loaded.agent.total_position_notional_usd = d("100");
        let mut order = approval_order();
        order.is_buy = false;
        order.reduce_only = true;
        order.kind = OrderKind::Limit { tif: Tif::Ioc };
        order.px = d("99");
        order.original = Some(OriginalRequest {
            kind: RequestedOrderKind::ClosePosition {
                position_size: d("1"),
                slippage_bps: d("100"),
            },
            reference_px: Some(d("100")),
            reference_at_ms: NOW_MS,
        });
        let id = propose_order(&f, &order, &loaded);
        let review = f
            .engine
            .operator_prepare_proposal(
                &id,
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("110"), NOW_MS),
                &loaded,
                NOW_MS,
            )
            .unwrap();
        assert_eq!(review.display().order().unwrap().px, d("108.9"));
        assert_eq!(review.display().order().unwrap().sz, d("1"));
        assert!(!review.display().order().unwrap().is_buy);
        loaded
            .agent
            .positions
            .insert("BTC".into(), PositionSnapshot { szi: changed_size });
        let head = f.ledger.chain_head().unwrap();
        assert!(matches!(
            f.engine.operator_confirm_review(
                review,
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("110"), NOW_MS),
                &loaded,
                NOW_MS
            ),
            Err(Refusal::Unevaluable(
                Unevaluable::ApprovalReviewChanged { .. }
            ))
        ));
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        assert!(matches!(
            f.engine.operator_prepare_proposal(
                &id,
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("110"), NOW_MS),
                &loaded,
                NOW_MS
            ),
            Err(Refusal::Unevaluable(
                Unevaluable::ApprovalReviewChanged { .. }
            ))
        ));
    }
}

#[test]
fn reviewed_fresh_exposure_refusal_is_typed_terminal_and_receipt_bound() {
    let f = DurableFixture::new();
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails
        .get_mut(&AgentId::new("alpha"))
        .unwrap()
        .max_position_usd = d("200");
    f.policy.replace(current.revision, next, NOW_MS).unwrap();
    acknowledge(&f.engine);
    let proposal = f.propose();
    let review = prepare(&f, proposal.id(), d("100"));
    let mut loaded = exposure(d("100000"));
    loaded
        .agent
        .positions
        .insert("BTC".into(), PositionSnapshot { szi: d("2") });
    loaded.agent.total_position_notional_usd = d("200");
    let result = f.engine.operator_confirm_review(
        review,
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", d("100"), NOW_MS),
        &loaded,
        NOW_MS,
    );
    assert!(
        matches!(result, Err(Refusal::PositionNotional { .. })),
        "{result:?}"
    );
    assert_consumed(&f, proposal.id());
    let events = f.events(EventKind::ApprovalDisposed);
    let outcome = &events[0].payload.as_ref().unwrap()["envelope"]["operation"]["outcome"];
    assert_eq!(outcome["disposition"], "refused");
    assert!(outcome["review_receipt"].is_string());
    assert!(f.events(EventKind::OrderIntent).is_empty());
    assert!(
        f.reopen()
            .engine
            .pending_proposals(NOW_MS)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reviewed_policy_change_does_not_publish_a_clearance() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let review = prepare(&f, proposal.id(), d("100"));
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    // A real change which still permits the order isolates the reviewed-policy
    // gate from ordinary order-limit refusal. Identical replacements are no-ops.
    next.guardrails
        .get_mut(&AgentId::new("alpha"))
        .unwrap()
        .max_order_usd = d("999999");
    let published = f.policy.replace(current.revision, next, NOW_MS).unwrap();
    assert_ne!(
        published.revision,
        review.display().order().unwrap().policy_revision
    );
    assert_ne!(
        f.ledger.event(published.revision).unwrap().unwrap().hash,
        review.display().order().unwrap().policy_hash
    );
    acknowledge(&f.engine);
    let head = f.ledger.chain_head().unwrap();
    let result = confirm(&f, review);
    assert!(result.is_err());
    assert!(matches!(
        result,
        Err(Refusal::Unevaluable(Unevaluable::ApprovalAuthority { .. }))
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert!(f.events(EventKind::ApprovalClaimed).is_empty());
    assert!(f.events(EventKind::OrderIntent).is_empty());
    assert_eq!(f.engine.pending_proposals(NOW_MS).unwrap(), vec![proposal]);
}

#[test]
fn reviewed_quote_changes_allow_same_action_but_refuse_material_market_repricing() {
    for (market_order, reference, allowed) in [
        (false, d("110"), true),
        (true, d("100.000001"), true),
        (true, d("110"), false),
    ] {
        let f = DurableFixture::new();
        let mut order = approval_order();
        order.kind = OrderKind::Limit { tif: Tif::Ioc };
        order.px = d("101");
        order.original = Some(OriginalRequest {
            kind: if market_order {
                RequestedOrderKind::Market {
                    slippage_bps: d("100"),
                }
            } else {
                RequestedOrderKind::Limit {
                    limit_px: d("101"),
                    tif: Tif::Ioc,
                }
            },
            reference_px: Some(d("100")),
            reference_at_ms: NOW_MS,
        });
        let id = propose_order(&f, &order, &exposure(d("100000")));
        let review = prepare(&f, &id, d("100"));
        let result = f.engine.operator_confirm_review(
            review,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", reference, NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        );
        if allowed {
            let cleared = result.unwrap();
            assert!(
                matches!(cleared.clearance().kind, ClearedKind::Order { px, reference_px, .. } if px == d("101") && reference_px == reference)
            );
            let claims = f.events(EventKind::ApprovalClaimed);
            assert_eq!(
                claims[0].payload.as_ref().unwrap()["envelope"]["operation"]["review"]["reference_px"],
                "100"
            );
            let receipts = f.events(EventKind::OrderIntent);
            assert_eq!(
                receipts[0].payload.as_ref().unwrap()["kind"]["reference_px"],
                serde_json::to_value(reference).unwrap()
            );
            assert!(
                f.engine.pending_proposals(NOW_MS).unwrap().is_empty(),
                "replay must bind actual receipt without overwriting displayed reference"
            );
        } else {
            assert!(matches!(
                result,
                Err(Refusal::Unevaluable(
                    Unevaluable::ApprovalReviewChanged { .. }
                ))
            ));
            assert!(f.events(EventKind::ApprovalClaimed).is_empty());
            assert!(f.events(EventKind::OrderIntent).is_empty());
        }
    }
}

#[test]
fn reviewed_claim_or_disposition_publication_failure_cannot_reissue_clearance() {
    for offset in [1, 3] {
        let f = DurableFixture::new();
        let proposal = f.propose();
        let review = prepare(&f, proposal.id(), d("100"));
        let head = f.ledger.chain_head().unwrap();
        f.fail_seq.store(head.seq + offset, Ordering::SeqCst);
        assert_approval_authority(confirm(&f, review));
        assert_eq!(f.ledger.chain_head().unwrap().seq, head.seq + offset);
        f.fail_seq.store(0, Ordering::SeqCst);
        let f = f.reopen();
        assert_consumed(&f, proposal.id());
    }
}

#[test]
fn reviewed_deadline_remains_exclusive_at_confirmation_and_final_signing() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let review = prepare(&f, proposal.id(), d("100"));
    let expiry = proposal.expires_at_ms();
    assert!(matches!(
        f.engine.operator_confirm_review(
            review,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), expiry),
            &exposure(d("100000")),
            expiry
        ),
        Err(Refusal::Unevaluable(Unevaluable::ApprovalExpired { .. }))
    ));
    let mut loaded = exposure(d("100000"));
    loaded.agent.as_of_ms = expiry - 1;
    let market = MarketRef::fresh("BTC", d("100"), expiry - 1);
    let review = f
        .engine
        .operator_prepare_proposal(
            proposal.id(),
            &asset("BTC", 2, 40),
            &market,
            &loaded,
            expiry - 1,
        )
        .unwrap();
    let cleared = f
        .engine
        .operator_confirm_review(review, &asset("BTC", 2, 40), &market, &loaded, expiry - 1)
        .unwrap();
    let result = f
        .engine
        .sign_cleared_authorized(cleared, expiry, None, || expiry, || Ok(()));
    assert!(
        matches!(result, Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::ApprovalExpired { expires_at_ms, now_ms }))) if expires_at_ms == expiry && now_ms == expiry),
        "{result:?}"
    );
    assert!(f.engine.pending_proposals(expiry).unwrap().is_empty());
    assert!(matches!(
        f.engine.operator_approve_proposal(
            proposal.id(),
            &asset("BTC", 2, 40),
            &market,
            &loaded,
            expiry
        ),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
}

#[test]
fn reviewed_route_retirement_is_terminal_and_never_retargets() {
    for regrant in [false, true] {
        let f = DurableFixture::new();
        let proposal = f.propose();
        let review = prepare(&f, proposal.id(), d("100"));
        let registry = RegistryJournal::open(f.ledger.clone(), authority_key()).unwrap();
        let route = registry.route_for_agent(proposal.agent()).unwrap();
        registry.retire(&route, NOW_MS).unwrap();
        if regrant {
            f.keys
                .rotate_agent_key(
                    proposal.agent(),
                    SecretText::new(format!("{:064x}", 2)),
                    NOW_MS + 90 * 86_400_000,
                    NOW_MS,
                )
                .unwrap();
            let account = oppen_hl::Address::from_bytes([2; 20]);
            registry
                .grant(
                    route_for(f.keys.as_ref(), "alpha", account, Some(account)).binding,
                    NOW_MS,
                )
                .unwrap();
        }
        let result = confirm(&f, review);
        assert!(
            matches!(
                result,
                Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
            ),
            "{result:?}"
        );
        assert_consumed(&f, proposal.id());
        assert!(f.events(EventKind::OrderIntent).is_empty());
        assert_eq!(f.events(EventKind::ApprovalDisposed).len(), 1);
    }
}

#[test]
fn reviewed_rejection_wins_and_confirmation_cannot_resurrect_it() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let review = prepare(&f, proposal.id(), d("100"));
    assert!(
        f.engine
            .operator_reject_proposal(proposal.id(), NOW_MS)
            .unwrap()
    );
    assert!(matches!(
        confirm(&f, review),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
    assert!(f.events(EventKind::ApprovalClaimed).is_empty());
    assert_eq!(f.events(EventKind::ApprovalDisposed).len(), 1);
    assert!(f.events(EventKind::OrderIntent).is_empty());
}

#[test]
fn review_price_and_drift_overflow_refuse_without_claim_or_append() {
    for market_order in [false, true] {
        let f = DurableFixture::new();
        let mut order = approval_order();
        if market_order {
            order.kind = OrderKind::Limit { tif: Tif::Ioc };
            order.px = d("101");
            order.original = Some(OriginalRequest {
                kind: RequestedOrderKind::Market {
                    slippage_bps: d("100"),
                },
                reference_px: Some(d("100")),
                reference_at_ms: NOW_MS,
            });
        } else {
            order.original = Some(OriginalRequest {
                kind: RequestedOrderKind::Limit {
                    limit_px: order.px,
                    tif: Tif::Gtc,
                },
                reference_px: Some(Decimal::new(1, 28)),
                reference_at_ms: NOW_MS,
            });
        }
        let id = propose_order(&f, &order, &exposure(d("100000")));
        let head = f.ledger.chain_head().unwrap();
        let reference = if market_order { Decimal::MAX } else { d("100") };
        let result = f.engine.operator_prepare_proposal(
            &id,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", reference, NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        );
        assert!(
            matches!(
                result,
                Err(Refusal::Unevaluable(Unevaluable::ArithmeticOverflow { .. }))
            ),
            "{result:?}"
        );
        assert_eq!(f.ledger.chain_head().unwrap(), head);
    }
}

#[test]
fn authorized_signing_refusal_is_audited_and_consumes_the_clearance() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let cleared = confirm(&f, prepare(&f, proposal.id(), d("100"))).unwrap();
    let before = f.events(EventKind::Refusal).len();
    let expected = Refusal::Unevaluable(Unevaluable::RouteAuthority {
        detail: "synthetic pairing revoked after key load".into(),
    });
    let result = f.engine.sign_cleared_authorized(
        cleared,
        NOW_MS,
        None,
        || NOW_MS,
        || Err::<(), _>(expected.clone()),
    );
    assert!(matches!(result, Err(SignClearedError::Refused(ref refusal)) if refusal == &expected));
    let refusals = f.events(EventKind::Refusal);
    assert_eq!(refusals.len(), before + 1);
    assert_eq!(
        refusals.last().unwrap().payload.as_ref().unwrap()["refusal_detail"],
        serde_json::to_value(expected).unwrap()
    );
    assert_consumed(&f, proposal.id());
}
