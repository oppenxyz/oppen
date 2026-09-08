//! Ledger tests.
//!
//! `AGENTS.md`: a ledger change needs the disconnect-reconcile test. The rest
//! of this file is the property set the module exists to hold — no gap and no
//! reorder in seq, tampering detected at the right row, a tombstoned payload
//! that still verifies, two networks with independent cursors, and an
//! interrupted append that leaves no half-row.
//!
//! The last section is adversarial: every test in it is an attack that this
//! module verified clean before it was written. A test that only exercises the
//! honest path is not evidence that the dishonest one is caught, which is how
//! the forged-redaction hole survived a suite that looked like it covered
//! redaction.
//!
//! The randomised tests use a fixed-seed splitmix64 rather than a proptest
//! dependency the crate does not have. Seeds are constants, so a failure is
//! reproducible by running the same test again.

use std::str::FromStr;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

// Audit-only clearances carry a complete route without granting signing
// authority or inserting extra rows into the historical chain under test.
pub(super) fn audit_route(
    agent: crate::guardrail::AgentId,
    container: oppen_hl::Address,
    at_ms: u64,
) -> AuthorizedRoute {
    use crate::keys::{KeyStore, MemoryKeyStore, SecretText};
    let keys = MemoryKeyStore::new(Network::Testnet);
    let wallet = keys
        .create_agent_key(
            &agent,
            SecretText::new(format!("{:064x}", 1)),
            at_ms + 86_400_000,
            at_ms,
        )
        .expect("synthetic wallet");
    AuthorizedRoute {
        network: Network::Testnet,
        binding_seq: 1,
        binding: RegistryBinding {
            agent,
            container,
            vault_address: None,
            wallet,
        },
    }
}

pub(super) fn audit_sink(ledger: Arc<Ledger>) -> LedgerAuditSink {
    LedgerAuditSink::new(Arc::new(PolicyJournal::new(Arc::new(
        RegistryJournal::open(ledger, Arc::new(crate::keys::HmacKey::from_bytes([31; 32])))
            .expect("registry replay"),
    ))))
}

// Audit serialization only; never accepted as persisted signing authority.
pub(super) const AUDIT_POLICY_REVISION: u64 = 41;

pub(super) fn initialize_policy(
    registry: Arc<RegistryJournal>,
    legacy_path: &std::path::Path,
    agent: crate::guardrail::AgentId,
    at_ms: u64,
) -> Arc<PolicyJournal> {
    use crate::guardrail::{AgentGuardrails, LegacyPolicyReview, PersistedState};
    let policy = Arc::new(PolicyJournal::new(registry));
    let review = LegacyPolicyReview::open(legacy_path, Network::Testnet, at_ms).unwrap();
    let mut state = PersistedState::paused(at_ms);
    state.guardrails.insert(agent, AgentGuardrails::default());
    policy.initialize(&review, state, at_ms).unwrap();
    policy
}

/// D6 / item 29: production audit writes survive reopening as one intact chain.
#[test]
fn production_audit_sink_persists_orders_decisions_and_typed_refusals() {
    use crate::guardrail::{
        AgentId, AuditEntry, AuditOutcome, AuditSink, ClearedKind, GuardrailEngine, Refusal,
    };
    use crate::keys::{KeyStore, MemoryKeyStore, SecretText};
    use rust_decimal::Decimal;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("audit.db");
    let agent = AgentId::new("agent-a");
    let reason = "<b>agent claim</b>\nkeep verbatim";
    let refusal = Refusal::OrderNotional {
        symbol: "BTC".into(),
        observed_usd: Decimal::from(100),
        limit_usd: Decimal::from(25),
    };
    let registry_key = Arc::new(crate::keys::HmacKey::from_bytes([31; 32]));
    let (head, route) = {
        let ledger = Arc::new(Ledger::open_at(&path, Network::Testnet).expect("open"));
        let keys = Arc::new(MemoryKeyStore::new(Network::Testnet));
        let wallet = keys
            .create_agent_key(
                &agent,
                SecretText::new(format!("{:064x}", 1)),
                86_400_999,
                999,
            )
            .expect("synthetic wallet");
        let registry =
            RegistryJournal::open(ledger.clone(), registry_key.clone()).expect("registry");
        let route = registry
            .grant(
                RegistryBinding {
                    agent: agent.clone(),
                    container: oppen_hl::Address::from_bytes([1; 20]),
                    vault_address: None,
                    wallet,
                },
                999,
            )
            .expect("explicit grant");
        let policy = initialize_policy(
            Arc::new(registry),
            &dir.path().join("guardrails.db"),
            agent.clone(),
            1_000,
        );
        let sink = Arc::new(LedgerAuditSink::new(policy.clone()));
        let engine = GuardrailEngine::new(policy.clone(), keys).expect("engine");
        engine.register_agent(&agent, 1_000).expect("register");
        let cancel = engine
            .clear_cancel(
                &agent,
                vec![oppen_hl::wire::CancelWire { a: 0, o: 42 }],
                reason,
                1_001,
            )
            .expect("real sink must allow cancel clearance");
        engine
            .clear_schedule_cancel(&agent, None, 1_002)
            .expect("real sink must allow dead-man clearance");

        // Exercise the adapter's order branch without duplicating the engine's
        // market/exposure fixtures. Clearance is audit data, not signing proof.
        let mut order = cancel.clearance().clone();
        order.policy_revision = policy.current().unwrap().revision;
        order.evaluated_at_ms = 1_003;
        order.kind = ClearedKind::Order {
            symbol: "BTC".into(),
            is_buy: true,
            px: Decimal::from(100),
            sz: Decimal::ONE,
            notional_usd: Decimal::from(100),
            reduce_only: false,
            slippage_bps: Decimal::ZERO,
            reference_px: Decimal::from(100),
            slippage_reference_px: Decimal::from(100),
            cloid: None,
            snapshot_id: None,
            snapshot_hash: None,
        };
        sink.record(&AuditEntry {
            agent: Some(&agent),
            at_ms: 1_003,
            reason,
            outcome: AuditOutcome::Cleared(&order),
        })
        .expect("order must use record_intent rather than append");
        sink.record(&AuditEntry {
            agent: Some(&agent),
            at_ms: 1_004,
            reason,
            outcome: AuditOutcome::Refused(&refusal),
        })
        .expect("refusal");
        assert!(ledger.verify().expect("verify").is_intact());
        (ledger.chain_head().expect("head"), route)
    };

    let ledger = Arc::new(Ledger::open_at(&path, Network::Testnet).expect("reopen"));
    let registry = RegistryJournal::open(ledger.clone(), registry_key).expect("replay registry");
    assert_eq!(
        registry.route_for_agent(&agent).expect("persisted route"),
        route
    );
    assert_eq!(ledger.chain_head().expect("head"), head);
    assert!(ledger.verify().expect("verify persisted chain").is_intact());
    let events = ledger.get_events(0, 10).expect("events").events;
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![
            EventKind::RegistryGranted,
            EventKind::PolicyInitialized,
            EventKind::AgentDecision,
            EventKind::AgentDecision,
            EventKind::OrderIntent,
            EventKind::Refusal,
        ]
    );
    assert_eq!(events[0].seq, route.binding_seq);
    assert_eq!(events[0].ts_ms, 999);
    assert_eq!(events[1].ts_ms, 1_000);
    assert_eq!(events[1].agent_id, None);
    for (index, event) in events.iter().skip(2).enumerate() {
        assert_eq!(event.agent_id.as_deref(), Some(agent.as_str()));
        assert_eq!(event.ts_ms, 1_001 + index as i64);
    }
    let cancel = events[2].payload.as_ref().expect("cancel payload");
    assert_eq!(cancel["kind"]["cleared"], "cancel");
    assert_eq!(cancel["kind"]["count"], 1);
    assert_eq!(cancel["reason"], reason);
    let deadman = events[3].payload.as_ref().expect("dead-man payload");
    assert_eq!(deadman["kind"]["cleared"], "schedule_cancel");
    assert_eq!(deadman["reason"], "dead-man's switch");
    let order = events[4].payload.as_ref().expect("order payload");
    assert_eq!(order["kind"]["cleared"], "order");
    assert_eq!(order["kind"]["notional_usd"], "100");
    assert_eq!(order["reason"], reason);
    let rejected = events[5].payload.as_ref().expect("refusal payload");
    assert_eq!(rejected["reason"], reason);
    assert_eq!(rejected["refusal"], refusal.to_string());
    assert_eq!(
        rejected["refusal_detail"],
        serde_json::to_value(&refusal).expect("typed refusal")
    );
}

/// D6 / item 29: a policy-cleared order is durable before it can be signed.
#[test]
fn engine_evaluates_and_signs_an_order_through_the_production_audit_sink() {
    engine_order_signing(false);
}

#[test]
fn independent_policy_change_between_evaluation_and_signing_refuses_the_real_order() {
    engine_order_signing(true);
}

fn engine_order_signing(change_policy: bool) {
    use crate::guardrail::{
        AccountSnapshot, AgentGuardrails, AgentId, Exposure, FeedQuality, GuardrailEngine,
        KillScope, MarketRef, OrderIntent, RestingExposure,
    };
    use crate::keys::{KeyStore, MemoryKeyStore, SecretText};
    use oppen_hl::meta::Asset;
    use oppen_hl::order::OrderKind;
    use oppen_hl::types::AssetInfo;
    use oppen_hl::wire::{Cloid, Grouping, Tif};
    use rust_decimal::Decimal;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("signed-audit.db");
    let ledger = Arc::new(Ledger::open_at(&path, Network::Testnet).expect("ledger"));
    let agent = AgentId::new("test-agent");
    let now_ms = 1_788_998_400_000;
    let keys = Arc::new(MemoryKeyStore::new(Network::Testnet));
    // Public test scalar, held only in memory; never opens an OS keychain.
    let wallet = keys
        .create_agent_key(
            &agent,
            SecretText::new(format!("{:064x}", 1)),
            now_ms + 86_400_000,
            now_ms,
        )
        .expect("test wallet");
    let account = oppen_hl::Address::from_bytes([1; 20]);
    let registry_key = Arc::new(crate::keys::HmacKey::from_bytes([31; 32]));
    let registry = RegistryJournal::open(ledger.clone(), registry_key.clone()).expect("registry");
    let route = registry
        .grant(
            RegistryBinding {
                agent: agent.clone(),
                container: account,
                vault_address: None,
                wallet,
            },
            now_ms,
        )
        .expect("explicit grant");
    let policy = initialize_policy(
        Arc::new(registry),
        &dir.path().join("policy.db"),
        agent.clone(),
        now_ms,
    );
    let engine = GuardrailEngine::new(policy.clone(), keys.clone()).expect("engine");
    engine.register_agent(&agent, now_ms).expect("register");
    engine
        .operator_set_guardrails(
            &agent,
            AgentGuardrails {
                symbols: ["BTC".to_owned()].into(),
                max_order_usd: Decimal::from(100),
                max_position_usd: Decimal::from(100),
                approval_required: false,
                ..AgentGuardrails::default()
            },
            now_ms,
        )
        .expect("valid policy");
    engine
        .operator_release_kill(&KillScope::Global, now_ms)
        .unwrap();
    let observation = engine.policy_observation().unwrap();
    engine
        .operator_acknowledge_policy(observation, now_ms)
        .unwrap();
    let cloid = Cloid::parse("0x00000000000000000000000000000001").expect("cloid");
    let intent = OrderIntent {
        original: None,
        symbol: "BTC".into(),
        is_buy: true,
        px: Decimal::from(100),
        sz: Decimal::ONE,
        kind: OrderKind::Limit { tif: Tif::Gtc },
        reduce_only: false,
        cloid: Some(cloid.clone()),
        grouping: Grouping::Na,
        builder: None,
        max_slippage_bps: None,
        reason: "<b>test agent claim</b>".into(),
    };
    let asset = Asset {
        index: 7,
        info: AssetInfo {
            name: "BTC".into(),
            sz_decimals: 2,
            max_leverage: 40,
            margin_table_id: 0,
            is_delisted: false,
            only_isolated: false,
        },
    };
    let market = MarketRef {
        symbol: "BTC".into(),
        reference_px: Some(intent.px),
        as_of_ms: now_ms,
        quality: FeedQuality::Ok,
        mark_divergence_bps: None,
        mark_divergent_since_ms: None,
        snapshot: None,
        sigma_day: None,
        vol_ratio: None,
    };
    let exposure = Exposure {
        account,
        agent: AccountSnapshot {
            as_of_ms: now_ms,
            reconciled: true,
            equity_usd: Decimal::from(1_000),
            peak_equity_usd: Decimal::from(1_000),
            realized_pnl_today_usd: Decimal::ZERO,
            unrealized_pnl_usd: Decimal::ZERO,
            day_start_ms: now_ms,
            total_position_notional_usd: Decimal::ZERO,
            positions: Default::default(),
            resting: Some(RestingExposure::default()),
        },
        fleet: None,
    };
    if !change_policy {
        let supervised =
            GuardrailEngine::new_supervised_alpha(policy.clone(), keys.clone()).unwrap();
        supervised
            .operator_acknowledge_policy(supervised.policy_observation().unwrap(), now_ms)
            .unwrap();
        let before = ledger.chain_head().unwrap();
        assert!(
            engine
                .preflight(&agent, &intent, &asset, &market, &exposure, now_ms)
                .would_clear
        );
        let verdict = supervised.preflight(&agent, &intent, &asset, &market, &exposure, now_ms);
        assert!(!verdict.would_clear);
        assert!(matches!(
            verdict.refusal,
            Some(crate::guardrail::Refusal::Unevaluable(
                crate::guardrail::Unevaluable::PilotBudgetUnavailable { .. }
            ))
        ));
        assert_eq!(
            ledger.chain_head().unwrap(),
            before,
            "preflight cannot record or reserve"
        );
    }
    let cleared = engine
        .evaluate(&agent, &intent, &asset, &market, &exposure, now_ms)
        .expect("policy-cleared order must reach the real ledger");
    assert_eq!(
        cleared.clearance().policy_revision,
        policy.current().unwrap().revision
    );
    let events = ledger
        .get_events(0, 100)
        .expect("events before signing")
        .events;
    let intents: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::OrderIntent)
        .collect();
    assert_eq!(intents.len(), 1);
    let stored = intents[0];
    assert_eq!(stored.agent_id.as_deref(), Some(agent.as_str()));
    assert_eq!(stored.ts_ms, now_ms as i64);
    let mut expected = serde_json::to_value(cleared.clearance()).expect("clearance");
    expected["reason"] = json!(intent.reason);
    assert_eq!(stored.payload.as_ref(), Some(&expected));
    assert_eq!(expected["kind"]["cloid"], json!(cloid));
    assert!(ledger.verify().expect("verify before signing").is_intact());
    let head = ledger.chain_head().expect("head before signing");

    if change_policy {
        let independent = Arc::new(Ledger::open_at(&path, Network::Testnet).unwrap());
        let authority = PolicyJournal::new(Arc::new(
            RegistryJournal::open(independent, registry_key.clone()).unwrap(),
        ));
        let mut next = authority.current().unwrap();
        next.state.guardrails.get_mut(&agent).unwrap().max_order_usd = Decimal::from(50);
        let changed = authority
            .replace(next.revision, next.state, now_ms)
            .unwrap();
        assert_ne!(changed.revision, cleared.clearance().policy_revision);
        assert!(matches!(
            engine.sign_cleared(cleared, now_ms, None, || now_ms),
            Err(crate::guardrail::SignClearedError::Refused(
                crate::guardrail::Refusal::Unevaluable(
                    crate::guardrail::Unevaluable::PolicyChanged
                )
            ))
        ));
        assert_eq!(ledger.event(stored.seq).unwrap().as_ref(), Some(stored));
        let restarted = GuardrailEngine::new(policy.clone(), keys).unwrap();
        assert!(matches!(
            restarted.evaluate(&agent, &intent, &asset, &market, &exposure, now_ms),
            Err(crate::guardrail::Refusal::Unevaluable(
                crate::guardrail::Unevaluable::PolicyAuthority { .. }
            ))
        ));
        assert_eq!(policy.current().unwrap().revision, changed.revision);
        assert!(ledger.verify().unwrap().is_intact());
        return;
    }

    let (request, clearance) = engine
        .sign_cleared(cleared, now_ms, None, || now_ms)
        .expect("real engine signs with the in-memory test wallet");
    let oppen_hl::Action::Order { orders, .. } = request.action() else {
        panic!("expected a signed order");
    };
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].c, Some(cloid));
    assert_eq!(request.nonce(), now_ms);
    assert_eq!(clearance.network, Network::Testnet);
    assert_eq!(clearance.agent, agent);
    assert_eq!(clearance.vault_address, None);
    assert_eq!(clearance.route, route);
    assert_eq!(ledger.chain_head().expect("head after signing"), head);
    drop(engine);
    drop(ledger);

    let reopened = Arc::new(Ledger::open_at(&path, Network::Testnet).expect("reopen"));
    let registry = RegistryJournal::open(reopened.clone(), registry_key).expect("replay registry");
    assert_eq!(
        registry.route_for_agent(&agent).expect("persisted route"),
        route
    );
    assert_eq!(
        reopened.event(stored.seq).expect("read intent").as_ref(),
        Some(stored)
    );
    assert_eq!(reopened.chain_head().expect("persisted head"), head);
    assert!(
        reopened
            .verify()
            .expect("verify persisted chain")
            .is_intact()
    );
}

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
    EventKind::KillSwitchChanged,
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
fn existing_only_open_never_creates_a_missing_database_or_directory() {
    let dir = TempDir::new().unwrap();
    assert!(Ledger::open_existing(dir.path(), Network::Testnet).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let missing = dir.path().join("missing");
    assert!(Ledger::open_existing(&missing, Network::Testnet).is_err());
    assert!(!missing.exists());
}

#[test]
fn existing_only_open_never_adopts_missing_or_rewrites_malformed_anchors() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(crate::db_file_name(Network::Testnet));
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 41);
    drop(ledger);
    let database = std::fs::read(&path).unwrap();
    let anchor = FileAnchor::beside(&path);
    std::fs::remove_file(anchor.path()).unwrap();
    assert!(
        matches!(Ledger::open_existing(dir.path(), Network::Testnet), Err(LedgerError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
    );
    assert!(!anchor.path().exists());
    assert_eq!(std::fs::read(&path).unwrap(), database);
    std::fs::write(anchor.path(), b"{}").unwrap();
    assert!(Ledger::open_existing(dir.path(), Network::Testnet).is_err());
    assert_eq!(std::fs::read(anchor.path()).unwrap(), b"{}");
    assert_eq!(std::fs::read(&path).unwrap(), database);
}

#[test]
fn existing_only_open_preserves_history_and_enables_durable_writes() {
    let dir = TempDir::new().unwrap();
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 42);
    let before = ledger.get_events(0, 100).unwrap();
    let head = ledger.chain_head().unwrap();
    drop(ledger);
    let path = dir.path().join(crate::db_file_name(Network::Testnet));
    let anchor = FileAnchor::beside(&path);
    let anchor_bytes = std::fs::read(anchor.path()).unwrap();
    let reopened = Ledger::open_existing(dir.path(), Network::Testnet).unwrap();
    assert_eq!(reopened.chain_head().unwrap(), head);
    assert_eq!(std::fs::read(anchor.path()).unwrap(), anchor_bytes);
    for (before, after) in before
        .events
        .iter()
        .zip(reopened.get_events(0, 100).unwrap().events.iter())
    {
        assert_eq!(before.hash, after.hash);
        assert_eq!(before.payload, after.payload);
    }
    {
        let guard = reopened.lock().unwrap();
        assert_eq!(
            schema::supported_version(&guard).unwrap(),
            schema::CURRENT_VERSION
        );
        let synchronous: i64 = guard
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2);
    }
    fill(&reopened, 1, 43);
    assert_eq!(anchor.load().unwrap().unwrap().seq, head.seq + 1);
    assert!(reopened.verify().unwrap().is_intact());
}

#[test]
fn existing_only_open_does_not_publish_the_one_row_crash_window() {
    let dir = TempDir::new().unwrap();
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 1, 44);
    let witnessed = ledger.chain_head().unwrap();
    fill(&ledger, 1, 45);
    let committed = ledger.chain_head().unwrap();
    let anchor = FileAnchor::beside(&dir.path().join(crate::db_file_name(Network::Testnet)));
    anchor.store(&witnessed).unwrap();
    drop(ledger);
    let reopened = Ledger::open_existing(dir.path(), Network::Testnet).unwrap();
    assert_eq!(reopened.chain_head().unwrap(), committed);
    assert_eq!(anchor.load().unwrap(), Some(witnessed));
    assert!(reopened.verify().unwrap().is_intact());
}

#[test]
fn existing_only_open_refuses_old_schema_wrong_network_and_corrupt_history() {
    let dir = TempDir::new().unwrap();
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 1, 46);
    let head = ledger.chain_head().unwrap();
    {
        let guard = ledger.lock().unwrap();
        guard
            .pragma_update(None, "user_version", schema::CURRENT_VERSION - 1)
            .unwrap();
    }
    assert!(Ledger::open_existing(dir.path(), Network::Testnet).is_err());
    {
        let guard = ledger.lock().unwrap();
        assert_eq!(
            schema::supported_version(&guard).unwrap(),
            schema::CURRENT_VERSION - 1
        );
        guard
            .pragma_update(None, "user_version", schema::CURRENT_VERSION + 1)
            .unwrap();
    }
    assert!(matches!(
        Ledger::open_existing(dir.path(), Network::Testnet),
        Err(LedgerError::SchemaTooNew { .. })
    ));
    {
        let guard = ledger.lock().unwrap();
        let version: usize = guard
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, schema::CURRENT_VERSION + 1);
        guard
            .pragma_update(None, "user_version", schema::CURRENT_VERSION)
            .unwrap();
        guard
            .execute("UPDATE events SET ts_ms = ts_ms + 1 WHERE seq = 1", [])
            .unwrap();
    }
    assert!(Ledger::open_existing(dir.path(), Network::Testnet).is_err());
    assert_eq!(ledger.chain_head().unwrap(), head);

    let dir = TempDir::new().unwrap();
    let path = dir.path().join(crate::db_file_name(Network::Testnet));
    let ledger = Ledger::open_at(&path, Network::Mainnet).unwrap();
    let head = ledger.chain_head().unwrap();
    assert!(matches!(
        Ledger::open_existing(dir.path(), Network::Testnet),
        Err(LedgerError::NetworkMismatch { .. })
    ));
    assert_eq!(ledger.chain_head().unwrap(), head);
}

#[cfg(unix)]
#[test]
fn existing_only_open_uses_canonical_anchor_and_refuses_conflicting_alias_anchor() {
    let dir = TempDir::new().unwrap();
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 1, 47);
    let path = dir.path().join(crate::db_file_name(Network::Testnet));
    let alias_dir = dir.path().join("alias");
    std::fs::create_dir(&alias_dir).unwrap();
    let alias_path = alias_dir.join(crate::db_file_name(Network::Testnet));
    std::os::unix::fs::symlink(&path, &alias_path).unwrap();
    let alias = Ledger::open_existing(&alias_dir, Network::Testnet).unwrap();
    assert_eq!(alias.coordination_path, ledger.coordination_path);
    assert_eq!(alias.chain_head().unwrap(), ledger.chain_head().unwrap());
    let alias_anchor = FileAnchor::beside(&alias_path);
    assert!(!alias_anchor.path().exists());
    alias_anchor.store(&ledger.chain_head().unwrap()).unwrap();
    assert!(Ledger::open_existing(&alias_dir, Network::Testnet).is_err());
    assert!(Ledger::open_at(&alias_path, Network::Testnet).is_err());
    assert!(ledger.verify().unwrap().is_intact());
}

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
            kind: EventKind::OrderStateChange,
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
            kind: EventKind::OrderStateChange,
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
            EventKind::OrderStateChange,
            1_756_500_000_500,
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
    assert_eq!(
        ledger
            .snapshot_body("snap-1", &snapshot_hash)
            .expect("body"),
        Some(book)
    );

    // Pruning the body leaves the chained reference, and the chain still holds.
    assert_eq!(
        ledger
            .prune_snapshots_before(1_756_800_000_000)
            .expect("prune"),
        1
    );
    assert_eq!(
        ledger
            .snapshot_body("snap-1", &snapshot_hash)
            .expect("body"),
        None
    );
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
        EventKind::RegistryGranted,
        EventKind::RegistryRetired,
        EventKind::PolicyInitialized,
        EventKind::PolicyReplaced,
        EventKind::ApprovalProposed,
        EventKind::ApprovalClaimed,
        EventKind::ApprovalDisposed,
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

// --- adversarial: redaction must be evidenced by the chain -------------------
//
// Every test from here down is an attack that verified clean before the fix.
// `an_unrecorded_tombstone_is_a_break` above only covered a tamperer who forgot
// to also set `redacted_at`; these cover the one who did not forget.

#[test]
fn a_forged_redacted_at_does_not_excuse_a_null_payload() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 10, 19);
    assert!(ledger.verify().expect("verify").is_intact());

    {
        let guard = ledger.connection.lock().expect("lock");
        // `redacted_at` and `redaction_reason` are not in the row-hash preimage,
        // so the same hand that nulls the payload writes them in the same
        // statement. Evidence an attacker can author is not evidence.
        guard
            .execute(
                "UPDATE events SET payload = NULL, redacted_at = 1, \
                 redaction_reason = 'legal request' WHERE seq = 7",
                [],
            )
            .expect("forge a redaction");
    }

    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 7);
    assert_eq!(broken.reason, BreakReason::UnrecordedTombstone);
    assert_eq!(report.rows_checked, 6);

    // No chained redaction exists to explain it, which is the whole point.
    assert!(
        !ledger
            .get_events(0, 100)
            .expect("page")
            .events
            .iter()
            .any(|event| event.kind == EventKind::PayloadRedacted)
    );
}

#[test]
fn a_redaction_row_cannot_be_appended_around_redact() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 26);

    // The chained redaction is what verification accepts as an explanation, so
    // it must never be writable without the null it explains.
    let claim = json!({ "redacted_seq": 2, "reason": "legal request" });
    assert!(matches!(
        ledger.append(&NewEvent {
            kind: EventKind::PayloadRedacted,
            ts_ms: 1,
            agent_id: None,
            payload: &claim,
            snapshot: None,
        }),
        Err(LedgerError::UseRedact)
    ));
    assert_eq!(ledger.get_events(0, 10).expect("page").events.len(), 3);
}

#[test]
fn a_redaction_cannot_itself_be_redacted() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 27);
    let tombstone = ledger.redact(2, "pii in reason", 10).expect("redact");

    assert!(matches!(
        ledger.redact(tombstone.seq, "housekeeping", 11),
        Err(LedgerError::RedactionIsNotRedactable(_))
    ));
    let row = ledger
        .event(tombstone.seq)
        .expect("event")
        .expect("present");
    assert_eq!(row.kind, EventKind::PayloadRedacted);
    assert_eq!(
        row.payload,
        Some(json!({ "redacted_seq": 2, "reason": "pii in reason" })),
        "which seq it explained, and why, survives"
    );
    assert!(ledger.verify().expect("verify").is_intact());
}

#[test]
fn erasing_a_tombstones_explanation_breaks_the_chain() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 3, 28);
    let tombstone = ledger.redact(2, "pii in reason", 10).expect("redact");
    assert!(ledger.verify().expect("verify").is_intact());

    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute(
                "UPDATE events SET payload = NULL, redacted_at = 1 WHERE seq = ?1",
                params![tombstone.seq as i64],
            )
            .expect("erase the explanation");
    }

    // Both rows are now unexplained: the one that was legitimately redacted,
    // because the record of why is gone, and the tombstone itself.
    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 2);
    assert_eq!(broken.reason, BreakReason::UnrecordedTombstone);
}

// --- adversarial: the end of the chain -------------------------------------

#[test]
fn a_truncated_tail_is_caught_by_the_anchored_head() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 20, 88);
    assert!(ledger.verify().expect("verify").is_intact());

    {
        let guard = ledger.connection.lock().expect("lock");
        let keep: String = guard
            .query_row("SELECT hash FROM events WHERE seq = 12", [], |row| {
                row.get(0)
            })
            .expect("hash of the row to keep");
        guard
            .execute("DELETE FROM events WHERE seq > 12", [])
            .expect("erase the tail");
        guard
            .execute(
                "UPDATE chain_head SET seq = 12, hash = ?1 WHERE id = 0",
                params![keep],
            )
            .expect("rewind the head to match");
    }

    // Every surviving row still verifies and the head agrees with the last of
    // them. Nothing inside the file is wrong, which is why the reference has to
    // come from outside it.
    let report = ledger.verify().expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 20);
    assert!(matches!(
        broken.reason,
        BreakReason::HeadBehindAnchor {
            anchor_seq: 20,
            found_seq: 12,
            ..
        }
    ));

    // And the honest statement of the boundary: without an anchor this is
    // undetectable, which is exactly why one is installed by default.
    let unanchored = Ledger::open_anchored(&dir.path().join("testnet.db"), Network::Testnet, None)
        .expect("open unanchored");
    assert!(
        unanchored.verify().expect("verify").is_intact(),
        "the file alone cannot tell that eight rows are missing from the end"
    );
}

#[test]
fn a_rewritten_chain_is_caught_against_a_head_kept_elsewhere() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    fill(&ledger, 20, 91);
    // What an operator, a backup job or a later keychain anchor wrote down.
    let witnessed = ledger.chain_head().expect("head");
    assert_eq!(witnessed.seq, 20);

    {
        let guard = ledger.connection.lock().expect("lock");
        let keep: String = guard
            .query_row("SELECT hash FROM events WHERE seq = 12", [], |row| {
                row.get(0)
            })
            .expect("hash");
        guard
            .execute("DELETE FROM events WHERE seq > 12", [])
            .expect("erase the tail");
        guard
            .execute(
                "UPDATE chain_head SET seq = 12, hash = ?1 WHERE id = 0",
                params![keep],
            )
            .expect("rewind the head");
    }
    // Rebuild the tail through the public API: every hash is valid, the head is
    // correct, and the ledger's own anchor moved along with the forgery.
    fill(&ledger, 8, 92);
    let report = ledger.verify().expect("verify");
    assert!(
        report.is_intact(),
        "the file is internally consistent again"
    );
    assert_eq!(report.head_seq, 20);

    let report = ledger.verify_against(&witnessed).expect("verify");
    let broken = report.first_break.expect("a break");
    assert_eq!(broken.seq, 20);
    assert!(matches!(
        broken.reason,
        BreakReason::HeadBehindAnchor { found_seq: 20, .. }
    ));
}

#[test]
fn the_anchor_is_adopted_on_first_open_and_moves_with_every_append() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("testnet.db");
    {
        let ledger = Ledger::open_anchored(&path, Network::Testnet, None).expect("open unanchored");
        fill(&ledger, 5, 71);
    }
    let sidecar = FileAnchor::beside(&path);
    assert!(sidecar.load().expect("load").is_none());

    // Adoption trusts the file once, at the first anchored open. Every rewind
    // after that is caught.
    let ledger = Ledger::open_at(&path, Network::Testnet).expect("open anchored");
    assert_eq!(
        sidecar.load().expect("load"),
        Some(ledger.chain_head().expect("head"))
    );

    fill(&ledger, 2, 72);
    let anchor = sidecar.load().expect("load").expect("anchor");
    assert_eq!(anchor.seq, 7);
    assert_eq!(anchor, ledger.chain_head().expect("head"));
    assert!(ledger.verify().expect("verify").is_intact());
}

// --- adversarial: the receipt is the type handed to the signer ---------------

#[test]
fn a_receipt_from_another_chain_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let testnet = open(&dir, Network::Testnet);
    let mainnet = open(&dir, Network::Mainnet);

    let body = json!({ "coin": "BTC", "sz": "0.001", "reason": "testnet probe" });
    let receipt = testnet
        .record_intent(&NewIntent {
            agent_id: "agent-a",
            ts_ms: 1,
            payload: &body,
            snapshot: None,
        })
        .expect("testnet intent");
    assert_eq!(receipt.chain(), testnet.genesis);

    // docs/decisions.md R4: a mainnet number that is actually a testnet number.
    let error = mainnet
        .record_outcome(
            &receipt,
            EventKind::OrderStateChange,
            2,
            &json!({ "oid": 1, "avg_px": "63000" }),
        )
        .expect_err("the mainnet ledger must refuse a testnet receipt");
    assert!(matches!(error, LedgerError::ReceiptFromAnotherChain { .. }));
    assert_eq!(mainnet.get_events(0, 10).expect("page").events.len(), 0);
}

#[test]
fn a_receipt_whose_row_no_longer_matches_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let body = json!({ "coin": "BTC", "sz": "0.001", "reason": "carry" });
    let receipt = ledger
        .record_intent(&NewIntent {
            agent_id: "agent-a",
            ts_ms: 1,
            payload: &body,
            snapshot: None,
        })
        .expect("intent");

    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute(
                "UPDATE events SET hash = 'forged' WHERE seq = ?1",
                params![receipt.seq() as i64],
            )
            .expect("tamper");
    }
    assert!(matches!(
        ledger.record_outcome(
            &receipt,
            EventKind::OrderStateChange,
            2,
            &json!({ "oid": 1 })
        ),
        Err(LedgerError::ReceiptRowMismatch { seq: 1, .. })
    ));

    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute(
                "DELETE FROM events WHERE seq = ?1",
                params![receipt.seq() as i64],
            )
            .expect("delete");
    }
    assert!(matches!(
        ledger.record_outcome(
            &receipt,
            EventKind::OrderStateChange,
            3,
            &json!({ "oid": 1 })
        ),
        Err(LedgerError::NoSuchEvent(1))
    ));
}

#[test]
fn an_outcome_is_attributed_to_the_agent_that_asked() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let body = json!({ "coin": "BTC", "sz": "0.001", "reason": "carry" });
    let receipt = ledger
        .record_intent(&NewIntent {
            agent_id: "agent-a",
            ts_ms: 1,
            payload: &body,
            snapshot: None,
        })
        .expect("intent");
    assert_eq!(receipt.agent_id(), "agent-a");

    // The attribution comes from the receipt, so there is no parameter through
    // which a fill could be booked against an agent that never asked.
    let appended = ledger
        .record_outcome(
            &receipt,
            EventKind::OrderStateChange,
            2,
            &json!({ "oid": 7 }),
        )
        .expect("outcome");
    let event = ledger.event(appended.seq).expect("event").expect("present");
    assert_eq!(event.agent_id.as_deref(), Some("agent-a"));
    assert!(ledger.verify().expect("verify").is_intact());
}

// --- adversarial: the decision-time book ------------------------------------

#[test]
fn a_snapshot_body_cannot_be_swapped_under_a_chained_reference() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let real = json!({ "bids": [["63000.0", "1.5"]], "asks": [["63001.0", "2.0"]] });
    let chained = ledger
        .put_snapshot("snap-1", 10, "BTC", &real)
        .expect("put snapshot");

    let body = json!({ "coin": "BTC", "reason": "thin book" });
    let receipt = ledger
        .record_intent(&NewIntent {
            agent_id: "agent-a",
            ts_ms: 11,
            payload: &body,
            snapshot: Some(SnapshotRef {
                id: "snap-1",
                hash: &chained,
            }),
        })
        .expect("intent");
    let row = ledger
        .event(receipt.seq())
        .expect("event")
        .expect("present");
    assert_eq!(row.snapshot_hash.as_deref(), Some(chained.as_str()));

    // Re-capturing the identical book stays idempotent.
    assert_eq!(
        ledger
            .put_snapshot("snap-1", 10, "BTC", &real)
            .expect("recapture"),
        chained
    );

    // A different book under the same id is refused, not silently substituted.
    let forged = json!({ "bids": [["1.0", "9999"]], "asks": [["2.0", "9999"]] });
    assert!(matches!(
        ledger.put_snapshot("snap-1", 10, "BTC", &forged),
        Err(LedgerError::SnapshotBodyConflict { .. })
    ));
    assert_eq!(
        ledger.snapshot_body("snap-1", &chained).expect("body"),
        Some(real)
    );
}

#[test]
fn a_replaced_snapshot_body_is_not_served_as_the_decision_time_book() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let real = json!({ "bids": [["63000.0", "1.5"]] });
    let chained = ledger
        .put_snapshot("snap-1", 10, "BTC", &real)
        .expect("put snapshot");

    // `book_snapshots` is unchained by design (R5), so anyone editing the file
    // can rewrite the body and the `snapshot_hash` column beside it. The read
    // path rehashes the body and checks it against the hash from the chained
    // row, which is the only copy an attacker would have to forge a chain to
    // change.
    {
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute(
                "UPDATE book_snapshots SET body = '{\"bids\":[[\"1.0\",\"9999\"]]}', \
                 snapshot_hash = 'forged' WHERE snapshot_id = 'snap-1'",
                [],
            )
            .expect("swap the book");
    }
    assert!(matches!(
        ledger.snapshot_body("snap-1", &chained),
        Err(LedgerError::SnapshotBodyConflict { .. })
    ));
}

// --- adversarial: a flapping feed -------------------------------------------

#[test]
fn a_flapping_feed_reuses_the_open_gap_for_its_scope() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);

    let first = ledger
        .open_gap("ws:user:0xabc", 100, None)
        .expect("open gap");
    let again = ledger
        .open_gap("ws:user:0xabc", 200, Some("close 1006"))
        .expect("open gap again");
    assert_eq!(again.gap_id, first.gap_id, "a second gap orphans the first");
    assert_eq!(
        again.opened_ts_ms, 100,
        "the window still starts when the feed actually dropped"
    );
    assert_eq!(
        ledger.get_events(0, 100).expect("page").events.len(),
        1,
        "and no second disconnect event was chained"
    );

    // A different scope is still a different gap.
    let other = ledger.open_gap("ws:trades", 150, None).expect("open gap");
    assert_ne!(other.gap_id, first.gap_id);

    ledger.close_gap(first.gap_id, 300).expect("close");
    ledger
        .mark_gap_reconciled(first.gap_id, 400)
        .expect("reconcile");
    let left = ledger.unreconciled_gaps().expect("gaps");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].scope, "ws:trades");

    // Once closed, the same scope can open a fresh window.
    let third = ledger
        .open_gap("ws:user:0xabc", 500, None)
        .expect("open gap");
    assert_ne!(third.gap_id, first.gap_id);

    // And the duplicate is unrepresentable in the schema, not merely avoided
    // by the code path above.
    let guard = ledger.connection.lock().expect("lock");
    let duplicate = guard.execute(
        "INSERT INTO feed_gaps (scope, opened_ts_ms, open_seq) VALUES ('ws:trades', 900, 2)",
        [],
    );
    assert!(duplicate.is_err());
}

// --- adversarial: what may enter the record of record -----------------------

#[test]
fn a_float_in_a_payload_is_refused() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);

    let floaty = json!({ "px": 63000.1, "sz": "0.1" });
    let error = ledger
        .append(&NewEvent {
            kind: EventKind::OrderStateChange,
            ts_ms: 1,
            agent_id: Some("agent-a"),
            payload: &floaty,
            snapshot: None,
        })
        .expect_err("a binary float must not enter the table kept forever");
    assert!(matches!(&error, LedgerError::FloatInPayload { pointer } if pointer == "/px"));

    let nested = json!({ "fills": [{ "notional": 6300.010000000001 }] });
    let error = ledger
        .append(&NewEvent {
            kind: EventKind::OrderStateChange,
            ts_ms: 2,
            agent_id: Some("agent-a"),
            payload: &nested,
            snapshot: None,
        })
        .expect_err("must refuse");
    assert!(
        matches!(&error, LedgerError::FloatInPayload { pointer } if pointer == "/fills/0/notional")
    );

    // Snapshots go through the same encoder.
    assert!(matches!(
        ledger.put_snapshot("snap-1", 1, "BTC", &json!({ "bids": [[63000.0, 1.5]] })),
        Err(LedgerError::FloatInPayload { .. })
    ));

    // Decimal strings and integers are what belong here, and they still work.
    ledger
        .append(&NewEvent {
            kind: EventKind::OrderStateChange,
            ts_ms: 3,
            agent_id: Some("agent-a"),
            payload: &json!({ "px": "63000.1", "sz": "0.1", "oid": 42 }),
            snapshot: None,
        })
        .expect("decimal strings are the supported form");
    assert_eq!(ledger.get_events(0, 10).expect("page").events.len(), 1);
}

#[test]
fn a_payload_that_nests_too_deeply_is_refused_rather_than_recursed_into() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);

    // A payload arrives over the MCP wire. Recursing over it without a limit is
    // a stack overflow, and a stack overflow is a panic on an input path.
    let mut deep = json!(1);
    for _ in 0..300 {
        deep = Value::Array(vec![deep]);
    }
    assert!(matches!(
        ledger.append(&NewEvent {
            kind: EventKind::OrderStateChange,
            ts_ms: 1,
            agent_id: None,
            payload: &deep,
            snapshot: None,
        }),
        Err(LedgerError::PayloadTooDeep { .. })
    ));
    assert_eq!(ledger.get_events(0, 10).expect("page").events.len(), 0);
}

#[test]
fn the_canonical_form_is_sorted_compact_and_integer_only() {
    // The preimage is written in hash.rs, not by serde_json, whose map order is
    // controlled by a Cargo feature any dependency in the workspace can switch
    // on. These bytes are what every payload_hash in every database is taken
    // over, so they are pinned here the way the chain vectors are.
    assert_eq!(
        hash::canonical_json(&json!({ "b": 1, "a": { "d": [1, 2], "c": "x" } }))
            .expect("canonical"),
        r#"{"a":{"c":"x","d":[1,2]},"b":1}"#
    );
    assert_eq!(
        hash::canonical_json(&json!({ "s": "he said \"hi\"\n\u{1}" })).expect("canonical"),
        "{\"s\":\"he said \\\"hi\\\"\\n\\u0001\"}"
    );
    assert_eq!(
        hash::canonical_json(&json!({ "n": u64::MAX, "z": null, "t": true })).expect("canonical"),
        r#"{"n":18446744073709551615,"t":true,"z":null}"#
    );
    assert!(matches!(
        hash::canonical_json(&json!({ "px": 1.0 })),
        Err(LedgerError::FloatInPayload { .. })
    ));
}

// --- the surface an agent may hold ------------------------------------------

/// Append one event owned by `agent`, or by nobody when `agent` is `None`.
fn append_for(ledger: &Ledger, agent: Option<&str>, n: u64) -> u64 {
    ledger
        .append(&NewEvent {
            // `append` refuses OrderIntent and Fill; those have their own
            // recording paths. AgentDecision is the agent-owned kind this can use.
            kind: EventKind::AgentDecision,
            ts_ms: 1_756_000_000_000 + n as i64,
            agent_id: agent,
            payload: &payload(n),
            snapshot: None,
        })
        .expect("append")
        .seq
}

#[test]
fn the_agent_view_reads_events_and_exposes_nothing_else() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = Arc::new(open(&dir, Network::Testnet));
    fill(&ledger, 4, 66);

    // AGENTS.md invariant 3. `oppen-mcp` is handed one of these, so redact,
    // upsert_sub_account, put_snapshot, prune and the gap surface are not
    // spellable from an agent tool — a compile error rather than a review note.
    let view = ledger.agent_view("agent-0");
    assert_eq!(view.agent_id(), "agent-0");
    // The operator surface still sees every row.
    assert_eq!(ledger.get_events(0, 10).expect("page").events.len(), 4);
}

/// `docs/decisions.md` C6. One agent's intents and reason strings are not
/// another agent's to read, and the ledger is shared per network (R4).
#[test]
fn an_agent_sees_its_own_events_and_the_ownerless_ones_but_not_another_agents() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = Arc::new(open(&dir, Network::Testnet));
    let mine = append_for(&ledger, Some("agent-a"), 1);
    let theirs = append_for(&ledger, Some("agent-b"), 2);
    let shared = append_for(&ledger, None, 3);

    let view = ledger.agent_view("agent-a");
    let seqs: Vec<u64> = view
        .get_events(0, 10)
        .expect("page")
        .events
        .iter()
        .map(|event| event.seq)
        .collect();
    assert_eq!(seqs, vec![mine, shared], "expected own + ownerless only");
    assert!(!seqs.contains(&theirs));
}

/// The kill switch, a feed dropping and an alert belong to no agent, and item
/// 18 promises every agent that taxonomy. Scoping must not drop it.
#[test]
fn account_wide_events_reach_every_agent() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = Arc::new(open(&dir, Network::Testnet));
    let shared = append_for(&ledger, None, 1);

    for agent in ["agent-a", "agent-b"] {
        let events = ledger.agent_view(agent).get_events(0, 10).expect("page");
        assert_eq!(events.events.len(), 1, "{agent} lost the ownerless event");
        assert_eq!(events.events[0].seq, shared);
    }
}

/// The cursor has to skip rows that were scanned and filtered out. Without it
/// an agent polling a ledger full of somebody else's events sits at the same
/// cursor forever and reports itself permanently behind the head.
#[test]
fn the_cursor_advances_past_another_agents_events() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = Arc::new(open(&dir, Network::Testnet));
    for n in 0..5 {
        append_for(&ledger, Some("agent-b"), n);
    }

    let view = ledger.agent_view("agent-a");
    let page = view.get_events(0, 10).expect("page");
    assert!(page.events.is_empty(), "none of those are agent-a's");
    assert!(!page.resync_required, "a filtered page is not a hole");
    assert_eq!(
        page.next_cursor, page.head_seq,
        "the cursor must reach the head, not stall at 0"
    );

    // And a later event for this agent is still delivered from that cursor.
    let mine = append_for(&ledger, Some("agent-a"), 9);
    let next = view.get_events(page.next_cursor, 10).expect("page");
    assert_eq!(
        next.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![mine]
    );
}

/// A full page stops at the last row returned, because nothing past it was
/// looked at. Advancing to the head there would skip unread events.
#[test]
fn a_full_page_does_not_advance_the_cursor_past_what_it_returned() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = Arc::new(open(&dir, Network::Testnet));
    let seqs: Vec<u64> = (0..5).map(|n| append_for(&ledger, Some("a"), n)).collect();

    let view = ledger.agent_view("a");
    let page = view.get_events(0, 2).expect("page");
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.next_cursor, seqs[1]);
    assert!(page.next_cursor < page.head_seq);

    let rest = view.get_events(page.next_cursor, 10).expect("page");
    assert_eq!(
        rest.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        seqs[2..].to_vec(),
        "no event may be skipped across the page boundary"
    );
}

/// Reading one event by seq is scoped the same way, and answers `None` rather
/// than an error — the two are indistinguishable to a caller who may not know
/// the row exists, and saying which would leak what the scope withholds.
#[test]
fn reading_one_event_by_seq_is_scoped_the_same_way() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = Arc::new(open(&dir, Network::Testnet));
    let mine = append_for(&ledger, Some("agent-a"), 1);
    let theirs = append_for(&ledger, Some("agent-b"), 2);
    let shared = append_for(&ledger, None, 3);

    let view = ledger.agent_view("agent-a");
    assert_eq!(view.event(mine).expect("read").expect("present").seq, mine);
    assert_eq!(
        view.event(shared).expect("read").expect("present").seq,
        shared
    );
    assert!(view.event(theirs).expect("read").is_none());
    // The operator surface is unscoped and still reads it.
    assert!(ledger.event(theirs).expect("read").is_some());
}

/// The forged-tail attack the confirmation pass found still open.
///
/// An attacker with write access to the database can null a payload and then
/// **append** a tombstone event it wrote itself, recomputing the row hash from
/// the open-source preimage and moving `chain_head` along. Comparing only at
/// the anchored seq accepted that, because the chain is still a valid prefix
/// of itself. The anchor now bounds the head from above as well.
///
/// The rows appended here are entirely legitimate — correct hashes, correct
/// head — so nothing but the ahead-of-anchor check can catch them. That is
/// deliberate: an earlier version of this test forged the head hash instead,
/// which tripped `HeadMismatch` and passed even with the new check disabled.
#[test]
fn a_chain_running_past_its_anchor_is_caught() {
    let dir = TempDir::new().expect("tempdir");
    let db = dir.path().join("testnet.db");
    let sidecar = FileAnchor::beside(&db);

    let ledger = Ledger::open_at(&db, Network::Testnet).expect("open anchored");
    fill(&ledger, 10, 91);
    let witnessed = ledger.chain_head().expect("head at 10");
    assert!(ledger.verify().expect("verify").is_intact());

    // Rows 11..=14, each perfectly valid. This is what a forged tail looks
    // like once the attacker has done its arithmetic correctly.
    fill(&ledger, 4, 92);
    assert!(
        ledger.verify().expect("verify").is_intact(),
        "the ledger's own anchor moved with the appends, so this alone is fine"
    );

    // The witness an operator, a backup or a keychain kept did not move.
    sidecar
        .store(&witnessed)
        .expect("restore the witnessed head");

    let report = ledger.verify().expect("verify");
    match report.first_break.as_ref().map(|b| &b.reason) {
        Some(BreakReason::ChainAheadOfAnchor {
            anchor_seq,
            head_seq,
            ahead,
        }) => {
            assert_eq!(*anchor_seq, 10);
            assert_eq!(*head_seq, 14);
            // Four unwitnessed rows, one of which a crash could explain.
            assert_eq!(*ahead, 3);
        }
        other => panic!("a chain past its anchor must be reported, got {other:?}"),
    }
}

/// A single unwitnessed row is the crash window, not an attack.
///
/// `note_head` writes the anchor after the row commits, so a crash in that
/// window legitimately leaves the chain one row ahead. Reporting that as
/// tampering would cry wolf on every unclean shutdown.
#[test]
fn one_row_ahead_of_the_anchor_is_the_crash_window() {
    let dir = TempDir::new().expect("tempdir");
    let db = dir.path().join("testnet.db");
    let sidecar = FileAnchor::beside(&db);

    let ledger = Ledger::open_at(&db, Network::Testnet).expect("open anchored");
    fill(&ledger, 10, 93);
    let witnessed = ledger.chain_head().expect("head at 10");
    fill(&ledger, 1, 94);
    sidecar.store(&witnessed).expect("anchor lags by one");

    assert!(
        ledger.verify().expect("verify").is_intact(),
        "one row of lag is a crash between commit and anchor write"
    );
}

/// Deleting the sidecar must not silently unanchor a live ledger.
///
/// `open_at` always installs a file anchor and populates it at open, so a
/// later read of `None` is positive proof the sidecar was removed. Folding
/// that into "this ledger has no anchor" was a fail-open on exactly the file
/// an attacker deletes first.
#[test]
fn deleting_the_sidecar_is_a_break_not_a_shrug() {
    let dir = TempDir::new().expect("tempdir");
    let db = dir.path().join("testnet.db");
    let ledger = Ledger::open_at(&db, Network::Testnet).expect("open anchored");
    fill(&ledger, 6, 92);
    assert!(ledger.verify().expect("verify").is_intact());

    let sidecar = FileAnchor::beside(&db);
    std::fs::remove_file(sidecar.path()).expect("remove the sidecar");

    let report = ledger.verify().expect("verify");
    assert_eq!(
        report.first_break.as_ref().map(|b| &b.reason),
        Some(&BreakReason::AnchorMissing),
        "a removed anchor must be reported, not treated as unanchored: {report:?}"
    );
}

// --- fills have one door, and the database is what closes it ----------------

/// A fill row is keyed on the venue's own identifiers, and the partial unique
/// index — not a caller's memory — is what refuses the second write.
#[test]
fn a_fill_is_keyed_by_the_database_and_written_once() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let account = "0x1111111111111111111111111111111111111111";
    let body = json!({ "account": account, "tid": 77, "px": "63000.1" });
    let fill = |tid: u64| NewFill {
        account,
        tid,
        ts_ms: 1_756_000_000_000,
        agent_id: Some("agent-a"),
        payload: &body,
    };

    assert!(ledger.record_fill(&fill(77)).expect("record").is_some());
    let head = ledger.chain_head().expect("head");
    assert!(
        ledger.record_fill(&fill(77)).expect("record").is_none(),
        "the same trade landed twice"
    );
    assert_eq!(ledger.chain_head().expect("head"), head);
    assert_eq!(ledger.get_events(0, 10).expect("page").events.len(), 1);

    // A different trade on the same container is not the same row.
    assert!(ledger.record_fill(&fill(78)).expect("record").is_some());
    assert_eq!(ledger.get_events(0, 10).expect("page").events.len(), 2);
    assert!(ledger.verify().expect("verify").is_intact());
}

/// The generic doors refuse a fill, so no path can write one unkeyed.
#[test]
fn the_generic_append_paths_refuse_a_fill() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let body = json!({ "tid": 5, "px": "1.0" });

    assert!(matches!(
        ledger.append(&NewEvent {
            kind: EventKind::Fill,
            ts_ms: 1,
            agent_id: None,
            payload: &body,
            snapshot: None,
        }),
        Err(LedgerError::UseRecordFill)
    ));

    let receipt = ledger
        .record_intent(&NewIntent {
            agent_id: "agent-a",
            ts_ms: 1,
            payload: &json!({ "coin": "BTC", "reason": "carry" }),
            snapshot: None,
        })
        .expect("intent");
    assert!(matches!(
        ledger.record_outcome(&receipt, EventKind::Fill, 2, &body),
        Err(LedgerError::UseRecordFill)
    ));
    assert_eq!(
        ledger.get_events(0, 10).expect("page").events.len(),
        1,
        "only the intent is in the chain"
    );
}

/// Upgrading a database written before `idem_key` existed keys the fills it
/// already holds.
///
/// Without the backfill every stored fill is unkeyed, so the first reconcile
/// after the upgrade records the whole recoverable window a second time into an
/// append-only chain. The column and index are dropped here to put the file
/// back in the shape an older build left it in, and `user_version` is rewound
/// so reopening re-runs the migration.
#[test]
fn upgrading_keys_the_fills_already_in_the_chain() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("testnet.db");
    let account = "0x1111111111111111111111111111111111111111";
    {
        let ledger = Ledger::open_at(&path, Network::Testnet).expect("open");
        for tid in [77u64, 78] {
            ledger
                .record_fill(&NewFill {
                    account,
                    tid,
                    ts_ms: 1_756_000_000_000,
                    agent_id: None,
                    payload: &json!({ "account": account, "tid": tid }),
                })
                .expect("record")
                .expect("written");
        }
        let guard = ledger.connection.lock().expect("lock");
        guard
            .execute_batch(
                "DROP INDEX events_submission_account; \
                 DROP INDEX events_idem_key; \
                 ALTER TABLE events DROP COLUMN idem_key; \
                 PRAGMA user_version = 1;",
            )
            .expect("rewind to the pre-idem_key schema");
    }

    let upgraded = Ledger::open_at(&path, Network::Testnet).expect("reopen and migrate");
    // The chain is untouched: idem_key is not in the row-hash preimage.
    assert!(upgraded.verify().expect("verify").is_intact());
    let keyed: i64 = upgraded
        .connection
        .lock()
        .expect("lock")
        .query_row(
            "SELECT COUNT(*) FROM events WHERE idem_key IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(keyed, 2, "the fills an older build wrote were not keyed");

    // And the reconcile that follows the upgrade records neither of them again.
    for tid in [77u64, 78] {
        assert!(
            upgraded
                .record_fill(&NewFill {
                    account,
                    tid,
                    ts_ms: 1_756_000_000_000,
                    agent_id: None,
                    payload: &json!({ "account": account, "tid": tid }),
                })
                .expect("record")
                .is_none(),
            "tid {tid} was chained a second time on upgrade"
        );
    }
    assert_eq!(upgraded.get_events(0, 10).expect("page").events.len(), 2);
}

#[test]
fn generic_writers_cannot_forge_submission_lifecycle_events() {
    let dir = TempDir::new().expect("tempdir");
    let ledger = open(&dir, Network::Testnet);
    let receipt = ledger
        .record_intent(&NewIntent {
            agent_id: "alpha",
            ts_ms: 1,
            payload: &json!({}),
            snapshot: None,
        })
        .expect("intent");
    for kind in [EventKind::SubmissionStarted, EventKind::SubmissionResolved] {
        let event = NewEvent {
            kind,
            ts_ms: 1,
            agent_id: Some("alpha"),
            payload: &json!({}),
            snapshot: None,
        };
        assert!(matches!(
            ledger.append(&event),
            Err(LedgerError::UseSubmissionJournal)
        ));
        assert!(matches!(
            ledger.record_outcome(&receipt, kind, 2, &json!({})),
            Err(LedgerError::UseSubmissionJournal)
        ));
    }
    for kind in [
        EventKind::PilotAuthorized,
        EventKind::PilotAdopted,
        EventKind::PilotHalted,
    ] {
        assert!(matches!(
            ledger.append(&NewEvent {
                kind,
                ts_ms: 1,
                agent_id: Some("alpha"),
                payload: &json!({}),
                snapshot: None,
            }),
            Err(LedgerError::UsePilotJournal)
        ));
        assert!(matches!(
            ledger.record_outcome(&receipt, kind, 2, &json!({})),
            Err(LedgerError::UsePilotJournal)
        ));
    }
    for kind in [EventKind::PairingIssued, EventKind::PairingRevoked] {
        assert!(matches!(
            ledger.append(&NewEvent {
                kind,
                ts_ms: 1,
                agent_id: None,
                payload: &json!({}),
                snapshot: None,
            }),
            Err(LedgerError::UsePairingJournal)
        ));
        assert!(matches!(
            ledger.record_outcome(&receipt, kind, 2, &json!({})),
            Err(LedgerError::UsePairingJournal)
        ));
    }
    for kind in [EventKind::RegistryGranted, EventKind::RegistryRetired] {
        assert!(matches!(
            ledger.append(&NewEvent {
                kind,
                ts_ms: 1,
                agent_id: Some("alpha"),
                payload: &json!({}),
                snapshot: None,
            }),
            Err(LedgerError::UseRegistryJournal)
        ));
        assert!(matches!(
            ledger.record_outcome(&receipt, kind, 2, &json!({})),
            Err(LedgerError::UseRegistryJournal)
        ));
    }
    for kind in [
        EventKind::ApprovalProposed,
        EventKind::ApprovalClaimed,
        EventKind::ApprovalDisposed,
    ] {
        assert!(matches!(
            ledger.record_outcome(&receipt, kind, 2, &json!({})),
            Err(LedgerError::UseApprovalJournal)
        ));
    }
    assert_eq!(ledger.get_events(0, 10).expect("events").events.len(), 1);
}

#[test]
fn authority_reader_barriers_preserve_the_existing_chain_on_upgrade() {
    for prior_version in [3, 4, 5, 6, 7, 8, 9, 10] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("testnet.db");
        let (head, original) = {
            let ledger = Ledger::open_at(&path, Network::Testnet).unwrap();
            let head = ledger
                .append(&NewEvent {
                    kind: EventKind::AgentDecision,
                    ts_ms: 1,
                    agent_id: None,
                    payload: &json!({"reason": "prior history"}),
                    snapshot: None,
                })
                .unwrap();
            ledger
                .connection
                .lock()
                .unwrap()
                .pragma_update(None, "user_version", prior_version)
                .unwrap();
            let original = ledger.get_events(0, 100).unwrap().events;
            (head, original)
        };
        let upgraded = Ledger::open_at(&path, Network::Testnet).unwrap();
        let report = upgraded.verify().unwrap();
        assert!(report.is_intact());
        assert_eq!(report.head_seq, head.seq);
        assert_eq!(upgraded.event(head.seq).unwrap().unwrap().hash, head.hash);
        assert_eq!(upgraded.get_events(0, 100).unwrap().events, original);
        let version: i64 = upgraded
            .connection
            .lock()
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 11);
    }
}

#[test]
fn generic_writers_cannot_forge_policy_authority() {
    let dir = TempDir::new().unwrap();
    let ledger = Ledger::open(dir.path(), Network::Testnet).unwrap();
    let head = ledger.chain_head().unwrap();
    for kind in [EventKind::PolicyInitialized, EventKind::PolicyReplaced] {
        let payload = json!({"forged": true});
        assert!(matches!(
            ledger.append(&NewEvent {
                kind,
                ts_ms: 100,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            }),
            Err(LedgerError::UsePolicyJournal)
        ));
    }
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert!(ledger.get_events(0, 100).unwrap().events.is_empty());
}

#[test]
fn approval_authority_rejects_generic_writes_and_is_operator_only() {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    for kind in [
        EventKind::ApprovalProposed,
        EventKind::ApprovalClaimed,
        EventKind::ApprovalDisposed,
    ] {
        let payload = json!({"fixture": "operator approval evidence"});
        let event = NewEvent {
            kind,
            ts_ms: 1,
            agent_id: Some("alpha"),
            payload: &payload,
            snapshot: None,
        };
        assert!(matches!(
            ledger.append(&event),
            Err(LedgerError::UseApprovalJournal)
        ));
        let written = ledger.append_committed(&event).unwrap();
        assert!(
            ledger
                .get_events_for_agent("alpha", 0, 100)
                .unwrap()
                .events
                .is_empty()
        );
        let views = EventViews::new(ledger.clone());
        assert!(
            views
                .for_agent("alpha")
                .event(written.seq)
                .unwrap()
                .is_none()
        );
        assert!(ledger.event(written.seq).unwrap().is_some());
    }
}

#[test]
fn a_reader_refuses_a_newer_authority_schema_without_rewriting_history() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("testnet.db");
    let ledger = Ledger::open_at(&path, Network::Testnet).unwrap();
    let head = ledger.chain_head().unwrap();
    ledger
        .connection
        .lock()
        .unwrap()
        .pragma_update(None, "user_version", 12)
        .unwrap();
    drop(ledger);
    assert!(matches!(
        Ledger::open_at(&path, Network::Testnet),
        Err(LedgerError::SchemaTooNew {
            found: 12,
            supported: 11
        })
    ));
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        12
    );
    let (seq, hash) = super::head(&connection).unwrap();
    assert_eq!(Anchor { seq, hash }, head);
}
