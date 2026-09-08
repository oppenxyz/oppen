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
