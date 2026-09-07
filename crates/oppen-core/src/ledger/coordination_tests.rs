//! Commit-to-anchor coordination across connections and processes.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use oppen_hl::{Address, wire::Cloid};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::guardrail::{
    AgentId, AuditEntry, AuditOutcome, AuditSink, Clearance, ClearedKind, Utilization,
};
use crate::ledger::tests::{audit_route, audit_sink};

const BLOCKED_WINDOW: Duration = Duration::from_millis(300);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_DATABASE: &str = "OPPEN_LEDGER_COORDINATION_TEST_DATABASE";
const CHILD_READY: &str = "OPPEN_LEDGER_COORDINATION_LOCK_HELD";
const CHILD_PAIRING: &str = "OPPEN_PAIRING_OWNER_TEST";

#[cfg(unix)]
#[test]
fn opening_an_alias_cannot_discard_its_legacy_anchor() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("legacy.db");
    File::create(&path).unwrap();
    let alias = dir.path().join("alias.db");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    let old_anchor = FileAnchor::beside(&alias);
    let ledger = Ledger::open_anchored(
        &alias,
        crate::Network::Testnet,
        Some(Box::new(old_anchor.clone())),
    )
    .unwrap();
    ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: 1,
            agent_id: None,
            payload: &json!({"action": "legacy alias fixture"}),
            snapshot: None,
        })
        .unwrap();
    let before = old_anchor.load().unwrap();
    drop(ledger);
    assert!(FileAnchor::beside(&path).load().unwrap().is_none());
    assert!(
        matches!(Ledger::open_at(&alias, crate::Network::Testnet), Err(LedgerError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput)
    );
    assert!(
        FileAnchor::beside(&path).load().unwrap().is_none(),
        "no fresh anchor may be adopted over legacy evidence"
    );
    assert_eq!(old_anchor.load().unwrap(), before);
}

fn alternate_path(path: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        let alias = path.with_file_name("alias.db");
        std::os::unix::fs::symlink(path, &alias).unwrap();
        alias
    }
    #[cfg(not(unix))]
    {
        path.to_owned()
    }
}

fn order(ledger: &Arc<Ledger>, id: u8, account: Address) -> Clearance {
    let clearance = Clearance {
        route: audit_route(AgentId::new("coordination-agent"), account, 100),
        agent: AgentId::new("coordination-agent"),
        vault_address: None,
        network: crate::Network::Testnet,
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
            cloid: Some(Cloid::from_bytes([id; 16])),
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
    };
    audit_sink(ledger.clone())
        .record(&AuditEntry {
            agent: Some(&clearance.agent),
            at_ms: 100,
            reason: "coordination fixture",
            outcome: AuditOutcome::Cleared(&clearance),
        })
        .unwrap();
    clearance
}

fn signing_fixture() -> (TempDir, Arc<Ledger>, RegistryJournal, Clearance) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), crate::Network::Testnet).unwrap());
    let registry = RegistryJournal::open(
        ledger.clone(),
        Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
    )
    .unwrap();
    let account = Address::from_bytes([1; 20]);
    let route = registry
        .grant(
            audit_route(AgentId::new("coordination-agent"), account, 100).binding,
            100,
        )
        .unwrap();
    let clearance = order(&ledger, 1, account);
    assert_eq!(clearance.route, route);
    (dir, ledger, registry, clearance)
}

#[test]
fn signing_snapshot_stays_stable_while_an_independent_raw_wal_writer_commits() {
    let (dir, ledger, registry, clearance) = signing_fixture();
    let raw = Connection::open(
        dir.path()
            .join(crate::db_file_name(crate::Network::Testnet)),
    )
    .unwrap();
    raw.busy_timeout(COMPLETION_TIMEOUT).unwrap();
    let journal_mode: String = raw
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");
    let permit = LedgerSigningPermit::new(ledger.lock().unwrap()).unwrap();
    assert!(!permit.is_autocommit());
    let route = &clearance.route;
    registry
        .verify_route_in(&permit, route, route.binding.wallet.address)
        .unwrap();
    let payload = |connection: &Connection| -> String {
        connection
            .query_row(
                "SELECT payload FROM events WHERE seq = ?1",
                params![route.binding_seq],
                |row| row.get(0),
            )
            .unwrap()
    };
    let original = payload(&permit);

    // Raw writers ignore the coordination file. WAL permits this commit;
    // the signing transaction must continue validating its earlier snapshot.
    assert_eq!(
        raw.execute(
            "UPDATE events SET payload = '{}' WHERE seq = ?1",
            params![route.binding_seq]
        )
        .unwrap(),
        1
    );
    assert_eq!(payload(&raw), "{}");
    assert_eq!(payload(&permit), original);
    registry
        .verify_route_in(&permit, route, route.binding.wallet.address)
        .unwrap();
    drop(permit);
    assert!(ledger.connection.lock().unwrap().is_autocommit());
    assert!(
        registry.route_for_agent(&clearance.agent).is_err(),
        "a new reader must see the raw mutation"
    );

    raw.execute(
        "UPDATE events SET payload = ?1 WHERE seq = ?2",
        params![original, route.binding_seq],
    )
    .unwrap();
    let before = ledger.chain_head().unwrap();
    let appended = ledger
        .append(&NewEvent {
            kind: EventKind::AgentDecision,
            ts_ms: 101,
            agent_id: Some(clearance.agent.as_str()),
            payload: &json!({"reason": "after signing snapshot drop"}),
            snapshot: None,
        })
        .unwrap();
    assert_eq!(appended.seq, before.seq + 1);
    assert!(ledger.verify().unwrap().is_intact());
}

#[test]
fn combined_signing_permit_rolls_back_on_identity_registry_and_pilot_errors() {
    for failure in ["identity", "registry", "pilot"] {
        let (_dir, ledger, registry, clearance) = signing_fixture();
        let mut actual_wallet = clearance.route.binding.wallet.clone();
        match failure {
            "identity" => actual_wallet.generation += 1,
            "registry" => {
                registry.retire(&clearance.route, 101).unwrap();
            }
            "pilot" => {
                pilot::PilotJournal::new(ledger.clone())
                    .authorize(
                        clearance.agent.clone(),
                        clearance.route.binding.container,
                        100,
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let sink = LedgerAuditSink::new(registry);
        let before = ledger.chain_head().unwrap();
        let refusal = sink
            .before_sign(&clearance, &actual_wallet, actual_wallet.address)
            .err()
            .expect("invalid signing authority must fail");
        assert!(
            matches!(refusal, crate::guardrail::Refusal::Unevaluable(_)),
            "{failure}: {refusal:?}"
        );
        assert!(
            ledger.connection.lock().unwrap().is_autocommit(),
            "{failure} leaked its read transaction"
        );
        assert_eq!(ledger.chain_head().unwrap(), before);
        let appended = ledger
            .append(&NewEvent {
                kind: EventKind::AgentDecision,
                ts_ms: 102,
                agent_id: Some(clearance.agent.as_str()),
                payload: &json!({"reason": "after refused signing"}),
                snapshot: None,
            })
            .unwrap();
        assert_eq!(appended.seq, before.seq + 1, "{failure}");
        assert!(ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn valid_route_cleanup_survives_redacted_pilot_evidence_but_still_checks_signer_and_registry() {
    use crate::guardrail::{Refusal, Unevaluable};
    for target in ["authorization", "submission"] {
        let (_dir, ledger, registry, mut clearance) = signing_fixture();
        let account = clearance.route.binding.container;
        pilot::PilotJournal::new(ledger.clone())
            .authorize(clearance.agent.clone(), account, 100)
            .unwrap();
        let authorization = ledger.chain_head().unwrap().seq;
        if let ClearedKind::Order {
            sz, notional_usd, ..
        } = &mut clearance.kind
        {
            *sz = Decimal::new(1, 1);
            *notional_usd = Decimal::from(10);
        }
        let sink = LedgerAuditSink::new(registry);
        sink.record(&AuditEntry {
            agent: Some(&clearance.agent),
            at_ms: 100,
            reason: "within pilot order limit",
            outcome: AuditOutcome::Cleared(&clearance),
        })
        .unwrap();
        EventViews::new(ledger.clone())
            .submissions()
            .begin(account, &clearance, 0, 101)
            .unwrap();
        let submission = ledger.chain_head().unwrap().seq;
        let wallet = clearance.route.binding.wallet.clone();
        drop(
            sink.before_sign(&clearance, &wallet, wallet.address)
                .unwrap(),
        );
        ledger
            .redact(
                if target == "authorization" {
                    authorization
                } else {
                    submission
                },
                "synthetic redaction",
                102,
            )
            .unwrap();
        assert!(ledger.verify().unwrap().is_intact());
        assert!(
            matches!(
                sink.before_sign(&clearance, &wallet, wallet.address),
                Err(Refusal::Unevaluable(
                    Unevaluable::PilotBudgetUnavailable { .. }
                ))
            ),
            "{target}"
        );

        for kind in [
            ClearedKind::Cancel { count: 1 },
            ClearedKind::ScheduleCancel { cancel_at_ms: None },
        ] {
            clearance.kind = kind;
            drop(
                sink.before_sign(&clearance, &wallet, wallet.address)
                    .unwrap(),
            );
            assert!(matches!(
                sink.before_sign(&clearance, &wallet, Address::from_bytes([9; 20])),
                Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
            ));
            assert!(ledger.connection.lock().unwrap().is_autocommit());
        }
        sink.registry.retire(&clearance.route, 103).unwrap();
        assert!(matches!(
            sink.before_sign(&clearance, &wallet, wallet.address),
            Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
        ));
    }
}

#[test]
fn an_unfinished_connection_transaction_is_refused_without_implicit_cleanup() {
    let dir = TempDir::new().unwrap();
    let ledger = Ledger::open(dir.path(), crate::Network::Testnet).unwrap();
    let before = ledger.chain_head().unwrap();
    ledger
        .connection
        .lock()
        .unwrap()
        .execute_batch("BEGIN DEFERRED")
        .unwrap();
    assert!(matches!(
        ledger.lock(),
        Err(LedgerError::UnfinishedTransaction)
    ));
    let marker = NewEvent {
        kind: EventKind::AgentDecision,
        ts_ms: 100,
        agent_id: None,
        payload: &json!({"reason": "after explicit cleanup"}),
        snapshot: None,
    };
    assert!(matches!(
        ledger.append(&marker),
        Err(LedgerError::UnfinishedTransaction)
    ));
    {
        let connection = ledger.connection.lock().unwrap();
        assert!(!connection.is_autocommit());
        assert_eq!(head(&connection).unwrap(), (before.seq, before.hash));
        connection.execute_batch("ROLLBACK").unwrap();
    }
    assert_eq!(ledger.append(&marker).unwrap().seq, before.seq + 1);
    assert!(ledger.verify().unwrap().is_intact());
}

#[derive(Debug)]
struct DelayedAnchor {
    file: FileAnchor,
    armed: Arc<AtomicBool>,
    entered: mpsc::Sender<Anchor>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl HeadAnchor for DelayedAnchor {
    fn load(&self) -> Result<Option<Anchor>> {
        self.file.load()
    }

    fn store(&self, head: &Anchor) -> Result<()> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered
                .send(head.clone())
                .map_err(|error| LedgerError::Io(std::io::Error::other(error.to_string())))?;
            self.release
                .lock()
                .map_err(|_| LedgerError::Poisoned)?
                .recv_timeout(COMPLETION_TIMEOUT)
                .map_err(|error| LedgerError::Io(std::io::Error::other(error.to_string())))?;
        }
        self.file.store(head)
    }
}

// Release the blocked writer even when a parent assertion fails.
struct ReleaseAnchor(mpsc::Sender<()>);

impl Drop for ReleaseAnchor {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
fn independent_handles_serialize_commit_through_anchor_publication() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("coordination.db");
    let armed = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = Arc::new(
        Ledger::open_anchored(
            &path,
            crate::Network::Testnet,
            Some(Box::new(DelayedAnchor {
                file: FileAnchor::beside(&path),
                armed: armed.clone(),
                entered: entered_tx,
                release: Mutex::new(release_rx),
            })),
        )
        .unwrap(),
    );
    let account_a = Address::from_bytes([1; 20]);
    let account_b = Address::from_bytes([2; 20]);
    let first_order = order(&first, 1, account_a);
    let second_order = order(&first, 2, account_b);
    let third_order = order(&first, 3, account_b);
    // A file symlink must share both the coordination lock and default anchor.
    let alias = alternate_path(&path);
    let second = Arc::new(Ledger::open_at(&alias, crate::Network::Testnet).unwrap());
    let before = first.chain_head().unwrap();

    armed.store(true, Ordering::SeqCst);
    let first_writer = {
        let journal = EventViews::new(first.clone()).submissions();
        std::thread::spawn(move || journal.begin(account_a, &first_order, 0, 101))
    };
    let release = ReleaseAnchor(release_tx);
    let paused_head = entered_rx.recv_timeout(COMPLETION_TIMEOUT).unwrap();
    assert_eq!(paused_head.seq, before.seq + 1);
    assert_eq!(FileAnchor::beside(&path).load().unwrap(), Some(before));
    // Read SQLite directly: using Ledger here would itself wait for the guard.
    // This proves the pause is after COMMIT, not inside its write transaction.
    let observer = Connection::open(&path).unwrap();
    assert_eq!(
        head(&observer).unwrap(),
        (paused_head.seq, paused_head.hash.clone())
    );

    let (attempted_tx, attempted_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    let second_writer = {
        let journal = EventViews::new(second.clone()).submissions();
        std::thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let result = (|| -> std::result::Result<(), SubmissionError> {
                let receipt = journal.begin(account_b, &second_order, 0, 102)?;
                journal.resolve(
                    &receipt,
                    SubmissionResolution::NotSent {
                        detail: "definitely not sent".into(),
                    },
                    103,
                )
            })();
            completed_tx.send(result).unwrap();
        })
    };
    attempted_rx.recv_timeout(COMPLETION_TIMEOUT).unwrap();
    let while_paused = completed_rx.recv_timeout(BLOCKED_WINDOW);
    let head_while_paused = head(&observer).unwrap();
    drop(release);
    let first_receipt = first_writer.join().unwrap().unwrap();
    second_writer.join().unwrap();
    assert!(
        matches!(while_paused, Err(mpsc::RecvTimeoutError::Timeout)),
        "another handle completed or failed its lifecycle pair before anchor publication: {while_paused:?}"
    );
    assert_eq!(head_while_paused, (paused_head.seq, paused_head.hash));
    completed_rx
        .recv_timeout(COMPLETION_TIMEOUT)
        .unwrap()
        .unwrap();

    let journal = EventViews::new(second.clone()).submissions();
    let state = journal.state(account_b).unwrap();
    assert!(state.pending.is_none());
    assert_eq!(state.revision, paused_head.seq + 2);
    let newest = journal
        .begin(account_b, &third_order, state.revision, 104)
        .unwrap();
    let events = second.get_events(0, 100).unwrap().events;
    let last = events.last().unwrap();
    let expected = Anchor {
        seq: last.seq,
        hash: last.hash.clone(),
    };
    assert_eq!(expected.seq, paused_head.seq + 3);
    assert_eq!(last.kind, EventKind::SubmissionStarted);
    assert_eq!(first.chain_head().unwrap(), expected);
    assert_eq!(
        FileAnchor::beside(&path).load().unwrap(),
        Some(expected.clone())
    );
    assert!(first.verify().unwrap().is_intact());
    assert!(second.verify().unwrap().is_intact());
    drop(journal);
    drop(first);
    drop(second);
    drop(observer);

    let reopened = Arc::new(Ledger::open_at(&path, crate::Network::Testnet).unwrap());
    let journal = EventViews::new(reopened.clone()).submissions();
    assert_eq!(reopened.chain_head().unwrap(), expected);
    assert_eq!(
        journal.state(account_a).unwrap().pending.unwrap().cloid(),
        first_receipt.cloid()
    );
    assert_eq!(
        journal.state(account_b).unwrap().pending.unwrap().cloid(),
        newest.cloid()
    );
    assert_eq!(journal.state(account_b).unwrap().revision, expected.seq);
    assert!(reopened.verify().unwrap().is_intact());
}

struct ChildGuard(Child, Option<std::thread::JoinHandle<()>>);

fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "coordination subprocess did not exit",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = wait_for_exit(&mut self.0, Duration::from_secs(2));
        if let Some(reader) = self.1.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

fn spawn_lock_holder(path: &Path, pairing: bool) -> ChildGuard {
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ledger::coordination_tests::subprocess_lock_holder",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_DATABASE, path)
            .env(CHILD_PAIRING, if pairing { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
        None,
    );
    let stdout = child.0.stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut ready = false;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if !ready && line.contains(CHILD_READY) {
                let _ = ready_tx.send(true);
                ready = true;
            }
        }
        if !ready {
            let _ = ready_tx.send(false);
        }
    });
    child.1 = Some(reader);
    assert!(
        ready_rx.recv_timeout(COMPLETION_TIMEOUT).unwrap(),
        "child exited before taking the lock"
    );
    child
}

#[test]
#[ignore = "subprocess helper, invoked by the coordination recovery test"]
fn subprocess_lock_holder() {
    let path = std::env::var_os(CHILD_DATABASE).expect("parent supplies fixture database");
    let ledger = Arc::new(Ledger::open_at(Path::new(&path), crate::Network::Testnet).unwrap());
    let (owner, guard) = if std::env::var(CHILD_PAIRING).as_deref() == Ok("1") {
        (
            Some(
                PairingJournal::open(
                    ledger.clone(),
                    Arc::new(crate::keys::HmacKey::from_bytes([11; 32])),
                )
                .unwrap(),
            ),
            None,
        )
    } else {
        (None, Some(ledger.lock().unwrap()))
    };
    println!("{CHILD_READY}");
    std::io::stdout().flush().unwrap();
    let mut release = [0];
    std::io::stdin().read_exact(&mut release).unwrap();
    drop(guard);
    drop(owner);
}

#[test]
fn pairing_owner_process_death_preserves_revocation_and_releases_ownership() {
    for graceful in [true, false] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pairing-process.db");
        let ledger = Arc::new(Ledger::open_at(&path, crate::Network::Testnet).unwrap());
        let key = Arc::new(crate::keys::HmacKey::from_bytes([11; 32]));
        let journal = PairingJournal::open(ledger.clone(), key.clone()).unwrap();
        let issued = journal
            .issue(
                PairingBinding {
                    agent: AgentId::new("coordination-agent"),
                    account: Address::from_bytes([1; 20]),
                },
                [12; 32],
                100,
            )
            .unwrap();
        assert!(journal.revoke(issued.id, 101).unwrap());
        drop(journal);
        let mut child = spawn_lock_holder(&alternate_path(&path), true);
        assert!(matches!(
            PairingJournal::open(ledger.clone(), key.clone()),
            Err(PairingError::AlreadyOwned)
        ));
        ledger
            .append(&NewEvent {
                kind: EventKind::OperatorAction,
                ts_ms: 102,
                agent_id: None,
                payload: &json!({"action": "ledger writes continue beside pairing ownership"}),
                snapshot: None,
            })
            .unwrap();
        if graceful {
            child.0.stdin.take().unwrap().write_all(&[1]).unwrap();
        } else {
            child.0.kill().unwrap();
        }
        let status = wait_for_exit(&mut child.0, COMPLETION_TIMEOUT).unwrap();
        assert_eq!(status.success(), graceful);
        let journal = PairingJournal::open(ledger.clone(), key).unwrap();
        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, issued.id);
        assert_eq!(records[0].revoked_at_ms, Some(101));
        assert!(ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn process_lock_releases_on_guard_drop_and_process_death() {
    for graceful in [true, false] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("process.db");
        let ledger = Arc::new(Ledger::open_at(&path, crate::Network::Testnet).unwrap());
        let mut child = spawn_lock_holder(&alternate_path(&path), false);
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let writer = {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                attempted_tx.send(()).unwrap();
                let result = ledger.append(&NewEvent {
                    kind: EventKind::OperatorAction,
                    ts_ms: 100,
                    agent_id: None,
                    payload: &json!({"action": "coordination test"}),
                    snapshot: None,
                });
                completed_tx.send(result).unwrap();
            })
        };
        attempted_rx.recv_timeout(COMPLETION_TIMEOUT).unwrap();
        let while_held = completed_rx.recv_timeout(BLOCKED_WINDOW);
        if graceful {
            child.0.stdin.take().unwrap().write_all(&[1]).unwrap();
        } else {
            child.0.kill().unwrap();
        }
        let status = wait_for_exit(&mut child.0, COMPLETION_TIMEOUT).unwrap();
        writer.join().unwrap();
        assert_eq!(status.success(), graceful);
        assert!(
            matches!(while_held, Err(mpsc::RecvTimeoutError::Timeout)),
            "parent write did not wait for the child-held lock: {while_held:?}"
        );
        let appended = completed_rx
            .recv_timeout(COMPLETION_TIMEOUT)
            .unwrap()
            .unwrap();
        let expected = Anchor {
            seq: appended.seq,
            hash: appended.hash,
        };
        assert_eq!(expected.seq, 1);
        assert_eq!(ledger.chain_head().unwrap(), expected);
        assert_eq!(
            FileAnchor::beside(&path).load().unwrap(),
            Some(expected.clone())
        );
        assert!(ledger.verify().unwrap().is_intact());
        drop(ledger);
        let reopened = Ledger::open_at(&path, crate::Network::Testnet).unwrap();
        assert_eq!(reopened.chain_head().unwrap(), expected);
        assert!(reopened.verify().unwrap().is_intact());
    }
}
