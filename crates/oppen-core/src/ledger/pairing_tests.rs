use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::ledger::{Anchor, Appended, HeadAnchor};

fn key() -> Arc<HmacKey> {
    Arc::new(HmacKey::from_bytes([42; 32]))
}

fn binding() -> PairingBinding {
    PairingBinding {
        agent: AgentId::new("synthetic-agent"),
        account: Address::from_bytes([1; 20]),
    }
}

fn fixture() -> (TempDir, Arc<Ledger>, PairingJournal) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = PairingJournal::open(ledger.clone(), key()).unwrap();
    (dir, ledger, journal)
}

fn append_payload(
    ledger: &Ledger,
    payload: &Value,
    kind: EventKind,
    idem: &str,
    publish: bool,
) -> Appended {
    let mut guard = ledger.lock().unwrap();
    let tx = guard
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let appended = crate::ledger::append_keyed_in_tx(
        &tx,
        &NewEvent {
            kind,
            ts_ms: 100,
            agent_id: None,
            payload,
            snapshot: None,
        },
        idem,
    )
    .unwrap()
    .unwrap();
    tx.commit().unwrap();
    if publish {
        ledger.note_head(&appended).unwrap();
    }
    appended
}

fn next_grant(journal: &PairingJournal) -> Value {
    let head = journal.ledger.chain_head().unwrap();
    journal
        .signed(Envelope {
            version: 1,
            network: journal.network(),
            seq: head.seq + 1,
            prev_hash: head.hash,
            at_ms: 100,
            authority: Authority::PairingIssued {
                id: PairingId {
                    network: journal.network(),
                    issued_seq: head.seq + 1,
                },
                binding: binding(),
                digest: hex::encode([7; 32]),
            },
        })
        .unwrap()
}

#[test]
fn issuance_revocation_reopen_and_monotonic_ids() {
    let (dir, ledger, journal) = fixture();
    assert_eq!(journal.network(), Network::Testnet);
    assert!(journal.records().unwrap().is_empty());
    let first = journal.issue(binding(), [1; 32], 100).unwrap();
    ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: 101,
            agent_id: None,
            payload: &json!({"synthetic": true}),
            snapshot: None,
        })
        .unwrap();
    let second = journal.issue(binding(), [2; 32], 102).unwrap();
    assert!(second.id.issued_seq > first.id.issued_seq + 1);
    assert_eq!(first.id.to_string(), "pairing-testnet-1");
    assert!(journal.revoke(first.id, 103).unwrap());
    let head = ledger.chain_head().unwrap();
    assert!(!journal.revoke(first.id, 104).unwrap());
    assert!(
        !journal
            .revoke(
                PairingId {
                    network: Network::Testnet,
                    issued_seq: 999
                },
                104
            )
            .unwrap()
    );
    assert_eq!(ledger.chain_head().unwrap(), head);
    let records = journal.records().unwrap();
    assert_eq!(records[0].revoked_at_ms, Some(103));
    assert_eq!(records[0].binding, first.binding);
    assert_eq!(records[1], second);
    assert!(journal.issue(binding(), [1; 32], 105).is_err());
    {
        let agent = ledger.agent_view("synthetic-agent");
        for seq in [first.id.issued_seq, second.id.issued_seq, head.seq] {
            assert!(
                agent.event(seq).unwrap().is_none(),
                "hidden pairing row {seq}"
            );
            let operator = ledger.event(seq).unwrap().unwrap();
            assert!(matches!(
                operator.kind,
                EventKind::PairingIssued | EventKind::PairingRevoked
            ));
        }
        let operator = ledger.get_events(0, 100).unwrap();
        assert_eq!(
            operator
                .events
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    EventKind::PairingIssued | EventKind::PairingRevoked
                ))
                .count(),
            3
        );
        let short = agent.get_events(0, 2).unwrap();
        assert_eq!(short.events.len(), 1);
        assert_eq!(short.events[0].kind, EventKind::OperatorAction);
        assert!(agent.event(short.events[0].seq).unwrap().is_some());
        assert_eq!(
            short.next_cursor, head.seq,
            "short page must advance over the hidden tail"
        );
        assert_eq!(short.head_seq, head.seq);
        assert!(!short.resync_required);
        let full = agent.get_events(0, 1).unwrap();
        assert_eq!(full.events.len(), 1);
        let tail = agent.get_events(full.next_cursor, 1).unwrap();
        assert!(tail.events.is_empty());
        assert_eq!(tail.next_cursor, head.seq);
        assert!(!tail.resync_required);
    }
    assert!(
        ledger
            .agent_view("synthetic-agent")
            .get_events(0, 100)
            .unwrap()
            .events
            .iter()
            .all(|event| !matches!(
                event.kind,
                EventKind::PairingIssued | EventKind::PairingRevoked
            ))
    );
    let event = ledger.event(first.id.issued_seq).unwrap().unwrap();
    assert!(event.agent_id.is_none());
    assert_eq!(
        event.payload.as_ref().unwrap()["envelope"]["authority"]["digest"],
        json!(hex::encode([1; 32]))
    );
    assert!(!format!("{first:?}").contains(&hex::encode(first.digest)));
    drop(journal);
    drop(ledger);
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = PairingJournal::open(ledger, key()).unwrap();
    assert_eq!(journal.records().unwrap(), records);
}

#[test]
fn validation_and_wrong_network_do_not_mutate_history() {
    let (_dir, ledger, journal) = fixture();
    for id in ["", "a/b", "a b", "\n", &"x".repeat(65), "\u{e9}"] {
        let mut invalid = binding();
        invalid.agent = AgentId::new(id);
        assert!(journal.issue(invalid, [1; 32], 100).is_err());
    }
    let mut invalid = binding();
    invalid.account = Address::ZERO;
    assert!(journal.issue(invalid, [1; 32], 100).is_err());
    assert!(journal.issue(binding(), [1; 32], u64::MAX).is_err());
    assert!(journal.records().unwrap().is_empty());
    assert_eq!(ledger.chain_head().unwrap().seq, 0);
    let issued = journal.issue(binding(), [1; 32], 100).unwrap();
    let head = ledger.chain_head().unwrap();
    assert!(
        journal
            .revoke(
                PairingId {
                    network: Network::Mainnet,
                    ..issued.id
                },
                101
            )
            .is_err()
    );
    assert!(journal.revoke(issued.id, 99).is_err());
    let mut different = binding();
    different.account = Address::from_bytes([2; 20]);
    assert!(journal.issue(different, [1; 32], 101).is_err());
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert_eq!(journal.records().unwrap(), vec![issued]);
}

#[test]
fn mac_refuses_forged_grant_inside_anchor_lag_and_after_anchor_publication() {
    let (_dir, ledger, journal) = fixture();
    let mut payload = next_grant(&journal);
    payload["mac"] = json!(hex::encode([0; 32]));
    let forged = append_payload(
        &ledger,
        &payload,
        EventKind::PairingIssued,
        &format!("pairing_digest:{}", hex::encode([7; 32])),
        false,
    );
    assert!(
        ledger.verify().unwrap().is_intact(),
        "one unanchored row is accepted by chain verification"
    );
    assert!(
        matches!(journal.records(), Err(PairingError::Unavailable { detail }) if detail.contains("MAC"))
    );
    ledger.note_head(&forged).unwrap();
    assert!(ledger.verify().unwrap().is_intact());
    assert!(
        journal.records().is_err(),
        "later anchor publication cannot authenticate a forged grant"
    );
    let head = ledger.chain_head().unwrap();
    assert!(journal.issue(binding(), [8; 32], 101).is_err());
    assert_eq!(ledger.chain_head().unwrap(), head);
}

#[test]
fn every_authority_field_is_mac_bound_and_unsigned_payloads_are_refused() {
    for field in [
        "version",
        "network",
        "seq",
        "prev_hash",
        "at_ms",
        "id",
        "agent",
        "account",
        "digest",
        "mac",
        "missing_mac",
        "extra",
    ] {
        let (_dir, ledger, journal) = fixture();
        let mut payload = next_grant(&journal);
        match field {
            "version" => payload["envelope"]["version"] = json!(2),
            "network" => payload["envelope"]["network"] = json!(Network::Mainnet),
            "seq" => payload["envelope"]["seq"] = json!(2),
            "prev_hash" => payload["envelope"]["prev_hash"] = json!(hex::encode([1; 32])),
            "at_ms" => payload["envelope"]["at_ms"] = json!(101),
            "id" => payload["envelope"]["authority"]["id"]["issued_seq"] = json!(2),
            "agent" => payload["envelope"]["authority"]["binding"]["agent"] = json!("other"),
            "account" => {
                payload["envelope"]["authority"]["binding"]["account"] =
                    json!(Address::from_bytes([2; 20]))
            }
            "digest" => payload["envelope"]["authority"]["digest"] = json!(hex::encode([8; 32])),
            "mac" => payload["mac"] = json!("AB".repeat(32)),
            "missing_mac" => {
                payload.as_object_mut().unwrap().remove("mac");
            }
            "extra" => payload["untrusted"] = json!(true),
            _ => unreachable!(),
        }
        append_payload(
            &ledger,
            &payload,
            EventKind::PairingIssued,
            &format!("pairing_digest:{}", hex::encode([7; 32])),
            true,
        );
        assert!(ledger.verify().unwrap().is_intact());
        assert!(journal.records().is_err(), "{field}");
    }
}

#[test]
fn mac_bound_revocation_and_network_copy_cannot_be_replayed() {
    let (dir, ledger, journal) = fixture();
    let issued = journal.issue(binding(), [7; 32], 90).unwrap();
    let head = ledger.chain_head().unwrap();
    let mut payload = journal
        .signed(Envelope {
            version: 1,
            network: journal.network(),
            seq: head.seq + 1,
            prev_hash: head.hash,
            at_ms: 100,
            authority: Authority::PairingRevoked {
                id: issued.id,
                issued_hash: ledger.event(issued.id.issued_seq).unwrap().unwrap().hash,
            },
        })
        .unwrap();
    payload["envelope"]["authority"]["issued_hash"] = json!(hex::encode([0; 32]));
    append_payload(
        &ledger,
        &payload,
        EventKind::PairingRevoked,
        "pairing_revoked:1",
        true,
    );
    assert!(journal.records().is_err());

    let source = ledger
        .event(issued.id.issued_seq)
        .unwrap()
        .unwrap()
        .payload
        .unwrap();
    let other = Arc::new(Ledger::open(dir.path(), Network::Mainnet).unwrap());
    let other_journal = PairingJournal::open(other.clone(), key()).unwrap();
    append_payload(
        &other,
        &source,
        EventKind::PairingIssued,
        &format!("pairing_digest:{}", hex::encode([7; 32])),
        true,
    );
    assert!(other.verify().unwrap().is_intact());
    assert!(other_journal.records().is_err());
}

#[test]
fn replay_refuses_redaction_unsigned_keys_and_corruption_even_when_empty() {
    for target in ["issued", "revoked", "idem_key", "hash", "deleted", "anchor"] {
        let (dir, ledger, journal) = fixture();
        let issued = journal.issue(binding(), [1; 32], 100).unwrap();
        journal.revoke(issued.id, 101).unwrap();
        match target {
            "issued" => {
                ledger
                    .redact(issued.id.issued_seq, "synthetic", 102)
                    .unwrap();
            }
            "revoked" => {
                ledger.redact(2, "synthetic", 102).unwrap();
            }
            "idem_key" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("UPDATE events SET idem_key = NULL WHERE seq = 1", [])
                    .unwrap();
            }
            "hash" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("UPDATE events SET hash = 'broken' WHERE seq = 1", [])
                    .unwrap();
            }
            "deleted" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("DELETE FROM events", [])
                    .unwrap();
            }
            "anchor" => {
                let path = crate::ledger::FileAnchor::beside(
                    &dir.path().join(crate::db_file_name(Network::Testnet)),
                )
                .path()
                .to_owned();
                assert!(path.exists());
                std::fs::remove_file(path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(journal.records().is_err(), "{target}");
    }
}

#[test]
fn wrong_key_and_unanchored_open_fail_without_mutation_or_leaked_lease() {
    let (dir, ledger, journal) = fixture();
    journal.issue(binding(), [1; 32], 100).unwrap();
    let head = ledger.chain_head().unwrap();
    drop(journal);
    assert!(PairingJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([9; 32]))).is_err());
    assert_eq!(ledger.chain_head().unwrap(), head);
    let journal = PairingJournal::open(ledger.clone(), key()).unwrap();
    assert_eq!(journal.records().unwrap().len(), 1);
    let unanchored = Arc::new(
        Ledger::open_anchored(&dir.path().join("unanchored.db"), Network::Testnet, None).unwrap(),
    );
    assert!(PairingJournal::open(unanchored, key()).is_err());
}

#[test]
fn owner_lease_survives_session_arc_and_releases_only_with_last_owner() {
    let (_dir, ledger, journal) = fixture();
    let runtime = Arc::new(journal);
    let session = runtime.clone();
    drop(runtime);
    assert!(matches!(
        PairingJournal::open(ledger.clone(), key()),
        Err(PairingError::AlreadyOwned)
    ));
    drop(session);
    assert!(PairingJournal::open(ledger, key()).is_ok());
}

#[cfg(unix)]
#[test]
fn symlink_alias_cannot_acquire_a_second_owner() {
    let (dir, ledger, journal) = fixture();
    let alias = dir.path().join("alias.db");
    std::os::unix::fs::symlink(
        dir.path().join(crate::db_file_name(Network::Testnet)),
        &alias,
    )
    .unwrap();
    let other = Arc::new(Ledger::open_at(&alias, Network::Testnet).unwrap());
    assert!(matches!(
        PairingJournal::open(other.clone(), key()),
        Err(PairingError::AlreadyOwned)
    ));
    drop(journal);
    let owner = PairingJournal::open(other, key()).unwrap();
    assert!(matches!(
        PairingJournal::open(ledger, key()),
        Err(PairingError::AlreadyOwned)
    ));
    drop(owner);
}

#[test]
fn concurrent_issuance_has_one_winner_for_a_lifetime_digest() {
    let (_dir, ledger, journal) = fixture();
    let journal = Arc::new(journal);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let journal = journal.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                journal.issue(binding(), [1; 32], 100)
            })
        })
        .collect();
    barrier.wait();
    let successes = workers
        .into_iter()
        .filter_map(|worker| worker.join().unwrap().ok())
        .count();
    assert_eq!(successes, 1);
    assert_eq!(journal.records().unwrap().len(), 1);
    assert_eq!(ledger.chain_head().unwrap().seq, 1);
}

#[derive(Debug)]
struct FailingAnchor {
    head: Arc<Mutex<Option<Anchor>>>,
    fail: Arc<AtomicBool>,
}

impl HeadAnchor for FailingAnchor {
    fn load(&self) -> std::result::Result<Option<Anchor>, LedgerError> {
        Ok(self.head.lock().unwrap().clone())
    }
    fn store(&self, anchor: &Anchor) -> std::result::Result<(), LedgerError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(LedgerError::Io(std::io::Error::other(
                "synthetic anchor failure",
            )));
        }
        *self.head.lock().unwrap() = Some(anchor.clone());
        Ok(())
    }
}

#[test]
fn one_row_commit_before_anchor_failure_recovers_issuance_and_revocation() {
    for revoke in [false, true] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("anchor.db");
        let head = Arc::new(Mutex::new(None));
        let fail = Arc::new(AtomicBool::new(false));
        let anchor = || {
            Box::new(FailingAnchor {
                head: head.clone(),
                fail: fail.clone(),
            }) as Box<dyn HeadAnchor>
        };
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let journal = PairingJournal::open(ledger.clone(), key()).unwrap();
        let issued = if revoke {
            Some(journal.issue(binding(), [1; 32], 100).unwrap())
        } else {
            None
        };
        let before = ledger.chain_head().unwrap();
        fail.store(true, Ordering::SeqCst);
        if let Some(issued) = issued {
            assert!(journal.revoke(issued.id, 101).is_err());
        } else {
            assert!(journal.issue(binding(), [1; 32], 100).is_err());
        }
        assert_eq!(ledger.chain_head().unwrap().seq, before.seq + 1);
        assert_eq!(*head.lock().unwrap(), Some(before));
        drop(journal);
        drop(ledger);
        fail.store(false, Ordering::SeqCst);
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let journal = PairingJournal::open(ledger, key()).unwrap();
        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].revoked_at_ms, revoke.then_some(101));
    }
}
