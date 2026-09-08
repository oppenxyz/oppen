//! ES31b evidence through real guarded MCP submission and synthetic HTTP.

use super::*;
use oppen_core::ledger::{Anchor, HeadAnchor, LedgerError};
use rusqlite::OptionalExtension;

fn evidence(runtime: &Runtime, kind: EventKind) -> Vec<oppen_core::ledger::Event> {
    runtime
        .ledger
        .get_events(0, 1000)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == kind)
        .collect()
}

fn assert_pending(runtime: &Runtime, cloid: &Cloid) {
    let receipt = runtime
        .gateway
        .inner
        .submissions
        .state(runtime.account)
        .unwrap()
        .pending
        .expect("acceptance is not exposure reconciliation");
    assert_eq!(receipt.cloid(), cloid);
    assert!(evidence(runtime, EventKind::SubmissionResolved).is_empty());
}

/// Lifecycle events are injected here; the HL socket regression owns detection
/// of silence before a buffered frame. This test owns the downstream recovery.
#[tokio::test]
async fn persistent_pump_recovers_gap_loss_before_reopening_order_admission() {
    use oppen_core::guardrail::PilotMetric;
    use oppen_core::ledger::{PilotJournal, PilotStop};
    use oppen_hl::ws::{ConnectionId, Disconnected, GapWindow, Reconnected, WsEvent};
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;

    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Arc::new(Runtime::open(dir.path(), venue.port(), keys.clone()).await);
    let startup = venue.hold_info("userFillsByTime");
    let (events, mut receiver) = mpsc::channel(8);
    let (quiesce, quiescing) = oneshot::channel();
    let (producers_done, producers) = oneshot::channel();
    let pumping = runtime.clone();
    let pump = tokio::spawn(async move {
        let inner = &pumping.gateway.inner;
        FeedPump::new(
            &inner.feed,
            &pumping.ledger,
            pumping.account,
            HttpSource(InfoClient::loopback_fixture(pumping.port).unwrap()),
            &inner.alerts,
            &inner.quotes,
            &NoSocket,
        )
        .unwrap()
        .run_until_shutdown(&mut receiver, quiescing, producers)
        .await;
    });
    timeout(Duration::from_secs(3), startup.entered.notified())
        .await
        .unwrap();
    startup.release.notify_one();
    timeout(Duration::from_secs(3), async {
        while !runtime.gateway.inner.feed.state().reconciled {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("persistent pump startup did not reconcile");
    // No Runtime::activate_orders/reconcile: neither may start a second pump
    // whose startup walk could accidentally supply the outage recovery proof.
    runtime
        .gateway
        .inner
        .engine
        .operator_release_kill(&KillScope::Global, now_ms())
        .unwrap();
    runtime.acknowledge_policy();
    PilotJournal::new(runtime.registry.clone())
        .authorize(AgentId::new("fixture-agent"), runtime.account, now_ms())
        .unwrap();
    let cloid = Cloid::from_bytes([184; 16]);
    let placed = runtime.call("place", place(cloid.as_str(), "0.15")).await;
    assert_eq!(placed["status"], "resting", "{placed}");
    let bound = Binding {
        agent: AgentId::new("fixture-agent"),
        account: runtime.account,
    };
    // Settle the initial observed resting receipt before the outage, so a
    // later order's reservation cannot substitute for the pump's catch-up.
    drop(runtime.gateway.reserve_submission(&bound).await.unwrap());
    let signed_before = evidence(&runtime, EventKind::SubmissionSigned);
    let accepted_before = evidence(&runtime, EventKind::SubmissionAccepted);
    let posts_before = venue.submissions();
    let subscription = Subscription::UserFills {
        user: runtime.account,
    };
    let dropped_at = now_ms();
    events
        .send(WsEvent::Disconnected(Box::new(Disconnected {
            connection: ConnectionId::new(0),
            at_ms: dropped_at,
            last_message_ms: Some(dropped_at),
            subscriptions: vec![subscription.clone()],
            unacked: Vec::new(),
            reason: "synthetic buffered-frame silence".into(),
        })))
        .await
        .unwrap();
    let tick_at = dropped_at + 1;
    events
        .send(WsEvent::Bbo {
            coin: "TEST".into(),
            venue_time_ms: tick_at,
            bid: None,
            ask: None,
        })
        .await
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while runtime.gateway.inner.feed.state().last_tick_ms != Some(tick_at) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pump did not process the buffered market tick");
    let after_tick = runtime.gateway.inner.feed.state();
    let open = runtime.ledger.unreconciled_gaps().unwrap();
    venue.fill_with_fee(cloid.as_str(), Decimal::new(5, 2), Decimal::from(5));
    let fills_before_recovery = evidence(&runtime, EventKind::Fill);
    let during_gap = runtime
        .call(
            "place",
            place(Cloid::from_bytes([185; 16]).as_str(), "0.10"),
        )
        .await;

    let recovery = venue.hold_info("userFillsByTime");
    let resumed_at = now_ms().max(tick_at);
    events
        .send(WsEvent::Reconnected(Box::new(Reconnected {
            connection: ConnectionId::new(0),
            at_ms: resumed_at,
            gap: GapWindow {
                start_ms: dropped_at,
                end_ms: resumed_at,
            },
            resubscribed: vec![subscription.clone()],
            attempts: 1,
        })))
        .await
        .unwrap();
    timeout(Duration::from_secs(3), recovery.entered.notified())
        .await
        .unwrap();
    let during_recovery_state = runtime.gateway.inner.feed.state();
    let closed = runtime.ledger.unreconciled_gaps().unwrap();
    let during_recovery = runtime
        .call(
            "place",
            place(Cloid::from_bytes([186; 16]).as_str(), "0.10"),
        )
        .await;
    let posts_during_recovery = venue.submissions();
    recovery.release.notify_one();
    timeout(Duration::from_secs(3), async {
        while !runtime.gateway.inner.feed.state().reconciled {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("persistent pump did not finish gap recovery");
    let recovered = PilotJournal::new(runtime.registry.clone())
        .state(runtime.account)
        .unwrap()
        .unwrap();
    let fills = evidence(&runtime, EventKind::Fill);
    let outstanding = runtime.ledger.unreconciled_gaps().unwrap();
    let after_recovery = runtime
        .call(
            "place",
            place(Cloid::from_bytes([187; 16]).as_str(), "0.10"),
        )
        .await;
    let halted = PilotJournal::new(runtime.registry.clone())
        .state(runtime.account)
        .unwrap()
        .unwrap();
    let signed = evidence(&runtime, EventKind::SubmissionSigned);
    let accepted = evidence(&runtime, EventKind::SubmissionAccepted);
    let posts = venue.submissions();

    let (ack, acknowledged) = oneshot::channel();
    quiesce.send(ack).unwrap();
    timeout(Duration::from_secs(3), acknowledged)
        .await
        .unwrap()
        .unwrap();
    drop(events);
    producers_done.send(()).unwrap();
    timeout(Duration::from_secs(3), pump)
        .await
        .unwrap()
        .unwrap();
    let feed = runtime.gateway.inner.feed.state();
    let runtime = Arc::try_unwrap(runtime)
        .ok()
        .expect("drained pump retains runtime");
    runtime.shutdown().await;
    venue.shutdown().await;

    assert!(
        !after_tick.reconciled,
        "market tick reopened order admission"
    );
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].scope, subscription.key());
    assert_eq!(open[0].opened_ts_ms, dropped_at as i64);
    assert!(open[0].closed_ts_ms.is_none());
    assert!(fills_before_recovery.is_empty());
    for reply in [&during_gap, &during_recovery] {
        assert_eq!(reply["status"], "rejected", "{reply}");
        assert_eq!(
            reply["refusal"]["unevaluable"], "unreconciled_account",
            "{reply}"
        );
    }
    assert!(!during_recovery_state.reconciled);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].gap_id, open[0].gap_id);
    assert_eq!(closed[0].closed_ts_ms, Some(resumed_at as i64));
    assert_eq!(posts_during_recovery, posts_before);
    assert!(outstanding.is_empty());
    assert_eq!(
        fills.len(),
        1,
        "gap fill must be durably recorded exactly once"
    );
    assert_eq!(fills[0].payload.as_ref().unwrap()["cloid"], cloid.as_str());
    assert_eq!(recovered.net_realized_pnl_usd, Decimal::from(-5));
    assert_eq!(
        recovered.halt,
        Some(PilotStop::Exhausted {
            metric: PilotMetric::RealizedLoss,
            observed_usd: Decimal::from(5),
            limit_usd: Decimal::from(5),
        })
    );
    assert_eq!(halted.halt, recovered.halt);
    assert_eq!(after_recovery["status"], "rejected", "{after_recovery}");
    assert_eq!(
        after_recovery["refusal"]["refusal"], "pilot_budget",
        "{after_recovery}"
    );
    assert_eq!(signed, signed_before);
    assert_eq!(accepted, accepted_before);
    assert_eq!(posts, posts_before);
    assert!(feed.failure.is_none(), "{feed:?}");
}

#[tokio::test]
async fn processed_disconnect_during_key_loading_refuses_order_before_signing() {
    disconnect_during_key_loading(false, false).await;
}

#[tokio::test]
async fn processed_disconnect_then_reconcile_refuses_old_order_snapshot() {
    disconnect_during_key_loading(false, true).await;
}

#[tokio::test]
async fn processed_disconnect_during_key_loading_refuses_reduce_only_order() {
    disconnect_during_key_loading(true, false).await;
}

#[tokio::test]
async fn processed_disconnect_then_reconcile_during_account_read_refuses_old_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Arc::new(Runtime::open(dir.path(), venue.port(), keys.clone()).await);
    runtime.activate_orders().await;
    let reads_before = keys.read_heads.lock().unwrap().len();
    // The venue freezes this response before notification, then releases its
    // book lock. Recovery reads are independent because the gate is one-shot.
    let gate = venue.hold_info("clearinghouseState");
    let calling = runtime.clone();
    let order = tokio::spawn(async move {
        calling
            .call(
                "place",
                place(Cloid::from_bytes([183; 16]).as_str(), "0.12"),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .expect("order never reached the captured account HTTP response");
    assert!(evidence(&runtime, EventKind::SubmissionStarted).is_empty());
    assert!(venue.submissions().is_empty());
    process_disconnect(&runtime);
    runtime.reconcile().await;
    assert!(runtime.gateway.inner.feed.state().reconciled);
    gate.release.notify_one();
    let reply = tokio::time::timeout(Duration::from_secs(5), order)
        .await
        .unwrap()
        .unwrap();
    let started = evidence(&runtime, EventKind::SubmissionStarted);
    let signed = evidence(&runtime, EventKind::SubmissionSigned);
    let accepted = evidence(&runtime, EventKind::SubmissionAccepted);
    let posts = venue.submissions();
    let reads_after = keys.read_heads.lock().unwrap().len();
    let pending = runtime
        .gateway
        .inner
        .submissions
        .state(runtime.account)
        .unwrap()
        .pending;
    let runtime = Arc::try_unwrap(runtime)
        .ok()
        .expect("order observer retains runtime");
    runtime.shutdown().await;
    venue.shutdown().await;

    assert_eq!(reply["status"], "rejected", "{reply}");
    assert_eq!(reply["refusal"]["unevaluable"], "feed_admission", "{reply}");
    assert_eq!(
        reads_after, reads_before,
        "old snapshot reached key loading"
    );
    assert!(started.is_empty(), "old snapshot reached submission begin");
    assert!(signed.is_empty());
    assert!(accepted.is_empty());
    assert!(posts.is_empty());
    assert!(pending.is_none());
}

async fn disconnect_during_key_loading(reduce_only: bool, recover: bool) {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Arc::new(Runtime::open(dir.path(), venue.port(), keys.clone()).await);
    runtime.activate_orders().await;
    if reduce_only {
        let seed = Cloid::from_bytes([181; 16]);
        let reply = runtime.call("place", place(seed.as_str(), "0.12")).await;
        assert_eq!(reply["status"], "resting", "{reply}");
        venue.fill(seed.as_str(), Decimal::new(12, 2));
        runtime.reconcile().await;
    }
    assert!(runtime.gateway.inner.feed.state().reconciled);
    let started_before = evidence(&runtime, EventKind::SubmissionStarted).len();
    let signed_before = evidence(&runtime, EventKind::SubmissionSigned);
    let accepted_before = evidence(&runtime, EventKind::SubmissionAccepted);
    let posts_before = venue.submissions();
    let cloid = Cloid::from_bytes([180; 16]);
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    *keys.read_gate.lock().unwrap() = Some(KeyReadGate {
        entered: entered.clone(),
        release: wait,
    });
    let mut arguments = place(cloid.as_str(), "0.12");
    if reduce_only {
        arguments["is_buy"] = json!(false);
        arguments["reduce_only"] = json!(true);
    }
    let calling = runtime.clone();
    let order = tokio::spawn(async move { calling.call("place", arguments).await });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .expect("evaluated order never reached key loading");
    assert_eq!(
        evidence(&runtime, EventKind::SubmissionStarted).len(),
        started_before + 1
    );
    let starts: Vec<_> = evidence(&runtime, EventKind::SubmissionStarted)
        .into_iter()
        .filter(|event| event.payload.as_ref().unwrap()["cloid"] == cloid.as_str())
        .collect();
    assert_eq!(starts.len(), 1, "exactly one start for the gated order");
    assert_eq!(
        evidence(&runtime, EventKind::SubmissionSigned),
        signed_before
    );
    assert_eq!(venue.submissions(), posts_before);

    process_disconnect(&runtime);
    if recover {
        // Only the actual pump can complete recovery; do not flip the feed flag.
        runtime.reconcile().await;
        assert!(runtime.gateway.inner.feed.state().reconciled);
    }
    release.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), order)
        .await
        .unwrap()
        .unwrap();
    let signed = evidence(&runtime, EventKind::SubmissionSigned);
    let accepted = evidence(&runtime, EventKind::SubmissionAccepted);
    let resolved: Vec<_> = evidence(&runtime, EventKind::SubmissionResolved)
        .into_iter()
        .filter(|event| {
            let payload = event.payload.as_ref().unwrap();
            payload["start_seq"] == starts[0].seq && payload["start_hash"] == starts[0].hash
        })
        .collect();
    let pending = runtime
        .gateway
        .inner
        .submissions
        .state(runtime.account)
        .unwrap()
        .pending;
    let posts = venue.submissions();
    let runtime = Arc::try_unwrap(runtime)
        .ok()
        .expect("order observer retains runtime");
    runtime.shutdown().await;
    venue.shutdown().await;

    assert_eq!(reply["status"], "rejected", "{reply}");
    assert_eq!(reply["refusal"]["unevaluable"], "feed_admission", "{reply}");
    assert_eq!(
        signed, signed_before,
        "disconnect must prevent signature publication"
    );
    assert_eq!(accepted, accepted_before);
    assert_eq!(
        posts, posts_before,
        "disconnect must prevent the actual exchange POST"
    );
    assert_eq!(
        resolved.len(),
        1,
        "exactly one resolution for the gated order"
    );
    assert_eq!(
        resolved[0].payload.as_ref().unwrap()["outcome"]["resolution"],
        "not_sent"
    );
    assert!(pending.is_none());
}

fn process_disconnect(runtime: &Runtime) {
    use oppen_hl::ws::{ConnectionId, Disconnected, Subscription, WsEvent};

    let at_ms = now_ms();
    let subscription = Subscription::UserFills {
        user: runtime.account,
    };
    runtime
        .gateway
        .inner
        .feed
        .apply(
            &runtime.ledger,
            &runtime.account.to_string(),
            &WsEvent::Disconnected(Box::new(Disconnected {
                connection: ConnectionId::new(0),
                at_ms,
                last_message_ms: Some(at_ms),
                subscriptions: vec![subscription.clone()],
                unacked: Vec::new(),
                reason: "synthetic disconnect after order evaluation".into(),
            })),
            at_ms,
        )
        .unwrap();
    assert!(!runtime.gateway.inner.feed.state().reconciled);
    assert!(
        runtime
            .ledger
            .unreconciled_gaps()
            .unwrap()
            .iter()
            .any(|gap| { gap.scope == subscription.key() && gap.closed_ts_ms.is_none() })
    );
}

#[tokio::test]
async fn processed_disconnect_preserves_runtime_halt_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    runtime.activate_orders().await;
    let reply = runtime
        .call(
            "place",
            place(Cloid::from_bytes([182; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(reply["status"], "resting", "{reply}");
    runtime.reconcile().await;
    process_disconnect(&runtime);
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
        .enforce_pauses(
            &[Binding {
                agent: AgentId::new("fixture-agent"),
                account: runtime.account,
            }],
            runtime.tracker(),
        )
        .await
        .unwrap();
    assert!(!runtime.gateway.inner.feed.state().reconciled);
    let posts = venue.submissions();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1]["action"]["type"], "cancel");
    assert_eq!(
        posts[1]["action"]["cancels"],
        json!([{"a":0,"o":reply["oid"]}])
    );
    let status = runtime
        .call("get_order_status", json!({"oid":reply["oid"]}))
        .await;
    assert_eq!(status["status"], "canceled", "{status}");
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn guarded_acknowledgment_persists_linked_non_executable_evidence_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let cloid = Cloid::from_bytes([160; 16]);
    let reply = runtime.call("place", place(cloid.as_str(), "0.12")).await;
    assert_eq!(reply["status"], "resting", "{reply}");
    let signed = evidence(&runtime, EventKind::SubmissionSigned);
    let accepted = evidence(&runtime, EventKind::SubmissionAccepted);
    assert_eq!(signed.len(), 1);
    assert_eq!(accepted.len(), 1);
    assert!(signed[0].seq < accepted[0].seq);
    let request = &signed[0].payload.as_ref().unwrap()["envelope"]["operation"]["request"];
    let acceptance = &accepted[0].payload.as_ref().unwrap()["envelope"]["operation"];
    assert_eq!(request["cloid"], cloid.as_str());
    assert_eq!(
        request["route"]["binding"]["container"],
        serde_json::to_value(runtime.account).unwrap()
    );
    assert_eq!(request["action"], venue.submissions()[0]["action"]);
    assert_eq!(request["request_digest"].as_str().unwrap().len(), 64);
    assert_eq!(acceptance["signed"]["seq"], signed[0].seq);
    assert_eq!(acceptance["signed"]["hash"], signed[0].hash);
    assert_eq!(acceptance["oid"], reply["oid"]);
    assert_eq!(acceptance["status"]["status"], "resting");
    assert_pending(&runtime, &cloid);

    let events = runtime
        .call("get_events", json!({"since_cursor":0,"limit":1000}))
        .await;
    assert!(events.get("protocol_error").is_none(), "{events}");
    let mut jsonl = Vec::new();
    runtime.ledger.export_jsonl(&mut jsonl).unwrap();
    let mut csv = Vec::new();
    runtime.ledger.export_csv(&mut csv).unwrap();
    let serialized = [
        events.to_string(),
        String::from_utf8(jsonl).unwrap(),
        String::from_utf8(csv).unwrap(),
        serde_json::to_string(&runtime.ledger.get_events(0, 1000).unwrap().events).unwrap(),
    ];
    let captured = venue.submissions();
    for output in &serialized {
        // Match the actual long signature scalars, not harmless digest names
        // or the one-byte recovery ID, which also occurs in ordinary data.
        for scalar in ["r", "s"] {
            let secret = captured[0]["signature"][scalar].as_str().unwrap();
            assert!(
                !output.contains(secret),
                "executable signature scalar leaked"
            );
            assert!(
                !output.contains(secret.trim_start_matches("0x")),
                "unprefixed signature scalar leaked"
            );
        }
        assert!(
            !output.contains(&captured[0].to_string()),
            "executable request leaked"
        );
    }
    runtime.shutdown().await;
    let runtime = Runtime::open(dir.path(), venue.port(), keys).await;
    assert_eq!(evidence(&runtime, EventKind::SubmissionSigned), signed);
    assert_eq!(evidence(&runtime, EventKind::SubmissionAccepted), accepted);
    assert_pending(&runtime, &cloid);
    assert_eq!(venue.submissions().len(), 1, "reopening never dispatches");
    assert!(runtime.ledger.verify().unwrap().is_intact());
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[derive(Debug)]
enum PublicationAction {
    Fail,
    Hold(std::sync::mpsc::Receiver<()>),
}

#[derive(Debug)]
struct EvidenceAnchor {
    db: std::path::PathBuf,
    head: Mutex<Option<Anchor>>,
    armed: Mutex<Option<(EventKind, PublicationAction)>>,
    reached: tokio::sync::Notify,
}

#[derive(Debug)]
struct EvidenceAnchorHandle(Arc<EvidenceAnchor>);

impl HeadAnchor for EvidenceAnchorHandle {
    fn load(&self) -> Result<Option<Anchor>, LedgerError> {
        Ok(self.0.head.lock().unwrap().clone())
    }

    fn store(&self, head: &Anchor) -> Result<(), LedgerError> {
        let action = {
            let mut armed = self.0.armed.lock().unwrap();
            if let Some((kind, _)) = armed.as_ref() {
                // Publication is after COMMIT. Read through a separate connection,
                // never recursively through Ledger while its writer is held.
                let db = rusqlite::Connection::open_with_flags(
                    &self.0.db,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                let current: Option<String> = db
                    .query_row(
                        "SELECT kind FROM events WHERE seq = ?1",
                        [head.seq],
                        |row| row.get(0),
                    )
                    .optional()
                    .unwrap();
                if current.as_deref() == Some(kind.as_str()) {
                    armed.take().map(|(_, action)| action)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(action) = action {
            self.0.reached.notify_one();
            match action {
                PublicationAction::Fail => {
                    return Err(LedgerError::Io(std::io::Error::other(
                        "synthetic evidence publication failure",
                    )));
                }
                PublicationAction::Hold(release) => {
                    release.recv_timeout(Duration::from_secs(15)).unwrap();
                }
            }
        }
        *self.0.head.lock().unwrap() = Some(head.clone());
        Ok(())
    }
}

fn anchor(path: &Path) -> Arc<EvidenceAnchor> {
    Arc::new(EvidenceAnchor {
        db: path.join("ledger.db"),
        head: Mutex::new(None),
        armed: Mutex::new(None),
        reached: tokio::sync::Notify::new(),
    })
}

#[tokio::test]
async fn signed_publication_failure_prevents_post_even_after_the_row_commits() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let anchor = anchor(dir.path());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        Arc::new(FixtureKeys::default()),
        Some(Box::new(EvidenceAnchorHandle(anchor.clone()))),
    )
    .await;
    runtime.activate_orders().await;
    *anchor.armed.lock().unwrap() = Some((EventKind::SubmissionSigned, PublicationAction::Fail));
    let reply = runtime
        .call(
            "place",
            place(Cloid::from_bytes([162; 16]).as_str(), "0.12"),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(1), anchor.reached.notified())
        .await
        .expect("failure must target signed publication, not an earlier ledger operation");
    assert_ne!(reply["status"], "resting", "{reply}");
    assert!(
        reply["status"] == "rejected" || reply.get("protocol_error").is_some(),
        "{reply}"
    );
    assert_eq!(evidence(&runtime, EventKind::SubmissionSigned).len(), 1);
    assert!(evidence(&runtime, EventKind::SubmissionAccepted).is_empty());
    assert!(
        venue.submissions().is_empty(),
        "uncertain publication cannot expose transport"
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn accepted_publication_failure_is_unknown_after_post_and_keeps_pending_liability() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let anchor = anchor(dir.path());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        Arc::new(FixtureKeys::default()),
        Some(Box::new(EvidenceAnchorHandle(anchor.clone()))),
    )
    .await;
    runtime.activate_orders().await;
    let cloid = Cloid::from_bytes([164; 16]);
    *anchor.armed.lock().unwrap() = Some((EventKind::SubmissionAccepted, PublicationAction::Fail));
    let reply = runtime.call("place", place(cloid.as_str(), "0.12")).await;
    tokio::time::timeout(Duration::from_secs(1), anchor.reached.notified())
        .await
        .expect("failure must reach accepted publication after the actual POST");
    let error = &reply["protocol_error"]["data"];
    assert_eq!(error["code"], "timeout_unknown_outcome", "{reply}");
    assert_eq!(error["retryable"], false);
    assert_eq!(error["cloid"], cloid.as_str());
    assert_eq!(venue.submissions().len(), 1);
    assert_eq!(evidence(&runtime, EventKind::SubmissionSigned).len(), 1);
    // This seam fails anchor publication after COMMIT. The accepted row is
    // present; publication uncertainty must not be rewritten as NotSent.
    assert_eq!(evidence(&runtime, EventKind::SubmissionAccepted).len(), 1);
    assert_pending(&runtime, &cloid);
    let observed = runtime
        .call("get_order_status", json!({"cloid":cloid.as_str()}))
        .await;
    assert_eq!(observed["status"], "open", "{observed}");
    assert_pending(&runtime, &cloid);
    assert_eq!(
        venue.submissions().len(),
        1,
        "read-only status lookup must not resend"
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn native_owner_drains_actual_accepted_publication_after_observer_timeout() {
    use crate::server::BoundServer;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let anchor = anchor(dir.path());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        Arc::new(FixtureKeys::default()),
        Some(Box::new(EvidenceAnchorHandle(anchor.clone()))),
    )
    .await;
    let binding = Binding {
        agent: AgentId::new("fixture-agent"),
        account: runtime.account,
    };
    let engine = &runtime.gateway.inner.engine;
    let mut policy = engine.guardrails(&binding.agent).unwrap();
    policy.approval_required = true;
    engine
        .operator_set_guardrails(&binding.agent, policy, now_ms())
        .unwrap();
    runtime.activate_orders().await;
    let bound = BoundServer::bind(0, &runtime.gateway, &runtime.pairings)
        .await
        .unwrap();
    let control = bound.operator_control();
    let mut status = bound.supervision_status();
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    let mut server = tokio::spawn(bound.serve(
        runtime.gateway.clone(),
        runtime.pairings.clone(),
        stop.clone(),
    ));
    timeout(Duration::from_secs(5), async {
        while status.borrow_and_update().completed_sequence == 0 {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let cloid = Cloid::from_bytes([163; 16]);
    let proposal = runtime.call("place", place(cloid.as_str(), "0.12")).await;
    assert_eq!(proposal["status"], "pending_approval", "{proposal}");
    let review = control
        .prepare(&binding, proposal["approval_id"].as_str().unwrap())
        .await
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    *anchor.armed.lock().unwrap() =
        Some((EventKind::SubmissionAccepted, PublicationAction::Hold(wait)));
    let confirming = control.clone();
    let mut confirm = tokio::spawn(async move { confirming.confirm(review).await });
    timeout(Duration::from_secs(3), anchor.reached.notified())
        .await
        .expect("real POST must reach accepted-evidence publication");
    assert_eq!(venue.submissions().len(), 1);
    timeout(
        Duration::from_millis(500),
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }),
    )
    .await
    .expect("accepted publication blocked the current-thread runtime")
    .unwrap();
    control.close();
    assert!(
        control
            .prepare(&binding, proposal["approval_id"].as_str().unwrap())
            .await
            .is_err()
    );
    stop.cancel();
    assert!(
        timeout(Duration::from_millis(100), &mut confirm)
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(100), &mut server)
            .await
            .is_err(),
        "native owner must retain the actual publication, not just an observer"
    );
    release.send(()).unwrap();
    let _result = timeout(Duration::from_secs(5), confirm)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(evidence(&runtime, EventKind::SubmissionSigned).len(), 1);
    assert_eq!(evidence(&runtime, EventKind::SubmissionAccepted).len(), 1);
    assert_pending(&runtime, &cloid);
    assert_eq!(venue.submissions().len(), 1);
    drop(control);
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn ordinary_retained_worker_honors_cancellation_and_revocation_before_crypto_or_post() {
    for stage in ["key_cancel", "key_revoke", "signed_cancel", "ledger_cancel"] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let anchor = anchor(dir.path());
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Arc::new(
            Runtime::open_with_anchor(
                dir.path(),
                venue.port(),
                keys.clone(),
                Some(Box::new(EvidenceAnchorHandle(anchor.clone()))),
            )
            .await,
        );
        runtime.activate_orders().await;
        let cloid = Cloid::from_bytes([165; 16]);
        let entered = Arc::new(tokio::sync::Notify::new());
        let (mut release, wait) = std::sync::mpsc::channel();
        if stage == "signed_cancel" {
            *anchor.armed.lock().unwrap() =
                Some((EventKind::SubmissionSigned, PublicationAction::Hold(wait)));
        } else if stage == "ledger_cancel" {
            // Fresh synthetic wallet creation writes address history, key, then
            // wallet metadata. Retain the observed opaque entry, not its name
            // or contents; a changed creation sequence must fail this fixture.
            let entry = {
                let entries = keys.written_entries.lock().unwrap();
                let [_, entry, _] = entries.as_slice() else {
                    panic!("expected the three writes of one fresh synthetic wallet");
                };
                entry.clone()
            };
            *keys.read_return_gate.lock().unwrap() = Some((
                entry,
                KeyReadGate {
                    entered: entered.clone(),
                    release: wait,
                },
            ));
        } else {
            *keys.read_gate.lock().unwrap() = Some(KeyReadGate {
                entered: entered.clone(),
                release: wait,
            });
        }
        let calling = runtime.clone();
        let call = tokio::spawn(async move {
            let response = calling
                .request(
                    "POST",
                    Some(json!({"jsonrpc":"2.0","id":60,"method":"tools/call",
                "params":{"name":"place","arguments":place(cloid.as_str(), "0.12")}})),
                )
                .await;
            // Revocation may close the HTTP stream without a result frame.
            let _body = response.into_body().collect().await;
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            if stage == "signed_cancel" {
                anchor.reached.notified().await;
            } else {
                entered.notified().await;
            }
        })
        .await
        .expect("worker failed to reach the selected pre-crypto/pre-POST boundary");
        assert_eq!(
            runtime.gateway.inner.submission_worker.available_permits(),
            0
        );
        let holder = if stage == "ledger_cancel" {
            let (ledger_release, ledger_wait) = std::sync::mpsc::channel();
            *anchor.armed.lock().unwrap() = Some((
                EventKind::AgentDecision,
                PublicationAction::Hold(ledger_wait),
            ));
            let ledger = runtime.ledger.clone();
            let holder = tokio::task::spawn_blocking(move || {
                ledger
                    .append(&oppen_core::ledger::NewEvent {
                        kind: EventKind::AgentDecision,
                        ts_ms: now_ms() as i64,
                        agent_id: None,
                        payload: &json!({"fixture":"hold ledger coordination after key read"}),
                        snapshot: None,
                    })
                    .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(2), anchor.reached.notified())
                .await
                .unwrap();
            // The gate follows all fixture key-read ledger instrumentation.
            // Its release lets the worker acquire initial pairing admission
            // and then wait on the held ledger coordination lock.
            release.send(()).unwrap();
            release = ledger_release;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match runtime.pairings.try_write() {
                        Err(std::sync::TryLockError::WouldBlock) => break,
                        Err(std::sync::TryLockError::Poisoned(_)) => {
                            panic!("pairing lock poisoned")
                        }
                        Ok(guard) => drop(guard),
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("worker never retained initial signing admission before ledger wait");
            Some(holder)
        } else {
            None
        };
        let account_queue = runtime.gateway.execution_queue(runtime.account);
        assert!(
            account_queue.try_lock().is_err(),
            "worker must retain the account slot"
        );
        let replacement = if stage == "key_revoke" {
            let pairings = runtime.pairings.clone();
            let token = runtime.token.clone();
            Some(
                tokio::task::spawn_blocking(move || {
                    let mut pairings = pairings.write().unwrap();
                    let authority = pairings.authenticate(&token).unwrap().authority();
                    let replacement = pairings.issue(authority.binding().clone()).unwrap();
                    assert!(pairings.revoke(authority.id).unwrap());
                    replacement.reveal().to_owned()
                })
                .await
                .unwrap(),
            )
        } else {
            let response = runtime
                .request(
                    "POST",
                    Some(json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                "params":{"requestId":60,"reason":"synthetic cancellation"}})),
                )
                .await;
            assert_eq!(response.status(), 202);
            None
        };
        tokio::time::timeout(Duration::from_secs(2), call)
            .await
            .expect("ordinary method did not cancel while worker was stalled")
            .unwrap();
        assert_eq!(
            runtime.gateway.inner.submission_worker.available_permits(),
            0,
            "observer completion must not release actual worker ownership"
        );
        assert!(venue.submissions().is_empty());
        assert!(
            account_queue.try_lock().is_err(),
            "canceled observer released the account slot"
        );
        release.send(()).unwrap();
        if let Some(holder) = holder {
            holder.await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.gateway.inner.submission_worker.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("actual worker failed to drain");
        assert!(
            account_queue.try_lock().is_ok(),
            "drained worker retained the account slot"
        );
        assert!(
            venue.submissions().is_empty(),
            "{stage}: canceled worker posted"
        );
        assert!(evidence(&runtime, EventKind::SubmissionAccepted).is_empty());
        assert_eq!(
            evidence(&runtime, EventKind::SubmissionSigned).len(),
            usize::from(stage == "signed_cancel")
        );
        let mut runtime = Arc::try_unwrap(runtime)
            .ok()
            .expect("request observer retains runtime");
        if let Some(token) = replacement {
            runtime.token = token;
        }
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn uncertain_http_results_persist_only_signed_evidence_and_pending_liability() {
    for behavior in [
        Behavior::WrongKind,
        Behavior::WrongKindError,
        Behavior::ExtraStatus,
        Behavior::AppliedMalformed,
        Behavior::AppliedDropped,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        runtime.activate_orders().await;
        let cloid = Cloid::from_bytes([161; 16]);
        venue.next_response(behavior);
        let reply = runtime.call("place", place(cloid.as_str(), "0.12")).await;
        assert_eq!(
            reply["protocol_error"]["data"]["code"], "timeout_unknown_outcome",
            "{behavior:?}: {reply}"
        );
        assert_eq!(reply["protocol_error"]["data"]["retryable"], false);
        assert_eq!(reply["protocol_error"]["data"]["cloid"], cloid.as_str());
        let signed = evidence(&runtime, EventKind::SubmissionSigned);
        assert_eq!(signed.len(), 1, "{behavior:?}");
        assert!(
            evidence(&runtime, EventKind::SubmissionAccepted).is_empty(),
            "{behavior:?}"
        );
        assert_pending(&runtime, &cloid);
        assert_eq!(venue.submissions().len(), 1);
        runtime.shutdown().await;
        let runtime = Runtime::open(dir.path(), venue.port(), keys).await;
        assert_eq!(evidence(&runtime, EventKind::SubmissionSigned), signed);
        assert!(evidence(&runtime, EventKind::SubmissionAccepted).is_empty());
        assert_pending(&runtime, &cloid);
        assert_eq!(venue.submissions().len(), 1);
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn canceled_discretionary_cancel_retains_account_worker_and_server_until_key_read_drains() {
    use crate::server::BoundServer;
    use tokio::io::AsyncReadExt;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let cloid = Cloid::from_bytes([178; 16]);
    let placed = runtime.call("place", place(cloid.as_str(), "0.12")).await;
    assert_eq!(placed["status"], "resting", "{placed}");
    assert_eq!(evidence(&runtime, EventKind::SubmissionAccepted).len(), 1);
    runtime.reconcile().await;
    let bound = BoundServer::bind(0, &runtime.gateway, &runtime.pairings)
        .await
        .unwrap();
    let addr = bound.local_addr();
    let mut status = bound.supervision_status();
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    let mut server = tokio::spawn(bound.serve(
        runtime.gateway.clone(),
        runtime.pairings.clone(),
        stop.clone(),
    ));
    timeout(Duration::from_secs(5), async {
        while status.borrow_and_update().completed_sequence == 0 {
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let session = decision::initialize(addr, &runtime.token).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    *keys.read_gate.lock().unwrap() = Some(KeyReadGate {
        entered: entered.clone(),
        release: wait,
    });
    let mut response = transport::send_http_request(
        addr,
        decision::request(
            addr,
            &runtime.token,
            Some(&session),
            json!({"jsonrpc":"2.0","id":61,"method":"tools/call","params":{"name":"cancel",
            "arguments":{"oid":placed["oid"],"reason":"retained cancellation worker"}}}),
        ),
    )
    .await;
    timeout(Duration::from_secs(3), entered.notified())
        .await
        .expect("cancel never reached the actual key read");
    let queue = runtime.gateway.execution_queue(runtime.account);
    assert!(queue.try_lock().is_err());
    assert_eq!(
        runtime.gateway.inner.submission_worker.available_permits(),
        0
    );
    let heartbeat = Request::builder()
        .uri(crate::server::MCP_PATH)
        .header("host", addr.to_string())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        timeout(
            Duration::from_millis(500),
            transport::http_request(addr, heartbeat)
        )
        .await
        .expect("cancel key read blocked the current-thread HTTP runtime")
        .0,
        401
    );
    let notification = decision::request(
        addr,
        &runtime.token,
        Some(&session),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":61,"reason":"stop synthetic cancel"}}),
    );
    assert_eq!(
        timeout(
            Duration::from_millis(500),
            transport::http_request(addr, notification)
        )
        .await
        .unwrap()
        .0,
        202
    );
    timeout(
        Duration::from_secs(2),
        response.read_to_end(&mut Vec::new()),
    )
    .await
    .expect("method observer failed to cancel while key read was blocked")
    .unwrap();
    assert_eq!(venue.submissions().len(), 1);
    assert!(
        queue.try_lock().is_err(),
        "observer cancellation released the account slot"
    );
    assert_eq!(
        runtime.gateway.inner.submission_worker.available_permits(),
        0
    );
    stop.cancel();
    assert!(
        timeout(Duration::from_millis(100), &mut server)
            .await
            .is_err(),
        "serve returned while the actual cancel worker retained key loading"
    );
    release.send(()).unwrap();
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(queue.try_lock().is_ok());
    assert_eq!(
        runtime.gateway.inner.submission_worker.available_permits(),
        1
    );
    assert_eq!(
        venue.submissions().len(),
        1,
        "canceled worker submitted a cancellation"
    );
    assert_eq!(evidence(&runtime, EventKind::SubmissionSigned).len(), 1);
    assert_eq!(evidence(&runtime, EventKind::SubmissionAccepted).len(), 1);
    assert_eq!(
        runtime
            .call("get_order_status", json!({"oid":placed["oid"]}))
            .await["status"],
        "open"
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}
