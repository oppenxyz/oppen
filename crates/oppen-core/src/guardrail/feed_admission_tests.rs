use super::*;
use crate::feed::FeedSession;
use std::sync::{atomic::AtomicU64, mpsc};
use std::time::Duration;

fn disconnect(f: &DurableFixture, recover: bool) {
    use oppen_hl::ws::{ConnectionId, Disconnected, WsEvent};
    f.engine
        .feed()
        .apply(
            &f.ledger,
            &vault().to_string(),
            &WsEvent::Disconnected(Box::new(Disconnected {
                connection: ConnectionId::new(0),
                at_ms: NOW_MS,
                last_message_ms: Some(NOW_MS - 1),
                subscriptions: Vec::new(),
                unacked: Vec::new(),
                reason: "controlled disconnect".into(),
            })),
            NOW_MS,
        )
        .unwrap();
    if recover {
        let feed = f.engine.feed();
        feed.reconciled(&feed.stamp(), NOW_MS);
    }
}

fn assert_feed_refusal(
    result: Result<(oppen_hl::exchange::ExchangeRequest, Clearance), SignClearedError>,
) {
    assert!(matches!(
        result,
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::FeedAdmission
        )))
    ));
}

#[test]
fn feed_admission_foreign_session_and_missing_stamp_cannot_evaluate() {
    let f = DurableFixture::new();
    let foreign = FeedSession::new();
    foreign.bind(Network::Testnet, vault()).unwrap();
    foreign.reconciled(&foreign.stamp(), NOW_MS);
    for stamp in [None, Some(foreign.stamp())] {
        let mut snapshot = exposure(d("100000"));
        snapshot.feed_stamp = stamp;
        let result = f.engine.evaluate(
            &AgentId::new("alpha"),
            &approval_order(),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &snapshot,
            NOW_MS,
        );
        assert!(matches!(
            result,
            Err(Refusal::Unevaluable(Unevaluable::FeedAdmission))
        ));
    }
    assert!(f.events(EventKind::ApprovalProposed).is_empty());
}

#[test]
fn feed_admission_valid_route_for_another_account_does_not_borrow_readiness() {
    let other = oppen_hl::Address::from_bytes([8; 20]);
    let mut route = alpha_route();
    route.binding.container = other;
    route.binding.vault_address = Some(other);
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(test_store()),
        Arc::new(NullAuditSink::new([route])),
    );
    let mut snapshot = exposure(d("100000"));
    snapshot.account = other;
    assert!(matches!(
        f.engine.evaluate(
            &f.agent,
            &approval_order(),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &snapshot,
            NOW_MS
        ),
        Err(Refusal::Unevaluable(Unevaluable::FeedAdmission))
    ));
}

#[test]
fn feed_admission_reduce_only_is_gated_but_cleanup_still_signs() {
    let f = Fixture::new(permissive(&["BTC"]));
    let feed = f.engine.feed();
    let mut snapshot = exposure(d("100000"));
    snapshot
        .agent
        .positions
        .insert("BTC".into(), PositionSnapshot { szi: d("1") });
    snapshot.agent.total_position_notional_usd = d("100");
    let mut order = intent("BTC", false, d("100"), d("1"));
    order.reduce_only = true;
    let cleared = f
        .engine
        .evaluate(
            &f.agent,
            &order,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &snapshot,
            NOW_MS,
        )
        .unwrap();
    feed.unreconciled();
    assert_feed_refusal(f.engine.sign_cleared(cleared, NOW_MS, None, || NOW_MS));
    let cleanup = f
        .engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 0, o: 7 }],
            "cleanup during disconnect",
            NOW_MS,
        )
        .unwrap();
    let (request, _) = f
        .engine
        .sign_cleared(cleanup, NOW_MS + 1, None, || NOW_MS)
        .unwrap();
    assert!(matches!(request.action(), oppen_hl::Action::Cancel { .. }));
}

#[test]
fn feed_admission_reconnection_rejects_old_clearance_but_fresh_reevaluation_signs() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let old = f.approve(proposal.id()).unwrap();
    disconnect(&f, true);
    assert_feed_refusal(f.engine.sign_cleared(old, NOW_MS, None, || NOW_MS));
    let mut order = approval_order();
    order.cloid = Some(Cloid::parse("0xabababababababababababababababab").unwrap());
    let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(&order) else {
        panic!("fresh snapshot must reach approval");
    };
    let fresh = f.approve(&approval_id).unwrap();
    let (request, _) = f
        .engine
        .sign_cleared(fresh, NOW_MS + 1, None, || NOW_MS)
        .unwrap();
    assert!(matches!(request.action(), oppen_hl::Action::Order { .. }));
}

#[test]
fn feed_admission_disconnect_during_key_or_ledger_wait_prevents_signed_evidence() {
    for ledger_wait in [false, true] {
        for recover in [false, true] {
            let mut f = DurableFixture::new();
            let feed = f.engine.feed();
            feed.reconciled(&feed.stamp(), NOW_MS);
            let (loading, loaded) = mpsc::channel();
            let (release, released) = mpsc::channel();
            f.engine = GuardrailEngine::new(
                f.policy.clone(),
                Arc::new(WaitingKeys {
                    inner: f.keys.clone(),
                    wait: Some((loading, Mutex::new(released))),
                }),
                feed.clone(),
            )
            .unwrap();
            acknowledge(&f.engine);
            let (cleared, journal, receipt) = reserved(&f);
            let (authorized, authorizing) = mpsc::channel();
            std::thread::scope(|scope| {
                let signer = scope.spawn(|| {
                    f.engine.sign_submission_authorized(
                        cleared,
                        &journal,
                        &receipt,
                        NOW_MS,
                        None,
                        || NOW_MS,
                        || {
                            let _ = authorized.send(());
                            Ok(())
                        },
                    )
                });
                loaded.recv_timeout(Duration::from_secs(5)).unwrap();
                let lock = ledger_wait.then(|| {
                    let lock = std::fs::File::options()
                        .read(true)
                        .write(true)
                        .open(f.dir.path().join("approval.db.lock"))
                        .unwrap();
                    lock.lock().unwrap();
                    lock
                });
                if ledger_wait {
                    release.send(()).unwrap();
                    authorizing.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                disconnect(&f, recover);
                if !ledger_wait {
                    release.send(()).unwrap();
                }
                drop(lock);
                assert!(matches!(
                    signer.join().unwrap(),
                    Err(SignClearedError::Refused(Refusal::Unevaluable(
                        Unevaluable::FeedAdmission
                    )))
                ));
            });
            assert!(f.events(EventKind::SubmissionSigned).is_empty());
            assert!(f.events(EventKind::SubmissionAccepted).is_empty());
            assert!(journal.state(vault()).unwrap().pending.is_some());
            feed.reconciled(&feed.stamp(), NOW_MS);
        }
    }
}

#[test]
fn feed_admission_disconnect_during_publication_withholds_dispatch_capability() {
    let f = DurableFixture::new();
    let (cleared, journal, receipt) = reserved(&f);
    let (entered, observing) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.pause.lock().unwrap() = Some((f.ledger.chain_head().unwrap().seq + 1, entered, released));
    std::thread::scope(|scope| {
        let signer = scope.spawn(|| {
            f.engine.sign_submission_authorized(
                cleared,
                &journal,
                &receipt,
                NOW_MS,
                None,
                || NOW_MS,
                || Ok(()),
            )
        });
        observing.recv_timeout(Duration::from_secs(5)).unwrap();
        // Publication owns the ledger lock, but must have released feed/state.
        disconnect(&f, true);
        release.send(()).unwrap();
        assert!(matches!(
            signer.join().unwrap(),
            Err(SignClearedError::Refused(Refusal::Unevaluable(
                Unevaluable::FeedAdmission
            )))
        ));
    });
    assert_eq!(
        f.events(EventKind::SubmissionSigned).len(),
        1,
        "crypto preceded publication"
    );
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
    assert!(journal.state(vault()).unwrap().pending.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_admission_disconnect_during_dispatch_wait_never_polls_http() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let (client, listener) = transport().await;
    let (authorized, authorizing) = mpsc::channel();
    let executor = tokio::runtime::Handle::current();
    std::thread::scope(|scope| {
        let lock = std::fs::File::options()
            .read(true)
            .write(true)
            .open(f.dir.path().join("approval.db.lock"))
            .unwrap();
        lock.lock().unwrap();
        let posting = scope.spawn(|| {
            executor.block_on(f.engine.post_submission_authorized(
                capability,
                &client,
                || NOW_MS,
                || {
                    let _ = authorized.send(());
                    Ok(())
                },
            ))
        });
        authorizing.recv_timeout(Duration::from_secs(5)).unwrap();
        disconnect(&f, true);
        drop(lock);
        assert!(matches!(
            posting.join().unwrap(),
            Err(SubmissionPostError::NotSent(Refusal::Unevaluable(
                Unevaluable::FeedAdmission
            )))
        ));
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
}

#[test]
fn feed_admission_wait_precedes_final_authorization_and_deadline_clock() {
    let f = DurableFixture::new();
    let deadline = NOW_MS + APPROVAL_TTL_MS;
    let (cleared, journal, receipt) = reserved(&f);
    let feed = f.engine.feed();
    let stamp = feed.stamp();
    let held = feed.admit(Some(&stamp), Network::Testnet, vault()).unwrap();
    let clock = AtomicU64::new(NOW_MS);
    let (authorized, observing) = mpsc::channel();
    std::thread::scope(|scope| {
        let signer = scope.spawn(|| {
            f.engine.sign_submission_authorized(
                cleared,
                &journal,
                &receipt,
                NOW_MS,
                None,
                || clock.load(Ordering::SeqCst),
                || {
                    authorized.send(()).unwrap();
                    Ok(())
                },
            )
        });
        observing.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            observing.recv_timeout(Duration::from_millis(30)).is_err(),
            "final authorization must follow the blocked feed admission"
        );
        clock.store(deadline, Ordering::SeqCst);
        drop(held);
        assert!(
            matches!(signer.join().unwrap(), Err(SignClearedError::Refused(
            Refusal::Unevaluable(Unevaluable::ApprovalExpired { now_ms, .. }))) if now_ms == deadline)
        );
    });
    assert!(f.events(EventKind::SubmissionSigned).is_empty());
}
