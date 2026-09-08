//! ES30: discretionary cancellation crosses the native review and real signer.

use super::*;
use oppen_core::guardrail::ApprovalReviewDisplay;

async fn seed(runtime: &Runtime, byte: u8) -> u64 {
    let result = runtime
        .call(
            "place",
            place(Cloid::from_bytes([byte; 16]).as_str(), "0.10"),
        )
        .await;
    assert_eq!(result["status"], "resting", "{result}");
    runtime.reconcile().await;
    result["oid"].as_u64().unwrap()
}

async fn propose(runtime: &Runtime, tool: &str, args: Value) -> String {
    let result = runtime.call(tool, args).await;
    assert_eq!(result["status"], "pending_approval", "{result}");
    assert_eq!(result["action"], "cancel", "{result}");
    assert!(!result["targets"].as_array().unwrap().is_empty());
    assert!(result["expires_at_ms"].as_u64().unwrap() > now_ms());
    result["approval_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn cancel_and_cancel_all_prepare_without_execution_and_confirm_exactly_once() {
    for all in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        let oid = seed(&runtime, 140).await;
        enable_approval(&runtime).await;
        let serving = Serving::start(&runtime).await;
        let reads = keys.read_heads.lock().unwrap().len();
        let starts = count(&runtime, EventKind::SubmissionStarted);
        let reason = "operator review, not a cleanup exemption";
        let id = propose(
            &runtime,
            if all { "cancel_all" } else { "cancel" },
            if all {
                json!({"reason":reason})
            } else {
                json!({"oid":oid,"reason":reason})
            },
        )
        .await;
        let review = serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .unwrap();
        let duplicate = serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .unwrap();
        let ApprovalReviewDisplay::Cancel(display) = review.display().clone() else {
            panic!("cancel review");
        };
        assert_eq!(display.proposal_id, id);
        assert_eq!(display.account, runtime.account);
        assert_eq!(display.reason, reason);
        assert_eq!(display.targets.len(), 1);
        assert_eq!(display.targets[0].oid, oid);
        assert_eq!(display.targets[0].sz, Decimal::new(10, 2));
        assert_eq!(display.targets[0].limit_px, Decimal::from(100));
        assert_eq!(display.route.binding.container, runtime.account);
        let serialized = serde_json::to_value(review.display()).unwrap();
        assert_eq!(serialized["kind"], "cancel");
        assert!(serialized["targets"][0]["sz"].is_string());
        assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 0);
        assert_eq!(count(&runtime, EventKind::SubmissionStarted), starts);
        assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
        assert_eq!(venue.submissions().len(), 1);

        let result = serving.control.confirm(review).await.unwrap();
        assert_eq!(result["status"], "canceled", "{result}");
        assert_eq!(result["requested"], 1);
        assert_eq!(result["canceled"], 1);
        assert_eq!(result["failed"], json!([]));
        assert_eq!(
            venue.submissions()[1]["action"],
            json!({"type":"cancel","cancels":[{"a":0,"o":oid}]})
        );
        assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
        let reads = keys.read_heads.lock().unwrap().len();
        if let Ok(result) = serving.control.confirm(duplicate).await {
            assert_eq!(result["status"], "rejected", "{result}");
        }
        assert_eq!(venue.submissions().len(), 2);
        assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
        assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
        assert!(runtime.ledger.verify().unwrap().is_intact());
        serving.finish().await;
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn cancel_all_review_never_expands_to_a_later_order() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    runtime.activate_orders().await;
    let original = seed(&runtime, 141).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let id = propose(&runtime, "cancel_all", json!({"reason":"frozen set"})).await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    // The later order is itself approved under the same policy revision.
    let order_id = mint(&runtime, 142).await;
    let order_review = serving
        .control
        .prepare(&binding(&runtime), &order_id)
        .await
        .unwrap();
    let later = serving.control.confirm(order_review).await.unwrap();
    assert_eq!(later["status"], "resting", "{later}");
    runtime.reconcile().await;
    let result = serving.control.confirm(review).await.unwrap();
    assert_eq!(result["status"], "canceled", "{result}");
    assert_eq!(result["requested"], 1);
    assert_eq!(
        venue.submissions().last().unwrap()["action"]["cancels"],
        json!([{"a":0,"o":original}])
    );
    let later_status = runtime
        .call("get_order_status", json!({"oid":later["oid"]}))
        .await;
    assert_eq!(later_status["status"], "open", "{later_status}");
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn missing_or_partially_filled_review_target_refuses_without_subset_submission() {
    for fill in [Decimal::new(5, 2), Decimal::new(10, 2)] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        seed(&runtime, 143).await;
        seed(&runtime, 144).await;
        enable_approval(&runtime).await;
        let serving = Serving::start(&runtime).await;
        let id = propose(
            &runtime,
            "cancel_all",
            json!({"reason":"no implicit subset"}),
        )
        .await;
        let review = serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .unwrap();
        venue.fill(Cloid::from_bytes([143; 16]).as_str(), fill);
        let reads = keys.read_heads.lock().unwrap().len();
        let result = serving.control.confirm(review).await.unwrap();
        assert_eq!(result["status"], "rejected", "{result}");
        assert_eq!(
            result["refusal"]["unevaluable"], "approval_review_changed",
            "{result}"
        );
        assert_eq!(venue.submissions().len(), 2);
        assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
        serving.finish().await;
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn protective_stop_loss_and_take_profit_cancellation_require_review() {
    for (tpsl, trigger) in [("sl", "95"), ("tp", "105")] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        let entry = runtime
            .call(
                "place",
                place(Cloid::from_bytes([145; 16]).as_str(), "0.12"),
            )
            .await;
        assert_eq!(entry["status"], "resting", "{entry}");
        venue.fill(Cloid::from_bytes([145; 16]).as_str(), Decimal::new(12, 2));
        runtime.reconcile().await;
        let stop = runtime
            .call(
                "place",
                json!({"symbol":"TEST","is_buy":false,"size":"0.12",
            "order_type":"stop_market","trigger_px":trigger,"tpsl":tpsl,"reduce_only":true,
            "cloid":Cloid::from_bytes([146;16]).as_str(),"reason":"protect position"}),
            )
            .await;
        assert_eq!(stop["status"], "resting", "{stop}");
        runtime.reconcile().await;
        enable_approval(&runtime).await;
        let serving = Serving::start(&runtime).await;
        let reads = keys.read_heads.lock().unwrap().len();
        let id = propose(
            &runtime,
            "cancel",
            json!({"oid":stop["oid"],"reason":"remove protection"}),
        )
        .await;
        let review = serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .unwrap();
        let ApprovalReviewDisplay::Cancel(display) = review.display() else {
            panic!("cancel review");
        };
        assert_eq!(display.targets.len(), 1);
        let target = &display.targets[0];
        assert!(target.is_trigger && target.reduce_only);
        assert_eq!(target.trigger_px, Some(trigger.parse().unwrap()));
        assert_eq!(
            target.order_type,
            if tpsl == "sl" {
                "Stop Market"
            } else {
                "Take Profit Market"
            }
        );
        assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
        assert_eq!(venue.submissions().len(), 2);
        let result = serving.control.confirm(review).await.unwrap();
        assert_eq!(result["canceled"], 1, "{result}");
        assert_eq!(venue.submissions().len(), 3);
        serving.finish().await;
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn reversed_snapshot_preserves_reviewed_target_order_and_partial_outcome_identity() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    runtime.activate_orders().await;
    let first = seed(&runtime, 147).await;
    let second = seed(&runtime, 148).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let id = propose(
        &runtime,
        "cancel_all",
        json!({"reason":"reviewed identities"}),
    )
    .await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    venue.reverse_open_orders();
    let gate = venue.hold_info("frontendOpenOrders");
    let control = serving.control.clone();
    let confirm = tokio::spawn(async move { control.confirm(review).await });
    timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    // The fresh snapshot is already captured. A fill races the eventual POST,
    // so this is a venue partial result, not a changed-target precheck refusal.
    venue.fill(Cloid::from_bytes([147; 16]).as_str(), Decimal::new(10, 2));
    gate.release.notify_one();
    let result = confirm.await.unwrap().unwrap();
    assert_eq!(result["status"], "canceled", "{result}");
    assert_eq!(result["requested"], 2);
    assert_eq!(result["canceled"], 1);
    assert_eq!(result["failed"].as_array().unwrap().len(), 1);
    assert_eq!(result["failed"][0]["oid"], first);
    assert_eq!(
        result["failed"][0]["cloid"],
        Cloid::from_bytes([147; 16]).as_str()
    );
    assert!(result["failed"][0]["venue_message"].is_string());
    assert_eq!(
        venue.submissions()[2]["action"]["cancels"],
        json!([{"a":0,"o":first},{"a":0,"o":second}])
    );
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn applied_malformed_cancel_is_nonretryable_unknown_not_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    runtime.activate_orders().await;
    let oid = seed(&runtime, 149).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let id = propose(
        &runtime,
        "cancel",
        json!({"oid":oid,"reason":"uncertain reply"}),
    )
    .await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    venue.next_response(Behavior::AppliedMalformed);
    let error = serving.control.confirm(review).await.unwrap_err();
    let data = error.data.unwrap();
    assert_eq!(data["code"], "timeout_unknown_outcome", "{data}");
    assert_eq!(data["retryable"], false);
    assert_eq!(data["action"], "cancel");
    assert_eq!(data["proposal_id"], id);
    assert_eq!(data["targets"][0]["oid"], oid);
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
    assert_eq!(venue.submissions().len(), 2);
    let status = runtime.call("get_order_status", json!({"oid":oid})).await;
    assert_eq!(status["status"], "canceled", "{status}");
    assert!(
        serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .is_err()
    );
    assert_eq!(venue.submissions().len(), 2);
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn cancellation_review_refuses_retired_changed_route_or_policy_without_signing() {
    for case in ["retired", "changed_route", "changed_policy"] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        let oid = seed(&runtime, 150).await;
        enable_approval(&runtime).await;
        let serving = Serving::start(&runtime).await;
        let id = propose(
            &runtime,
            "cancel",
            json!({"oid":oid,"reason":"authority can change"}),
        )
        .await;
        let review = serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .unwrap();
        let reads = keys.read_heads.lock().unwrap().len();
        let agent = binding(&runtime).agent;
        if case == "changed_policy" {
            let engine = &runtime.gateway.inner.engine;
            let before = engine.policy_observation().unwrap();
            let mut policy = engine.guardrails(&agent).unwrap();
            policy.max_order_usd += Decimal::ONE;
            engine
                .operator_set_guardrails(&agent, policy, now_ms())
                .unwrap();
            assert_ne!(engine.policy_observation().unwrap(), before);
            runtime.acknowledge_policy();
        } else {
            let route = runtime.registry.route_for_agent(&agent).unwrap();
            assert!(runtime.registry.retire(&route, now_ms()).unwrap());
            if case == "changed_route" {
                // Retired containers and signers are never reusable. The same
                // agent receives a distinct synthetic replacement authority.
                let mut replacement = route.binding.clone();
                replacement.container = Address::from_bytes([8; 20]);
                replacement.wallet.address = Address::from_bytes([7; 20]);
                replacement.vault_address = None;
                runtime.registry.grant(replacement, now_ms()).unwrap();
            }
        }
        match serving.control.confirm(review).await {
            Ok(result) => {
                assert_eq!(result["status"], "rejected", "{case}: {result}");
                assert!(result["refusal"].is_object(), "{result}");
            }
            Err(error) => {
                let data = error.data.unwrap();
                assert!(
                    data["code"] == "unavailable" || data["code"] == "guardrail_reject",
                    "{case}: {data}"
                );
                assert_eq!(data["retryable"], false);
            }
        }
        assert_eq!(venue.submissions().len(), 1, "{case}");
        assert_eq!(keys.read_heads.lock().unwrap().len(), reads, "{case}");
        serving.finish().await;
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn revoked_cancellation_review_cannot_use_a_live_replacement_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let mut runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let oid = seed(&runtime, 151).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let id = propose(
        &runtime,
        "cancel",
        json!({"oid":oid,"reason":"pinned authority"}),
    )
    .await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    let pinned = review.pairing_id();
    let pairings = runtime.pairings.clone();
    let binding = binding(&runtime);
    let reads = keys.read_heads.lock().unwrap().len();
    runtime.token = tokio::task::spawn_blocking(move || {
        let mut pairings = pairings.write().unwrap();
        let replacement = pairings.issue(binding).unwrap();
        assert_ne!(replacement.id, pinned);
        assert!(pairings.revoke(pinned).unwrap());
        replacement.reveal().to_owned()
    })
    .await
    .unwrap();
    unavailable(serving.control.confirm(review).await.unwrap_err());
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 0);
    assert_eq!(venue.submissions().len(), 1);
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn halt_cleanup_remains_immediate_with_an_unavailable_approval_journal() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    runtime.activate_orders().await;
    let oid = seed(&runtime, 152).await;
    enable_approval(&runtime).await;
    let id = propose(
        &runtime,
        "cancel",
        json!({"oid":oid,"reason":"HALT cleanup"}),
    )
    .await;
    // Even a safety-looking reason above is discretionary. Only the actual
    // operator stop and server supervisor below select immediate cleanup.
    assert_eq!(venue.submissions().len(), 1);
    let serving = Serving::start(&runtime).await;
    let route = runtime
        .registry
        .route_for_agent(&binding(&runtime).agent)
        .unwrap();
    assert!(
        runtime
            .gateway
            .inner
            .engine
            .pending_proposals(now_ms())
            .unwrap()
            .iter()
            .any(|proposal| proposal.id() == id)
    );
    // Legitimate retention redacts the actual authenticated proposal. The
    // chain remains intact, but approval replay cannot recover its evidence.
    let proposal_seq = runtime
        .ledger
        .get_events(0, 1000)
        .unwrap()
        .events
        .into_iter()
        .find(|event| event.kind == EventKind::ApprovalProposed)
        .expect("the MCP-minted proposal is present")
        .seq;
    runtime
        .ledger
        .redact(
            proposal_seq,
            "synthetic approval retention",
            now_ms() as i64,
        )
        .unwrap();
    assert!(runtime.ledger.verify().unwrap().is_intact());
    assert_eq!(
        runtime
            .registry
            .route_for_agent(&binding(&runtime).agent)
            .unwrap(),
        route
    );
    assert!(
        runtime
            .gateway
            .inner
            .engine
            .pending_proposals(now_ms())
            .is_err()
    );
    unavailable(
        serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .err()
            .expect("redacted approval evidence must refuse preparation"),
    );
    assert_eq!(venue.submissions().len(), 1);
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
    let mut status = serving.supervision.status();
    let baseline = serving.supervision.request_sweep();
    timeout(Duration::from_secs(2), async {
        loop {
            let current = status.borrow_and_update().clone();
            if current.completed_sequence > baseline && !current.in_progress {
                assert!(current.last_error.is_none(), "{current:?}");
                break;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("HALT cleanup must not wait for the periodic sweep or approval recovery");
    assert_eq!(venue.submissions().len(), 2);
    assert_eq!(
        venue.submissions()[1]["action"],
        json!({"type":"cancel","cancels":[{"a":0,"o":oid}]})
    );
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 0);
    assert!(
        runtime
            .gateway
            .inner
            .engine
            .pending_proposals(now_ms())
            .is_err()
    );
    assert!(runtime.ledger.verify().unwrap().is_intact());
    assert_eq!(
        runtime
            .registry
            .route_for_agent(&binding(&runtime).agent)
            .unwrap(),
        route
    );
    let status = runtime.call("get_order_status", json!({"oid":oid})).await;
    assert_eq!(status["status"], "canceled", "{status}");
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}
