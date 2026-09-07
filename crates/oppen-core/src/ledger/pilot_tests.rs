use std::sync::{Barrier, mpsc};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::guardrail::{AuditEntry, AuditOutcome, AuditSink, Utilization};
use crate::ledger::{EventViews, LedgerAuditSink, SubmissionReceipt};

const BASE: u64 = 1_800_000_000_000;

fn account() -> Address {
    Address::from_bytes([1; 20])
}
fn agent() -> AgentId {
    AgentId::new("synthetic-pilot")
}
fn dec(value: &str) -> Decimal {
    Decimal::from_str_exact(value).unwrap()
}
fn fixture() -> (TempDir, Arc<Ledger>, PilotJournal) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = PilotJournal::new(ledger.clone());
    (dir, ledger, journal)
}
fn order(id: u8, size: &str) -> Clearance {
    Clearance {
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
    LedgerAuditSink::new(ledger.clone())
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
        PilotJournal::new(mainnet)
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
        let journal = PilotJournal::new(ledger.clone());
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
    let journal = PilotJournal::new(ledger.clone());
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
    let state = PilotJournal::new(ledger.clone())
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
        let result = PilotJournal::new(other).authorize(agent(), account(), BASE);
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
