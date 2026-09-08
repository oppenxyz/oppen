use super::*;
use crate::guardrail::{SignedSubmission, SubmissionPostError};
use crate::ledger::{SubmissionJournal, SubmissionReceipt};
use oppen_hl::exchange::ExchangeClient;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn reserved(f: &DurableFixture) -> (Cleared, SubmissionJournal, SubmissionReceipt) {
    let proposal = f.propose();
    let cleared = f.approve(proposal.id()).unwrap();
    let journal = f.engine.submissions().unwrap();
    let revision = journal.state(vault()).unwrap().revision;
    let receipt = journal
        .begin(vault(), cleared.clearance(), revision, NOW_MS)
        .unwrap();
    (cleared, journal, receipt)
}

fn signed(f: &DurableFixture) -> SignedSubmission {
    let (cleared, journal, receipt) = reserved(f);
    f.engine
        .sign_submission_authorized(
            cleared,
            &journal,
            &receipt,
            NOW_MS,
            None,
            || NOW_MS,
            || Ok(()),
        )
        .unwrap()
}

fn payload(f: &DurableFixture, kind: EventKind) -> Value {
    let rows = f.events(kind);
    assert_eq!(rows.len(), 1);
    rows[0].payload.clone().unwrap()
}

pub(super) async fn transport() -> (ExchangeClient, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = ExchangeClient::loopback_fixture(listener.local_addr().unwrap().port()).unwrap();
    (client, listener)
}

pub(super) async fn reply(listener: TcpListener, body: &'static str) -> Value {
    let (mut stream, _) =
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
    let mut bytes = Vec::new();
    let (end, len) = loop {
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes[..end]).unwrap();
            let len = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + len {
                break (end + 4, len);
            }
        }
    };
    let request = serde_json::from_slice(&bytes[end..end + len]).unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    request
}

const RESTING: &str =
    r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":731}}]}}}"#;

#[test]
fn submission_evidence_signing_is_digest_only_bound_and_not_acceptance() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let row = payload(&f, EventKind::SubmissionSigned);
    let request = &row["envelope"]["operation"]["request"];
    assert_eq!(
        request["start"]["seq"],
        f.events(EventKind::SubmissionStarted)[0].seq
    );
    assert_eq!(request["route"]["binding"]["agent"], "alpha");
    assert_eq!(request["cloid"], approval_order().cloid.unwrap().as_str());
    assert_eq!(request["action"]["type"], "order");
    assert_eq!(request["request_digest"].as_str().unwrap().len(), 64);
    assert!(!serde_json::to_string(&row).unwrap().contains("signature"));
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
    drop(capability);
    let f = f.reopen();
    assert!(f.engine.policy_status().admission_inhibited);
    assert!(
        f.engine
            .submissions()
            .unwrap()
            .state(vault())
            .unwrap()
            .pending
            .is_some()
    );
    assert_eq!(payload(&f, EventKind::SubmissionSigned), row);
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
}

#[test]
fn submission_evidence_failed_signing_publication_never_returns_capability_or_resigns() {
    let f = DurableFixture::new();
    let (cleared, journal, receipt) = reserved(&f);
    let clearance = cleared.clearance().clone();
    let head = f.ledger.chain_head().unwrap();
    let refusals = f.events(EventKind::Refusal).len();
    f.fail_seq.store(head.seq + 1, Ordering::SeqCst);
    let result = f.engine.sign_submission_authorized(
        cleared,
        &journal,
        &receipt,
        NOW_MS,
        None,
        || NOW_MS,
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::SubmissionAuthority { .. }
        )))
    ));
    assert_eq!(f.ledger.chain_head().unwrap().seq, head.seq + 1);
    assert_eq!(f.events(EventKind::Refusal).len(), refusals);
    f.fail_seq.store(0, Ordering::SeqCst);
    assert!(
        journal.verify_reserved(&receipt, &clearance).is_err(),
        "a committed signing row must forbid re-signing after anchor repair"
    );
    let f = f.reopen();
    assert_eq!(f.events(EventKind::SubmissionSigned).len(), 1);
    assert!(
        f.engine
            .submissions()
            .unwrap()
            .state(vault())
            .unwrap()
            .pending
            .is_some()
    );
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
}

#[tokio::test]
async fn submission_evidence_actual_response_binds_oid_and_reopens_without_resolving() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let (client, listener) = transport().await;
    let server = tokio::spawn(reply(listener, RESTING));
    let response = f
        .engine
        .post_submission_authorized(capability, &client, || NOW_MS, || Ok(()))
        .await
        .unwrap();
    assert!(matches!(
        response.statuses.as_slice(),
        [oppen_hl::exchange::Status::Resting { oid: 731 }]
    ));
    let actual = server.await.unwrap();
    let signed = payload(&f, EventKind::SubmissionSigned);
    // The durable commitment is independently reproducible from the actual
    // wire request, including fixed-width normalization of min-hex scalars.
    use sha3::{Digest, Keccak256};
    let scalar = |name: &str| {
        let wire = actual["signature"][name].as_str().unwrap();
        format!("0x{:0>64}", wire.strip_prefix("0x").unwrap())
    };
    let mut preimage = serde_json::json!({
        "network": "testnet",
        "ledger_genesis": signed["envelope"]["genesis"],
        "action": actual["action"],
        "nonce": actual["nonce"],
        "vault_address": actual["vaultAddress"],
        "expires_after": actual["expiresAfter"],
        "signature": { "r": scalar("r"), "s": scalar("s"), "v": actual["signature"]["v"] }
    });
    // Workspace feature unification may enable serde_json/preserve_order.
    // Sort nested objects explicitly, independently of the production encoder.
    preimage.sort_all_objects();
    let mut hash = Keccak256::new();
    hash.update(b"oppen.signed-submission.v1\0");
    hash.update(serde_json::to_string(&preimage).unwrap().as_bytes());
    assert_eq!(
        signed["envelope"]["operation"]["request"]["request_digest"],
        hex::encode(hash.finalize())
    );
    assert_eq!(
        actual["action"],
        signed["envelope"]["operation"]["request"]["action"]
    );
    let accepted = payload(&f, EventKind::SubmissionAccepted);
    assert_eq!(accepted["envelope"]["operation"]["oid"], 731);
    assert_eq!(
        accepted["envelope"]["operation"]["signed"]["seq"],
        f.events(EventKind::SubmissionSigned)[0].seq
    );
    assert!(f.events(EventKind::SubmissionResolved).is_empty());
    let f = f.reopen();
    assert!(
        f.engine
            .submissions()
            .unwrap()
            .state(vault())
            .unwrap()
            .pending
            .is_some()
    );
    assert_eq!(payload(&f, EventKind::SubmissionAccepted), accepted);
    assert!(f.ledger.verify().unwrap().is_intact());
}

#[tokio::test]
async fn submission_evidence_acceptance_publication_failure_is_unknown_not_not_sent() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let head = f.ledger.chain_head().unwrap();
    f.fail_seq.store(head.seq + 1, Ordering::SeqCst);
    let (client, listener) = transport().await;
    let server = tokio::spawn(reply(listener, RESTING));
    let result = f
        .engine
        .post_submission_authorized(capability, &client, || NOW_MS, || Ok(()))
        .await;
    server.await.unwrap();
    assert!(matches!(
        result,
        Err(SubmissionPostError::JournalUncertain(_))
    ));
    assert_eq!(f.ledger.chain_head().unwrap().seq, head.seq + 1);
    assert!(f.events(EventKind::SubmissionResolved).is_empty());
    f.fail_seq.store(0, Ordering::SeqCst);
    let f = f.reopen();
    assert!(
        f.engine
            .submissions()
            .unwrap()
            .state(vault())
            .unwrap()
            .pending
            .is_some()
    );
    assert_eq!(
        payload(&f, EventKind::SubmissionAccepted)["envelope"]["operation"]["oid"],
        731
    );
}

#[tokio::test]
async fn submission_evidence_dispatch_rechecks_owner_deadline_and_authorization() {
    for scenario in ["owner", "deadline", "authorization"] {
        let f = DurableFixture::new();
        let capability = signed(&f);
        let other = GuardrailEngine::new(f.policy.clone(), f.keys.clone()).unwrap();
        acknowledge(&other);
        let engine = if scenario == "owner" {
            &other
        } else {
            &f.engine
        };
        let now = if scenario == "deadline" {
            NOW_MS + APPROVAL_TTL_MS
        } else {
            NOW_MS
        };
        let (client, listener) = transport().await;
        let result = engine
            .post_submission_authorized(
                capability,
                &client,
                || now,
                || {
                    if scenario == "authorization" {
                        Err(Unevaluable::RouteAuthority {
                            detail: "synthetic revoked pairing".into(),
                        }
                        .into())
                    } else {
                        Ok(())
                    }
                },
            )
            .await;
        assert!(
            matches!(result, Err(SubmissionPostError::NotSent(_))),
            "{scenario}: {result:?}"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        assert!(f.events(EventKind::SubmissionAccepted).is_empty());
        assert!(f.events(EventKind::SubmissionResolved).is_empty());
    }
}

#[tokio::test]
async fn submission_evidence_nonaccepting_responses_do_not_mint_ownership_or_resolve() {
    for body in [
        r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"resting":{"oid":731}}]}}}"#,
        r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"fixture refusal"}]}}}"#,
        r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":731}},{"resting":{"oid":732}}]}}}"#,
    ] {
        let f = DurableFixture::new();
        let capability = signed(&f);
        let (client, listener) = transport().await;
        let server = tokio::spawn(reply(listener, body));
        let _ = f
            .engine
            .post_submission_authorized(capability, &client, || NOW_MS, || Ok(()))
            .await;
        server.await.unwrap();
        assert!(f.events(EventKind::SubmissionAccepted).is_empty());
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
}

#[tokio::test]
async fn submission_evidence_lost_response_retains_signed_only_unknown_reservation() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let (client, listener) = transport().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        assert!(stream.read(&mut buffer).await.unwrap() > 0);
        // Closing without a response cannot establish any venue acceptance.
    });
    let result = f
        .engine
        .post_submission_authorized(capability, &client, || NOW_MS, || Ok(()))
        .await;
    server.await.unwrap();
    assert!(matches!(result, Err(SubmissionPostError::Transport(_))));
    let f = f.reopen();
    assert!(
        f.engine
            .submissions()
            .unwrap()
            .state(vault())
            .unwrap()
            .pending
            .is_some()
    );
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
}

#[test]
fn submission_evidence_authority_mismatch_refuses_before_authorize_or_signing() {
    let f = DurableFixture::new();
    let (cleared, _, receipt) = reserved(&f);
    let other = DurableFixture::new();
    let journal = other.engine.submissions().unwrap();
    let result = f.engine.sign_submission_authorized(
        cleared,
        &journal,
        &receipt,
        NOW_MS,
        None,
        || NOW_MS,
        || -> Result<(), Refusal> {
            panic!("wrong journal must be refused before caller authorization")
        },
    );
    assert!(matches!(
        result,
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::SubmissionAuthority { .. }
        )))
    ));
    assert!(f.events(EventKind::SubmissionSigned).is_empty());
}

#[test]
fn submission_evidence_append_failure_returns_no_capability() {
    let f = DurableFixture::new();
    let (cleared, journal, receipt) = reserved(&f);
    let head = f.ledger.chain_head().unwrap();
    let raw = rusqlite::Connection::open(f.dir.path().join("approval.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER fail_signed BEFORE INSERT ON events WHEN NEW.kind = 'submission_signed' BEGIN SELECT RAISE(ABORT, 'synthetic signing append failure'); END;").unwrap();
    let result = f.engine.sign_submission_authorized(
        cleared,
        &journal,
        &receipt,
        NOW_MS,
        None,
        || NOW_MS,
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::SubmissionAuthority { .. }
        )))
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert!(f.events(EventKind::SubmissionSigned).is_empty());
    assert!(journal.state(vault()).unwrap().pending.is_some());
}

#[test]
fn submission_evidence_deadline_is_rechecked_after_publication() {
    let f = DurableFixture::new();
    let (cleared, journal, receipt) = reserved(&f);
    let samples = std::cell::Cell::new(0);
    let result = f.engine.sign_submission_authorized(
        cleared,
        &journal,
        &receipt,
        NOW_MS,
        None,
        || {
            let count = samples.get();
            samples.set(count + 1);
            if count == 0 {
                NOW_MS
            } else {
                NOW_MS + APPROVAL_TTL_MS
            }
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::ApprovalExpired { .. }
        )))
    ));
    assert_eq!(f.events(EventKind::SubmissionSigned).len(), 1);
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
    assert!(journal.state(vault()).unwrap().pending.is_some());
}

#[tokio::test]
async fn submission_evidence_dropped_post_does_not_publish_acceptance() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let (client, listener) = transport().await;
    {
        let mut post = std::pin::pin!(f.engine.post_submission_authorized(
            capability,
            &client,
            || NOW_MS,
            || Ok(())
        ));
        let accepted = tokio::select! {
            result = &mut post => panic!("request unexpectedly completed: {result:?}"),
            accepted = listener.accept() => accepted.unwrap(),
        };
        drop(accepted);
        // Dropping the owned future cannot publish an invented response.
    }
    let f = f.reopen();
    assert!(
        f.engine
            .submissions()
            .unwrap()
            .state(vault())
            .unwrap()
            .pending
            .is_some()
    );
    assert_eq!(f.events(EventKind::SubmissionSigned).len(), 1);
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
}

#[tokio::test]
async fn submission_evidence_redacted_signature_blocks_post_and_reopened_replay() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let seq = f.events(EventKind::SubmissionSigned)[0].seq;
    f.ledger
        .redact(seq, "synthetic evidence retention", NOW_MS as i64)
        .unwrap();
    assert!(
        f.ledger.verify().unwrap().is_intact(),
        "a valid tombstone does not break the event chain"
    );
    let (client, listener) = transport().await;
    let result = f
        .engine
        .post_submission_authorized(capability, &client, || NOW_MS, || Ok(()))
        .await;
    assert!(matches!(
        result,
        Err(SubmissionPostError::NotSent(Refusal::Unevaluable(
            Unevaluable::SubmissionAuthority { .. }
        )))
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
    assert!(f.events(EventKind::SubmissionResolved).is_empty());
    let f = f.reopen();
    assert!(f.engine.submissions().unwrap().state(vault()).is_err());
    assert!(f.ledger.verify().unwrap().is_intact());
}

#[tokio::test]
async fn submission_evidence_corrupt_mac_blocks_post_without_repairing_history() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let mut row = payload(&f, EventKind::SubmissionSigned);
    row["mac"] = Value::String("00".repeat(32));
    let raw = rusqlite::Connection::open(f.dir.path().join("approval.db")).unwrap();
    raw.execute(
        "UPDATE events SET payload = ?1 WHERE kind = 'submission_signed'",
        [serde_json::to_string(&row).unwrap()],
    )
    .unwrap();
    drop(raw);
    let (client, listener) = transport().await;
    let result = f
        .engine
        .post_submission_authorized(capability, &client, || NOW_MS, || Ok(()))
        .await;
    assert!(matches!(result, Err(SubmissionPostError::NotSent(_))));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
    assert!(f.events(EventKind::SubmissionResolved).is_empty());
    assert!(!f.ledger.verify().unwrap().is_intact());
    assert!(f.engine.submissions().unwrap().state(vault()).is_err());
}

#[tokio::test]
async fn submission_evidence_redacted_acceptance_is_not_silently_lost_on_reopen() {
    let f = DurableFixture::new();
    let capability = signed(&f);
    let (client, listener) = transport().await;
    let server = tokio::spawn(reply(listener, RESTING));
    f.engine
        .post_submission_authorized(capability, &client, || NOW_MS, || Ok(()))
        .await
        .unwrap();
    server.await.unwrap();
    let seq = f.events(EventKind::SubmissionAccepted)[0].seq;
    f.ledger
        .redact(seq, "synthetic acceptance retention", NOW_MS as i64)
        .unwrap();
    let f = f.reopen();
    assert!(f.ledger.verify().unwrap().is_intact());
    assert!(f.engine.submissions().unwrap().state(vault()).is_err());
    assert!(f.events(EventKind::SubmissionResolved).is_empty());
}

#[test]
fn submission_evidence_cancellation_during_ledger_wait_refuses_before_crypto() {
    use std::sync::{RwLock, atomic::AtomicBool, mpsc};
    use std::time::Duration;

    let mut f = DurableFixture::new();
    let (loading, loaded) = mpsc::channel();
    let (release_keys, keys_released) = mpsc::channel();
    f.engine = GuardrailEngine::new(
        f.policy.clone(),
        Arc::new(WaitingKeys {
            inner: f.keys.clone(),
            wait: Some((loading, Mutex::new(keys_released))),
        }),
    )
    .unwrap();
    acknowledge(&f.engine);
    let (cleared, journal, receipt) = reserved(&f);
    let cancelled = AtomicBool::new(false);
    let pairing = RwLock::new(());
    let (authorized, initial_authorized) = mpsc::channel();
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
                    if cancelled.load(Ordering::SeqCst) {
                        return Err(Unevaluable::RouteAuthority {
                            detail: "synthetic cancellation".into(),
                        }
                        .into());
                    }
                    let guard = pairing.try_read().unwrap();
                    authorized.send(()).unwrap();
                    Ok(guard)
                },
            )
        });
        // Reservation verification is complete before loading reaches this seam.
        loaded.recv_timeout(Duration::from_secs(5)).unwrap();
        let ledger = std::fs::File::options()
            .read(true)
            .write(true)
            .open(f.dir.path().join("approval.db.lock"))
            .unwrap();
        ledger.lock().unwrap();
        release_keys.send(()).unwrap();
        initial_authorized
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(
            pairing.try_write().is_err(),
            "initial caller guard stays held during the ledger wait"
        );
        cancelled.store(true, Ordering::SeqCst);
        drop(ledger);
        let result = signer.join().unwrap();
        assert!(matches!(
            result,
            Err(SignClearedError::Refused(Refusal::Unevaluable(
                Unevaluable::RouteAuthority { .. }
            )))
        ));
    });
    assert!(pairing.try_write().is_ok());
    assert!(f.events(EventKind::SubmissionSigned).is_empty());
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
    assert!(journal.state(vault()).unwrap().pending.is_some());
    assert!(
        f.events(EventKind::Refusal).iter().any(|row| row
            .payload
            .as_ref()
            .is_some_and(|payload| payload["reason"] == "pre-sign gate")),
        "refusal audit completes after the retained guards drop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submission_evidence_cancellation_during_dispatch_ledger_wait_never_polls_http() {
    use std::sync::{RwLock, atomic::AtomicBool, mpsc};
    use std::time::Duration;

    let f = DurableFixture::new();
    let capability = signed(&f);
    let (client, listener) = transport().await;
    let cancelled = AtomicBool::new(false);
    let pairing = RwLock::new(());
    let (authorized, initial_authorized) = mpsc::channel();
    let runtime = tokio::runtime::Handle::current();
    // Keep synchronous ledger admission off the async worker, as production's
    // retained order worker does. This scoped worker cannot outlive the test.
    std::thread::scope(|scope| {
        let ledger = std::fs::File::options()
            .read(true)
            .write(true)
            .open(f.dir.path().join("approval.db.lock"))
            .unwrap();
        ledger.lock().unwrap();
        let post = scope.spawn(|| {
            runtime.block_on(f.engine.post_submission_authorized(
                capability,
                &client,
                || NOW_MS,
                || {
                    if cancelled.load(Ordering::SeqCst) {
                        return Err(Unevaluable::RouteAuthority {
                            detail: "synthetic shutdown".into(),
                        }
                        .into());
                    }
                    let guard = pairing.try_read().unwrap();
                    authorized.send(()).unwrap();
                    Ok(guard)
                },
            ))
        });
        initial_authorized
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(pairing.try_write().is_err());
        cancelled.store(true, Ordering::SeqCst);
        drop(ledger);
        assert!(matches!(
            post.join().unwrap(),
            Err(SubmissionPostError::NotSent(Refusal::Unevaluable(
                Unevaluable::RouteAuthority { .. }
            )))
        ));
    });
    assert!(pairing.try_write().is_ok());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert!(f.events(EventKind::SubmissionAccepted).is_empty());
    assert!(f.events(EventKind::SubmissionResolved).is_empty());
}
