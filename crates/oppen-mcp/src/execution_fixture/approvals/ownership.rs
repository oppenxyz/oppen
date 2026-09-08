//! ES31c: venue observation alone is not cancellation ownership.

use super::*;

async fn place_owned(runtime: &Runtime, byte: u8) -> u64 {
    let reply = runtime
        .call(
            "place",
            place(Cloid::from_bytes([byte; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(reply["status"], "resting", "{reply}");
    assert_eq!(count(runtime, EventKind::SubmissionAccepted), 1);
    runtime.reconcile().await;
    reply["oid"].as_u64().unwrap()
}

fn ownership_refusal(reply: &Value) {
    assert_eq!(reply["status"], "rejected", "{reply}");
    assert_eq!(
        reply["refusal"]["unevaluable"], "submission_authority",
        "{reply}"
    );
}

#[tokio::test]
async fn lost_response_order_remains_unowned_after_reopen_in_both_approval_modes() {
    for approval in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        let cloid = Cloid::from_bytes([175; 16]);
        venue.next_response(Behavior::AppliedDropped);
        let reply = runtime.call("place", place(cloid.as_str(), "0.12")).await;
        assert_eq!(
            reply["protocol_error"]["data"]["code"], "timeout_unknown_outcome",
            "{reply}"
        );
        assert_eq!(count(&runtime, EventKind::SubmissionSigned), 1);
        assert_eq!(count(&runtime, EventKind::SubmissionAccepted), 0);
        runtime.shutdown().await;
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        if approval {
            enable_approval(&runtime).await;
        }
        assert_eq!(
            runtime
                .call("get_order_status", json!({"cloid":cloid.as_str()}))
                .await["status"],
            "open"
        );
        let reads = keys.read_heads.lock().unwrap().len();
        ownership_refusal(
            &runtime
                .call(
                    "cancel",
                    json!({"cloid":cloid.as_str(),"reason":"observation is not acceptance"}),
                )
                .await,
        );
        assert_eq!(venue.submissions().len(), 1);
        assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
        assert_eq!(count(&runtime, EventKind::SubmissionAccepted), 0);
        assert_eq!(count(&runtime, EventKind::ApprovalProposed), 0);
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn authenticated_trigger_cancels_in_both_approval_modes() {
    for approval in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let runtime =
            Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
        runtime.activate_orders().await;
        let reply = runtime
            .call(
                "place",
                json!({"symbol":"TEST","is_buy":true,"size":"0.12",
            "order_type":"stop_market","trigger_px":"105","tpsl":"sl","reason":"owned trigger",
            "cloid":Cloid::from_bytes([176;16]).as_str()}),
            )
            .await;
        assert_eq!(reply["status"], "resting", "{reply}");
        assert_eq!(count(&runtime, EventKind::SubmissionAccepted), 1);
        runtime.reconcile().await;
        if approval {
            enable_approval(&runtime).await;
        }
        let result = runtime
            .call(
                "cancel",
                json!({"oid":reply["oid"],"reason":"cancel owned trigger"}),
            )
            .await;
        if approval {
            assert_eq!(result["status"], "pending_approval", "{result}");
            let serving = Serving::start(&runtime).await;
            let review = serving
                .control
                .prepare(&binding(&runtime), result["approval_id"].as_str().unwrap())
                .await
                .unwrap();
            assert_eq!(
                review.display().cancel().unwrap().targets[0]
                    .trigger_condition
                    .as_deref(),
                Some("Price above 105")
            );
            assert_eq!(
                serving.control.confirm(review).await.unwrap()["canceled"],
                1
            );
            serving.finish().await;
        } else {
            assert_eq!(result["canceled"], 1, "{result}");
        }
        assert_eq!(venue.submissions().len(), 2);
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn acceptance_redacted_during_cancel_key_loading_refuses_at_final_signing() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let oid = place_owned(&runtime, 177).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let result = runtime
        .call(
            "cancel",
            json!({"oid":oid,"reason":"final ownership recheck"}),
        )
        .await;
    assert_eq!(result["status"], "pending_approval", "{result}");
    let review = serving
        .control
        .prepare(&binding(&runtime), result["approval_id"].as_str().unwrap())
        .await
        .unwrap();
    let accepted = runtime
        .ledger
        .get_events(0, 1000)
        .unwrap()
        .events
        .into_iter()
        .find(|event| event.kind == EventKind::SubmissionAccepted)
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    *keys.read_gate.lock().unwrap() = Some(KeyReadGate {
        entered: entered.clone(),
        release: wait,
    });
    let control = serving.control.clone();
    let confirm = tokio::spawn(async move { control.confirm(review).await });
    timeout(Duration::from_secs(3), entered.notified())
        .await
        .expect("cancel never reached key loading");
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
    runtime
        .ledger
        .redact(
            accepted.seq,
            "synthetic final-sign retention",
            now_ms() as i64,
        )
        .unwrap();
    release.send(()).unwrap();
    let reply = timeout(Duration::from_secs(5), confirm)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    ownership_refusal(&reply);
    assert_eq!(
        venue.submissions().len(),
        1,
        "redacted ownership reached cancel POST"
    );
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn mixed_manual_and_owned_targets_refuse_entire_cancel_all_in_both_approval_modes() {
    for approval in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        let oid = place_owned(&runtime, 170).await;
        let manual = venue.manual_copy(oid, Cloid::from_bytes([171; 16]));
        if approval {
            enable_approval(&runtime).await;
        }
        let reads = keys.read_heads.lock().unwrap().len();
        for (tool, args) in [
            (
                "cancel",
                json!({"oid":manual,"reason":"manual is not owned"}),
            ),
            (
                "cancel_all",
                json!({"reason":"must not narrow the mixed set"}),
            ),
        ] {
            ownership_refusal(&runtime.call(tool, args).await);
            assert_eq!(venue.submissions().len(), 1);
            assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
            assert_eq!(count(&runtime, EventKind::ApprovalProposed), 0);
        }
        for oid in [oid, manual] {
            assert_eq!(
                runtime.call("get_order_status", json!({"oid":oid})).await["status"],
                "open"
            );
        }
        // Runtime cleanup remains independent of ordinary ownership and approval.
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
        runtime
            .gateway
            .enforce_pauses(&[binding(&runtime)], runtime.tracker())
            .await
            .unwrap();
        assert_eq!(venue.submissions().len(), 2);
        assert_eq!(
            venue.submissions()[1]["action"]["cancels"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn partial_fill_preserves_original_ownership_before_cancel_review_in_both_modes() {
    for approval in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let runtime =
            Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
        runtime.activate_orders().await;
        let oid = place_owned(&runtime, 172).await;
        venue.fill(Cloid::from_bytes([172; 16]).as_str(), Decimal::new(4, 2));
        runtime.reconcile().await;
        if approval {
            enable_approval(&runtime).await;
        }
        let reply = runtime
            .call(
                "cancel",
                json!({"oid":oid,"reason":"remaining owned quantity"}),
            )
            .await;
        if approval {
            assert_eq!(reply["status"], "pending_approval", "{reply}");
            assert_eq!(venue.submissions().len(), 1);
            let serving = Serving::start(&runtime).await;
            let review = serving
                .control
                .prepare(&binding(&runtime), reply["approval_id"].as_str().unwrap())
                .await
                .unwrap();
            let display = review.display().cancel().unwrap();
            assert_eq!(display.targets[0].orig_sz, Decimal::new(12, 2));
            assert_eq!(display.targets[0].sz, Decimal::new(8, 2));
            assert_eq!(
                serving.control.confirm(review).await.unwrap()["canceled"],
                1
            );
            serving.finish().await;
        } else {
            assert_eq!(reply["status"], "canceled", "{reply}");
            assert_eq!(reply["canceled"], 1);
        }
        assert_eq!(venue.submissions().len(), 2);
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn changed_limit_tif_or_protective_price_cannot_use_the_old_acceptance() {
    for approval in [false, true] {
        for trigger in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let venue = Venue::start().await;
            let keys = Arc::new(FixtureKeys::default());
            let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
            runtime.activate_orders().await;
            let oid = if trigger {
                let reply = runtime.call("place", json!({"symbol":"TEST","is_buy":true,"size":"0.12",
                    "order_type":"stop_market","trigger_px":"105","tpsl":"sl","reason":"trigger identity",
                    "cloid":Cloid::from_bytes([173;16]).as_str()})).await;
                assert_eq!(reply["status"], "resting", "{reply}");
                runtime.reconcile().await;
                reply["oid"].as_u64().unwrap()
            } else {
                place_owned(&runtime, 173).await
            };
            if approval {
                enable_approval(&runtime).await;
            }
            venue.alter_order(oid, |wire| {
                if trigger {
                    let oppen_hl::wire::OrderType::Trigger { trigger_px, .. } = &mut wire.t else {
                        panic!("trigger");
                    };
                    *trigger_px = WireFloat::from_decimal(Decimal::from(106)).unwrap();
                } else {
                    wire.t = oppen_hl::wire::OrderType::Limit {
                        tif: oppen_hl::wire::Tif::Alo,
                    };
                }
            });
            let reads = keys.read_heads.lock().unwrap().len();
            ownership_refusal(
                &runtime
                    .call(
                        "cancel",
                        json!({"oid":oid,"reason":"changed venue identity"}),
                    )
                    .await,
            );
            assert_eq!(venue.submissions().len(), 1);
            assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
            assert_eq!(count(&runtime, EventKind::ApprovalProposed), 0);
            runtime.shutdown().await;
            venue.shutdown().await;
        }
    }
}

#[tokio::test]
async fn redacted_acceptance_after_native_review_refuses_without_signing() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let oid = place_owned(&runtime, 174).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let proposal = runtime
        .call(
            "cancel",
            json!({"oid":oid,"reason":"evidence may disappear"}),
        )
        .await;
    assert_eq!(proposal["status"], "pending_approval", "{proposal}");
    let review = serving
        .control
        .prepare(
            &binding(&runtime),
            proposal["approval_id"].as_str().unwrap(),
        )
        .await
        .unwrap();
    let accepted = runtime
        .ledger
        .get_events(0, 1000)
        .unwrap()
        .events
        .into_iter()
        .find(|event| event.kind == EventKind::SubmissionAccepted)
        .unwrap();
    runtime
        .ledger
        .redact(
            accepted.seq,
            "synthetic evidence retention",
            now_ms() as i64,
        )
        .unwrap();
    assert!(runtime.ledger.verify().unwrap().is_intact());
    let reads = keys.read_heads.lock().unwrap().len();
    let reply = serving.control.confirm(review).await.unwrap();
    ownership_refusal(&reply);
    assert_eq!(venue.submissions().len(), 1);
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}
