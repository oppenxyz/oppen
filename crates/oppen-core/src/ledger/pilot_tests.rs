use std::sync::{Barrier, mpsc};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::guardrail::{AuditEntry, AuditOutcome, AuditSink, Utilization};
use crate::ledger::tests::{audit_route, audit_sink};
use crate::ledger::{EventViews, SubmissionReceipt};

const BASE: u64 = 1_800_000_000_000;

// Only these legacy fixtures may create unsigned consent. Production exposes
// the registry-backed journal; old cumulative histories remain migration input.
struct LegacyPilotJournal(Arc<Ledger>);

impl LegacyPilotJournal {
    fn new(ledger: Arc<Ledger>) -> Self {
        Self(ledger)
    }

    fn authorize(&self, agent: AgentId, account: Address, at_ms: u64) -> Result<PilotState> {
        if self.0.network != Network::Testnet
            || crate::keys::checked_agent_id(&agent).is_err()
            || account == Address::ZERO
        {
            return Err(unavailable("invalid legacy fixture identity"));
        }
        let mut guard = self.0.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let history = history(&self.0, &tx, true)?;
        if !history.authorities.is_empty() {
            return Err(unavailable("pilot already authorized"));
        }
        check_baseline(&history, account, &agent)?;
        let (seq, hash) = super::super::head(&tx)?;
        let data = Authorized {
            version: 1,
            network: Network::Testnet,
            agent,
            account,
            baseline_at_ms: at_ms,
            baseline: Anchor { seq, hash },
            order_limit_usd: Decimal::from(15),
            executed_limit_usd: Decimal::from(150),
            realized_loss_limit_usd: Decimal::from(5),
        };
        let payload = serde_json::to_value(&data)?;
        let appended = super::super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::PilotAuthorized,
                ts_ms: at_ms as i64,
                agent_id: Some(data.agent.as_str()),
                payload: &payload,
                snapshot: None,
            },
            &format!("pilot_authorized:{account}"),
        )?
        .unwrap();
        tx.commit()?;
        self.0.note_head(&appended)?;
        Ok(initial(&data))
    }

    fn state(&self, account: Address) -> Result<Option<PilotState>> {
        let guard = self.0.lock()?;
        let history = history(&self.0, &guard, true)?;
        history
            .authorities
            .iter()
            .find(|a| a.data.account == account)
            .map(|a| project(&history, a))
            .transpose()
    }
}

fn account() -> Address {
    Address::from_bytes([1; 20])
}

fn authenticated_fixture() -> (
    TempDir,
    Arc<Ledger>,
    Arc<super::super::RegistryJournal>,
    PilotJournal,
) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let registry = Arc::new(
        super::super::RegistryJournal::open(
            ledger.clone(),
            Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
        )
        .unwrap(),
    );
    registry
        .grant(audit_route(agent(), account(), BASE - 1).binding, BASE - 1)
        .unwrap();
    let journal = PilotJournal::new(registry.clone());
    (dir, ledger, registry, journal)
}

#[test]
fn consent_authentication_is_required_at_both_execution_checks_but_not_inspection() {
    let (_dir, ledger, registry, journal) = authenticated_fixture();
    let mut clearance = order(1, "1");
    clearance.route = registry.route_for_agent(&agent()).unwrap();
    {
        let guard = ledger.lock().unwrap();
        for required in [false, true] {
            assert_eq!(
                check_authenticated_admission(&registry, &guard, account(), &clearance, required)
                    .is_err(),
                required
            );
            assert_eq!(
                check_authenticated_before_sign(&registry, &guard, &clearance, required).is_err(),
                required
            );
        }
    }
    journal.authorize(agent(), account(), BASE).unwrap();
    let signed = ledger.get_events(0, 100).unwrap().events.pop().unwrap();
    let payload = signed.payload.unwrap();
    assert_eq!(payload["envelope"]["version"], 2);
    assert_eq!(
        payload["envelope"]["operation"]["authorization"]["executed_limit_usd"],
        "150"
    );
    assert_eq!(
        status(&ledger, account()).unwrap().unwrap().authentication,
        PilotAuthentication::Unverified
    );
    assert_eq!(
        journal.status(account()).unwrap().unwrap().authentication,
        PilotAuthentication::Verified
    );
    persist(&ledger, &clearance);
    let submissions = super::super::SubmissionJournal::authenticated(registry.clone(), true);
    let before = ledger.chain_head().unwrap();
    submissions.preflight(&clearance).unwrap();
    assert_eq!(ledger.chain_head().unwrap(), before);
    submissions
        .begin(account(), &clearance, 0, BASE + 2)
        .unwrap();
    let guard = ledger.lock().unwrap();
    check_authenticated_before_sign(&registry, &guard, &clearance, true).unwrap();
    let mut different = clearance.clone();
    different.route.binding.wallet.generation += 1;
    assert!(check_authenticated_before_sign(&registry, &guard, &different, true).is_err());
    different.agent = AgentId::new("other");
    assert!(check_authenticated_admission(&registry, &guard, account(), &different, true).is_err());
}

#[test]
fn authenticated_preflight_preserves_exhausted_budget_without_writes() {
    let (_dir, ledger, registry, journal) = authenticated_fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let mut clearance = order(1, "1");
    clearance.route = registry.route_for_agent(&agent()).unwrap();
    begin(&ledger, &clearance);
    record(&ledger, 1, &payload(1, 1, "1", "0", "5", BASE + 10));
    let submissions = super::super::SubmissionJournal::authenticated(registry, true);
    let before = ledger.chain_head().unwrap();
    assert!(matches!(
        submissions.preflight(&clearance),
        Err(PilotError::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
    assert_eq!(ledger.chain_head().unwrap(), before);
    assert!(journal.state(account()).unwrap().unwrap().halt.is_some());
}

#[test]
fn consent_rejects_invalid_identity_wrong_key_and_wrong_account_without_writes() {
    let (_dir, ledger, _registry, journal) = authenticated_fixture();
    let before = ledger.chain_head().unwrap();
    for name in ["", "bad agent", "../agent", "nonascii-\u{00e9}"] {
        assert!(
            journal
                .authorize(AgentId::new(name), account(), BASE)
                .is_err()
        );
    }
    assert!(
        journal
            .authorize(AgentId::new("a".repeat(65)), account(), BASE)
            .is_err()
    );
    assert!(journal.authorize(agent(), Address::ZERO, BASE).is_err());
    assert!(
        journal
            .authorize(agent(), Address::from_bytes([2; 20]), BASE)
            .is_err()
    );
    assert!(
        super::super::RegistryJournal::open(
            ledger.clone(),
            Arc::new(crate::keys::HmacKey::from_bytes([99; 32]))
        )
        .is_err()
    );
    assert_eq!(ledger.chain_head().unwrap(), before);
}

#[test]
fn status_keeps_one_snapshot_when_a_raw_writer_changes_accounting_after_verification() {
    for authenticated in [false, true] {
        let (dir, ledger, _registry, journal) = authenticated_fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        begin(&ledger, &order(1, "1"));
        let path = dir.path().join(crate::db_file_name(Network::Testnet));
        super::super::submission::after_verified_walk(move || {
            let raw = Connection::open(path).unwrap();
            assert_eq!(
                raw.execute(
                    "UPDATE events SET payload = NULL WHERE kind = 'submission_started'",
                    []
                )
                .unwrap(),
                1
            );
        });
        let result = if authenticated {
            journal.status(account())
        } else {
            status(&ledger, account())
        };
        let observed = result.unwrap().unwrap();
        assert!(
            matches!(observed.accounting, PilotAccounting::Known { reserved_usd, .. } if reserved_usd == dec("10"))
        );
        assert_eq!(
            observed.authentication,
            if authenticated {
                PilotAuthentication::Verified
            } else {
                PilotAuthentication::Unverified
            }
        );
        assert!(
            journal.status(account()).is_err(),
            "a later snapshot must observe the damaged evidence"
        );
        assert!(status(&ledger, account()).is_err());
        assert!(ledger.connection.lock().unwrap().is_autocommit());
    }
}

#[test]
fn legacy_adoption_preserves_baseline_fills_pending_cancel_allocation_and_stops() {
    for scenario in ["pending", "canceled", "stopped"] {
        let (dir, ledger, registry, journal) = authenticated_fixture();
        let legacy = LegacyPilotJournal::new(ledger.clone());
        legacy.authorize(agent(), account(), BASE).unwrap();
        let receipt = begin(&ledger, &order(1, "1"));
        if scenario != "pending" {
            observed(&ledger, &receipt, 1, "canceled");
        }
        record(
            &ledger,
            1,
            &payload(
                1,
                1,
                "0.5",
                "0",
                if scenario == "stopped" { "5" } else { "0.01" },
                BASE + 10,
            ),
        );
        let original = legacy.state(account()).unwrap().unwrap();
        assert_eq!(original.executed_usd, dec("5"));
        assert_eq!(original.reserved_usd, dec("5"));
        assert_eq!(
            status(&ledger, account()).unwrap().unwrap().authentication,
            PilotAuthentication::LegacyReviewRequired
        );
        assert!(journal.state(account()).is_err());
        {
            let guard = ledger.lock().unwrap();
            for required in [false, true] {
                assert!(
                    check_authenticated_admission(
                        &registry,
                        &guard,
                        account(),
                        &order(2, "1"),
                        required
                    )
                    .is_err()
                );
                assert!(
                    check_authenticated_before_sign(&registry, &guard, &order(1, "1"), required)
                        .is_err()
                );
            }
        }
        let rows = ledger.get_events(0, 100).unwrap().events;
        let pending = EventViews::new(ledger.clone())
            .submissions()
            .state(account())
            .unwrap()
            .pending;
        let review = journal.review_legacy(account()).unwrap();
        let review_view = serde_json::to_value(&review).unwrap();
        assert_eq!(review_view["state"]["executed_usd"], "5.0");
        assert_eq!(review_view["state"]["account"], json!(account()));
        assert_eq!(
            review_view["head"]["seq"],
            json!(ledger.chain_head().unwrap().seq)
        );
        assert_eq!(journal.adopt_legacy(&review, BASE + 20).unwrap(), original);
        let adopted_head = ledger.chain_head().unwrap();
        assert_eq!(journal.adopt_legacy(&review, BASE + 21).unwrap(), original);
        assert_eq!(ledger.chain_head().unwrap(), adopted_head);
        let after = ledger.get_events(0, 100).unwrap().events;
        for (old, preserved) in rows.iter().zip(&after) {
            assert_eq!(old.hash, preserved.hash);
            assert_eq!(old.payload, preserved.payload);
        }
        assert_eq!(
            EventViews::new(ledger.clone())
                .submissions()
                .state(account())
                .unwrap()
                .pending,
            pending
        );
        assert_eq!(journal.state(account()).unwrap().unwrap(), original);
        drop(journal);
        drop(registry);
        drop(legacy);
        drop(ledger);
        let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
        let registry = Arc::new(
            super::super::RegistryJournal::open(
                ledger.clone(),
                Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
            )
            .unwrap(),
        );
        assert_eq!(
            PilotJournal::new(registry)
                .state(account())
                .unwrap()
                .unwrap(),
            original
        );
        assert!(ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn legacy_review_is_cas_bound_and_does_not_automatically_adopt_on_open() {
    let (_dir, ledger, _registry, journal) = authenticated_fixture();
    LegacyPilotJournal::new(ledger.clone())
        .authorize(agent(), account(), BASE)
        .unwrap();
    let review = journal.review_legacy(account()).unwrap();
    ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: (BASE + 1) as i64,
            agent_id: None,
            payload: &json!({"synthetic":true}),
            snapshot: None,
        })
        .unwrap();
    let before = ledger.chain_head().unwrap();
    assert!(journal.adopt_legacy(&review, BASE + 2).is_err());
    assert_eq!(ledger.chain_head().unwrap(), before);
    assert!(journal.state(account()).is_err());
    journal
        .adopt_legacy(&journal.review_legacy(account()).unwrap(), BASE + 3)
        .unwrap();
    assert!(journal.authorize(agent(), account(), BASE + 4).is_err());
}

#[test]
fn defensive_invalid_mac_is_rejected_without_changing_accounting_projection() {
    let (_dir, ledger, registry, journal) = authenticated_fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let mut guard = ledger.lock().unwrap();
    let tx = guard.transaction().unwrap();
    let mut history = history(&ledger, &tx, true).unwrap();
    let authority = &mut history.authorities[0];
    let payload = authority.event.payload.as_mut().unwrap();
    payload["mac"] = json!("00".repeat(32));
    // Unit-test the MAC verifier directly. This temporary malformed fixture is
    // rolled back; no forged chain or replacement anchor is constructed.
    tx.execute(
        "UPDATE events SET payload = ?1 WHERE seq = ?2",
        params![
            super::super::hash::canonical_json(payload).unwrap(),
            authority.seq
        ],
    )
    .unwrap();
    assert!(
        matches!(authority::verify(&registry, &tx, &history), Err(PilotError::Unavailable { detail }) if detail.contains("MAC mismatch"))
    );
    let state = project(&history, &history.authorities[0]).unwrap();
    assert_eq!(state.executed_usd, Decimal::ZERO);
    tx.rollback().unwrap();
}
fn agent() -> AgentId {
    AgentId::new("synthetic-pilot")
}
fn dec(value: &str) -> Decimal {
    Decimal::from_str_exact(value).unwrap()
}
fn fixture() -> (TempDir, Arc<Ledger>, LegacyPilotJournal) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = LegacyPilotJournal::new(ledger.clone());
    (dir, ledger, journal)
}

// Pilot-only lock proofs retain the guard for the simulated signing window.
// Production combines route and pilot validation under its own single guard.
fn before_sign<'a>(
    ledger: &'a Ledger,
    clearance: &Clearance,
) -> Result<super::super::LedgerGuard<'a>> {
    let guard = ledger.lock()?;
    check_before_sign(ledger, &guard, clearance)?;
    Ok(guard)
}

fn order(id: u8, size: &str) -> Clearance {
    Clearance {
        approval_review_digest: None,
        policy_revision: crate::ledger::tests::AUDIT_POLICY_REVISION,
        route: audit_route(agent(), account(), BASE + 1),
        agent: agent(),
        vault_address: None,
        network: Network::Testnet,
        evaluated_at_ms: BASE + 1,
        kind: ClearedKind::Order {
            symbol: "BTC".into(),
            is_buy: true,
            px: dec("10"),
            sz: dec(size),
            notional_usd: dec("10") * dec(size),
            reduce_only: false,
            slippage_bps: Decimal::ZERO,
            reference_px: dec("10"),
            slippage_reference_px: dec("10"),
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
            order_tokens_remaining: Decimal::from(100),
            global_tokens_remaining: Decimal::from(1000),
        },
    }
}
fn persist(ledger: &Arc<Ledger>, clearance: &Clearance) {
    audit_sink(ledger.clone())
        .record(&AuditEntry {
            agent: Some(&clearance.agent),
            at_ms: clearance.evaluated_at_ms,
            reason: "synthetic fixture only",
            outcome: AuditOutcome::Cleared(clearance),
        })
        .unwrap();
}
fn begin(ledger: &Arc<Ledger>, clearance: &Clearance) -> SubmissionReceipt {
    persist(ledger, clearance);
    let submissions = EventViews::new(ledger.clone()).submissions();
    submissions
        .begin(
            account(),
            clearance,
            submissions.state(account()).unwrap().revision,
            BASE + 2,
        )
        .unwrap()
}
fn observed(ledger: &Arc<Ledger>, receipt: &SubmissionReceipt, id: u8, status: &str) {
    EventViews::new(ledger.clone())
        .submissions()
        .resolve(
            receipt,
            SubmissionResolution::Observed {
                oid: u64::from(id) + 1000,
                status: status.into(),
            },
            BASE + 3,
        )
        .unwrap();
}
fn payload(id: u8, tid: u64, size: &str, pnl: &str, fee: &str, ts_ms: u64) -> Value {
    json!({"account": account(), "tid": tid, "ts_ms": ts_ms, "oid": u64::from(id) + 1000,
        "cloid": Cloid::from_bytes([id; 16]), "coin": "BTC", "side": "buy", "px": "10",
        "sz": size, "closed_pnl": pnl, "fee": fee, "fee_token": "USDC"})
}
fn record(ledger: &Ledger, tid: u64, value: &Value) -> Option<Appended> {
    ledger
        .record_fill(&NewFill {
            account: &account().to_string(),
            tid,
            ts_ms: value["ts_ms"].as_u64().unwrap_or(BASE + 10) as i64,
            agent_id: None,
            payload: value,
        })
        .expect("budget evidence must not roll back the fill")
}
fn halt_count(ledger: &Ledger) -> usize {
    ledger
        .get_events(0, 1000)
        .unwrap()
        .events
        .iter()
        .filter(|e| {
            e.kind == EventKind::PilotHalted
                || (e.kind == EventKind::Fill
                    && e.payload
                        .as_ref()
                        .is_some_and(|p| p.get("pilot_stop").is_some()))
        })
        .count()
}

#[test]
fn status_reports_known_accounting_and_verified_stops_without_writes() {
    for scenario in ["empty", "awaiting", "embedded", "standalone"] {
        let (_dir, ledger, journal) = fixture();
        let views = EventViews::new(ledger.clone());
        assert!(views.pilot_status(account()).unwrap().is_none());
        journal.authorize(agent(), account(), BASE).unwrap();
        if scenario != "empty" {
            begin(&ledger, &order(1, "1"));
            let mut value = payload(1, 1, "1", "0", "0", BASE + 10);
            match scenario {
                "awaiting" => value["cloid"] = Value::Null,
                "embedded" => value["fee"] = json!("5"),
                _ => {}
            }
            record(&ledger, 1, &value);
            if scenario == "standalone" {
                value["fee"] = json!("1");
                assert!(record(&ledger, 1, &value).is_none());
            }
        }
        let head = ledger.chain_head().unwrap();
        let view = views.pilot_status(account()).unwrap().unwrap();
        let state = journal.state(account()).unwrap().unwrap();
        assert_eq!(view.agent, agent());
        assert_eq!(view.account, account());
        assert_eq!(view.halt, state.halt);
        match &view.accounting {
            PilotAccounting::Known {
                executed_usd,
                reserved_usd,
                net_realized_pnl_usd,
            } => {
                assert_eq!(*executed_usd, state.executed_usd);
                assert_eq!(*reserved_usd, state.reserved_usd);
                assert_eq!(*net_realized_pnl_usd, state.net_realized_pnl_usd);
            }
            other => panic!("{scenario}: unexpected accounting {other:?}"),
        }
        match scenario {
            "empty" => assert!(view.halt.is_none()),
            "awaiting" => assert!(matches!(view.halt, Some(PilotStop::AwaitingReconciliation))),
            "embedded" => assert!(matches!(
                view.halt,
                Some(PilotStop::Exhausted {
                    metric: PilotMetric::RealizedLoss,
                    ..
                })
            )),
            "standalone" => assert!(matches!(view.halt, Some(PilotStop::Unavailable { .. }))),
            _ => unreachable!(),
        }
        let encoded = serde_json::to_value(&view).unwrap();
        assert_eq!(encoded["accounting"], json!("known"));
        for field in ["executed_usd", "reserved_usd", "net_realized_pnl_usd"] {
            assert!(encoded[field].is_string(), "{scenario}: {field}");
        }
        assert!(
            views
                .pilot_status(Address::from_bytes([2; 20]))
                .unwrap()
                .is_none()
        );
        assert_eq!(ledger.chain_head().unwrap(), head);
    }
}

#[test]
fn status_retains_verified_halt_when_contradictory_fill_accounting_fails() {
    let (dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let receipt = begin(&ledger, &order(1, "1"));
    EventViews::new(ledger.clone())
        .submissions()
        .resolve(
            &receipt,
            SubmissionResolution::NotSent {
                detail: "synthetic unsent".into(),
            },
            BASE + 3,
        )
        .unwrap();
    record(&ledger, 1, &payload(1, 1, "1", "0", "0", BASE + 10));
    assert!(journal.state(account()).is_err());
    let view = status(&ledger, account()).unwrap().unwrap();
    assert_eq!(view.agent, agent());
    assert_eq!(view.account, account());
    assert!(matches!(&view.halt, Some(PilotStop::Unavailable { detail }) if !detail.is_empty()));
    assert!(
        matches!(&view.accounting, PilotAccounting::Unavailable { detail } if !detail.is_empty())
    );
    let encoded = serde_json::to_value(&view).unwrap();
    assert_eq!(encoded["accounting"], json!("unavailable"));
    for field in ["executed_usd", "reserved_usd", "net_realized_pnl_usd"] {
        assert!(
            encoded.get(field).is_none(),
            "unavailable accounting must not invent {field}"
        );
    }
    assert!(
        status(&ledger, Address::from_bytes([2; 20]))
            .unwrap()
            .is_none()
    );
    drop(journal);
    drop(ledger);
    let ledger = Ledger::open(dir.path(), Network::Testnet).unwrap();
    assert_eq!(
        serde_json::to_value(status(&ledger, account()).unwrap().unwrap()).unwrap(),
        encoded
    );
}

#[test]
fn status_rejects_redacted_required_history_or_corrupt_chain() {
    for target in ["authority", "intent", "start", "fill", "halt", "hash"] {
        let (_dir, ledger, journal) = fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        let auth_seq = ledger.chain_head().unwrap().seq;
        begin(&ledger, &order(1, "1"));
        let start_seq = ledger.chain_head().unwrap().seq;
        let mut value = payload(1, 1, "1", "0", "0", BASE + 10);
        let fill_seq = record(&ledger, 1, &value).unwrap().seq;
        value["fee"] = json!("1");
        record(&ledger, 1, &value);
        let halt_seq = ledger.chain_head().unwrap().seq;
        let seq = match target {
            "authority" => auth_seq,
            "intent" => start_seq - 1,
            "start" => start_seq,
            "fill" => fill_seq,
            _ => halt_seq,
        };
        if target == "hash" {
            ledger
                .lock()
                .unwrap()
                .execute(
                    "UPDATE events SET hash = 'broken' WHERE seq = ?1",
                    params![seq],
                )
                .unwrap();
        } else {
            ledger
                .redact(seq, "synthetic retention", (BASE + 20) as i64)
                .unwrap();
        }
        assert!(status(&ledger, account()).is_err(), "{target}");
    }
}

#[test]
fn authorization_is_immutable_testnet_bound_and_excludes_old_rows() {
    let (dir, ledger, journal) = fixture();
    record(&ledger, 1, &payload(99, 1, "100", "-100", "5", BASE - 10));
    let baseline = ledger.chain_head().unwrap();
    let state = journal.authorize(agent(), account(), BASE).unwrap();
    let authorization = ledger
        .event(baseline.seq + 1)
        .unwrap()
        .unwrap()
        .payload
        .unwrap();
    assert_eq!(authorization["order_limit_usd"], json!("15"));
    assert_eq!(authorization["executed_limit_usd"], json!("150"));
    assert_eq!(authorization["realized_loss_limit_usd"], json!("5"));
    assert!(
        journal
            .authorize(AgentId::new("other"), Address::from_bytes([2; 20]), BASE)
            .is_err()
    );
    assert_eq!(state.baseline, baseline);
    assert_eq!(state.executed_usd, Decimal::ZERO);
    assert_eq!(state.net_realized_pnl_usd, Decimal::ZERO);
    assert!(
        journal
            .authorize(agent(), account(), BASE + 86_400_000)
            .is_err()
    );
    assert!(
        journal
            .authorize(agent(), Address::from_bytes([2; 20]), BASE)
            .is_err()
    );
    assert!(
        journal
            .authorize(AgentId::new("other"), account(), BASE)
            .is_err()
    );
    assert!(
        journal
            .state(Address::from_bytes([2; 20]))
            .unwrap()
            .is_none()
    );
    let mainnet = Arc::new(Ledger::open(dir.path(), Network::Mainnet).unwrap());
    assert!(
        LegacyPilotJournal::new(mainnet)
            .authorize(agent(), account(), BASE)
            .is_err()
    );
    assert!(ledger.verify().unwrap().is_intact());
}

#[test]
fn authorization_refuses_pending_or_unfilled_observed_baseline() {
    for resolution in [None, Some("canceled"), Some("resting")] {
        let (_dir, ledger, journal) = fixture();
        let receipt = begin(&ledger, &order(1, "1"));
        if let Some(status) = resolution {
            observed(&ledger, &receipt, 1, status);
        }
        assert!(journal.authorize(agent(), account(), BASE + 10).is_err());
    }
    let (_dir, ledger, journal) = fixture();
    let receipt = begin(&ledger, &order(1, "1"));
    observed(&ledger, &receipt, 1, "filled");
    record(&ledger, 1, &payload(1, 1, "1", "0", "0", BASE + 4));
    assert!(
        journal.authorize(agent(), account(), BASE + 10).is_ok(),
        "fully accounted prior order, operator still confirms flatness"
    );
}

#[test]
fn before_sign_requires_the_exact_pending_reservation_and_conservative_order_limit() {
    let (_dir, ledger, journal) = fixture();
    let clearance = order(1, "1");
    drop(before_sign(&ledger, &clearance).unwrap());
    journal.authorize(agent(), account(), BASE).unwrap();
    persist(&ledger, &clearance);
    assert!(matches!(
        before_sign(&ledger, &clearance),
        Err(PilotError::Unavailable { .. })
    ));
    let receipt = begin(&ledger, &clearance);
    drop(before_sign(&ledger, &clearance).unwrap());
    let mut changed = clearance.clone();
    changed.evaluated_at_ms += 1;
    assert!(before_sign(&ledger, &changed).is_err());
    changed = clearance.clone();
    changed.network = Network::Mainnet;
    assert!(before_sign(&ledger, &changed).is_err());
    changed = clearance.clone();
    changed.vault_address = Some(Address::from_bytes([2; 20]));
    assert!(before_sign(&ledger, &changed).is_err());
    observed(&ledger, &receipt, 1, "canceled");
    assert!(before_sign(&ledger, &clearance).is_err());
    let mut costly = order(2, "1");
    if let ClearedKind::Order { reference_px, .. } = &mut costly.kind {
        *reference_px = dec("15.01");
    }
    let guard = ledger.lock().unwrap();
    assert!(
        matches!(check_admission(&ledger, &guard, account(), &costly),
        Err(PilotError::Exhausted { metric: PilotMetric::OrderNotional, observed_usd, .. }) if observed_usd == dec("15.01"))
    );
}

#[test]
fn partial_fills_keep_canceled_allocation_and_full_quantity_releases_it() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let receipt = begin(&ledger, &order(1, "1"));
    observed(&ledger, &receipt, 1, "canceled");
    let initial = journal.state(account()).unwrap().unwrap();
    assert_eq!(
        (initial.executed_usd, initial.reserved_usd),
        (Decimal::ZERO, dec("10"))
    );
    record(&ledger, 1, &payload(1, 1, "0.4", "0", "0.1", BASE + 10));
    let partial = journal.state(account()).unwrap().unwrap();
    assert_eq!(
        (partial.executed_usd, partial.reserved_usd),
        (dec("4"), dec("6"))
    );
    assert_eq!(partial.net_realized_pnl_usd, dec("-0.1"));
    record(&ledger, 2, &payload(1, 2, "0.6", "0", "0", BASE + 11));
    let full = journal.state(account()).unwrap().unwrap();
    assert_eq!(
        (full.executed_usd, full.reserved_usd),
        (dec("10"), Decimal::ZERO)
    );
}

#[test]
fn definite_notsent_or_rejected_releases_allocation_but_conflicting_fill_blocks() {
    for outcome in [
        SubmissionResolution::NotSent {
            detail: "no transport".into(),
        },
        SubmissionResolution::Rejected {
            message: "venue refused".into(),
        },
    ] {
        let (_dir, ledger, journal) = fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        let receipt = begin(&ledger, &order(1, "1"));
        EventViews::new(ledger.clone())
            .submissions()
            .resolve(&receipt, outcome, BASE + 3)
            .unwrap();
        assert_eq!(
            journal.state(account()).unwrap().unwrap().reserved_usd,
            Decimal::ZERO
        );
        assert!(record(&ledger, 1, &payload(1, 1, "1", "0", "0", BASE + 10)).is_some());
        assert!(journal.state(account()).is_err());
        assert_eq!(halt_count(&ledger), 1);
    }
}

#[test]
fn committed_limit_counts_canceled_allocations_without_calling_them_executed() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    for id in 1..=10 {
        let receipt = begin(&ledger, &order(id, "1.5"));
        observed(&ledger, &receipt, id, "canceled");
    }
    let state = journal.state(account()).unwrap().unwrap();
    assert_eq!(state.executed_usd, Decimal::ZERO);
    assert_eq!(state.reserved_usd, dec("150"));
    assert!(
        state.halt.is_none(),
        "allocation alone is not an executed threshold hit"
    );
    let guard = ledger.lock().unwrap();
    assert!(
        matches!(check_admission(&ledger, &guard, account(), &order(11, "1")),
        Err(PilotError::Exhausted { metric: PilotMetric::CommittedNotional, observed_usd, .. }) if observed_usd == dec("160"))
    );
}

#[test]
fn executed_threshold_counts_open_and_close_and_survives_restart_and_midnight() {
    let (dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    for id in 1..=10 {
        let mut clearance = order(id, "1.5");
        if let ClearedKind::Order {
            reduce_only,
            is_buy,
            ..
        } = &mut clearance.kind
        {
            *reduce_only = id % 2 == 0;
            *is_buy = id % 2 != 0;
        }
        let receipt = begin(&ledger, &clearance);
        observed(&ledger, &receipt, id, "filled");
        let mut value = payload(
            id,
            u64::from(id),
            "1.5",
            "0",
            "0",
            BASE + u64::from(id) * 86_400_000,
        );
        value["side"] = json!(if id % 2 == 0 { "sell" } else { "buy" });
        record(&ledger, u64::from(id), &value);
    }
    let state = journal.state(account()).unwrap().unwrap();
    assert_eq!(state.executed_usd, dec("150"));
    assert_eq!(state.reserved_usd, Decimal::ZERO);
    assert!(matches!(
        state.halt,
        Some(PilotStop::Exhausted {
            metric: PilotMetric::ExecutedNotional,
            ..
        })
    ));
    assert_eq!(halt_count(&ledger), 1);
    let head = ledger.chain_head().unwrap();
    drop(journal);
    drop(ledger);
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert!(matches!(
        before_sign(&ledger, &order(11, "1")),
        Err(PilotError::Exhausted {
            metric: PilotMetric::ExecutedNotional,
            ..
        })
    ));
    assert!(ledger.verify().unwrap().is_intact());
}

#[test]
fn fees_hit_loss_limit_and_later_profit_cannot_rearm_even_reduce_only() {
    let (dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let clearance = order(1, "1.5");
    let _receipt = begin(&ledger, &clearance);
    record(&ledger, 1, &payload(1, 1, "0.5", "0", "2.5", BASE + 10));
    drop(before_sign(&ledger, &clearance).unwrap());
    record(&ledger, 2, &payload(1, 2, "0.5", "0", "2.5", BASE + 11));
    record(
        &ledger,
        3,
        &payload(1, 3, "0.5", "20", "0", BASE + 86_400_000),
    );
    let state = journal.state(account()).unwrap().unwrap();
    assert_eq!(state.net_realized_pnl_usd, dec("15"));
    assert!(
        matches!(state.halt, Some(PilotStop::Exhausted { metric: PilotMetric::RealizedLoss, observed_usd, .. }) if observed_usd == dec("5"))
    );
    assert_eq!(halt_count(&ledger), 1);
    drop(journal);
    drop(ledger);
    let ledger = Ledger::open(dir.path(), Network::Testnet).unwrap();
    let mut reduce = clearance;
    if let ClearedKind::Order { reduce_only, .. } = &mut reduce.kind {
        *reduce_only = true;
    }
    assert!(matches!(
        before_sign(&ledger, &reduce),
        Err(PilotError::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
    reduce.kind = ClearedKind::Cancel { count: 1 };
    drop(before_sign(&ledger, &reduce).unwrap());
    reduce.kind = ClearedKind::ScheduleCancel { cancel_at_ms: None };
    drop(before_sign(&ledger, &reduce).unwrap());
}

#[test]
fn out_of_order_fill_prefix_latches_loss_even_when_current_pnl_is_positive() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    begin(&ledger, &order(1, "1"));
    record(&ledger, 2, &payload(1, 2, "0.5", "20", "0", BASE + 20));
    assert!(journal.state(account()).unwrap().unwrap().halt.is_none());
    record(&ledger, 1, &payload(1, 1, "0.5", "-5", "0", BASE + 10));
    let state = journal.state(account()).unwrap().unwrap();
    assert_eq!(state.net_realized_pnl_usd, dec("15"));
    assert!(matches!(
        state.halt,
        Some(PilotStop::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
    assert_eq!(halt_count(&ledger), 1);
}

#[test]
fn duplicate_metadata_is_idempotent_but_changed_economics_persist_halt() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    begin(&ledger, &order(1, "1"));
    let mut value = payload(1, 1, "1", "1", "0.1", BASE + 10);
    record(&ledger, 1, &value);
    let head = ledger.chain_head().unwrap();
    value["attribution"] = json!("attributed");
    value["recovered_from_gap"] = json!(9);
    assert!(record(&ledger, 1, &value).is_none());
    assert_eq!(ledger.chain_head().unwrap(), head);
    value["closed_pnl"] = json!("2");
    value["fee"] = json!("1.1");
    assert!(
        record(&ledger, 1, &value).is_none(),
        "same net but different economic components is conflicting evidence"
    );
    let state = journal.state(account()).unwrap().unwrap();
    assert_eq!(state.executed_usd, dec("10"));
    assert_eq!(state.net_realized_pnl_usd, dec("0.9"));
    assert!(matches!(state.halt, Some(PilotStop::Unavailable { .. })));
    assert_eq!(halt_count(&ledger), 1);
    assert!(ledger.verify().unwrap().is_intact());
}

#[test]
fn valid_unmatched_fills_remain_counted_and_block_admission() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    record(&ledger, 1, &payload(99, 1, "1", "-1", "0.1", BASE + 10));
    let state = journal.state(account()).unwrap().unwrap();
    assert_eq!(state.executed_usd, dec("10"));
    assert_eq!(state.net_realized_pnl_usd, dec("-1.1"));
    assert!(matches!(
        state.halt,
        Some(PilotStop::AwaitingReconciliation)
    ));
    assert_eq!(halt_count(&ledger), 0);
    assert!(before_sign(&ledger, &order(1, "1")).is_err());
}

#[test]
fn oid_linkage_is_account_scoped_and_conflicting_symbol_side_cloid_or_quantity_blocks() {
    for mutation in ["oid_only", "oid", "coin", "side", "cloid", "quantity"] {
        let (_dir, ledger, journal) = fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        let receipt = begin(&ledger, &order(1, "1"));
        observed(&ledger, &receipt, 1, "filled");
        let mut value = payload(1, 1, "1", "0", "0", BASE + 10);
        match mutation {
            "oid_only" => value["cloid"] = Value::Null,
            "oid" => value["oid"] = json!(9999),
            "coin" => value["coin"] = json!("ETH"),
            "side" => value["side"] = json!("sell"),
            "cloid" => value["cloid"] = json!(Cloid::from_bytes([9; 16])),
            "quantity" => value["sz"] = json!("1.1"),
            _ => unreachable!(),
        }
        record(&ledger, 1, &value);
        if mutation == "oid_only" {
            let state = journal.state(account()).unwrap().unwrap();
            assert_eq!(state.executed_usd, dec("10"));
            assert_eq!(state.reserved_usd, Decimal::ZERO);
            assert!(state.halt.is_none());
        } else {
            assert!(journal.state(account()).is_err(), "{mutation}");
        }
    }
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let receipt = begin(&ledger, &order(1, "1"));
    observed(&ledger, &receipt, 1, "filled");
    let other = Address::from_bytes([2; 20]).to_string();
    let mut value = payload(1, 1, "1", "-10", "0", BASE + 10);
    value["account"] = json!(other);
    ledger
        .record_fill(&NewFill {
            account: &other,
            tid: 1,
            ts_ms: (BASE + 10) as i64,
            agent_id: None,
            payload: &value,
        })
        .unwrap();
    assert_eq!(
        journal.state(account()).unwrap().unwrap().executed_usd,
        Decimal::ZERO
    );
}

#[test]
fn fill_before_http_ack_recovers_without_resetting_accounting() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let receipt = begin(&ledger, &order(1, "1"));
    let mut value = payload(1, 1, "1", "-1", "0.1", BASE + 10);
    value["cloid"] = Value::Null;
    record(&ledger, 1, &value);
    let waiting = journal.state(account()).unwrap().unwrap();
    assert_eq!(waiting.executed_usd, dec("10"));
    assert!(matches!(
        waiting.halt,
        Some(PilotStop::AwaitingReconciliation)
    ));
    assert!(before_sign(&ledger, &order(1, "1")).is_err());
    assert_eq!(halt_count(&ledger), 0);
    observed(&ledger, &receipt, 1, "filled");
    let reconciled = journal.state(account()).unwrap().unwrap();
    assert_eq!(reconciled.executed_usd, dec("10"));
    assert_eq!(reconciled.net_realized_pnl_usd, dec("-1.1"));
    assert_eq!(reconciled.reserved_usd, Decimal::ZERO);
    assert!(reconciled.halt.is_none());
    let head = ledger.chain_head().unwrap();
    value["cloid"] = json!(Cloid::from_bytes([1; 16]));
    assert!(record(&ledger, 1, &value).is_none());
    assert_eq!(ledger.chain_head().unwrap(), head);
    begin(&ledger, &order(2, "1"));
    assert!(before_sign(&ledger, &order(2, "1")).is_ok());
    value["cloid"] = json!(Cloid::from_bytes([9; 16]));
    assert!(record(&ledger, 1, &value).is_none());
    let conflicted = journal.state(account()).unwrap().unwrap();
    assert_eq!(conflicted.executed_usd, dec("10"));
    assert!(matches!(
        conflicted.halt,
        Some(PilotStop::Unavailable { .. })
    ));
    assert_eq!(halt_count(&ledger), 1);
    assert!(before_sign(&ledger, &order(2, "1")).is_err());
}

#[test]
fn duplicate_cloid_before_observed_is_durably_halted_even_when_correct() {
    for duplicate_id in [9, 1] {
        let (dir, ledger, journal) = fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        let receipt = begin(&ledger, &order(1, "1"));
        let mut value = payload(1, 1, "1", "-1", "0.1", BASE + 10);
        value["cloid"] = Value::Null;
        let fill = record(&ledger, 1, &value).unwrap();
        assert!(matches!(
            journal.state(account()).unwrap().unwrap().halt,
            Some(PilotStop::AwaitingReconciliation)
        ));
        value["cloid"] = json!(Cloid::from_bytes([duplicate_id; 16]));
        assert!(record(&ledger, 1, &value).is_none());
        let stopped = journal.state(account()).unwrap().unwrap();
        assert!(
            matches!(stopped.halt, Some(PilotStop::Unavailable { .. })),
            "cloid {duplicate_id} has no durable oid binding yet"
        );
        assert_eq!(stopped.executed_usd, dec("10"));
        assert_eq!(halt_count(&ledger), 1);
        assert_eq!(
            ledger.event(fill.seq).unwrap().unwrap().payload.unwrap()["cloid"],
            Value::Null
        );
        observed(&ledger, &receipt, 1, "filled");
        assert_eq!(
            journal.state(account()).unwrap().unwrap().halt,
            stopped.halt
        );
        drop(journal);
        drop(ledger);
        let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
        let journal = LegacyPilotJournal::new(ledger.clone());
        let reopened = journal.state(account()).unwrap().unwrap();
        assert_eq!(reopened.halt, stopped.halt);
        assert_eq!(reopened.executed_usd, dec("10"));
        assert_eq!(reopened.net_realized_pnl_usd, dec("-1.1"));
        assert_eq!(halt_count(&ledger), 1);
        let guard = ledger.lock().unwrap();
        assert!(matches!(
            check_admission(&ledger, &guard, account(), &order(2, "1")),
            Err(PilotError::Unavailable { .. })
        ));
        drop(guard);
        assert!(matches!(
            before_sign(&ledger, &order(2, "1")),
            Err(PilotError::Unavailable { .. })
        ));
        assert!(ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn unmatched_fill_still_latches_actual_loss_threshold() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let receipt = begin(&ledger, &order(1, "1"));
    let mut value = payload(1, 1, "1", "0", "5", BASE + 10);
    value["cloid"] = Value::Null;
    record(&ledger, 1, &value);
    assert_eq!(halt_count(&ledger), 1);
    observed(&ledger, &receipt, 1, "filled");
    assert!(matches!(
        journal.state(account()).unwrap().unwrap().halt,
        Some(PilotStop::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
}

#[test]
fn deleted_authorization_cannot_hide_from_unconfigured_read_or_sign() {
    let (_dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    ledger
        .lock()
        .unwrap()
        .execute("DELETE FROM events", [])
        .unwrap();
    assert!(journal.state(account()).is_err());
    assert!(before_sign(&ledger, &order(1, "1")).is_err());
}

#[test]
fn unavailable_pilot_evidence_blocks_orders_but_not_cleanup() {
    for target in ["authorization", "submission"] {
        let (_dir, ledger, journal) = fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        let authorization = ledger.chain_head().unwrap().seq;
        let mut clearance = order(1, "1");
        begin(&ledger, &clearance);
        let submission = ledger.chain_head().unwrap().seq;
        drop(before_sign(&ledger, &clearance).unwrap());
        ledger
            .redact(
                if target == "authorization" {
                    authorization
                } else {
                    submission
                },
                "synthetic redaction",
                (BASE + 10) as i64,
            )
            .unwrap();
        assert!(ledger.verify().unwrap().is_intact());
        assert!(before_sign(&ledger, &clearance).is_err(), "{target}");
        for kind in [
            ClearedKind::Cancel { count: 1 },
            ClearedKind::ScheduleCancel { cancel_at_ms: None },
        ] {
            clearance.kind = kind;
            drop(before_sign(&ledger, &clearance).unwrap());
        }
    }
}

#[test]
fn malformed_or_late_fills_are_durable_and_fail_closed() {
    for mutation in [
        "decimal",
        "numeric",
        "currency",
        "account",
        "late",
        "overprecision",
    ] {
        let (_dir, ledger, journal) = fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        begin(&ledger, &order(1, "1"));
        let mut value = payload(1, 1, "1", "0", "0", BASE + 10);
        match mutation {
            "decimal" => value["fee"] = json!("NaN"),
            "numeric" => value["px"] = json!(10),
            "currency" => value["fee_token"] = json!("HYPE"),
            "account" => value
                .as_object_mut()
                .unwrap()
                .remove("account")
                .map(|_| ())
                .unwrap(),
            "late" => value["ts_ms"] = json!(BASE - 1),
            "overprecision" => value["fee"] = json!("0.123456789012345678901234567890123"),
            _ => unreachable!(),
        }
        let appended = record(&ledger, 1, &value).unwrap();
        let mut stored = ledger
            .event(appended.seq)
            .unwrap()
            .unwrap()
            .payload
            .unwrap();
        stored.as_object_mut().unwrap().remove("pilot_stop");
        assert_eq!(stored, value);
        assert!(journal.state(account()).is_err(), "{mutation}");
        assert!(before_sign(&ledger, &order(1, "1")).is_err());
        assert!(ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn redacted_authority_submission_or_fill_and_chain_corruption_fail_closed() {
    for target in ["authority", "intent", "start", "fill", "hash"] {
        let (_dir, ledger, journal) = fixture();
        journal.authorize(agent(), account(), BASE).unwrap();
        let auth_seq = ledger.chain_head().unwrap().seq;
        begin(&ledger, &order(1, "1"));
        let start_seq = ledger.chain_head().unwrap().seq;
        let fill_seq = record(&ledger, 1, &payload(1, 1, "0.5", "0", "0", BASE + 10))
            .unwrap()
            .seq;
        let seq = match target {
            "authority" => auth_seq,
            "intent" => start_seq - 1,
            "start" => start_seq,
            _ => fill_seq,
        };
        if target == "hash" {
            ledger
                .lock()
                .unwrap()
                .execute(
                    "UPDATE events SET hash = 'broken' WHERE seq = ?1",
                    params![seq],
                )
                .unwrap();
        } else {
            ledger
                .redact(seq, "synthetic retention", (BASE + 20) as i64)
                .unwrap();
        }
        assert!(journal.state(account()).is_err(), "{target}");
        assert!(before_sign(&ledger, &order(1, "1")).is_err());
    }
}

#[test]
fn independent_admissions_cannot_overreserve_the_remaining_pilot_budget() {
    let (dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    for id in 1..=9 {
        let receipt = begin(&ledger, &order(id, "1.5"));
        observed(&ledger, &receipt, id, "canceled");
    }
    let revision = EventViews::new(ledger.clone())
        .submissions()
        .state(account())
        .unwrap()
        .revision;
    persist(&ledger, &order(10, "1.5"));
    persist(&ledger, &order(11, "1.5"));
    let other = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [ledger.clone(), other]
        .into_iter()
        .enumerate()
        .map(|(index, ledger)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                EventViews::new(ledger).submissions().begin(
                    account(),
                    &order(index as u8 + 10, "1.5"),
                    revision,
                    BASE + 4,
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        journal.state(account()).unwrap().unwrap().reserved_usd,
        dec("150")
    );
}

#[test]
fn signing_permit_holds_coordination_until_crypto_caller_drops_it() {
    let (dir, ledger, journal) = fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    let clearance = order(1, "1");
    begin(&ledger, &clearance);
    let other = Ledger::open(dir.path(), Network::Testnet).unwrap();
    let permit = before_sign(&ledger, &clearance).unwrap();
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        attempt_tx.send(()).unwrap();
        record(&other, 1, &payload(1, 1, "1", "0", "5", BASE + 10));
        done_tx.send(()).unwrap();
    });
    attempt_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let blocked = done_rx.recv_timeout(Duration::from_millis(100));
    drop(permit);
    worker.join().unwrap();
    assert!(matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)));
    assert!(matches!(
        before_sign(&ledger, &clearance),
        Err(PilotError::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
}

#[derive(Debug)]
struct FailedPublication {
    witnessed: Arc<std::sync::Mutex<Option<Anchor>>>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl crate::ledger::HeadAnchor for FailedPublication {
    fn load(&self) -> crate::ledger::Result<Option<Anchor>> {
        Ok(self.witnessed.lock().unwrap().clone())
    }
    fn store(&self, anchor: &Anchor) -> crate::ledger::Result<()> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(LedgerError::Io(std::io::Error::other(
                "simulated crash before anchor publication",
            )));
        }
        *self.witnessed.lock().unwrap() = Some(anchor.clone());
        Ok(())
    }
}

#[test]
fn signed_consent_and_adoption_retry_publish_the_verified_head_without_reset() {
    use std::sync::atomic::Ordering;
    for adopting in [false, true] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("publication.db");
        let witnessed = Arc::new(std::sync::Mutex::new(None));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let anchor = || {
            Box::new(FailedPublication {
                witnessed: witnessed.clone(),
                fail: fail.clone(),
            })
        };
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let registry = Arc::new(
            super::super::RegistryJournal::open(
                ledger.clone(),
                Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
            )
            .unwrap(),
        );
        registry
            .grant(audit_route(agent(), account(), BASE - 1).binding, BASE - 1)
            .unwrap();
        let journal = PilotJournal::new(registry.clone());
        let review = if adopting {
            LegacyPilotJournal::new(ledger.clone())
                .authorize(agent(), account(), BASE)
                .unwrap();
            begin(&ledger, &order(1, "1"));
            Some(journal.review_legacy(account()).unwrap())
        } else {
            None
        };
        let mutate = || match &review {
            Some(review) => journal.adopt_legacy(review, BASE + 10),
            None => journal.authorize(agent(), account(), BASE),
        };
        let before = ledger.chain_head().unwrap();
        fail.store(true, Ordering::SeqCst);
        assert!(mutate().is_err());
        let committed = ledger.chain_head().unwrap();
        assert_eq!(committed.seq, before.seq + 1);
        assert_eq!(*witnessed.lock().unwrap(), Some(before.clone()));
        let durable = journal.state(account()).unwrap().unwrap();
        assert_eq!(
            durable.reserved_usd,
            if adopting { dec("10") } else { Decimal::ZERO }
        );
        assert!(
            mutate().is_err(),
            "continued anchor failure cannot return success"
        );
        assert_eq!(ledger.chain_head().unwrap(), committed);
        fail.store(false, Ordering::SeqCst);
        assert_eq!(mutate().unwrap(), durable);
        assert_eq!(*witnessed.lock().unwrap(), Some(committed.clone()));
        assert_eq!(ledger.chain_head().unwrap(), committed);
        drop(journal);
        drop(registry);
        drop(ledger);
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let registry = Arc::new(
            super::super::RegistryJournal::open(
                ledger.clone(),
                Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
            )
            .unwrap(),
        );
        let journal = PilotJournal::new(registry);
        assert_eq!(journal.state(account()).unwrap().unwrap(), durable);
        {
            let mut guard = ledger.lock().unwrap();
            let tx = guard.transaction().unwrap();
            tx.execute("DELETE FROM events WHERE seq > ?1", params![before.seq])
                .unwrap();
            tx.execute(
                "UPDATE chain_head SET seq = ?1, hash = ?2 WHERE id = 0",
                params![before.seq, before.hash],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        assert!(
            journal.state(account()).is_err(),
            "successful publication must expose rollback"
        );
    }
}

#[test]
fn signed_pilot_fill_projection_needs_no_key_owner_and_still_latches_stop() {
    let (_dir, ledger, registry, journal) = authenticated_fixture();
    journal.authorize(agent(), account(), BASE).unwrap();
    begin(&ledger, &order(1, "1"));
    drop(journal);
    drop(registry);
    record(&ledger, 1, &payload(1, 1, "1", "0", "5", BASE + 10));
    let status = status(&ledger, account()).unwrap().unwrap();
    assert_eq!(status.authentication, PilotAuthentication::Unverified);
    assert!(matches!(
        status.halt,
        Some(PilotStop::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
    assert!(
        matches!(status.accounting, PilotAccounting::Known { executed_usd, reserved_usd, .. } if executed_usd == dec("10") && reserved_usd == Decimal::ZERO)
    );
    assert!(ledger.verify().unwrap().is_intact());
}

#[test]
fn atomic_fill_and_halt_survive_reopen_with_precommit_anchor() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("crash.db");
    let witnessed = Arc::new(std::sync::Mutex::new(None));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let anchor = || {
        Box::new(FailedPublication {
            witnessed: witnessed.clone(),
            fail: fail.clone(),
        })
    };
    let ledger = Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
    let journal = LegacyPilotJournal::new(ledger.clone());
    journal.authorize(agent(), account(), BASE).unwrap();
    let clearance = order(1, "1");
    begin(&ledger, &clearance);
    let before = ledger.chain_head().unwrap();
    fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let value = payload(1, 1, "1", "0", "5", BASE + 10);
    assert!(
        ledger
            .record_fill(&NewFill {
                account: &account().to_string(),
                tid: 1,
                ts_ms: (BASE + 10) as i64,
                agent_id: None,
                payload: &value
            })
            .is_err()
    );
    assert_eq!(ledger.chain_head().unwrap().seq, before.seq + 1);
    assert_eq!(*witnessed.lock().unwrap(), Some(before.clone()));
    let events = ledger.get_events(before.seq, 10).unwrap().events;
    assert_eq!(events[0].kind, EventKind::Fill);
    assert_eq!(events.len(), 1);
    let halt = &events[0].payload.as_ref().unwrap()["pilot_stop"];
    assert_eq!(halt["trigger_seq"], json!(events[0].seq));
    assert_eq!(halt["trigger_hash"], Value::Null);
    assert_eq!(halt["account"], json!(account()));
    drop(journal);
    drop(ledger);
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    let ledger = Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
    assert!(
        ledger.verify().unwrap().is_intact(),
        "embedded stop has the ordinary single-row crash window"
    );
    let state = LegacyPilotJournal::new(ledger.clone())
        .state(account())
        .unwrap()
        .unwrap();
    assert_eq!(state.executed_usd, dec("10"));
    assert!(matches!(
        state.halt,
        Some(PilotStop::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
    assert!(matches!(
        before_sign(&ledger, &clearance),
        Err(PilotError::Exhausted {
            metric: PilotMetric::RealizedLoss,
            ..
        })
    ));
}

#[test]
fn unconfigured_signing_permit_also_serializes_authorization() {
    let (dir, ledger, _journal) = fixture();
    let other = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let permit = before_sign(&ledger, &order(1, "1")).unwrap();
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        attempt_tx.send(()).unwrap();
        let result = LegacyPilotJournal::new(other).authorize(agent(), account(), BASE);
        done_tx.send(result).unwrap();
    });
    attempt_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let blocked = done_rx.recv_timeout(Duration::from_millis(100));
    drop(permit);
    worker.join().unwrap();
    assert!(matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)));
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(before_sign(&ledger, &order(1, "1")).is_err());
}
