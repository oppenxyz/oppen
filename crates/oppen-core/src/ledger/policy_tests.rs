use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::guardrail::{AgentGuardrails, AgentId};
use crate::keys::HmacKey;
use crate::ledger::Ledger;

fn key() -> Arc<HmacKey> {
    Arc::new(HmacKey::from_bytes([42; 32]))
}

fn fixture() -> (TempDir, Arc<Ledger>, Arc<RegistryJournal>, PolicyJournal) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let registry = Arc::new(RegistryJournal::open(ledger.clone(), key()).unwrap());
    let journal = PolicyJournal::new(registry.clone());
    (dir, ledger, registry, journal)
}

fn state() -> PersistedState {
    let mut state = PersistedState::default();
    state
        .guardrails
        .insert(AgentId::new("synthetic-agent"), AgentGuardrails::default());
    let mut encoded = serde_json::to_value(state).unwrap();
    encoded["kill"]["global"] = json!({
        "engaged_at_ms": 90,
        "reason": {"reason": "operator"},
    });
    serde_json::from_value(encoded).unwrap()
}

fn review(dir: &TempDir) -> LegacyPolicyReview {
    LegacyPolicyReview::open(
        dir.path().join("guardrails-testnet.db"),
        Network::Testnet,
        90,
    )
    .unwrap()
}

fn append_payload(
    ledger: &Ledger,
    payload: &Value,
    kind: EventKind,
    idem_key: &str,
    publish: bool,
) -> super::super::Appended {
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
        idem_key,
    )
    .unwrap()
    .unwrap();
    tx.commit().unwrap();
    if publish {
        ledger.note_head(&appended).unwrap();
    }
    appended
}

#[test]
fn constructor_never_initializes_or_substitutes_empty_policy() {
    let (_dir, ledger, registry, journal) = fixture();
    let before = ledger.chain_head().unwrap();
    let second = PolicyJournal::new(registry);
    assert!(matches!(
        journal.current(),
        Err(PolicyError::MigrationRequired)
    ));
    assert!(matches!(
        second.current(),
        Err(PolicyError::MigrationRequired)
    ));
    assert!(matches!(
        journal.replace(0, state(), 100),
        Err(PolicyError::MigrationRequired)
    ));
    assert_eq!(ledger.chain_head().unwrap(), before);
}

#[test]
fn inspection_is_read_only_and_explicitly_does_not_authenticate_mac() {
    let (dir, ledger, _registry, journal) = fixture();
    assert_eq!(journal.network(), Network::Testnet);
    assert_eq!(PolicyJournal::inspect(&ledger).unwrap(), None);
    let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
    let before = ledger.chain_head().unwrap();
    assert_eq!(
        PolicyJournal::inspect(&ledger).unwrap(),
        Some(initial.clone())
    );
    assert_eq!(ledger.chain_head().unwrap(), before);
    let mut payload = ledger
        .event(initial.revision)
        .unwrap()
        .unwrap()
        .payload
        .unwrap();
    payload["mac"] = json!("0".repeat(64));
    replace_tail_payload(
        &ledger,
        &crate::ledger::hash::canonical_json(&payload).unwrap(),
    );
    let before = ledger.chain_head().unwrap();
    assert!(journal.current().is_err());
    assert_eq!(PolicyJournal::inspect(&ledger).unwrap(), Some(initial));
    assert_eq!(ledger.chain_head().unwrap(), before);
}

#[test]
fn inspection_never_treats_malformed_or_redacted_policy_as_absent() {
    for damage in ["redaction", "duplicate", "missing_field", "deletion"] {
        let (dir, ledger, _registry, journal) = fixture();
        let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
        match damage {
            "redaction" => {
                ledger.redact(initial.revision, "synthetic", 101).unwrap();
            }
            "deletion" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("DELETE FROM events", [])
                    .unwrap();
            }
            _ => {
                let mut payload = ledger
                    .event(initial.revision)
                    .unwrap()
                    .unwrap()
                    .payload
                    .unwrap();
                let raw = if damage == "duplicate" {
                    let raw = crate::ledger::hash::canonical_json(&payload).unwrap();
                    format!(
                        "{{\"mac\":\"{}\",{}",
                        payload["mac"].as_str().unwrap(),
                        &raw[1..]
                    )
                } else {
                    payload["envelope"]["state"]
                        .as_object_mut()
                        .unwrap()
                        .remove("kill");
                    crate::ledger::hash::canonical_json(&payload).unwrap()
                };
                replace_tail_payload(&ledger, &raw);
            }
        }
        assert!(PolicyJournal::inspect(&ledger).is_err(), "{damage}");
    }
}

#[test]
fn missing_policy_still_verifies_the_chain_before_returning_migration_required() {
    let (_dir, ledger, _registry, journal) = fixture();
    ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: 100,
            agent_id: None,
            payload: &json!({"synthetic": true}),
            snapshot: None,
        })
        .unwrap();
    ledger
        .lock()
        .unwrap()
        .execute("DELETE FROM events", [])
        .unwrap();
    assert!(matches!(
        journal.current(),
        Err(PolicyError::Unavailable { .. })
    ));
    assert!(matches!(
        journal.replace(0, state(), 101),
        Err(PolicyError::Unavailable { .. })
    ));
}

#[test]
fn forged_unanchored_policy_is_not_blessed_by_later_anchor_publication() {
    let (_dir, ledger, _registry, journal) = fixture();
    let head = ledger.chain_head().unwrap();
    let envelope = Envelope {
        version: 1,
        network: Network::Testnet,
        seq: head.seq + 1,
        prev_hash: head.hash,
        at_ms: 100,
        previous_policy: None,
        operation: Operation::PolicyReplaced,
        state: state(),
    };
    let payload = serde_json::to_value(Signed {
        envelope,
        mac: hex::encode([0; 32]),
    })
    .unwrap();
    let forged = append_payload(
        &ledger,
        &payload,
        EventKind::PolicyReplaced,
        "policy_after:0",
        false,
    );
    assert!(ledger.verify().unwrap().is_intact());
    assert!(
        matches!(journal.current(), Err(PolicyError::Unavailable { detail }) if detail.contains("MAC"))
    );
    ledger.note_head(&forged).unwrap();
    assert!(ledger.verify().unwrap().is_intact());
    assert!(
        matches!(journal.current(), Err(PolicyError::Unavailable { detail }) if detail.contains("MAC"))
    );
}

#[test]
fn valid_mac_cannot_replace_a_missing_initialization() {
    let (_dir, ledger, registry, journal) = fixture();
    let head = ledger.chain_head().unwrap();
    let envelope = Envelope {
        version: 1,
        network: Network::Testnet,
        seq: head.seq + 1,
        prev_hash: head.hash,
        at_ms: 100,
        previous_policy: None,
        operation: Operation::PolicyReplaced,
        state: state(),
    };
    let mac = registry
        .authority_key()
        .sign(message(&envelope).unwrap().as_bytes())
        .unwrap();
    let payload = serde_json::to_value(Signed {
        envelope,
        mac: hex::encode(mac),
    })
    .unwrap();
    append_payload(
        &ledger,
        &payload,
        EventKind::PolicyReplaced,
        "policy_after:0",
        true,
    );
    assert!(
        matches!(journal.current(), Err(PolicyError::Unavailable { detail }) if detail.contains("initialization"))
    );
}

#[test]
fn explicit_paused_initialization_replacement_and_reopen_use_event_revision() {
    let (dir, ledger, _registry, journal) = fixture();
    let review = review(&dir);
    let mut unpaused = state();
    unpaused.kill = Default::default();
    assert!(journal.initialize(&review, unpaused, 100).is_err());
    assert_eq!(ledger.chain_head().unwrap().seq, 0);
    let initialized = journal.initialize(&review, state(), 100).unwrap();
    assert_eq!(initialized.revision, ledger.chain_head().unwrap().seq);
    assert_eq!(
        journal.initialize(&review, state(), 101).unwrap(),
        initialized
    );
    ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: 102,
            agent_id: None,
            payload: &json!({"synthetic": true}),
            snapshot: None,
        })
        .unwrap();
    let mut next = initialized.state.clone();
    next.guardrails
        .get_mut(&AgentId::new("synthetic-agent"))
        .unwrap()
        .approval_required = false;
    let replaced = journal
        .replace(initialized.revision, next.clone(), 103)
        .unwrap();
    assert!(replaced.revision > initialized.revision + 1);
    assert_eq!(replaced.state, next);
    assert_eq!(journal.current().unwrap(), replaced);
    let head = ledger.chain_head().unwrap();
    assert_eq!(
        journal.replace(initialized.revision, next, 104).unwrap(),
        replaced
    );
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert!(journal.initialize(&review, state(), 105).is_err());
    drop(journal);
    drop(_registry);
    drop(ledger);
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = PolicyJournal::new(Arc::new(RegistryJournal::open(ledger, key()).unwrap()));
    assert_eq!(journal.current().unwrap(), replaced);
}

#[test]
fn changed_legacy_evidence_and_wrong_network_review_refuse_initialization() {
    let (dir, ledger, _registry, journal) = fixture();
    let old = review(&dir);
    let legacy = Connection::open(dir.path().join("guardrails-testnet.db")).unwrap();
    legacy.execute_batch("CREATE TABLE legacy_unknown(value TEXT); INSERT INTO legacy_unknown VALUES ('unreviewed')").unwrap();
    assert!(journal.initialize(&old, state(), 100).is_err());
    assert_eq!(ledger.chain_head().unwrap().seq, 0);
    let wrong = LegacyPolicyReview::open(
        dir.path().join("guardrails-testnet.db"),
        Network::Mainnet,
        90,
    )
    .unwrap();
    assert!(journal.initialize(&wrong, state(), 100).is_err());
    let reviewed = review(&dir);
    let version = journal.initialize(&reviewed, state(), 100).unwrap();
    let payload = ledger
        .event(version.revision)
        .unwrap()
        .unwrap()
        .payload
        .unwrap();
    assert_eq!(
        payload["envelope"]["operation"]["review"],
        serde_json::to_value(reviewed.evidence()).unwrap()
    );
    assert_eq!(
        payload["envelope"]["operation"]["rechecked_at_ms"],
        json!(100)
    );
    assert_eq!(
        legacy
            .query_row("SELECT value FROM legacy_unknown", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "unreviewed"
    );
}

#[test]
fn stale_replacements_cannot_overwrite_other_policy_or_kill_scopes() {
    let (dir, ledger, _registry, journal) = fixture();
    let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
    let other = PolicyJournal::new(Arc::new(
        RegistryJournal::open(
            Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap()),
            key(),
        )
        .unwrap(),
    ));
    let mut next = serde_json::to_value(&initial.state).unwrap();
    next["kill"]["agents"]["other"] =
        json!({"engaged_at_ms": 101, "reason": {"reason": "operator"}});
    let winner = other
        .replace(initial.revision, serde_json::from_value(next).unwrap(), 101)
        .unwrap();
    let mut stale = initial.state;
    stale.account_limits.max_daily_loss_usd = Some(rust_decimal::Decimal::from(99));
    assert!(matches!(journal.replace(initial.revision, stale, 102),
        Err(PolicyError::StaleRevision { expected, actual }) if expected == initial.revision && actual == winner.revision));
    assert_eq!(journal.current().unwrap(), winner);
    assert_eq!(ledger.chain_head().unwrap().seq, winner.revision);
}

#[test]
fn concurrent_independent_handles_have_one_cas_winner() {
    let (dir, ledger, registry, journal) = fixture();
    let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
    let other = PolicyJournal::new(Arc::new(
        RegistryJournal::open(
            Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap()),
            key(),
        )
        .unwrap(),
    ));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = [journal, other]
        .into_iter()
        .enumerate()
        .map(|(index, journal)| {
            let barrier = barrier.clone();
            let mut next = initial.state.clone();
            next.account_limits.max_daily_loss_usd =
                Some(rust_decimal::Decimal::from(index as u64 + 1));
            let expected = initial.revision;
            std::thread::spawn(move || {
                barrier.wait();
                journal.replace(expected, next, 101)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(PolicyError::StaleRevision { .. })))
            .count(),
        1
    );
    assert_eq!(
        PolicyJournal::new(registry).current().unwrap().revision,
        initial.revision + 1
    );
    assert_eq!(ledger.chain_head().unwrap().seq, initial.revision + 1);
}

#[test]
fn generic_writers_cannot_mint_policy_authority() {
    let (_dir, ledger, _registry, _journal) = fixture();
    let receipt = ledger
        .record_intent(&crate::ledger::NewIntent {
            ts_ms: 90,
            agent_id: "synthetic-agent",
            payload: &json!({"synthetic": true}),
            snapshot: None,
        })
        .unwrap();
    let head = ledger.chain_head().unwrap();
    for kind in [EventKind::PolicyInitialized, EventKind::PolicyReplaced] {
        assert!(matches!(
            ledger.append(&NewEvent {
                kind,
                ts_ms: 100,
                agent_id: None,
                payload: &json!({}),
                snapshot: None,
            }),
            Err(LedgerError::UsePolicyJournal)
        ));
        assert!(matches!(
            ledger.record_outcome(&receipt, kind, 100, &json!({})),
            Err(LedgerError::UsePolicyJournal)
        ));
    }
    assert_eq!(ledger.chain_head().unwrap(), head);
}

#[test]
fn agent_pages_and_single_events_hide_complete_policy_with_cursor_progress() {
    let (dir, ledger, _registry, journal) = fixture();
    let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
    let global = ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: 101,
            agent_id: None,
            payload: &json!({"synthetic": true}),
            snapshot: None,
        })
        .unwrap();
    let mut next = state();
    next.account_limits.max_daily_loss_usd = Some(rust_decimal::Decimal::from(5));
    let replaced = journal.replace(initial.revision, next, 102).unwrap();
    for name in ["synthetic-agent", "other"] {
        let agent = ledger.agent_view(name);
        for seq in [initial.revision, replaced.revision] {
            assert!(agent.event(seq).unwrap().is_none());
            assert!(ledger.event(seq).unwrap().unwrap().payload.is_some());
        }
        let short = agent.get_events(0, 2).unwrap();
        assert_eq!(short.events.len(), 1);
        assert_eq!(short.events[0].seq, global.seq);
        assert_eq!(short.next_cursor, replaced.revision);
        assert!(!short.resync_required);
        assert!(agent.event(global.seq).unwrap().is_some());
        let full = agent.get_events(0, 1).unwrap();
        let tail = agent.get_events(full.next_cursor, 1).unwrap();
        assert!(tail.events.is_empty());
        assert_eq!(tail.next_cursor, replaced.revision);
    }
    assert_eq!(ledger.get_events(0, 100).unwrap().events.len(), 3);
}

fn replace_tail_payload(ledger: &Ledger, raw: &str) {
    let head = ledger.chain_head().unwrap();
    let event = ledger.event(head.seq).unwrap().unwrap();
    let payload_hash = crate::ledger::hash::payload_hash(raw.as_bytes());
    let hash = crate::ledger::hash::row_hash(&crate::ledger::hash::RowHashInput {
        prev_hash: &event.prev_hash,
        seq: event.seq,
        kind: event.kind.as_str(),
        ts_ms: event.ts_ms,
        agent_id: event.agent_id.as_deref(),
        payload_hash: &payload_hash,
        snapshot_id: event.snapshot_id.as_deref(),
        snapshot_hash: event.snapshot_hash.as_deref(),
    });
    let mut guard = ledger.lock().unwrap();
    let tx = guard
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "UPDATE events SET payload = ?1, payload_hash = ?2, hash = ?3 WHERE seq = ?4",
        rusqlite::params![raw, payload_hash, hash, event.seq],
    )
    .unwrap();
    tx.execute(
        "UPDATE chain_head SET hash = ?1 WHERE id = 0",
        rusqlite::params![hash],
    )
    .unwrap();
    tx.commit().unwrap();
    ledger
        .note_head(&crate::ledger::Appended {
            seq: event.seq,
            hash,
        })
        .unwrap();
}

#[test]
fn duplicate_keys_in_raw_sql_json_fail_even_when_value_and_mac_are_unchanged() {
    for field in ["map", "nested", "root"] {
        let (dir, ledger, _registry, journal) = fixture();
        let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
        let payload = ledger
            .event(initial.revision)
            .unwrap()
            .unwrap()
            .payload
            .unwrap();
        let raw = crate::ledger::hash::canonical_json(&payload).unwrap();
        let duplicated = match field {
            "map" => raw.replacen(
                "\"guardrails\":{",
                &format!(
                    "\"guardrails\":{{\"synthetic-agent\":{},",
                    serde_json::to_string(
                        &initial.state.guardrails[&AgentId::new("synthetic-agent")]
                    )
                    .unwrap()
                ),
                1,
            ),
            "nested" => raw.replacen(
                "\"approval_required\":",
                "\"approval_required\":false,\"approval_required\":",
                1,
            ),
            "root" => format!(
                "{{\"mac\":\"{}\",{}",
                payload["mac"].as_str().unwrap(),
                &raw[1..]
            ),
            _ => unreachable!(),
        };
        assert_ne!(duplicated, raw);
        assert_eq!(serde_json::from_str::<Value>(&duplicated).unwrap(), payload);
        replace_tail_payload(&ledger, &duplicated);
        assert!(ledger.verify().unwrap().is_intact());
        assert!(
            matches!(journal.current(), Err(PolicyError::Unavailable { detail }) if detail.contains("duplicate")),
            "{field}"
        );
    }
}

#[test]
fn required_missing_unknown_or_tampered_snapshot_fields_never_default() {
    for field in [
        "map_entry",
        "approval",
        "kill",
        "global",
        "account_limits",
        "optional_limit",
        "unknown_nested",
        "numeric_money",
        "revision",
        "predecessor",
        "network",
        "review",
    ] {
        let (dir, ledger, _registry, journal) = fixture();
        let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
        let mut payload = ledger
            .event(initial.revision)
            .unwrap()
            .unwrap()
            .payload
            .unwrap();
        let state = &mut payload["envelope"]["state"];
        match field {
            "map_entry" => {
                state["guardrails"]
                    .as_object_mut()
                    .unwrap()
                    .remove("synthetic-agent");
            }
            "approval" => {
                state["guardrails"]["synthetic-agent"]
                    .as_object_mut()
                    .unwrap()
                    .remove("approval_required");
            }
            "kill" => {
                state.as_object_mut().unwrap().remove("kill");
            }
            "global" => {
                state["kill"].as_object_mut().unwrap().remove("global");
            }
            "account_limits" => {
                state.as_object_mut().unwrap().remove("account_limits");
            }
            "optional_limit" => {
                state["guardrails"]["synthetic-agent"]["risk"]
                    .as_object_mut()
                    .unwrap()
                    .remove("max_open_exposure_usd");
            }
            "unknown_nested" => {
                state["guardrails"]["synthetic-agent"]["risk"]["unknown"] = json!(true)
            }
            "numeric_money" => state["guardrails"]["synthetic-agent"]["max_order_usd"] = json!(25),
            "revision" => payload["envelope"]["seq"] = json!(999),
            "predecessor" => {
                payload["envelope"]
                    .as_object_mut()
                    .unwrap()
                    .remove("previous_policy");
            }
            "network" => payload["envelope"]["network"] = json!(Network::Mainnet),
            "review" => {
                payload["envelope"]["operation"]["review"]["fingerprint"] = json!("0".repeat(64))
            }
            _ => unreachable!(),
        }
        replace_tail_payload(
            &ledger,
            &crate::ledger::hash::canonical_json(&payload).unwrap(),
        );
        assert!(ledger.verify().unwrap().is_intact());
        assert!(journal.current().is_err(), "{field}");
    }
}

#[test]
fn redacted_deleted_wrong_key_and_idempotence_key_history_fail_closed() {
    for target in ["initial", "latest", "deleted", "key", "wrong_hmac"] {
        let (dir, ledger, _registry, journal) = fixture();
        let initial = journal.initialize(&review(&dir), state(), 100).unwrap();
        let mut next = state();
        next.account_limits.max_daily_loss_usd = Some(rust_decimal::Decimal::from(5));
        let latest = journal.replace(initial.revision, next, 101).unwrap();
        match target {
            "initial" => {
                ledger.redact(initial.revision, "synthetic", 102).unwrap();
            }
            "latest" => {
                ledger.redact(latest.revision, "synthetic", 102).unwrap();
            }
            "deleted" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("DELETE FROM events", [])
                    .unwrap();
            }
            "key" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute(
                        "UPDATE events SET idem_key = NULL WHERE seq = ?1",
                        [latest.revision],
                    )
                    .unwrap();
            }
            "wrong_hmac" => {
                let wrong = PolicyJournal::new(Arc::new(
                    RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([9; 32])))
                        .unwrap(),
                ));
                assert!(wrong.current().is_err());
                assert_eq!(journal.current().unwrap(), latest);
                continue;
            }
            _ => unreachable!(),
        }
        assert!(journal.current().is_err(), "{target}");
    }
}

#[test]
fn initialization_and_replace_retries_publish_head_or_keep_returning_error() {
    use crate::ledger::{Anchor, HeadAnchor};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Debug)]
    struct TestAnchor {
        head: Arc<Mutex<Option<Anchor>>>,
        fail: Arc<AtomicBool>,
    }
    impl HeadAnchor for TestAnchor {
        fn load(&self) -> crate::ledger::Result<Option<Anchor>> {
            Ok(self.head.lock().unwrap().clone())
        }
        fn store(&self, head: &Anchor) -> crate::ledger::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(LedgerError::Io(std::io::Error::other(
                    "synthetic anchor failure",
                )));
            }
            *self.head.lock().unwrap() = Some(head.clone());
            Ok(())
        }
    }
    for replacing in [false, true] {
        let dir = TempDir::new().unwrap();
        let witnessed = Arc::new(Mutex::new(None));
        let fail = Arc::new(AtomicBool::new(false));
        let anchor = || {
            Box::new(TestAnchor {
                head: witnessed.clone(),
                fail: fail.clone(),
            }) as Box<dyn HeadAnchor>
        };
        let path = dir.path().join("anchor.db");
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let journal = PolicyJournal::new(Arc::new(
            RegistryJournal::open(ledger.clone(), key()).unwrap(),
        ));
        let review = review(&dir);
        let initial = replacing.then(|| journal.initialize(&review, state(), 100).unwrap());
        let mut next = state();
        if replacing {
            next.account_limits.max_daily_loss_usd = Some(rust_decimal::Decimal::from(5));
        }
        let before = ledger.chain_head().unwrap();
        let mutate = |at_ms| match &initial {
            Some(initial) => journal.replace(initial.revision, next.clone(), at_ms),
            None => journal.initialize(&review, next.clone(), at_ms),
        };
        fail.store(true, Ordering::SeqCst);
        assert!(mutate(101).is_err());
        let committed = ledger.chain_head().unwrap();
        assert_eq!(committed.seq, before.seq + 1);
        assert_eq!(*witnessed.lock().unwrap(), Some(before.clone()));
        assert_eq!(
            journal.current().unwrap().state,
            next,
            "publication failure has an uncertain durable outcome, not rollback"
        );
        assert!(mutate(102).is_err());
        assert_eq!(ledger.chain_head().unwrap(), committed);
        assert_eq!(*witnessed.lock().unwrap(), Some(before.clone()));
        fail.store(false, Ordering::SeqCst);
        let acknowledged = mutate(103).unwrap();
        assert_eq!(acknowledged.revision, committed.seq);
        assert_eq!(ledger.chain_head().unwrap(), committed);
        assert_eq!(*witnessed.lock().unwrap(), Some(committed));
        drop(journal);
        drop(ledger);
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let journal = PolicyJournal::new(Arc::new(
            RegistryJournal::open(ledger.clone(), key()).unwrap(),
        ));
        assert_eq!(journal.current().unwrap(), acknowledged);
        {
            let mut guard = ledger.lock().unwrap();
            let tx = guard
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            tx.execute("DELETE FROM events WHERE seq > ?1", [before.seq])
                .unwrap();
            tx.execute(
                "UPDATE chain_head SET seq = ?1, hash = ?2 WHERE id = 0",
                rusqlite::params![before.seq, before.hash],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        assert!(!ledger.verify().unwrap().is_intact());
        assert!(journal.current().is_err());
        assert!(PolicyJournal::inspect(&ledger).is_err());
    }
}
