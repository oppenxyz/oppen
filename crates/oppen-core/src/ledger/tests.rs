//! Ledger tests.
//!
//! `AGENTS.md`: a ledger change needs the disconnect-reconcile test. The rest
//! of this file is the property set the module exists to hold — no gap and no
//! reorder in seq, tampering detected at the right row, a tombstoned payload
//! that still verifies, two networks with independent cursors, and an
//! interrupted append that leaves no half-row.
//!
//! The randomised tests use a fixed-seed splitmix64 rather than a proptest
//! dependency the crate does not have. Seeds are constants, so a failure is
//! reproducible by running the same test again.

use std::str::FromStr;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

/// Deterministic splitmix64. A test that fails only sometimes is a test nobody
/// trusts, so the generator is seeded from a constant and never from the clock.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }
}

const KINDS: [EventKind; 8] = [
    EventKind::ApprovalDecision,
    EventKind::AgentDecision,
    EventKind::Refusal,
    EventKind::Fill,
    EventKind::OrderStateChange,
    EventKind::OperatorAction,
    EventKind::GuardrailTrip,
    EventKind::Alert,
];

fn open(dir: &TempDir, network: Network) -> Ledger {
    Ledger::open(dir.path(), network).expect("open ledger")
}

fn payload(n: u64) -> Value {
    json!({ "n": n, "px": "1234.5", "reason": "test" })
}

/// Append `count` pseudo-random events and return their assigned seqs.
fn fill(ledger: &Ledger, count: u64, seed: u64) -> Vec<u64> {
    let mut rng = Rng::new(seed);
    let mut seqs = Vec::new();
    for n in 0..count {
        let kind = KINDS[(rng.below(KINDS.len() as u64)) as usize];
        let agent = format!("agent-{}", rng.below(4));
        let body = json!({
            "n": n,
            "nonce": rng.next_u64(),
            "sz": "0.01",
            "reason": "randomised body",
        });
        let appended = ledger
            .append(&NewEvent {
                kind,
                ts_ms: 1_756_000_000_000 + n as i64,
                agent_id: Some(&agent),
                payload: &body,
                snapshot: None,
            })
            .expect("append");
        seqs.push(appended.seq);
    }
    seqs
}

// --- durability configuration ------------------------------------------------

#[test]
fn wal_and_synchronous_full_are_set() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let guard = ledger.connection.lock().expect("lock");

    let mode: String = guard
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal_mode");
    assert_eq!(mode.to_ascii_lowercase(), "wal");

    // 2 == FULL. The intent row has to be on the platter before the signer
    // runs, and NORMAL can lose a WAL commit to a power cut.
    let synchronous: i64 = guard
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("synchronous");
    assert_eq!(synchronous, 2);
}

#[test]
fn the_file_name_is_the_one_db_file_name_gives() {
    let dir = TempDir::new().expect("tempdir");
    let _testnet = open(&dir, Network::Testnet);
    let _mainnet = open(&dir, Network::Mainnet);
    assert!(dir.path().join("testnet.db").exists());
    assert!(dir.path().join("mainnet.db").exists());
}

// --- the chain preimage is pinned -------------------------------------------

#[test]
fn chain_hashes_are_pinned() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let body = json!({ "coin": "ETH", "sz": "0.05" });
    let receipt = ledger
        .record_intent(&NewIntent {
            agent_id: "agent-a",
            ts_ms: 1_756_000_000_000,
            payload: &body,
            snapshot: None,
        })
        .expect("record intent");

    // Changing the preimage rewrites every hash in every existing database, so
    // it has to be a deliberate migration and not a silent refactor.
    assert_eq!(
        ledger.genesis,
        "a88085080e59469bce01fcdd764b563c68f4d635d17e912aea24910c062b83f0"
    );
    assert_eq!(
        receipt.hash(),
        "b8593bf7c2b0a01b46cc4d8c41fa36190e6b0a1eeeb920f75ec7b82ec9726e84"
    );

    // The genesis binds the network name, so the same row on the other network
    // has a different hash from the first link onwards (docs/decisions.md R4).
    let mainnet = open(&dir, Network::Mainnet);
    assert_ne!(mainnet.genesis, ledger.genesis);
}

// --- no gap, no reorder ------------------------------------------------------

#[test]
fn prop_append_has_no_gap_and_no_reorder() {
    for seed in [1u64, 7, 42, 9_999] {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir, Network::Testnet);
        let count = 200 + seed % 60;
        let seqs = fill(&ledger, count, seed);

        // Appends hand out 1..=count with no gap and no repeat.
        assert_eq!(seqs, (1..=count).collect::<Vec<_>>());

        // Paging with arbitrary limits reproduces exactly that order.
        let mut rng = Rng::new(seed ^ 0xABCD);
        let mut cursor = 0u64;
        let mut seen = Vec::new();
        loop {
            let limit = 1 + rng.below(17) as usize;
            let page = ledger.get_events(cursor, limit).expect("get_events");
            assert!(!page.resync_required);
            assert_eq!(page.head_seq, count);
            if page.events.is_empty() {
                assert_eq!(page.next_cursor, cursor);
                break;
            }
            for event in &page.events {
                seen.push(event.seq);
            }
            assert_eq!(page.next_cursor, page.events[page.events.len() - 1].seq);
            cursor = page.next_cursor;
        }
        assert_eq!(seen, (1..=count).collect::<Vec<_>>());
        assert!(ledger.verify().expect("verify").is_intact());
    }
}

#[test]
fn a_page_is_capped_at_max_page() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let count = MAX_PAGE as u64 + 5;
    fill(&ledger, count, 3);

    // An agent cannot ask for the whole ledger in one response, however far
    // behind it has fallen: it pages, and the cursor tells it where it is.
    let page = ledger.get_events(0, usize::MAX).expect("get_events");
    assert_eq!(page.events.len(), MAX_PAGE);
    assert_eq!(page.next_cursor, MAX_PAGE as u64);
    assert_eq!(page.head_seq, count);
    assert!(!page.resync_required);

    let rest = ledger
        .get_events(page.next_cursor, usize::MAX)
        .expect("get_events");
    assert_eq!(rest.events.len(), 5);
    assert_eq!(rest.next_cursor, count);
}

// --- tampering is detected at the right row ---------------------------------

#[test]
fn prop_payload_tamper_is_detected_at_the_right_row() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let count = 60;
    fill(&ledger, count, 11);

    let mut rng = Rng::new(0x5EED);
    for _ in 0..12 {
        let victim = 1 + rng.below(count);
        let original: String = {
            let guard = ledger.connection.lock().expect("lock");
            let original = guard
                .query_row(
                    "SELECT payload FROM events WHERE seq = ?1",
                    params![victim as i64],
                    |row| row.get(0),
                )
                .expect("read payload");
            guard
                .execute(
                    "UPDATE events SET payload = ?2 WHERE seq = ?1",
                    params![victim as i64, r#"{"n":-1,"reason":"forged"}"#],
                )
                .expect("tamper");
            original
        };

        let report = ledger.verify().expect("verify");
        let broken = report.first_break.expect("a break");
        assert_eq!(broken.seq, victim, "break reported at the wrong row");
        assert!(matches!(
            broken.reason,
            BreakReason::PayloadHashMismatch { .. }
        ));
        assert_eq!(report.rows_checked, victim - 1);

        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute(
                "UPDATE events SET payload = ?2 WHERE seq = ?1",
                params![victim as i64, original],
            )
            .expect("restore");
        drop(guard);
        assert!(ledger.verify().expect("verify").is_intact());
    }
}

#[test]
fn prop_chained_field_tamper_is_detected_at_the_right_row() {
    let count = 40u64;
    let mut rng = Rng::new(0xC0FFEE);
    for column in ["ts_ms", "kind", "agent_id", "hash", "prev_hash"] {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir, Network::Testnet);
        fill(&ledger, count, 23);
        let victim = 1 + rng.below(count - 2);

        {
            let guard = ledger.connection.lock().expect("lock");
            let sql = if column == "ts_ms" {
                "UPDATE events SET ts_ms = ts_ms + 1 WHERE seq = ?1".to_owned()
            } else {
                format!("UPDATE events SET {column} = 'tampered' WHERE seq = ?1")
            };
            guard.execute(&sql, params![victim as i64]).expect("tamper");
        }

        let report = ledger.verify().expect("verify");
        let broken = report.first_break.expect("a break");
        assert_eq!(
            broken.seq, victim,
            "editing {column} was reported at the wrong row"
        );
        // Editing prev_hash breaks the link into the row; editing any other
        // chained field breaks the row's own commitment.
        if column == "prev_hash" {
            assert!(matches!(
                broken.reason,
                BreakReason::PrevHashMismatch { .. }
            ));
        } else {
            assert!(matches!(broken.reason, BreakReason::RowHashMismatch { .. }));
        }
    }
}

#[test]
fn a_deleted_row_is_reported_as_a_seq_gap() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 20, 5);
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute("DELETE FROM events WHERE seq = ?1", params![9i64])
            .expect("delete");
    }
    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 10);
    assert_eq!(broken.reason, BreakReason::SeqGap { expected: 9 });
    assert_eq!(report.rows_checked, 8);
}

#[test]
fn a_stale_head_is_reported_even_when_every_row_verifies() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 5, 17);
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute("UPDATE chain_head SET seq = 3 WHERE id = 0", [])
            .expect("rewind head");
    }
    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 5);
    assert!(matches!(broken.reason, BreakReason::HeadMismatch { .. }));
}

#[test]
fn a_payload_rewritten_as_a_number_is_still_caught() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 6, 31);
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute("UPDATE events SET payload = 12345 WHERE seq = 4", [])
            .expect("tamper");
    }
    // The column has TEXT affinity, so SQLite stores '12345' and the tamper
    // shows up as a payload that no longer hashes to what the chain committed
    // to rather than as an unreadable column.
    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 4);
    assert!(matches!(
        broken.reason,
        BreakReason::PayloadHashMismatch { .. }
    ));
}

#[test]
fn a_payload_column_whose_schema_was_rewritten_is_reported_unreadable() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("testnet.db");
    {
        let ledger = Ledger::open_at(&path, Network::Testnet).expect("open");
        fill(&ledger, 6, 31);
    }

    // Anyone who can edit the file can edit its schema too. Dropping the
    // column's TEXT affinity lets a raw integer be stored where the payload
    // belongs, which is the one shape the payload hash cannot be computed over.
    {
        let raw = Connection::open(&path).expect("raw open");
        raw.execute_batch("PRAGMA writable_schema = ON")
            .expect("writable");
        raw.execute(
            "UPDATE sqlite_master SET sql = replace(sql, 'payload          TEXT', \
             'payload          BLOB') WHERE type = 'table' AND name = 'events'",
            [],
        )
        .expect("rewrite schema");
        raw.execute_batch("PRAGMA writable_schema = RESET")
            .expect("reset");
    }

    let ledger = Ledger::open_at(&path, Network::Testnet).expect("reopen");
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute("UPDATE events SET payload = 12345 WHERE seq = 4", [])
            .expect("tamper");
        let kind: String = guard
            .query_row(
                "SELECT typeof(payload) FROM events WHERE seq = 4",
                [],
                |row| row.get(0),
            )
            .expect("typeof");
        assert_eq!(kind, "integer");
    }
    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 4);
    assert_eq!(broken.reason, BreakReason::PayloadUnreadable);
}

// --- redaction ---------------------------------------------------------------

#[test]
fn prop_a_tombstoned_payload_still_verifies() {
    for seed in [2u64, 64, 512] {
        let dir = TempDir::new().expect("tempdir");
        let ledger = open(&dir, Network::Testnet);
        let count = 80;
        fill(&ledger, count, seed);

        let mut rng = Rng::new(seed);
        let mut redacted = Vec::new();
        for _ in 0..15 {
            let victim = 1 + rng.below(count);
            if redacted.contains(&victim) {
                continue;
            }
            ledger
                .redact(victim, "operator request", 1_756_100_000_000)
                .expect("redact");
            redacted.push(victim);
        }
        assert!(!redacted.is_empty());

        let report = ledger.verify().expect("verify");
        assert!(report.is_intact(), "redaction broke the chain: {report:?}");

        for seq in redacted {
            let event = ledger.event(seq).expect("event").expect("present");
            // The record survives; only the content is gone (docs/decisions.md D-e).
            assert!(event.payload.is_none());
            assert_eq!(event.redacted_at, Some(1_756_100_000_000));
            assert_eq!(event.redaction_reason.as_deref(), Some("operator request"));
            assert!(!event.payload_hash.is_empty());
        }
    }
}

#[test]
fn a_redaction_is_itself_recorded() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 13);
    let appended = ledger
        .redact(2, "pii in reason", 1_756_200_000_000)
        .expect("redact");
    assert_eq!(appended.seq, 4);

    let event = ledger.event(4).expect("event").expect("present");
    assert_eq!(event.kind, EventKind::PayloadRedacted);
    assert_eq!(
        event.payload,
        Some(json!({ "redacted_seq": 2, "reason": "pii in reason" }))
    );
    assert!(ledger.verify().expect("verify").is_intact());
}

#[test]
fn an_unrecorded_tombstone_is_a_break() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 6, 19);
    {
        let guard = ledger.connection.lock().expect("lock");
        // Nulling a payload without recording the redaction is someone deleting
        // evidence, not retention policy.
        guard
            .execute("UPDATE events SET payload = NULL WHERE seq = 3", [])
            .expect("tamper");
    }
    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 3);
    assert_eq!(broken.reason, BreakReason::UnrecordedTombstone);
}

#[test]
fn redacting_twice_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 2, 4);
    ledger.redact(1, "first", 1).expect("redact");
    assert!(matches!(
        ledger.redact(1, "second", 2),
        Err(LedgerError::AlreadyRedacted(1))
    ));
    assert!(matches!(
        ledger.redact(99, "missing", 3),
        Err(LedgerError::NoSuchEvent(99))
    ));
}

// --- network isolation -------------------------------------------------------

#[test]
fn two_networks_have_independent_cursors() {
    let dir = TempDir::new().expect("tempdir");
    let testnet = open(&dir, Network::Testnet);
    let mainnet = open(&dir, Network::Mainnet);

    fill(&testnet, 12, 101);
    fill(&mainnet, 3, 202);

    // Same cursor value, different chains, no bleed in either direction.
    assert_eq!(testnet.get_events(0, 100).expect("page").events.len(), 12);
    assert_eq!(mainnet.get_events(0, 100).expect("page").events.len(), 3);
    assert_eq!(testnet.get_events(2, 100).expect("page").next_cursor, 12);
    assert!(mainnet.get_events(2, 100).expect("page").next_cursor == 3);

    // A testnet cursor beyond the mainnet head is an explicit resync, never a
    // silently empty page (docs/decisions.md R4).
    let page = mainnet.get_events(12, 100).expect("page");
    assert!(page.resync_required);
    assert_eq!(page.head_seq, 3);

    assert_ne!(testnet.genesis, mainnet.genesis);
    assert!(testnet.verify().expect("verify").is_intact());
    assert!(mainnet.verify().expect("verify").is_intact());
}

#[test]
fn opening_a_testnet_file_as_mainnet_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    {
        let testnet = open(&dir, Network::Testnet);
        fill(&testnet, 2, 8);
    }
    let path = dir.path().join("testnet.db");
    let error = Ledger::open_at(&path, Network::Mainnet).expect_err("must refuse");
    assert!(matches!(
        error,
        LedgerError::NetworkMismatch {
            expected: "mainnet",
            ..
        }
    ));
}

// --- resync ------------------------------------------------------------------

#[test]
fn a_cursor_older_than_what_is_retained_demands_resync() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 10, 77);
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute("DELETE FROM events WHERE seq <= 4", [])
            .expect("prune");
    }

    let page = ledger.get_events(0, 10).expect("page");
    assert!(page.resync_required);
    assert!(page.events.is_empty());
    assert_eq!(page.next_cursor, 0);

    // A cursor that is still inside the retained range keeps working.
    let page = ledger.get_events(4, 10).expect("page");
    assert!(!page.resync_required);
    assert_eq!(page.events.len(), 6);
}

#[test]
fn an_idle_cursor_does_not_drift() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 4, 6);
    let page = ledger.get_events(4, 10).expect("page");
    assert!(page.events.is_empty());
    assert!(!page.resync_required);
    assert_eq!(page.next_cursor, 4);
    assert_eq!(page.head_seq, 4);
}

#[test]
fn an_absurd_cursor_is_refused_without_panicking() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 12);

    // The cursor arrives over the MCP wire, so it is untrusted input and no
    // value of it may panic (AGENTS.md conventions).
    let page = ledger.get_events(u64::MAX, 10).expect("page");
    assert!(page.resync_required);
    assert_eq!(page.next_cursor, u64::MAX);

    let page = ledger.get_events(0, 0).expect("page");
    assert!(page.events.is_empty());
    assert!(!page.resync_required);
    assert_eq!(page.next_cursor, 0);

    assert!(matches!(
        ledger.event(u64::MAX),
        Err(LedgerError::SeqOutOfRange)
    ));
}

// --- an interrupted append leaves no half-row -------------------------------

#[test]
fn an_append_that_fails_midway_leaves_no_half_row() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 21);

    // Fail the head update after the row insert has already succeeded: the
    // exact half-row the append transaction exists to prevent.
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute_batch(
                "CREATE TEMP TRIGGER halt_head BEFORE UPDATE ON chain_head \
                 BEGIN SELECT RAISE(ABORT, 'power cut'); END;",
            )
            .expect("arm trigger");
    }

    let body = payload(99);
    let error = ledger
        .append(&NewEvent {
            kind: EventKind::Fill,
            ts_ms: 1_756_300_000_000,
            agent_id: Some("agent-a"),
            payload: &body,
            snapshot: None,
        })
        .expect_err("the append must fail");
    assert!(matches!(error, LedgerError::Sqlite(_)));

    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute_batch("DROP TRIGGER halt_head")
            .expect("disarm");
        let count: i64 = guard
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 3, "a half-row survived the failed append");
    }

    let report = ledger.verify().expect("verify");
    assert!(report.is_intact());
    assert_eq!(report.head_seq, 3);

    // The seq the failed append would have taken is handed out again.
    let next = ledger
        .append(&NewEvent {
            kind: EventKind::Fill,
            ts_ms: 1_756_300_000_001,
            agent_id: Some("agent-a"),
            payload: &body,
            snapshot: None,
        })
        .expect("append");
    assert_eq!(next.seq, 4);
    assert!(ledger.verify().expect("verify").is_intact());
}

#[test]
fn an_uncommitted_append_leaves_no_half_row() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 22);

    let body = payload(7);
    {
        let mut guard = ledger.connection.lock().expect("lock");
        let transaction = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin");
        let appended = append_in_tx(
            &transaction,
            &NewEvent {
                kind: EventKind::OrderIntent,
                ts_ms: 1_756_400_000_000,
                agent_id: Some("agent-b"),
                payload: &body,
                snapshot: None,
            },
        )
        .expect("append in tx");
        assert_eq!(appended.seq, 4);
        // Dropped without commit: the machine lost power here.
        drop(transaction);
    }

    let report = ledger.verify().expect("verify");
    assert!(report.is_intact());
    assert_eq!(report.head_seq, 3);
    assert_eq!(report.rows_checked, 3);
    assert!(ledger.event(4).expect("event").is_none());
}

// --- the intent is durable before the signer runs ---------------------------

#[test]
fn an_intent_is_committed_before_the_receipt_exists() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("testnet.db");
    let body = json!({ "coin": "BTC", "is_buy": true, "sz": "0.001", "reason": "carry" });

    let receipt = {
        let ledger = Ledger::open_at(&path, Network::Testnet).expect("open");
        let receipt = ledger
            .record_intent(&NewIntent {
                agent_id: "agent-a",
                ts_ms: 1_756_500_000_000,
                payload: &body,
                snapshot: None,
            })
            .expect("record intent");

        // A second connection to the same file sees the row already: the intent
        // is durable at the moment the receipt exists, which is what makes the
        // signer's `&IntentReceipt` parameter a real ordering guarantee.
        let reader = Ledger::open_at(&path, Network::Testnet).expect("second open");
        let seen = reader
            .event(receipt.seq())
            .expect("event")
            .expect("present");
        assert_eq!(seen.kind, EventKind::OrderIntent);
        assert_eq!(seen.payload.as_ref(), Some(&body));
        receipt
    };

    // And it is still there after the process that wrote it is gone.
    let reopened = Ledger::open_at(&path, Network::Testnet).expect("reopen");
    let outcome = reopened
        .record_outcome(
            &receipt,
            EventKind::Fill,
            1_756_500_000_500,
            Some("agent-a"),
            &json!({ "oid": 42, "avg_px": "63000.5" }),
        )
        .expect("record outcome");
    let event = reopened
        .event(outcome.seq)
        .expect("event")
        .expect("present");
    assert_eq!(
        event.payload,
        Some(json!({
            "intent_seq": receipt.seq(),
            "intent_hash": receipt.hash(),
            "outcome": { "oid": 42, "avg_px": "63000.5" },
        }))
    );
    assert!(reopened.verify().expect("verify").is_intact());
}

#[test]
fn an_intent_cannot_be_appended_around_the_receipt() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let body = payload(1);

    // The generic append would give a durable row and no receipt, which is
    // exactly the bypass the receipt exists to prevent.
    assert!(matches!(
        ledger.append(&NewEvent {
            kind: EventKind::OrderIntent,
            ts_ms: 1,
            agent_id: Some("agent-a"),
            payload: &body,
            snapshot: None,
        }),
        Err(LedgerError::UseRecordIntent)
    ));
    assert_eq!(ledger.get_events(0, 10).expect("page").events.len(), 0);
}

// --- disconnect and reconcile ------------------------------------------------

#[test]
fn a_disconnect_opens_a_gap_and_reconcile_closes_it() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 2, 44);

    let gap = ledger
        .open_gap("ws:user:0xabc", 1_756_600_000_000, Some("close 1006"))
        .expect("open gap");
    assert_eq!(gap.open_seq, 3);
    assert_eq!(gap.closed_ts_ms, None);

    let open_now = ledger.unreconciled_gaps().expect("gaps");
    assert_eq!(open_now.len(), 1);
    assert_eq!(open_now[0].gap_id, gap.gap_id);

    let closed = ledger
        .close_gap(gap.gap_id, 1_756_600_030_000)
        .expect("close gap");
    assert_eq!(closed.seq, 4);
    let reconnect = ledger.event(4).expect("event").expect("present");
    assert_eq!(reconnect.kind, EventKind::WsReconnected);
    assert_eq!(
        reconnect.payload,
        Some(json!({ "scope": "ws:user:0xabc", "gap_id": gap.gap_id, "down_ms": 30_000 }))
    );

    // Reconnected is not the same fact as caught up: the gap stays on the work
    // list until the window has been backfilled.
    assert_eq!(ledger.unreconciled_gaps().expect("gaps").len(), 1);
    ledger
        .mark_gap_reconciled(gap.gap_id, 1_756_600_045_000)
        .expect("reconcile");
    assert!(ledger.unreconciled_gaps().expect("gaps").is_empty());

    assert!(matches!(
        ledger.close_gap(gap.gap_id, 1),
        Err(LedgerError::GapAlreadyClosed(_))
    ));
    assert!(matches!(
        ledger.mark_gap_reconciled(4_242, 1),
        Err(LedgerError::NoSuchGap(4_242))
    ));
    assert!(ledger.verify().expect("verify").is_intact());
}

#[test]
fn a_gap_cannot_be_reconciled_before_it_is_closed() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let gap = ledger.open_gap("ws:trades", 1, None).expect("open gap");
    assert!(matches!(
        ledger.mark_gap_reconciled(gap.gap_id, 2),
        Err(LedgerError::GapStillOpen(_))
    ));
}

// --- snapshot plumbing -------------------------------------------------------

#[test]
fn a_snapshot_reference_is_chained_and_its_body_is_prunable() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let book = json!({ "bids": [["63000.0", "1.5"]], "asks": [["63001.0", "2.0"]] });
    let snapshot_hash = ledger
        .put_snapshot("snap-1", 1_756_700_000_000, "BTC", &book)
        .expect("put snapshot");

    let body = payload(1);
    let receipt = ledger
        .record_intent(&NewIntent {
            agent_id: "agent-a",
            ts_ms: 1_756_700_000_100,
            payload: &body,
            snapshot: Some(SnapshotRef {
                id: "snap-1",
                hash: &snapshot_hash,
            }),
        })
        .expect("record intent");

    let event = ledger
        .event(receipt.seq())
        .expect("event")
        .expect("present");
    assert_eq!(event.snapshot_id.as_deref(), Some("snap-1"));
    assert_eq!(event.snapshot_hash.as_deref(), Some(snapshot_hash.as_str()));
    assert_eq!(ledger.snapshot_body("snap-1").expect("body"), Some(book));

    // Pruning the body leaves the chained reference, and the chain still holds.
    assert_eq!(
        ledger
            .prune_snapshots_before(1_756_800_000_000)
            .expect("prune"),
        1
    );
    assert_eq!(ledger.snapshot_body("snap-1").expect("body"), None);
    assert!(ledger.verify().expect("verify").is_intact());

    // The reference is inside the preimage: editing it breaks this row.
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute(
                "UPDATE events SET snapshot_hash = 'forged' WHERE seq = ?1",
                params![receipt.seq() as i64],
            )
            .expect("tamper");
    }
    let report = ledger.verify().expect("verify");
    assert_eq!(report.first_break.expect("a break").seq, receipt.seq());
}

// --- sub-account registry ----------------------------------------------------

#[test]
fn the_sub_account_owner_discriminator_is_stored_and_constrained() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);

    let owned = SubAccount {
        address: "0x0000000000000000000000000000000000000001".to_owned(),
        name: "carry-agent".to_owned(),
        owner: Some(Owner {
            owner_type: OwnerType::Agent,
            owner_id: "agent-a".to_owned(),
        }),
        recorded: true,
        provisioned_by_oppen: true,
        active: true,
        created_ts_ms: 1_756_800_000_000,
    };
    // Discovered, not provisioned: no owner, and not recorded by default (R3).
    let discovered = SubAccount {
        address: "0x0000000000000000000000000000000000000002".to_owned(),
        name: "unknown sub-account".to_owned(),
        owner: None,
        recorded: false,
        provisioned_by_oppen: false,
        active: true,
        created_ts_ms: 1_756_800_000_001,
    };
    ledger.upsert_sub_account(&owned).expect("upsert");
    ledger.upsert_sub_account(&discovered).expect("upsert");

    let all = ledger.sub_accounts().expect("list");
    assert_eq!(all, vec![owned.clone(), discovered]);
    assert_eq!(
        ledger.sub_account(&owned.address).expect("get"),
        Some(owned.clone())
    );

    // A workflow-owned account is representable today even though the product
    // rule is open (docs/decisions.md R2).
    let workflow = SubAccount {
        owner: Some(Owner {
            owner_type: OwnerType::Workflow,
            owner_id: "position-guardian".to_owned(),
        }),
        name: "guardian".to_owned(),
        ..owned.clone()
    };
    ledger.upsert_sub_account(&workflow).expect("upsert");
    assert_eq!(
        ledger.sub_account(&owned.address).expect("get"),
        Some(workflow)
    );

    // A half-set owner is rejected by the schema, not merely by convention.
    let guard = ledger.connection.lock().expect("lock");
    let half = guard.execute(
        "INSERT INTO sub_accounts (address, name, owner_type, owner_id, created_ts_ms) \
         VALUES ('0x03', 'half', 'agent', NULL, 0)",
        [],
    );
    assert!(half.is_err());
    let bad_type = guard.execute(
        "INSERT INTO sub_accounts (address, name, owner_type, owner_id, created_ts_ms) \
         VALUES ('0x04', 'bad', 'vault', 'v1', 0)",
        [],
    );
    assert!(bad_type.is_err());
}

// --- export ------------------------------------------------------------------

#[test]
fn export_is_csv_and_json_lines_over_the_same_rows() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 5, 55);
    let awkward = json!({ "reason": "he said \"buy, now\"\nand left" });
    ledger
        .append(&NewEvent {
            kind: EventKind::AgentDecision,
            ts_ms: 1_756_900_000_000,
            agent_id: Some("agent-a"),
            payload: &awkward,
            snapshot: None,
        })
        .expect("append");
    ledger.redact(2, "pii", 1_756_900_000_001).expect("redact");

    let mut jsonl = Vec::new();
    let written = ledger.export_jsonl(&mut jsonl).expect("jsonl");
    assert_eq!(written, 7);
    let text = String::from_utf8(jsonl).expect("utf8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 7);
    for (index, line) in lines.iter().enumerate() {
        let parsed: Value = serde_json::from_str(line).expect("line parses");
        assert_eq!(parsed["seq"], json!(index + 1));
    }
    // The redacted row exports with a null payload and its hashes intact.
    let redacted: Value = serde_json::from_str(lines[1]).expect("line parses");
    assert_eq!(redacted["payload"], Value::Null);
    assert_eq!(redacted["redaction_reason"], json!("pii"));
    assert!(
        redacted["payload_hash"]
            .as_str()
            .is_some_and(|h| h.len() == 64)
    );

    let mut csv = Vec::new();
    let written = ledger.export_csv(&mut csv).expect("csv");
    assert_eq!(written, 7);
    let csv = String::from_utf8(csv).expect("utf8");
    assert!(csv.starts_with("seq,ts_ms,kind,agent_id,"));
    // The embedded quote is doubled and the embedded newline lives inside a
    // quoted field, so the file is still RFC 4180.
    assert!(csv.contains(r#""{""reason"":""he said \""buy, now\""\nand left""}""#));
}

// --- kind round trip ---------------------------------------------------------

#[test]
fn every_event_kind_round_trips_through_its_stored_name() {
    let kinds = [
        EventKind::OrderIntent,
        EventKind::AgentDecision,
        EventKind::Refusal,
        EventKind::Fill,
        EventKind::OrderStateChange,
        EventKind::OperatorAction,
        EventKind::ApprovalDecision,
        EventKind::KillSwitchChanged,
        EventKind::GuardrailTrip,
        EventKind::WsDisconnected,
        EventKind::WsReconnected,
        EventKind::Alert,
        EventKind::AgentWalletExpiryWarning,
        EventKind::PayloadRedacted,
    ];
    for kind in kinds {
        assert_eq!(EventKind::from_str(kind.as_str()).expect("parse"), kind);
        // The serde name and the stored name are the same string, so an event
        // read out of the ledger and an event on the MCP wire agree.
        assert_eq!(
            serde_json::to_value(kind).expect("serialize"),
            json!(kind.as_str())
        );
    }
    assert!(matches!(
        EventKind::from_str("not_a_kind"),
        Err(LedgerError::UnknownKind(_))
    ));
}
