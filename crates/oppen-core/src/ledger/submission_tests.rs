use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};

use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::ledger::tests::{audit_route, audit_sink};
use crate::{
    Network,
    guardrail::{AgentId, AuditEntry, AuditOutcome, AuditSink, Utilization},
    ledger::{Anchor, HeadAnchor},
};

fn account(n: u8) -> Address {
    Address::from_bytes([n; 20])
}

fn clearance(n: u8) -> Clearance {
    Clearance {
        policy_revision: crate::ledger::tests::AUDIT_POLICY_REVISION,
        route: audit_route(AgentId::new("agent-a"), account(1), 100),
        agent: AgentId::new("agent-a"),
        vault_address: None,
        network: Network::Testnet,
        evaluated_at_ms: 100,
        kind: ClearedKind::Order {
            symbol: "BTC".into(),
            is_buy: true,
            px: Decimal::from(100),
            sz: Decimal::ONE,
            notional_usd: Decimal::from(100),
            reduce_only: false,
            slippage_bps: Decimal::ZERO,
            reference_px: Decimal::from(100),
            slippage_reference_px: Decimal::from(100),
            cloid: Some(Cloid::from_bytes([n; 16])),
            snapshot_id: None,
            snapshot_hash: None,
        },
        utilization: Utilization {
            order_notional_pct: None,
            position_notional_pct: None,
            daily_loss_pct: None,
            drawdown_pct: None,
            vol_scaled_position_pct: None,
            leverage: Decimal::ONE,
            order_tokens_remaining: Decimal::from(10),
            global_tokens_remaining: Decimal::from(100),
        },
    }
}

fn persist(ledger: &Arc<Ledger>, clearance: &Clearance) {
    audit_sink(ledger.clone())
        .record(&AuditEntry {
            agent: Some(&clearance.agent),
            at_ms: clearance.evaluated_at_ms,
            reason: "untrusted <reason>",
            outcome: AuditOutcome::Cleared(clearance),
        })
        .unwrap();
}

fn fixture() -> (TempDir, Arc<Ledger>, SubmissionJournal) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = SubmissionJournal::new(ledger.clone());
    (dir, ledger, journal)
}

fn not_sent() -> SubmissionResolution {
    SubmissionResolution::NotSent {
        detail: "signing failed before transport".into(),
    }
}

#[test]
fn begin_reopen_pending_resolve_reopen_empty() {
    let (dir, ledger, journal) = fixture();
    let order = clearance(1);
    persist(&ledger, &order);
    let receipt = journal.begin(account(1), &order, 0, 101).unwrap();
    let start_seq = receipt.seq;
    let intent = ledger.event(receipt.start.intent_seq).unwrap().unwrap();
    assert_eq!(receipt.start.intent_hash, intent.hash);
    assert_eq!(journal.state(account(1)).unwrap().revision, start_seq);
    drop(receipt);
    drop(journal);
    drop(ledger);

    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = SubmissionJournal::new(ledger.clone());
    let pending = journal.state(account(1)).unwrap().pending.unwrap();
    assert_eq!(pending.seq, start_seq);
    assert_eq!(pending.cloid(), &Cloid::from_bytes([1; 16]));
    journal
        .resolve(
            &pending,
            SubmissionResolution::Observed {
                oid: 42,
                status: "resting".into(),
            },
            102,
        )
        .unwrap();
    let resolved_seq = journal.state(account(1)).unwrap().revision;
    assert!(resolved_seq > start_seq);
    drop(journal);
    drop(ledger);

    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let state = SubmissionJournal::new(ledger.clone())
        .state(account(1))
        .unwrap();
    assert!(state.pending.is_none());
    assert_eq!(state.revision, resolved_seq);
    assert!(ledger.verify().unwrap().is_intact());
}

#[test]
fn only_definite_resolution_clears_and_each_variant_is_durable() {
    for resolution in [
        not_sent(),
        SubmissionResolution::Rejected {
            message: "venue rejection".into(),
        },
        SubmissionResolution::Observed {
            oid: 99,
            status: "filled".into(),
        },
    ] {
        let (_dir, ledger, journal) = fixture();
        persist(&ledger, &clearance(1));
        let receipt = journal.begin(account(1), &clearance(1), 0, 101).unwrap();
        // An unrelated outcome, time passing, and dropping a receipt do not
        // constitute authoritative evidence that this reservation can clear.
        drop(receipt.clone());
        ledger
            .append(&NewEvent {
                kind: EventKind::OrderStateChange,
                ts_ms: 999,
                agent_id: Some("agent-a"),
                payload: &json!({"status": "unknown"}),
                snapshot: None,
            })
            .unwrap();
        assert!(matches!(
            journal.resolve(&receipt, resolution.clone(), u64::MAX),
            Err(SubmissionError::InvalidRecord { .. })
        ));
        assert_eq!(
            journal.state(account(1)).unwrap().pending,
            Some(receipt.clone())
        );
        journal.resolve(&receipt, resolution, 1000).unwrap();
        assert!(journal.state(account(1)).unwrap().pending.is_none());
        assert_eq!(
            ledger
                .get_events(0, 100)
                .unwrap()
                .events
                .iter()
                .filter(|event| event.kind == EventKind::SubmissionResolved)
                .count(),
            1
        );
    }
}

#[test]
fn duplicate_cloid_is_forbidden_for_account_lifetime_but_accounts_are_isolated() {
    let (_dir, ledger, journal) = fixture();
    let order = clearance(1);
    persist(&ledger, &order);
    let first = journal.begin(account(1), &order, 0, 101).unwrap();
    assert!(matches!(
        journal.begin(account(1), &order, first.seq, 102),
        Err(SubmissionError::Busy { .. })
    ));
    let mut other_order = order.clone();
    other_order.route.binding.container = account(2);
    persist(&ledger, &other_order);
    let other = journal.begin(account(2), &other_order, 0, 103).unwrap();
    journal.resolve(&first, not_sent(), 104).unwrap();
    let revision = journal.state(account(1)).unwrap().revision;
    assert!(matches!(
        journal.begin(account(1), &order, revision, 105),
        Err(SubmissionError::DuplicateCloid)
    ));
    assert_eq!(journal.state(account(2)).unwrap().pending, Some(other));
    assert_eq!(journal.state(account(3)).unwrap().revision, 0);
}

#[test]
fn optimistic_revision_rejects_stale_exposure_after_complete_lifecycle() {
    let (_dir, ledger, journal) = fixture();
    persist(&ledger, &clearance(1));
    persist(&ledger, &clearance(2));
    let before = journal.state(account(1)).unwrap();
    let receipt = journal
        .begin(account(1), &clearance(1), before.revision, 101)
        .unwrap();
    journal.resolve(&receipt, not_sent(), 102).unwrap();
    let head = ledger.chain_head().unwrap();
    assert!(matches!(
        journal.begin(account(1), &clearance(2), before.revision, 103),
        Err(SubmissionError::StaleRevision)
    ));
    assert_eq!(ledger.chain_head().unwrap(), head);
    journal
        .begin(
            account(1),
            &clearance(2),
            journal.state(account(1)).unwrap().revision,
            104,
        )
        .unwrap();
}

#[test]
fn independent_sqlite_handles_race_only_one_wins() {
    let (dir, ledger, journal) = fixture();
    persist(&ledger, &clearance(1));
    persist(&ledger, &clearance(2));
    let second = SubmissionJournal::new(Arc::new(
        Ledger::open(dir.path(), Network::Testnet).unwrap(),
    ));
    assert_eq!(second.state(account(1)).unwrap().revision, 0);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [journal.clone(), second]
        .into_iter()
        .enumerate()
        .map(|(index, journal)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                journal.begin(account(1), &clearance(index as u8 + 1), 0, 101)
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(SubmissionError::Busy { .. })))
            .count(),
        1
    );
    assert!(journal.state(account(1)).unwrap().pending.is_some());
    assert_eq!(
        ledger
            .get_events(0, 100)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == EventKind::SubmissionStarted)
            .count(),
        1
    );
}

#[test]
fn missing_mismatched_intent_network_and_missing_cloid_refuse_without_append() {
    let (_dir, ledger, journal) = fixture();
    assert!(matches!(
        journal.begin(account(1), &clearance(1), 0, 101),
        Err(SubmissionError::MissingIntent)
    ));
    persist(&ledger, &clearance(1));
    let head = ledger.chain_head().unwrap();
    let mut wrong = clearance(1);
    wrong.evaluated_at_ms += 1;
    assert!(matches!(
        journal.begin(account(1), &wrong, 0, 101),
        Err(SubmissionError::MissingIntent)
    ));
    wrong = clearance(1);
    wrong.agent = AgentId::new("another-agent");
    wrong.route.binding.agent = wrong.agent.clone();
    assert!(matches!(
        journal.begin(account(1), &wrong, 0, 101),
        Err(SubmissionError::MissingIntent)
    ));
    wrong = clearance(1);
    wrong.network = Network::Mainnet;
    assert!(matches!(
        journal.begin(account(1), &wrong, 0, 101),
        Err(SubmissionError::WrongNetwork)
    ));
    wrong = clearance(1);
    if let ClearedKind::Order { cloid, .. } = &mut wrong.kind {
        *cloid = None;
    }
    assert!(matches!(
        journal.begin(account(1), &wrong, 0, 101),
        Err(SubmissionError::MissingIntent)
    ));
    wrong.kind = ClearedKind::Cancel { count: 1 };
    assert!(matches!(
        journal.begin(account(1), &wrong, 0, 101),
        Err(SubmissionError::MissingIntent)
    ));
    assert_eq!(ledger.chain_head().unwrap(), head);
}

#[test]
fn inconsistent_route_identity_refuses_before_intent_lookup_without_append() {
    let (_dir, ledger, journal) = fixture();
    persist(&ledger, &clearance(1));
    let head = ledger.chain_head().unwrap();
    for mismatch in ["agent", "network", "container", "vault"] {
        let mut wrong = clearance(1);
        match mismatch {
            "agent" => wrong.route.binding.agent = AgentId::new("another-agent"),
            "network" => wrong.route.network = Network::Mainnet,
            "container" => wrong.route.binding.container = account(2),
            "vault" => wrong.route.binding.vault_address = Some(account(1)),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                journal.begin(account(1), &wrong, 0, 101),
                Err(SubmissionError::InvalidRecord { .. })
            ),
            "{mismatch}"
        );
        assert_eq!(ledger.chain_head().unwrap(), head);
    }
}

#[test]
fn replayed_resolution_cannot_clear_new_pending_or_append_again() {
    let (_dir, ledger, journal) = fixture();
    persist(&ledger, &clearance(1));
    let first = journal.begin(account(1), &clearance(1), 0, 101).unwrap();
    journal.resolve(&first, not_sent(), 102).unwrap();
    let head = ledger.chain_head().unwrap();
    journal.resolve(&first, not_sent(), 103).unwrap();
    assert_eq!(ledger.chain_head().unwrap(), head);
    persist(&ledger, &clearance(2));
    let second = journal
        .begin(
            account(1),
            &clearance(2),
            journal.state(account(1)).unwrap().revision,
            104,
        )
        .unwrap();
    let head = ledger.chain_head().unwrap();
    journal
        .resolve(
            &first,
            SubmissionResolution::Observed {
                oid: 99,
                status: "filled".into(),
            },
            105,
        )
        .unwrap();
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert_eq!(journal.state(account(1)).unwrap().pending, Some(second));
}

#[test]
fn redacted_lifecycle_or_linked_intent_fails_closed_even_for_other_accounts() {
    for target in ["start", "resolve", "intent"] {
        let (_dir, ledger, journal) = fixture();
        persist(&ledger, &clearance(1));
        let receipt = journal.begin(account(1), &clearance(1), 0, 101).unwrap();
        let seq = match target {
            "start" => receipt.seq,
            "intent" => receipt.start.intent_seq,
            _ => {
                journal.resolve(&receipt, not_sent(), 102).unwrap();
                ledger.chain_head().unwrap().seq
            }
        };
        ledger.redact(seq, "retention", 103).unwrap();
        assert!(ledger.verify().unwrap().is_intact());
        assert!(matches!(
            journal.state(account(1)),
            Err(SubmissionError::InvalidRecord { .. })
        ));
        assert!(matches!(
            journal.state(account(99)),
            Err(SubmissionError::InvalidRecord { .. })
        ));
        assert!(journal.begin(account(99), &clearance(1), 0, 104).is_err());
        assert!(journal.resolve(&receipt, not_sent(), 104).is_err());
    }
}

#[test]
fn clearance_vault_must_match_bound_account() {
    let (_dir, ledger, journal) = fixture();
    let mut order = clearance(1);
    order.vault_address = Some(account(1));
    order.route.binding.vault_address = Some(account(1));
    persist(&ledger, &order);
    let head = ledger.chain_head().unwrap();
    assert!(matches!(
        journal.begin(account(2), &order, 0, 101),
        Err(SubmissionError::InvalidRecord { .. })
    ));
    assert_eq!(ledger.chain_head().unwrap(), head);
    let receipt = journal.begin(account(1), &order, 0, 101).unwrap();
    assert_eq!(journal.state(account(1)).unwrap().pending, Some(receipt));
}

#[test]
fn corrupted_payload_hash_or_idempotence_key_fails_closed() {
    for column in ["payload", "hash", "idem_key"] {
        let (_dir, ledger, journal) = fixture();
        persist(&ledger, &clearance(1));
        let receipt = journal.begin(account(1), &clearance(1), 0, 101).unwrap();
        ledger
            .lock()
            .unwrap()
            .execute(
                &format!("UPDATE events SET {column} = ?2 WHERE seq = ?1"),
                params![
                    receipt.seq as i64,
                    if column == "payload" { "{}" } else { "corrupt" }
                ],
            )
            .unwrap();
        assert!(journal.state(account(1)).is_err());
        assert!(journal.state(account(99)).is_err());
        assert!(journal.resolve(&receipt, not_sent(), 102).is_err());
        assert!(journal.begin(account(99), &clearance(1), 0, 102).is_err());
    }
}

// Append hash-valid but semantically invalid lifecycle records, bypassing the
// public one-door restriction solely to exercise replay's validation.
fn raw(ledger: &Ledger, kind: EventKind, payload: Value, key: &str) {
    let mut guard = ledger.lock().unwrap();
    let tx = guard
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let appended = super::super::append_keyed_in_tx(
        &tx,
        &NewEvent {
            kind,
            ts_ms: 200,
            agent_id: Some("agent-a"),
            payload: &payload,
            snapshot: None,
        },
        key,
    )
    .unwrap()
    .unwrap();
    tx.commit().unwrap();
    ledger.note_head(&appended).unwrap();
}

#[test]
fn a_prior_pending_intent_without_route_is_never_released_on_reopen() {
    prior_pending_survives_upgrade_without("route");
}

#[test]
fn v6_pending_without_policy_revision_is_preserved_and_never_auto_released() {
    prior_pending_survives_upgrade_without("policy_revision");
}

fn prior_pending_survives_upgrade_without(missing: &str) {
    let (dir, ledger, journal) = fixture();
    let order = clearance(1);
    let mut legacy = serde_json::to_value(&order).unwrap();
    legacy.as_object_mut().unwrap().remove(missing);
    let intent = ledger
        .record_intent(&super::super::NewIntent {
            agent_id: "agent-a",
            ts_ms: 100,
            payload: &legacy,
            snapshot: None,
        })
        .unwrap();
    let event = ledger.event(intent.seq()).unwrap().unwrap();
    let start = Started {
        version: 1,
        account: account(1),
        cloid: Cloid::from_bytes([1; 16]),
        intent_seq: event.seq,
        intent_hash: event.hash,
    };
    raw(
        &ledger,
        EventKind::SubmissionStarted,
        serde_json::to_value(&start).unwrap(),
        &start_key(&start),
    );
    let revision = ledger.chain_head().unwrap().seq;
    let next = clearance(2);
    persist(&ledger, &next);
    let before = ledger.get_events(0, 100).unwrap().events;
    let head = ledger.chain_head().unwrap();
    ledger
        .lock()
        .unwrap()
        .pragma_update(None, "user_version", 6)
        .unwrap();
    drop(journal);
    drop(ledger);

    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    assert_eq!(
        ledger
            .lock()
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        10
    );
    let journal = SubmissionJournal::new(ledger.clone());
    // A stricter typed reader may refuse this legacy intent. Neither reader
    // may reinterpret missing authority as evidence that the order was not sent.
    match journal.state(account(1)) {
        Ok(state) => assert_eq!(state.pending.unwrap().cloid(), &start.cloid),
        Err(SubmissionError::InvalidRecord { .. }) => {}
        other => panic!("unexpected legacy pending state: {other:?}"),
    }
    assert!(matches!(
        journal.begin(account(1), &next, revision, 201),
        Err(SubmissionError::Busy { .. } | SubmissionError::InvalidRecord { .. })
    ));
    assert_eq!(ledger.get_events(0, 100).unwrap().events, before);
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert!(ledger.verify().unwrap().is_intact());
}

#[test]
fn policy_revision_is_payload_not_the_account_submission_revision() {
    let (dir, ledger, journal) = fixture();
    let mut order = clearance(1);
    order.policy_revision = 901;
    persist(&ledger, &order);
    // No submissions exist for this account: its revision is zero regardless
    // of the independently supplied audit-only policy revision.
    let receipt = journal.begin(account(1), &order, 0, 101).unwrap();
    assert_ne!(receipt.seq, order.policy_revision);
    let intent = ledger.event(receipt.start.intent_seq).unwrap().unwrap();
    assert_eq!(
        intent.payload.as_ref().unwrap()["policy_revision"],
        json!(901)
    );
    let before = ledger.get_events(0, 100).unwrap().events;
    let head = ledger.chain_head().unwrap();
    drop(journal);
    drop(ledger);
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let state = SubmissionJournal::new(ledger.clone())
        .state(account(1))
        .unwrap();
    assert_eq!(state.revision, receipt.seq);
    assert_eq!(state.pending.unwrap().cloid(), &receipt.start.cloid);
    assert_eq!(ledger.get_events(0, 100).unwrap().events, before);
    assert_eq!(ledger.chain_head().unwrap(), head);
}

#[test]
fn typed_replay_rejects_invalid_version_shape_linkage_and_order() {
    for mutation in ["version", "shape", "hash", "intent_seq", "cloid", "overlap"] {
        let (_dir, ledger, journal) = fixture();
        persist(&ledger, &clearance(1));
        let receipt = journal.begin(account(1), &clearance(1), 0, 101).unwrap();
        let mut payload = serde_json::to_value(&receipt.start).unwrap();
        let mut key = "test-malformed".to_owned();
        match mutation {
            "version" => payload["version"] = json!(2),
            "shape" => payload["cloid"] = json!(42),
            "hash" => payload["intent_hash"] = json!("wrong"),
            "intent_seq" => payload["intent_seq"] = json!(999),
            "cloid" => payload["cloid"] = json!(Cloid::from_bytes([2; 16])),
            "overlap" => {
                persist(&ledger, &clearance(2));
                let intent = ledger.get_events(0, 100).unwrap().events.pop().unwrap();
                payload["cloid"] = json!(Cloid::from_bytes([2; 16]));
                payload["intent_seq"] = json!(intent.seq);
                payload["intent_hash"] = json!(intent.hash);
            }
            _ => unreachable!(),
        }
        if let Ok(start) = serde_json::from_value::<Started>(payload.clone()) {
            // An earlier valid start already owns this key; use a new account
            // except when testing overlapping starts on the same account.
            if mutation != "overlap" {
                payload["account"] = json!(account(2));
            }
            key = start_key(&serde_json::from_value::<Started>(payload.clone()).unwrap());
            drop(start);
        }
        raw(&ledger, EventKind::SubmissionStarted, payload, &key);
        assert!(ledger.verify().unwrap().is_intact());
        assert!(
            matches!(
                journal.state(account(99)),
                Err(SubmissionError::InvalidRecord { .. })
            ),
            "{mutation}"
        );
    }
    for outcome in [
        json!({"resolution": "timeout"}),
        json!({"resolution": "observed", "oid": "bad", "status": "filled"}),
    ] {
        let (_dir, ledger, journal) = fixture();
        raw(
            &ledger,
            EventKind::SubmissionResolved,
            json!({"version": 1, "account": account(1),
            "start_seq": 99, "start_hash": "missing", "outcome": outcome}),
            &resolve_key(99),
        );
        assert!(journal.state(account(1)).is_err());
    }
}

#[test]
fn receipt_must_match_chain_and_exact_start_row() {
    let (_dir, ledger, journal) = fixture();
    persist(&ledger, &clearance(1));
    let receipt = journal.begin(account(1), &clearance(1), 0, 101).unwrap();
    let mut forged = receipt.clone();
    forged.hash = "wrong".into();
    assert!(matches!(
        journal.resolve(&forged, not_sent(), 102),
        Err(SubmissionError::InvalidRecord { .. })
    ));
    let dir = TempDir::new().unwrap();
    let mainnet = SubmissionJournal::new(Arc::new(
        Ledger::open(dir.path(), Network::Mainnet).unwrap(),
    ));
    assert!(matches!(
        mainnet.resolve(&receipt, not_sent(), 102),
        Err(SubmissionError::WrongNetwork)
    ));
    assert_eq!(journal.state(account(1)).unwrap().pending, Some(receipt));
}

#[test]
fn v2_upgrade_preserves_existing_events_hashes_and_head() {
    fn stored_rows(ledger: &Ledger) -> Vec<Vec<rusqlite::types::Value>> {
        let guard = ledger.lock().unwrap();
        let mut statement = guard.prepare("SELECT * FROM events ORDER BY seq").unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns).map(|column| row.get(column)).collect()
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("migration.db");
    let ledger = Arc::new(Ledger::open_at(&path, Network::Testnet).unwrap());
    // V3 adds only this index. Removing it and rewinding the version restores
    // the V2 schema, without rewriting any event or duplicating schema SQL.
    ledger
        .lock()
        .unwrap()
        .execute_batch("DROP INDEX events_submission_account; PRAGMA user_version = 2;")
        .unwrap();
    persist(&ledger, &clearance(1));
    ledger
        .record_fill(&super::super::NewFill {
            account: &account(1).to_string(),
            tid: 77,
            ts_ms: 101,
            agent_id: Some("agent-a"),
            payload: &json!({"account": account(1), "tid": 77, "px": "100"}),
        })
        .unwrap()
        .unwrap();
    let redacted = ledger
        .append(&NewEvent {
            kind: EventKind::AgentDecision,
            ts_ms: 102,
            agent_id: Some("agent-a"),
            payload: &json!({"reason": "retained as a tombstone"}),
            snapshot: None,
        })
        .unwrap();
    ledger.redact(redacted.seq, "retention", 103).unwrap();
    assert_eq!(
        ledger
            .lock()
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0),)
            .unwrap(),
        2
    );
    assert!(ledger.verify().unwrap().is_intact());
    let before_rows = stored_rows(&ledger);
    let before_events = ledger.get_events(0, 100).unwrap();
    let before_head = ledger.chain_head().unwrap();
    assert_eq!(before_rows.len(), 4);
    drop(ledger);

    let upgraded = Ledger::open_at(&path, Network::Testnet).unwrap();
    assert_eq!(
        upgraded
            .lock()
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0),)
            .unwrap(),
        10
    );
    let index_count: i64 = upgraded.lock().unwrap().query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'index' AND name = 'events_submission_account'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(index_count, 1);
    assert_eq!(
        stored_rows(&upgraded),
        before_rows,
        "migration must preserve every column, including payload bytes, hashes, tombstones and idem_key"
    );
    assert_eq!(upgraded.get_events(0, 100).unwrap(), before_events);
    assert_eq!(upgraded.chain_head().unwrap(), before_head);
    assert!(upgraded.verify().unwrap().is_intact());
}

#[derive(Debug)]
struct FailingAnchor {
    value: Arc<std::sync::Mutex<Option<Anchor>>>,
    fail: Arc<AtomicBool>,
}

impl HeadAnchor for FailingAnchor {
    fn load(&self) -> super::super::Result<Option<Anchor>> {
        Ok(self.value.lock().unwrap().clone())
    }
    fn store(&self, head: &Anchor) -> super::super::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(LedgerError::Io(std::io::Error::other("anchor unavailable")));
        }
        *self.value.lock().unwrap() = Some(head.clone());
        Ok(())
    }
}

#[test]
fn anchor_error_after_start_commit_preserves_pending() {
    let dir = TempDir::new().unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let ledger = Arc::new(
        Ledger::open_anchored(
            &dir.path().join("ledger.db"),
            Network::Testnet,
            Some(Box::new(FailingAnchor {
                value: Default::default(),
                fail: fail.clone(),
            })),
        )
        .unwrap(),
    );
    persist(&ledger, &clearance(1));
    let journal = SubmissionJournal::new(ledger);
    fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        journal.begin(account(1), &clearance(1), 0, 101),
        Err(SubmissionError::Ledger(_))
    ));
    assert!(journal.state(account(1)).unwrap().pending.is_some());
    assert!(matches!(
        journal.begin(account(1), &clearance(1), 0, 102),
        Err(SubmissionError::Busy { .. })
    ));
}
