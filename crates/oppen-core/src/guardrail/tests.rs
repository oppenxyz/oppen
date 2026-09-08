//! Tests for the guardrail engine.
//!
//! This module is a sibling of `engine`, so it cannot call `Cleared::new` —
//! that constructor is private to `engine.rs`. Every [`Cleared`] below came
//! out of [`GuardrailEngine::evaluate`], [`GuardrailEngine::clear_cancel`] or
//! [`GuardrailEngine::clear_schedule_cancel`], because there is no other way
//! to obtain one. That the file compiles is itself part of the proof.
//!
//! The centrepiece is [`no_input_produces_a_signable_value_without_passing_every_predicate`]:
//! twenty-four thousand pseudo-random cases, each re-checked against every
//! predicate independently of the engine that produced it, and against the
//! bytes of the action that would actually be signed rather than the request
//! that was made.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;

use oppen_hl::meta::Asset;
use oppen_hl::order::OrderKind;
use oppen_hl::types::AssetInfo;
use oppen_hl::wire::{CancelByCloidWire, CancelWire, Cloid, Grouping, OrderWire, Tif, Tpsl};
use oppen_hl::{Action, Network};

use crate::keys::KeyStore;

use super::config::{
    APPROVAL_TTL_MS, DEFAULT_DAILY_LOSS_USD, DEFAULT_MARK_DIVERGENCE_BPS,
    DEFAULT_MARK_DIVERGENCE_WINDOW_MS, DEFAULT_MAX_ORDER_USD, DEFAULT_MAX_POSITION_USD,
    DEFAULT_ORDER_RATE, MAX_REASON_BYTES,
};
use super::deadman::DEAD_MAN_MIN_LEAD_MS;
use super::store::MemoryStore;
use super::*;
use crate::ledger::{AuthorizedRoute, RegistryBinding};

#[path = "approval_journal_tests.rs"]
mod approval_journal_tests;

// ---- fixtures -----------------------------------------------------------

fn test_store() -> MemoryStore {
    let store = MemoryStore::new();
    let revision = store.load().unwrap().revision;
    store
        .compare_exchange(revision, &PersistedState::default(), NOW_MS)
        .unwrap();
    store
}

fn initialize_policy(
    registry: crate::ledger::RegistryJournal,
    legacy: &std::path::Path,
) -> Arc<crate::ledger::PolicyJournal> {
    let policy = Arc::new(crate::ledger::PolicyJournal::new(Arc::new(registry)));
    let review = LegacyPolicyReview::open(legacy, Network::Testnet, NOW_MS).unwrap();
    policy
        .initialize(&review, PersistedState::paused(NOW_MS), NOW_MS)
        .unwrap();
    policy
}

fn open_test_policy(path: &std::path::Path, initialize: bool) -> Arc<crate::ledger::PolicyJournal> {
    let ledger = Arc::new(crate::ledger::Ledger::open_at(path, Network::Testnet).unwrap());
    let registry = crate::ledger::RegistryJournal::open(
        ledger,
        Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
    )
    .unwrap();
    if initialize {
        initialize_policy(registry, &path.with_extension("legacy.db"))
    } else {
        Arc::new(crate::ledger::PolicyJournal::new(Arc::new(registry)))
    }
}

#[derive(Debug)]
struct TestRoutes(Mutex<std::collections::BTreeMap<AgentId, AuthorizedRoute>>);
impl TestRoutes {
    fn new(routes: impl IntoIterator<Item = AuthorizedRoute>) -> Self {
        Self(Mutex::new(
            routes
                .into_iter()
                .map(|r| (r.binding.agent.clone(), r))
                .collect(),
        ))
    }
    fn get(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        self.0
            .lock()
            .unwrap()
            .get(agent)
            .cloned()
            .ok_or_else(|| route_error("test route missing"))
    }
    fn permit(
        &self,
        clearance: &Clearance,
        wallet: &crate::keys::AgentWallet,
        signer: oppen_hl::Address,
    ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
        let guard = self.0.lock().unwrap();
        if guard.get(&clearance.agent) != Some(&clearance.route)
            || &clearance.route.binding.wallet != wallet
            || clearance.route.binding.wallet.address != signer
            || clearance.agent != clearance.route.binding.agent
            || clearance.network != clearance.route.network
            || clearance.vault_address != clearance.route.binding.vault_address
        {
            return Err(route_error("test route changed or signer mismatched"));
        }
        Ok(Box::new(TestRoutePermit { _guard: guard }))
    }
}
struct TestRoutePermit<'a> {
    _guard: std::sync::MutexGuard<'a, std::collections::BTreeMap<AgentId, AuthorizedRoute>>,
}
impl SigningPermit for TestRoutePermit<'_> {}
fn route_error(detail: &str) -> Refusal {
    Unevaluable::RouteAuthority {
        detail: detail.into(),
    }
    .into()
}
fn route_for(
    keys: &dyn KeyStore,
    agent: &str,
    container: oppen_hl::Address,
    vault_address: Option<oppen_hl::Address>,
) -> AuthorizedRoute {
    AuthorizedRoute {
        network: keys.network(),
        binding_seq: 1,
        binding: RegistryBinding {
            agent: AgentId::new(agent),
            container,
            vault_address,
            wallet: keys.agent_wallet(&AgentId::new(agent)).unwrap().unwrap(),
        },
    }
}
fn alpha_route() -> AuthorizedRoute {
    route_for(
        key_store(&["alpha"]).as_ref(),
        "alpha",
        vault(),
        Some(vault()),
    )
}

fn expiring_wallet_fixture(config: AgentGuardrails) -> Fixture {
    let keys = Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet));
    let agent = AgentId::new("alpha");
    keys.create_agent_key(
        &agent,
        crate::keys::SecretText::new(format!("{:064x}", 1)),
        NOW_MS + 1_000,
        NOW_MS,
    )
    .unwrap();
    let route = route_for(keys.as_ref(), "alpha", vault(), Some(vault()));
    let engine = GuardrailEngine::from_parts(
        Arc::new(test_store()),
        Arc::new(NullAuditSink::new([route])),
        keys,
        Network::Testnet,
    )
    .unwrap();
    engine.register_agent(&agent, NOW_MS).unwrap();
    engine
        .operator_set_guardrails(&agent, config, NOW_MS)
        .unwrap();
    acknowledge(&engine);
    Fixture { engine, agent }
}

#[test]
fn order_approval_window_is_start_inclusive_end_exclusive_without_admission_effects() {
    for approval_required in [false, true] {
        let mut config = permissive(&["BTC"]);
        config.approval_required = approval_required;
        config.order_rate.count = 1;
        let f = expiring_wallet_fixture(config);
        let order = intent("BTC", true, d("100"), d("1"));
        let instrument = asset("BTC", 2, 40);
        let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
        let state = exposure(d("100000"));
        for at_ms in [NOW_MS - 1, NOW_MS + 1_000, NOW_MS + 1_001] {
            assert!(matches!(
                f.engine
                    .evaluate(&f.agent, &order, &instrument, &market, &state, at_ms),
                Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
            ));
            assert!(
                !f.engine
                    .preflight(&f.agent, &order, &instrument, &market, &state, at_ms)
                    .would_clear
            );
            assert!(f.engine.pending_proposals(at_ms).unwrap().is_empty());
        }
        let result = f.evaluate(&order, &instrument, &market, &state);
        if approval_required {
            assert!(matches!(result, Err(Refusal::ApprovalRequired { .. })));
            assert_eq!(f.engine.pending_proposals(NOW_MS).unwrap().len(), 1);
        } else {
            assert_eq!(
                result
                    .unwrap()
                    .clearance()
                    .utilization
                    .order_tokens_remaining,
                Decimal::ZERO
            );
        }
    }
}

#[test]
fn order_wallet_expiry_between_evaluation_and_signature_refuses_at_exact_boundary() {
    let f = expiring_wallet_fixture(permissive(&["BTC"]));
    let cleared = route_clearance(&f, 0);
    assert!(matches!(
        f.engine.sign_cleared(cleared, 1, None, || NOW_MS + 1_000),
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::RouteAuthority { .. }
        )))
    ));
    let cleared = route_clearance(&f, 0);
    f.engine
        .sign_cleared(cleared, 2, None, || NOW_MS + 999)
        .expect("last approved millisecond signs");
}

#[test]
fn expired_wallet_keeps_live_route_cleanup_authority_for_cancels_and_schedule_cancel() {
    let f = expiring_wallet_fixture(permissive(&["BTC"]));
    let expired = NOW_MS + 1_000;
    for kind in 1u8..5 {
        let cleared = match kind {
            1 => f.engine.clear_cancel(
                &f.agent,
                vec![CancelWire { a: 0, o: 1 }],
                "expired cleanup",
                expired,
            ),
            2 => f.engine.clear_cancel_by_cloid(
                &f.agent,
                vec![oppen_hl::wire::CancelByCloidWire {
                    asset: 0,
                    cloid: cloid(),
                }],
                "expired cleanup",
                expired,
            ),
            3 => f
                .engine
                .clear_schedule_cancel(&f.agent, Some(expired + 60_000), expired),
            4 => f.engine.clear_schedule_cancel(&f.agent, None, expired),
            _ => unreachable!(),
        }
        .unwrap();
        f.engine
            .sign_cleared(cleared, u64::from(kind), None, || expired)
            .expect("cleanup retains route and signer checks, not order expiry gate");
    }
}

struct WaitingKeys {
    inner: Arc<crate::keys::MemoryKeyStore>,
    wait: Option<(
        std::sync::mpsc::Sender<()>,
        Mutex<std::sync::mpsc::Receiver<()>>,
    )>,
}

impl KeyStore for WaitingKeys {
    fn network(&self) -> Network {
        self.inner.network()
    }
    fn write(
        &self,
        entry: &crate::keys::EntryName,
        secret: &str,
    ) -> Result<(), crate::keys::KeyStoreError> {
        self.inner.write(entry, secret)
    }
    fn read(
        &self,
        entry: &crate::keys::EntryName,
    ) -> Result<Option<crate::keys::SecretText>, crate::keys::KeyStoreError> {
        self.inner.read(entry)
    }
    fn remove(&self, entry: &crate::keys::EntryName) -> Result<(), crate::keys::KeyStoreError> {
        self.inner.remove(entry)
    }
    fn load_agent_key_with_wallet(
        &self,
        agent: &AgentId,
    ) -> Result<(oppen_hl::AgentKey, crate::keys::AgentWallet), crate::keys::KeyStoreError> {
        if let Some((entered, release)) = &self.wait {
            entered.send(()).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }
        self.inner.load_agent_key_with_wallet(agent)
    }
}

struct TimedLedgerSink {
    inner: crate::ledger::LedgerAuditSink,
    entering_permit: Option<std::sync::mpsc::Sender<()>>,
    refused_at: Mutex<Vec<u64>>,
}

impl AuditSink for TimedLedgerSink {
    fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        self.inner.route_for_agent(agent)
    }
    fn before_sign(
        &self,
        clearance: &Clearance,
        wallet: &crate::keys::AgentWallet,
        signer: oppen_hl::Address,
    ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
        if let Some(entered) = &self.entering_permit {
            entered.send(()).unwrap();
        }
        self.inner.before_sign(clearance, wallet, signer)
    }
    fn record(&self, entry: &AuditEntry<'_>) -> Result<(), AuditError> {
        self.inner.record(entry)?;
        if matches!(entry.outcome, AuditOutcome::Refused(_)) {
            self.refused_at.lock().unwrap().push(entry.at_ms);
        }
        Ok(())
    }
}

// Both waits use real generation loading, registry grants, and durable permits.
// Only time and the point at which a dependency is released are synthetic.
fn expiry_during_signing_wait(key_wait: bool) {
    use std::sync::{atomic::AtomicU64, mpsc};
    use std::time::Duration;

    for reduce_only in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("expiry.db");
        let ledger = Arc::new(crate::ledger::Ledger::open_at(&path, Network::Testnet).unwrap());
        let inner_keys = Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet));
        let agent = AgentId::new("alpha");
        inner_keys
            .create_agent_key(
                &agent,
                crate::keys::SecretText::new(format!("{:064x}", 1)),
                NOW_MS + 1_000,
                NOW_MS,
            )
            .unwrap();
        let registry = crate::ledger::RegistryJournal::open(
            ledger,
            Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
        )
        .unwrap();
        registry
            .grant(
                route_for(inner_keys.as_ref(), "alpha", vault(), Some(vault())).binding,
                NOW_MS,
            )
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let keys = Arc::new(WaitingKeys {
            inner: inner_keys,
            wait: key_wait.then(|| (entered_tx.clone(), Mutex::new(release_rx))),
        });
        let policy = initialize_policy(registry, &dir.path().join("legacy.db"));
        let sink = Arc::new(TimedLedgerSink {
            inner: crate::ledger::LedgerAuditSink::new(policy.clone()),
            entering_permit: (!key_wait).then_some(entered_tx),
            refused_at: Mutex::new(Vec::new()),
        });
        let engine = GuardrailEngine::from_parts(
            Arc::new(SqliteGuardrailStore::new(policy)),
            sink.clone(),
            keys,
            Network::Testnet,
        )
        .unwrap();
        engine.register_agent(&agent, NOW_MS).unwrap();
        engine
            .operator_release_kill(&KillScope::Global, NOW_MS)
            .unwrap();
        engine
            .operator_set_guardrails(&agent, permissive(&["BTC"]), NOW_MS)
            .unwrap();
        acknowledge(&engine);
        let mut order = intent("BTC", !reduce_only, d("100"), Decimal::ONE);
        order.reduce_only = reduce_only;
        let mut state = exposure(d("100000"));
        if reduce_only {
            state
                .agent
                .positions
                .insert("BTC".into(), PositionSnapshot { szi: Decimal::ONE });
            state.agent.total_position_notional_usd = d("100");
        }
        let cleared = engine
            .evaluate(
                &agent,
                &order,
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &state,
                NOW_MS,
            )
            .unwrap();
        let clock = AtomicU64::new(NOW_MS);
        let samples = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            // An independent handle holds the actual ledger coordination file.
            let coordination = (!key_wait).then(|| {
                let file = std::fs::File::options()
                    .read(true)
                    .write(true)
                    .open(path.with_extension("db.lock"))
                    .unwrap();
                file.lock().unwrap();
                file
            });
            let (done_tx, done_rx) = mpsc::channel();
            let engine = &engine;
            let clock = &clock;
            let samples = &samples;
            scope.spawn(move || {
                let result = engine.sign_cleared(cleared, 1, None, || {
                    samples.fetch_add(1, Ordering::SeqCst);
                    clock.load(Ordering::SeqCst)
                });
                done_tx.send(result).unwrap();
            });
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let waiting = done_rx.recv_timeout(Duration::from_millis(100));
            let samples_before_release = samples.load(Ordering::SeqCst);
            clock.store(NOW_MS + 1_000, Ordering::SeqCst);
            drop(coordination);
            if key_wait {
                release_tx.send(()).unwrap();
            }
            assert!(matches!(waiting, Err(mpsc::RecvTimeoutError::Timeout)));
            assert_eq!(
                samples_before_release, 0,
                "clock must follow dependency waits"
            );
            assert!(
                matches!(
                    done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                    Err(SignClearedError::Refused(Refusal::Unevaluable(
                        Unevaluable::RouteAuthority { .. }
                    )))
                ),
                "expired order returned a signature or the wrong refusal; reduce_only={reduce_only}"
            );
        });
        assert_eq!(
            samples.load(Ordering::SeqCst),
            1,
            "audit must reuse the final observation"
        );
        assert_eq!(*sink.refused_at.lock().unwrap(), [NOW_MS + 1_000]);
    }
}

#[test]
fn order_expiry_is_sampled_after_key_loading_including_reduce_only() {
    expiry_during_signing_wait(true);
}

#[test]
fn order_expiry_is_sampled_after_ledger_contention_including_reduce_only() {
    expiry_during_signing_wait(false);
}

fn route_clearance(f: &Fixture, kind: u8) -> Cleared {
    match kind {
        0 => f
            .evaluate(
                &intent("BTC", true, d("100"), d("1")),
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &exposure(d("100000")),
            )
            .unwrap(),
        1 => f
            .engine
            .clear_cancel(
                &f.agent,
                vec![CancelWire { a: 0, o: 1 }],
                "route test",
                NOW_MS,
            )
            .unwrap(),
        2 => f
            .engine
            .clear_cancel_by_cloid(
                &f.agent,
                vec![oppen_hl::wire::CancelByCloidWire {
                    asset: 0,
                    cloid: cloid(),
                }],
                "route test",
                NOW_MS,
            )
            .unwrap(),
        3 => f
            .engine
            .clear_schedule_cancel(&f.agent, Some(NOW_MS + 60_000), NOW_MS)
            .unwrap(),
        4 => f
            .engine
            .clear_schedule_cancel(&f.agent, None, NOW_MS)
            .unwrap(),
        _ => unreachable!(),
    }
}

#[test]
fn route_retirement_and_revision_changes_refuse_every_signed_action() {
    for kind in 0..5 {
        for retire in [false, true] {
            let sink = Arc::new(CountingSink::new([alpha_route()]));
            let f = Fixture::with(permissive(&["BTC"]), Arc::new(test_store()), sink.clone());
            let cleared = route_clearance(&f, kind);
            assert_eq!(cleared.clearance().route.binding.container, vault());
            if retire {
                sink.routes.0.lock().unwrap().remove(&f.agent);
            } else {
                sink.routes
                    .0
                    .lock()
                    .unwrap()
                    .get_mut(&f.agent)
                    .unwrap()
                    .binding_seq += 1;
            }
            assert!(
                matches!(
                    f.engine.sign_cleared(cleared, 1, None, || NOW_MS),
                    Err(SignClearedError::Refused(Refusal::Unevaluable(
                        Unevaluable::RouteAuthority { .. }
                    )))
                ),
                "kind {kind}, retired {retire}"
            );
            assert_eq!(
                sink.refused.load(Ordering::Relaxed),
                1,
                "permit must release before refusal audit"
            );
        }
    }
}

#[test]
fn exposure_account_and_route_identity_must_agree_before_clearance() {
    let f = Fixture::new(permissive(&["BTC"]));
    let mut wrong = exposure(d("100000"));
    wrong.account = oppen_hl::Address::from_bytes([8; 20]);
    let order = intent("BTC", true, d("100"), d("1"));
    let instrument = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    assert!(matches!(
        f.evaluate(&order, &instrument, &market, &wrong),
        Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
    ));
    assert!(
        !f.preflight(&order, &instrument, &market, &wrong)
            .would_clear
    );
    for variant in 0..4 {
        let mut route = alpha_route();
        match variant {
            0 => route.network = Network::Mainnet,
            1 => route.binding_seq = 0,
            2 => route.binding.container = oppen_hl::Address::from_bytes([8; 20]),
            3 => route.binding.agent = AgentId::new("other"),
            _ => unreachable!(),
        }
        let sink = Arc::new(NullAuditSink::new([]));
        sink.routes
            .0
            .lock()
            .unwrap()
            .insert(AgentId::new("alpha"), route);
        let f = Fixture::with(permissive(&["BTC"]), Arc::new(test_store()), sink);
        assert!(matches!(
            f.evaluate(&order, &instrument, &market, &exposure(d("100000"))),
            Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
        ));
    }
}

#[test]
fn policy_registration_and_restart_never_grant_route_authority() {
    let store = Arc::new(test_store());
    for _ in 0..2 {
        let engine = GuardrailEngine::from_parts(
            store.clone(),
            Arc::new(NullAuditSink::new([])),
            key_store(&["alpha"]),
            Network::Testnet,
        )
        .unwrap();
        let agent = AgentId::new("alpha");
        let first = engine.register_agent(&agent, NOW_MS).unwrap();
        assert_eq!(engine.register_agent(&agent, NOW_MS).unwrap(), first);
        assert!(matches!(
            engine.route_for_agent(&agent),
            Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
        ));
        assert!(matches!(
            engine.clear_cancel(&agent, vec![CancelWire { a: 0, o: 1 }], "no grant", NOW_MS),
            Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
        ));
    }
}

#[test]
fn every_wallet_field_in_the_grant_must_match_the_loaded_wallet_for_all_actions() {
    for kind in 0..5 {
        for field in 0..4 {
            let mut route = alpha_route();
            match field {
                0 => route.binding.wallet.generation += 1,
                1 => route.binding.wallet.address = oppen_hl::Address::from_bytes([8; 20]),
                2 => route.binding.wallet.approved_at_ms -= 1,
                3 => route.binding.wallet.valid_until_ms += 1,
                _ => unreachable!(),
            }
            let f = Fixture::with(
                permissive(&["BTC"]),
                Arc::new(test_store()),
                Arc::new(NullAuditSink::new([route])),
            );
            let cleared = route_clearance(&f, kind);
            assert!(
                matches!(
                    f.engine.sign_cleared(cleared, 1, None, || NOW_MS),
                    Err(SignClearedError::Refused(Refusal::Unevaluable(
                        Unevaluable::RouteAuthority { .. }
                    )))
                ),
                "kind {kind}, wallet field {field}"
            );
        }
    }
}
#[derive(Debug)]
struct NullAuditSink {
    routes: TestRoutes,
}
impl NullAuditSink {
    fn new(routes: impl IntoIterator<Item = AuthorizedRoute>) -> Self {
        Self {
            routes: TestRoutes::new(routes),
        }
    }
}
impl AuditSink for NullAuditSink {
    fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        self.routes.get(agent)
    }
    fn before_sign(
        &self,
        clearance: &Clearance,
        wallet: &crate::keys::AgentWallet,
        signer: oppen_hl::Address,
    ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
        self.routes.permit(clearance, wallet, signer)
    }
    fn record(&self, _: &AuditEntry<'_>) -> Result<(), AuditError> {
        Ok(())
    }
}

/// 2026-09-03T00:00:00Z, an exact UTC midnight.
const MIDNIGHT_MS: u64 = 1_788_998_400_000;
/// Noon that day, the `now` of most tests.
const NOW_MS: u64 = MIDNIGHT_MS + 12 * 3_600_000;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("decimal literal")
}

/// The sub-account D1 pairs `alpha` with.
fn vault() -> oppen_hl::Address {
    oppen_hl::Address::parse("0x0d1d9635d0640821d15e323ac8adadfa9c111414").expect("address")
}

/// A key store holding one agent wallet per named agent.
///
/// The hex is derived from the agent's position in the list, so no two agents
/// share a signer — which is the whole point of the container model, and the
/// property [`a_clearance_is_signed_by_the_key_of_the_agent_it_names`] leans
/// on. Testnet, because `AGENTS.md` invariant 5 makes that the default and
/// [`GuardrailEngine::new`] refuses a store bound to the other network.
fn key_store(agents: &[&str]) -> Arc<crate::keys::MemoryKeyStore> {
    let store = Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet));
    for (n, agent) in agents.iter().enumerate() {
        store
            .create_agent_key(
                &AgentId::new(*agent),
                crate::keys::SecretText::new(format!("{:064x}", n + 1)),
                NOW_MS + 90 * 86_400_000,
                NOW_MS,
            )
            .expect("a wallet for the agent");
    }
    store
}

/// The signer bound to `agent` in `store`, for tests that assert which key
/// actually signed.
fn signer_of(store: &crate::keys::MemoryKeyStore, agent: &str) -> oppen_hl::Address {
    store
        .load_agent_key(&AgentId::new(agent))
        .expect("wallet")
        .address()
}

fn cloid() -> Cloid {
    Cloid::parse("0x00000000000000000000000000000001").expect("cloid")
}

fn asset(name: &str, sz_decimals: u32, max_leverage: u32) -> Asset {
    Asset {
        index: 7,
        info: AssetInfo {
            name: name.to_owned(),
            sz_decimals,
            max_leverage,
            margin_table_id: 0,
            is_delisted: false,
            only_isolated: false,
        },
    }
}

fn account(equity: Decimal) -> AccountSnapshot {
    AccountSnapshot {
        as_of_ms: NOW_MS,
        reconciled: true,
        equity_usd: equity,
        peak_equity_usd: equity,
        realized_pnl_today_usd: Decimal::ZERO,
        unrealized_pnl_usd: Decimal::ZERO,
        day_start_ms: MIDNIGHT_MS,
        total_position_notional_usd: Decimal::ZERO,
        positions: BTreeMap::new(),
        resting: Some(RestingExposure::none()),
    }
}

/// An exposure whose working book holds `szi` of `symbol` at `px`.
fn with_resting(mut exposure: Exposure, symbol: &str, szi: Decimal, px: Decimal) -> Exposure {
    let mut book = RestingExposure::none();
    if szi >= Decimal::ZERO {
        book.buys.insert(symbol.to_owned(), szi);
    } else {
        book.sells.insert(symbol.to_owned(), -szi);
    }
    book.notional_usd = szi.abs() * px;
    book.notional_by_symbol
        .insert(symbol.to_owned(), book.notional_usd);
    exposure.agent.resting = Some(book);
    exposure
}

fn exposure(equity: Decimal) -> Exposure {
    Exposure {
        account: vault(),
        agent: account(equity),
        fleet: None,
    }
}

fn intent(symbol: &str, is_buy: bool, px: Decimal, sz: Decimal) -> OrderIntent {
    OrderIntent {
        original: None,
        symbol: symbol.to_owned(),
        is_buy,
        px,
        sz,
        kind: OrderKind::Limit { tif: Tif::Gtc },
        reduce_only: false,
        cloid: None,
        grouping: Grouping::Na,
        builder: None,
        max_slippage_bps: None,
        reason: "test".to_owned(),
    }
}

/// D-c's defaults, loosened only where a test needs room. Approval mode goes
/// off because item 28's queue is not built yet and every test that is not
/// about approval would otherwise stop there.
fn permissive(symbols: &[&str]) -> AgentGuardrails {
    AgentGuardrails {
        symbols: symbols.iter().map(|s| (*s).to_owned()).collect(),
        max_order_usd: d("1000000"),
        max_position_usd: d("1000000"),
        max_slippage_bps: d("10000"),
        order_rate: OrderRate {
            count: 1_000,
            per_ms: 1_000,
        },
        reduce_only: false,
        risk: RiskSettings {
            max_leverage: 50,
            margin_mode: MarginMode::Cross,
            max_open_exposure_usd: None,
            max_risk_usd: None,
        },
        loss: LossLimits::UNSET,
        approval_required: false,
        freshness: Freshness::default(),
        max_mark_divergence_bps: DEFAULT_MARK_DIVERGENCE_BPS,
        mark_divergence_window_ms: DEFAULT_MARK_DIVERGENCE_WINDOW_MS,
    }
}

#[derive(Debug)]
struct CountingSink {
    routes: TestRoutes,
    route_reads: AtomicUsize,
    cleared: AtomicUsize,
    refused: AtomicUsize,
    operator: Mutex<Vec<OperatorAction>>,
}

impl AuditSink for CountingSink {
    fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        self.route_reads.fetch_add(1, Ordering::Relaxed);
        self.routes.get(agent)
    }
    fn before_sign(
        &self,
        clearance: &Clearance,
        wallet: &crate::keys::AgentWallet,
        signer: oppen_hl::Address,
    ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
        self.routes.permit(clearance, wallet, signer)
    }

    fn record(&self, entry: &AuditEntry<'_>) -> Result<(), AuditError> {
        match entry.outcome {
            AuditOutcome::Cleared(_) => {
                self.cleared.fetch_add(1, Ordering::Relaxed);
            }
            AuditOutcome::Refused(_) => {
                self.refused.fetch_add(1, Ordering::Relaxed);
            }
            AuditOutcome::Operator(action) => self
                .operator
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(action.clone()),
        }
        Ok(())
    }
}

impl CountingSink {
    fn new(routes: impl IntoIterator<Item = AuthorizedRoute>) -> Self {
        Self {
            routes: TestRoutes::new(routes),
            route_reads: AtomicUsize::new(0),
            cleared: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            operator: Mutex::new(Vec::new()),
        }
    }
    fn operator_actions(&self) -> Vec<OperatorAction> {
        self.operator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[derive(Debug)]
struct FailingSink {
    routes: TestRoutes,
}

impl AuditSink for FailingSink {
    fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        self.routes.get(agent)
    }
    fn before_sign(
        &self,
        _clearance: &Clearance,
        _wallet: &crate::keys::AgentWallet,
        _signer: oppen_hl::Address,
    ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
        Err(Unevaluable::AuditWriteFailed {
            detail: "the ledger disk is full".into(),
        }
        .into())
    }

    fn record(&self, entry: &AuditEntry<'_>) -> Result<(), AuditError> {
        // This seam fails decision writes after explicit fixture activation.
        if matches!(
            entry.outcome,
            AuditOutcome::Operator(OperatorAction::PolicyAcknowledgmentRequested { .. })
        ) {
            return Ok(());
        }
        Err(AuditError {
            detail: "the ledger disk is full".to_owned(),
        })
    }
}

/// A store whose writes fail, for the fail-closed path where a kill-switch
/// engagement cannot be persisted.
#[derive(Debug)]
struct FailingStore {
    inner: MemoryStore,
    failed: std::sync::atomic::AtomicBool,
}

impl Default for FailingStore {
    fn default() -> Self {
        Self {
            inner: test_store(),
            failed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl GuardrailStore for FailingStore {
    fn load(&self) -> Result<PolicyVersion, StoreError> {
        self.inner.load()
    }
    fn compare_exchange(
        &self,
        expected: u64,
        next: &PersistedState,
        at_ms: u64,
    ) -> Result<PolicyVersion, StoreError> {
        if next.kill != KillSwitch::new() || self.failed.load(Ordering::SeqCst) {
            self.failed.store(true, Ordering::SeqCst);
            return Err(StoreError::Poisoned);
        }
        self.inner.compare_exchange(expected, next, at_ms)
    }
}

fn acknowledge(engine: &GuardrailEngine) {
    let observed = engine
        .policy_observation()
        .expect("synthetic operator policy review");
    engine
        .operator_acknowledge_policy(observed, NOW_MS)
        .expect("explicit synthetic acknowledgment");
}

struct ControlledPolicyStore {
    inner: MemoryStore,
    fail_load: std::sync::atomic::AtomicBool,
    uncertain_write: std::sync::atomic::AtomicBool,
    wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl ControlledPolicyStore {
    fn new() -> Self {
        Self {
            inner: test_store(),
            fail_load: false.into(),
            uncertain_write: false.into(),
            wait: Mutex::new(None),
        }
    }
}

impl GuardrailStore for ControlledPolicyStore {
    fn load(&self) -> Result<PolicyVersion, StoreError> {
        if self.fail_load.load(Ordering::SeqCst) {
            return Err(StoreError::Poisoned);
        }
        self.inner.load()
    }
    fn compare_exchange(
        &self,
        expected: u64,
        next: &PersistedState,
        at_ms: u64,
    ) -> Result<PolicyVersion, StoreError> {
        let wait = self.wait.lock().unwrap().take();
        if let Some((entered, release)) = wait {
            entered.send(()).unwrap();
            release
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }
        let committed = self.inner.compare_exchange(expected, next, at_ms)?;
        if self.uncertain_write.load(Ordering::SeqCst) {
            return Err(StoreError::Poisoned);
        }
        Ok(committed)
    }
}

#[test]
fn startup_and_independent_policy_changes_require_explicit_acknowledgment() {
    let store = Arc::new(test_store());
    let f = Fixture::with(
        permissive(&["BTC"]),
        store.clone(),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    let reopened = GuardrailEngine::from_parts(
        store.clone(),
        Arc::new(NullAuditSink::new([alpha_route()])),
        key_store(&["alpha"]),
        Network::Testnet,
    )
    .unwrap();
    let rejected = || {
        reopened.evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), Decimal::ONE),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
    };
    assert!(matches!(
        rejected(),
        Err(Refusal::Unevaluable(Unevaluable::PolicyAuthority { .. }))
    ));
    assert!(reopened.cancellation_needed(&AgentId::new("not-in-policy")));
    let observed = reopened.policy_observation().unwrap();
    assert!(
        reopened.cancellation_needed(&f.agent),
        "refresh never acknowledges"
    );
    reopened
        .operator_acknowledge_policy(observed, NOW_MS)
        .unwrap();
    assert!(rejected().is_ok());
    let version = store.load().unwrap();
    let mut next = version.state;
    next.account_limits.max_drawdown_usd = Some(d("9999"));
    store
        .compare_exchange(version.revision, &next, NOW_MS)
        .unwrap();
    assert!(matches!(
        rejected(),
        Err(Refusal::Unevaluable(Unevaluable::PolicyAuthority { .. }))
    ));
    assert!(
        reopened
            .operator_acknowledge_policy(observed, NOW_MS)
            .is_err()
    );
}

#[test]
fn cached_policy_status_neither_reads_authority_nor_acknowledges_a_refresh() {
    let store = Arc::new(ControlledPolicyStore::new());
    let f = Fixture::with(
        permissive(&["BTC"]),
        store.clone(),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    let admitted = f.engine.policy_status();
    assert!(!admitted.admission_inhibited);
    assert_eq!(
        admitted.cached_revision,
        admitted.acknowledgment.map(|a| a.revision)
    );
    store.fail_load.store(true, Ordering::SeqCst);
    assert_eq!(
        f.engine.policy_status(),
        admitted,
        "cached status performs no authority read"
    );
    assert!(f.engine.policy_observation().is_err());
    let inhibited = f.engine.policy_status();
    assert!(inhibited.admission_inhibited);
    assert_eq!(inhibited.cached_revision, admitted.cached_revision);
    assert!(inhibited.acknowledgment.is_none());
    assert!(inhibited.stop_generation > admitted.stop_generation);
    store.fail_load.store(false, Ordering::SeqCst);
    f.engine.policy_observation().unwrap();
    assert_eq!(
        f.engine.policy_status(),
        inhibited,
        "verification cannot acknowledge"
    );
}

#[test]
fn unavailable_policy_preserves_registry_cleanup_and_local_stop_visibility() {
    let store = Arc::new(ControlledPolicyStore::new());
    store.fail_load.store(true, Ordering::SeqCst);
    let engine = GuardrailEngine::from_parts(
        store,
        Arc::new(NullAuditSink::new([alpha_route()])),
        key_store(&["alpha"]),
        Network::Testnet,
    )
    .unwrap();
    let f = Fixture {
        engine,
        agent: AgentId::new("alpha"),
    };
    assert!(f.engine.guardrails(&f.agent).is_none());
    assert!(f.engine.route_for_agent(&f.agent).is_ok());
    assert!(f.engine.cancellation_needed(&f.agent));
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), Decimal::ONE),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000"))
        ),
        Err(Refusal::Unevaluable(Unevaluable::PolicyAuthority { .. }))
    ));
    for kind in 1u8..5 {
        let cleared = route_clearance(&f, kind);
        assert_eq!(cleared.clearance().policy_revision, 0);
        f.engine
            .sign_cleared(cleared, u64::from(kind), None, || NOW_MS)
            .expect("registry-only cleanup signs");
    }
}

#[test]
fn production_constructor_without_policy_still_signs_registry_verified_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(
        crate::ledger::Ledger::open_at(&dir.path().join("cleanup.db"), Network::Testnet).unwrap(),
    );
    let keys = key_store(&["alpha"]);
    let registry = crate::ledger::RegistryJournal::open(
        ledger,
        Arc::new(crate::keys::HmacKey::from_bytes([31; 32])),
    )
    .unwrap();
    registry
        .grant(
            route_for(keys.as_ref(), "alpha", vault(), Some(vault())).binding,
            NOW_MS,
        )
        .unwrap();
    let policy = Arc::new(crate::ledger::PolicyJournal::new(Arc::new(registry)));
    assert!(policy.current().is_err());
    let engine = GuardrailEngine::new(policy, keys).unwrap();
    let f = Fixture {
        engine,
        agent: AgentId::new("alpha"),
    };
    assert!(f.engine.policy_observation().is_err());
    assert!(f.engine.cancellation_needed(&f.agent));
    assert!(f.engine.route_for_agent(&f.agent).is_ok());
    for kind in 1u8..5 {
        let cleared = route_clearance(&f, kind);
        f.engine
            .sign_cleared(cleared, u64::from(kind), None, || NOW_MS)
            .expect("no policy fallback needed for cleanup");
    }
}

#[test]
fn stale_engine_cas_cannot_overwrite_another_policy_or_acknowledge_it() {
    let store = Arc::new(test_store());
    let f = Fixture::with(
        permissive(&["BTC"]),
        store.clone(),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    let current = store.load().unwrap();
    let mut changed = current.state;
    changed.kill.engage(
        KillScope::Global,
        Engagement {
            engaged_at_ms: NOW_MS,
            reason: KillReason::Operator,
        },
    );
    let committed = store
        .compare_exchange(current.revision, &changed, NOW_MS)
        .unwrap();
    assert!(
        f.engine
            .operator_set_guardrails(&f.agent, permissive(&["ETH"]), NOW_MS)
            .is_err()
    );
    assert_eq!(store.load().unwrap(), committed);
    assert!(f.engine.cancellation_needed(&f.agent));
    f.engine.policy_observation().unwrap();
    assert!(f.engine.cancellation_needed(&f.agent));
    assert!(f.engine.kill_switch().is_engaged(&KillScope::Global));
}

#[test]
fn uncertain_cas_and_reload_cannot_clear_emergency_stop_or_enable_restart() {
    let store = Arc::new(ControlledPolicyStore::new());
    let f = Fixture::with(
        permissive(&["BTC"]),
        store.clone(),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    let old = f.engine.policy_observation().unwrap();
    store.uncertain_write.store(true, Ordering::SeqCst);
    assert!(
        f.engine
            .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
            .is_err()
    );
    assert!(
        store
            .load()
            .unwrap()
            .state
            .kill
            .is_engaged(&KillScope::Global),
        "error does not mean uncommitted"
    );
    assert_eq!(f.engine.take_pending_kill_effects().len(), 1);
    f.engine.policy_observation().unwrap();
    assert!(f.engine.operator_acknowledge_policy(old, NOW_MS).is_err());
    assert!(f.engine.cancellation_needed(&f.agent));
    store.fail_load.store(true, Ordering::SeqCst);
    assert!(f.engine.policy_observation().is_err());
    assert!(f.engine.kill_switch().is_engaged(&KillScope::Global));
    let reopened = GuardrailEngine::from_parts(
        store,
        Arc::new(NullAuditSink::new([alpha_route()])),
        key_store(&["alpha"]),
        Network::Testnet,
    )
    .unwrap();
    assert!(reopened.cancellation_needed(&f.agent));
}

#[test]
fn an_older_release_cannot_clear_a_new_stop_while_cas_is_waiting() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let store = Arc::new(ControlledPolicyStore::new());
    let f = Fixture::with(
        permissive(&["BTC"]),
        store.clone(),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        .unwrap();
    let observed = f.engine.policy_observation().unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *store.wait.lock().unwrap() = Some((entered_tx, release_rx));
    std::thread::scope(|scope| {
        let release = scope.spawn(|| f.engine.operator_release_kill(&KillScope::Global, NOW_MS));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let trip = scope.spawn(|| {
            f.engine.operator_engage_kill(
                KillScope::agent(f.agent.clone()),
                KillReason::FeedFailure,
                NOW_MS,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let newer = loop {
            // This read can complete while CAS waits: no engine-state lock is held.
            let next = f.engine.policy_observation().unwrap();
            if next.stop_generation != observed.stop_generation {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::yield_now();
        };
        release_tx.send(()).unwrap();
        assert!(
            newer,
            "the newer stop must be installed before persistence waits"
        );
        assert!(release.join().unwrap().is_err());
        trip.join().unwrap().unwrap();
    });
    assert!(
        f.engine.kill_switch().is_engaged(&KillScope::Global),
        "old release must not clear local evidence"
    );
    assert!(
        f.engine
            .kill_switch()
            .is_engaged(&KillScope::agent(f.agent.clone()))
    );
    assert!(
        f.engine
            .operator_acknowledge_policy(observed, NOW_MS)
            .is_err()
    );
    assert!(f.engine.cancellation_needed(&f.agent));
}

struct AcknowledgmentAuditSink {
    routes: TestRoutes,
    fail: std::sync::atomic::AtomicBool,
    requests: Mutex<Vec<(u64, u64, u64)>>,
    wait: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl AuditSink for AcknowledgmentAuditSink {
    fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
        self.routes.get(agent)
    }
    fn before_sign(
        &self,
        clearance: &Clearance,
        wallet: &crate::keys::AgentWallet,
        signer: oppen_hl::Address,
    ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
        self.routes.permit(clearance, wallet, signer)
    }
    fn record(&self, entry: &AuditEntry<'_>) -> Result<(), AuditError> {
        if let AuditOutcome::Operator(OperatorAction::PolicyAcknowledgmentRequested {
            revision,
            stop_generation,
        }) = entry.outcome
        {
            let wait = self.wait.lock().unwrap().take();
            if let Some((entered, release)) = wait {
                entered.send(()).unwrap();
                release
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(AuditError {
                    detail: "ack audit unavailable".into(),
                });
            }
            self.requests
                .lock()
                .unwrap()
                .push((entry.at_ms, *revision, *stop_generation));
        }
        Ok(())
    }
}

#[test]
fn acknowledgment_requires_audit_success_with_exact_reviewed_identity_and_time() {
    let sink = Arc::new(AcknowledgmentAuditSink {
        routes: TestRoutes::new([alpha_route()]),
        fail: false.into(),
        requests: Mutex::new(Vec::new()),
        wait: Mutex::new(None),
    });
    let f = Fixture::with(permissive(&["BTC"]), Arc::new(test_store()), sink.clone());
    let observed = f.engine.policy_observation().unwrap();
    sink.fail.store(true, Ordering::SeqCst);
    assert!(
        f.engine
            .operator_acknowledge_policy(observed, NOW_MS + 1)
            .is_err()
    );
    assert!(f.engine.cancellation_needed(&f.agent));
    sink.fail.store(false, Ordering::SeqCst);
    assert!(
        f.engine
            .operator_acknowledge_policy(observed, NOW_MS + 2)
            .is_err(),
        "failed audit advanced inhibition generation"
    );
    let current = f.engine.policy_observation().unwrap();
    f.engine
        .operator_acknowledge_policy(current, NOW_MS + 3)
        .unwrap();
    assert_eq!(
        sink.requests.lock().unwrap().last(),
        Some(&(NOW_MS + 3, current.revision, current.stop_generation))
    );
    assert!(!f.engine.cancellation_needed(&f.agent));
}

#[test]
fn acknowledgment_recorded_before_a_new_stop_does_not_enable_admission() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let sink = Arc::new(AcknowledgmentAuditSink {
        routes: TestRoutes::new([alpha_route()]),
        fail: false.into(),
        requests: Mutex::new(Vec::new()),
        wait: Mutex::new(None),
    });
    let f = Fixture::with(permissive(&["BTC"]), Arc::new(test_store()), sink.clone());
    let observed = f.engine.policy_observation().unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *sink.wait.lock().unwrap() = Some((entered_tx, release_rx));
    std::thread::scope(|scope| {
        let ack = scope.spawn(|| f.engine.operator_acknowledge_policy(observed, NOW_MS));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            f.engine.cancellation_needed(&f.agent),
            "audit completion precedes enabling"
        );
        let trip = scope.spawn(|| {
            f.engine
                .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let newer = loop {
            if f.engine.policy_observation().unwrap().stop_generation != observed.stop_generation {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::yield_now();
        };
        release_tx.send(()).unwrap();
        assert!(newer, "audit wait must not hold engine state");
        assert!(ack.join().unwrap().is_err());
        trip.join().unwrap().unwrap();
    });
    assert!(sink.requests.lock().unwrap().contains(&(
        NOW_MS,
        observed.revision,
        observed.stop_generation
    )));
    assert!(f.engine.cancellation_needed(&f.agent));
}

struct Fixture {
    engine: GuardrailEngine,
    agent: AgentId,
}

impl Fixture {
    fn without_keys(config: AgentGuardrails) -> Self {
        let engine = GuardrailEngine::from_parts(
            Arc::new(test_store()),
            Arc::new(NullAuditSink::new([alpha_route()])),
            Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet)),
            Network::Testnet,
        )
        .expect("engine without signing credentials");
        let agent = AgentId::new("alpha");
        engine.register_agent(&agent, NOW_MS).unwrap();
        engine
            .operator_set_guardrails(&agent, config, NOW_MS)
            .unwrap();
        acknowledge(&engine);
        Self { engine, agent }
    }

    fn new(config: AgentGuardrails) -> Self {
        Self::with(
            config,
            Arc::new(test_store()),
            Arc::new(NullAuditSink::new([alpha_route()])),
        )
    }

    fn with(
        config: AgentGuardrails,
        store: Arc<dyn GuardrailStore>,
        sink: Arc<dyn AuditSink>,
    ) -> Self {
        let engine = GuardrailEngine::from_parts(
            store,
            sink,
            key_store(&["alpha"]) as Arc<dyn KeyStore>,
            Network::Testnet,
        )
        .expect("engine");
        let agent = AgentId::new("alpha");
        engine.register_agent(&agent, NOW_MS).expect("register");
        engine
            .operator_set_guardrails(&agent, config, NOW_MS)
            .expect("set guardrails");
        acknowledge(&engine);
        Fixture { engine, agent }
    }

    /// The same fixture in either shape of D1 container: a sub-account, or
    /// the **top-level** account the revised D1 (V2) makes the Hyperliquid
    /// default, which carries no `vaultAddress` on the wire at all.
    fn in_container(config: AgentGuardrails, vault_address: Option<oppen_hl::Address>) -> Self {
        let mut route = alpha_route();
        route.binding.vault_address = vault_address;
        let engine = GuardrailEngine::from_parts(
            Arc::new(test_store()),
            Arc::new(NullAuditSink::new([route])),
            key_store(&["alpha"]) as Arc<dyn KeyStore>,
            Network::Testnet,
        )
        .expect("engine");
        let agent = AgentId::new("alpha");
        engine.register_agent(&agent, NOW_MS).expect("register");
        engine
            .operator_set_guardrails(&agent, config, NOW_MS)
            .expect("set guardrails");
        acknowledge(&engine);
        Fixture { engine, agent }
    }

    fn evaluate(
        &self,
        intent: &OrderIntent,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
    ) -> Result<Cleared, Refusal> {
        self.engine
            .evaluate(&self.agent, intent, asset, market, exposure, NOW_MS)
    }

    fn preflight(
        &self,
        intent: &OrderIntent,
        asset: &Asset,
        market: &MarketRef,
        exposure: &Exposure,
    ) -> Verdict {
        self.engine
            .preflight(&self.agent, intent, asset, market, exposure, NOW_MS)
    }
}

/// The order the engine actually built, read back off the wire.
fn wire_of(cleared: &Cleared) -> OrderWire {
    match cleared.action() {
        Action::Order { orders, .. } => {
            assert_eq!(orders.len(), 1, "one intent must produce one order");
            orders[0].clone()
        }
        other => panic!("expected an order action, got {other:?}"),
    }
}

fn wire_px(wire: &OrderWire) -> Decimal {
    Decimal::from_str(wire.p.as_str()).expect("wire price parses")
}

fn wire_sz(wire: &OrderWire) -> Decimal {
    Decimal::from_str(wire.s.as_str()).expect("wire size parses")
}

// ---- D-c: the refusal is the onboarding ---------------------------------

#[test]
fn a_freshly_paired_agent_is_refused_and_told_which_limit_to_raise() {
    let store: Arc<dyn GuardrailStore> = Arc::new(test_store());
    let engine = GuardrailEngine::from_parts(
        store,
        Arc::new(NullAuditSink::new([alpha_route()])),
        key_store(&["alpha"]) as Arc<dyn KeyStore>,
        Network::Testnet,
    )
    .expect("engine");
    let agent = AgentId::new("alpha");
    let config = engine.register_agent(&agent, NOW_MS).expect("register");
    acknowledge(&engine);

    // D-c verbatim.
    assert!(config.symbols.is_empty());
    assert_eq!(config.max_order_usd, DEFAULT_MAX_ORDER_USD);
    assert_eq!(config.max_position_usd, DEFAULT_MAX_POSITION_USD);
    assert_eq!(config.loss.max_daily_loss_usd, Some(DEFAULT_DAILY_LOSS_USD));
    assert_eq!(config.order_rate, DEFAULT_ORDER_RATE);
    assert!(config.approval_required);

    let btc = asset("BTC", 5, 40);
    let refusal = engine
        .evaluate(
            &agent,
            &intent("BTC", true, d("100"), d("0.2")),
            &btc,
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("1000")),
            NOW_MS,
        )
        .expect_err("a fresh agent's first order must be refused");
    match refusal {
        Refusal::SymbolNotAllowed { symbol, allowed } => {
            assert_eq!(symbol, "BTC");
            assert!(allowed.is_empty(), "the refusal names the empty allowlist");
        }
        other => panic!("expected the allowlist refusal, got {other}"),
    }
}

// ---- boundaries: exactly at the limit, and one unit past -----------------

#[test]
fn the_symbol_allowlist_is_exact() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let eth = asset("ETH", 2, 40);
    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .is_ok()
    );
    assert!(matches!(
        f.evaluate(
            &intent("ETH", true, d("100"), d("1")),
            &eth,
            &MarketRef::fresh("ETH", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::SymbolNotAllowed { .. })
    ));
}

#[test]
fn the_order_notional_cap_allows_the_limit_and_refuses_a_cent_past() {
    let mut config = permissive(&["BTC"]);
    config.max_order_usd = d("25");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("25"), NOW_MS);

    let at = f
        .evaluate(
            &intent("BTC", true, d("25"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect("exactly at the cap clears");
    assert_eq!(wire_px(&wire_of(&at)) * wire_sz(&wire_of(&at)), d("25"));

    let past = f
        .evaluate(
            &intent("BTC", true, d("25.01"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("a cent past the cap is refused");
    match past {
        Refusal::OrderNotional {
            observed_usd,
            limit_usd,
            ..
        } => {
            assert_eq!(observed_usd, d("25.01"));
            assert_eq!(limit_usd, d("25"));
        }
        other => panic!("expected the notional refusal, got {other}"),
    }
}

#[test]
fn the_position_cap_is_measured_after_the_fill() {
    let mut config = permissive(&["BTC"]);
    config.max_position_usd = d("100");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok(),
        "a fill landing exactly on the cap clears"
    );
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1.01")),
            &btc,
            &market,
            &exposure(d("100000")),
        ),
        Err(Refusal::PositionNotional { .. })
    ));

    // An existing position counts, and a sell against a long reduces it.
    let mut long = exposure(d("100000"));
    long.agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("1") });
    long.agent.total_position_notional_usd = d("100");
    assert!(
        matches!(
            f.evaluate(
                &intent("BTC", true, d("100"), d("0.1")),
                &btc,
                &market,
                &long
            ),
            Err(Refusal::PositionNotional { .. })
        ),
        "adding to a position already at the cap is refused"
    );
    assert!(
        f.evaluate(
            &intent("BTC", false, d("100"), d("0.5")),
            &btc,
            &market,
            &long
        )
        .is_ok(),
        "halving the same position clears"
    );
}

#[test]
fn the_leverage_cap_uses_the_tighter_of_the_operator_and_the_venue() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_leverage = 2;
    let f = Fixture::new(config);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let btc = asset("BTC", 2, 40);
    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("2")),
            &btc,
            &market,
            &exposure(d("100")),
        )
        .is_ok(),
        "$200 of notional on $100 of equity is exactly 2x"
    );
    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("2.01")),
            &btc,
            &market,
            &exposure(d("100")),
        )
        .expect_err("2.01x is past the cap")
    {
        Refusal::Leverage {
            observed, limit, ..
        } => {
            assert_eq!(observed, d("2.01"));
            assert_eq!(limit, 2);
        }
        other => panic!("expected the leverage refusal, got {other}"),
    }

    // The venue's own maximum is the harder bound of the two.
    let thin = asset("BTC", 2, 1);
    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("2")),
            &thin,
            &market,
            &exposure(d("100")),
        )
        .expect_err("the venue caps this asset at 1x")
    {
        Refusal::Leverage { limit, .. } => assert_eq!(limit, 1),
        other => panic!("expected the leverage refusal, got {other}"),
    }
}

// --- preflight: the same verdict, none of the effects (item 20) -------------

/// The property the whole design rests on. An agent that could preflight for
/// free *and* be told "clear" while paying nothing would have an unmetered
/// oracle for the guardrails; an agent charged for asking would stop asking.
/// So: every predicate runs, and the budget does not move.
#[test]
fn a_preflight_spends_no_order_rate_token() {
    let mut config = permissive(&["BTC"]);
    // Two tokens, refilling slowly enough that nothing accrues during the test.
    config.order_rate = OrderRate {
        count: 2,
        per_ms: 3_600_000,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    for _ in 0..20 {
        let verdict = f.preflight(&order, &btc, &market, &exposure(d("100000")));
        assert!(verdict.would_clear, "{:?}", verdict.refusal);
    }

    // Both real orders still clear: twenty questions cost nothing.
    f.evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect("first order");
    f.evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect("second order");
    // And the third is refused, so the cap is real rather than absent.
    let refusal = f
        .evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect_err("the third exceeds a two-token budget");
    assert!(matches!(refusal, Refusal::OrderRate { .. }), "{refusal}");
}

/// It reports the cap it did not spend. An agent told "clear" by a check that
/// skipped the rate predicate would send an order that is refused on arrival.
#[test]
fn a_preflight_still_reports_an_exhausted_order_rate() {
    let mut config = permissive(&["BTC"]);
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 3_600_000,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    f.evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect("the one token");
    let verdict = f.preflight(&order, &btc, &market, &exposure(d("100000")));
    assert!(!verdict.would_clear);
    assert!(
        matches!(verdict.refusal, Some(Refusal::OrderRate { .. })),
        "{:?}",
        verdict.refusal
    );
}

/// Item 28's queue is for orders somebody asked for. A preflight asks a
/// question, so it must not leave a human a proposal to approve.
#[test]
fn a_preflight_reports_approval_without_minting_a_proposal() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    let verdict = f.preflight(&order, &btc, &market, &exposure(d("100000")));
    match verdict.refusal {
        Some(Refusal::ApprovalRequired {
            ref approval_id, ..
        }) => {
            assert!(approval_id.is_empty(), "a preflight mints no receipt");
        }
        other => panic!("expected approval_required, got {other:?}"),
    }
    assert!(
        f.engine.pending_proposals(NOW_MS).unwrap().is_empty(),
        "a preflight left a proposal in the operator's queue"
    );
}

/// Every predicate, not a convenient subset. A preflight that answered "clear"
/// on a symbol the agent may not trade would be worse than no preflight.
#[test]
fn a_preflight_refuses_everything_a_real_evaluation_would() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let doge = asset("DOGE", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let doge_market = MarketRef::fresh("DOGE", d("100"), NOW_MS);
    let fat = exposure(d("100000"));

    // Off the allowlist.
    let off = f.preflight(
        &intent("DOGE", true, d("100"), d("1")),
        &doge,
        &doge_market,
        &fat,
    );
    assert!(matches!(
        off.refusal,
        Some(Refusal::SymbolNotAllowed { .. })
    ));

    // Below the venue minimum.
    let tiny = f.preflight(
        &intent("BTC", true, d("100"), d("0.01")),
        &btc,
        &market,
        &fat,
    );
    assert!(
        matches!(tiny.refusal, Some(Refusal::VenueRule(_))),
        "{:?}",
        tiny.refusal
    );

    // Missing reference price is fail-closed here too.
    let blind = MarketRef {
        reference_px: None,
        ..MarketRef::fresh("BTC", d("100"), NOW_MS)
    };
    let dark = f.preflight(&intent("BTC", true, d("100"), d("1")), &btc, &blind, &fat);
    assert!(!dark.would_clear, "a preflight must fail closed too");
}

/// A clearing preflight hands back the utilization, which is the number an
/// agent sizes against — and no `Cleared`, which is the capability to sign.
#[test]
fn a_clearing_preflight_reports_utilization_and_no_authority_to_sign() {
    let mut config = permissive(&["BTC"]);
    config.max_order_usd = d("1000");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let verdict = f.preflight(
        &intent("BTC", true, d("100"), d("5")),
        &btc,
        &market,
        &exposure(d("100000")),
    );
    assert!(verdict.would_clear);
    let used = verdict.utilization.expect("utilization on a clear verdict");
    // $500 of a $1000 cap.
    assert_eq!(used.order_notional_pct, Some(d("50")));
}

#[test]
fn slippage_is_measured_only_in_the_direction_that_costs_money() {
    let mut config = permissive(&["BTC"]);
    config.max_slippage_bps = d("10");
    let f = Fixture::new(config.clone());
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // 10 bps above the mid on a buy: exactly at the cap.
    let at = f
        .evaluate(
            &intent("BTC", true, d("100.1"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect("exactly at the cap clears");
    match &at.clearance().kind {
        ClearedKind::Order { slippage_bps, .. } => assert_eq!(*slippage_bps, d("10")),
        other => panic!("expected an order clearance, got {other:?}"),
    }

    match f
        .evaluate(
            &intent("BTC", true, d("100.2"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("20 bps is past a 10 bps cap")
    {
        Refusal::Slippage {
            observed_bps,
            limit_bps,
            ..
        } => {
            assert_eq!(observed_bps, d("20"));
            assert_eq!(limit_bps, d("10"));
        }
        other => panic!("expected the slippage refusal, got {other}"),
    }

    // A passive bid far below the mid is not slippage, and must not be
    // refused for being a long way from the reference.
    assert!(
        f.evaluate(
            &intent("BTC", true, d("50"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok()
    );
    // An agent may bind itself tighter than its guardrail, never looser.
    let mut tight = intent("BTC", true, d("100.1"), d("1"));
    tight.max_slippage_bps = Some(d("5"));
    assert!(matches!(
        f.evaluate(&tight, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::Slippage { limit_bps, .. }) if limit_bps == d("5")
    ));
    let mut loose = intent("BTC", true, d("100.2"), d("1"));
    loose.max_slippage_bps = Some(d("100000"));
    assert!(matches!(
        f.evaluate(&loose, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::Slippage { limit_bps, .. }) if limit_bps == d("10")
    ));
}

#[test]
fn a_protective_stop_is_priced_against_its_own_trigger_not_the_mid() {
    let mut config = permissive(&["BTC"]);
    config.max_slippage_bps = d("50");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // A stop-market sell 10% below the mid: enormous distance from the mid,
    // but the limit price sits 10 bps under its own trigger.
    let mut stop = intent("BTC", false, d("89.91"), d("1"));
    stop.kind = OrderKind::Trigger {
        is_market: true,
        trigger_px: d("90"),
        tpsl: Tpsl::Sl,
    };
    let cleared = f
        .evaluate(&stop, &btc, &market, &exposure(d("100000")))
        .expect("a protective stop is not a slippage breach");
    match &cleared.clearance().kind {
        ClearedKind::Order {
            slippage_bps,
            slippage_reference_px,
            ..
        } => {
            assert_eq!(
                *slippage_reference_px,
                d("90"),
                "the trigger is the reference"
            );
            assert_eq!(*slippage_bps, d("10"));
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }

    // The same stop with a limit far under its trigger is a real breach.
    let mut wide = stop.clone();
    wide.px = d("80");
    assert!(matches!(
        f.evaluate(&wide, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::Slippage { .. })
    ));
}

#[test]
fn reduce_only_mode_requires_the_flag_and_an_actual_reduction() {
    let mut config = permissive(&["BTC"]);
    config.reduce_only = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let mut long = exposure(d("100000"));
    long.agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("2") });
    long.agent.total_position_notional_usd = d("200");

    let reduce = |sz: &str, is_buy: bool, flagged: bool| {
        let mut i = intent("BTC", is_buy, d("100"), d(sz));
        i.reduce_only = flagged;
        i
    };

    assert!(matches!(
        f.evaluate(&reduce("1", false, false), &btc, &market, &long),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::NotFlagged,
            ..
        })
    ));
    assert!(matches!(
        f.evaluate(&reduce("1", true, true), &btc, &market, &long),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::SameSide,
            ..
        })
    ));
    assert!(matches!(
        f.evaluate(&reduce("2.01", false, true), &btc, &market, &long),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::Oversized,
            ..
        })
    ));
    assert!(matches!(
        f.evaluate(
            &reduce("1", false, true),
            &btc,
            &market,
            &exposure(d("100000"))
        ),
        Err(Refusal::ReduceOnly {
            detail: ReduceOnlyBreach::NoPosition,
            ..
        })
    ));
    // Exactly closing the position is the boundary and must clear.
    assert!(
        f.evaluate(&reduce("2", false, true), &btc, &market, &long)
            .is_ok()
    );
}

#[test]
fn the_order_rate_cap_holds_at_the_engine_and_refills_over_time() {
    let mut config = permissive(&["BTC"]);
    config.order_rate = DEFAULT_ORDER_RATE; // 5 per 5 minutes (D-c)
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    for i in 0..5 {
        assert!(
            f.evaluate(&order, &btc, &market, &exposure(d("100000")))
                .is_ok(),
            "order {i} is inside the budget"
        );
    }
    match f
        .evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect_err("the sixth order exhausts the budget")
    {
        Refusal::OrderRate {
            limit,
            window_ms,
            retry_after_ms,
            ..
        } => {
            assert_eq!(limit, 5);
            assert_eq!(window_ms, 300_000);
            assert_eq!(retry_after_ms, 60_000);
        }
        other => panic!("expected the rate refusal, got {other}"),
    }

    // One token exactly 60s later, and not a millisecond earlier.
    let later = |ms: u64| {
        let mut market = market.clone();
        market.as_of_ms = NOW_MS + ms;
        let mut exposure = exposure(d("100000"));
        exposure.agent.as_of_ms = NOW_MS + ms;
        (market, exposure)
    };
    let (m, e) = later(59_999);
    assert!(
        f.engine
            .evaluate(&f.agent, &order, &btc, &m, &e, NOW_MS + 59_999)
            .is_err()
    );
    let (m, e) = later(60_000);
    assert!(
        f.engine
            .evaluate(&f.agent, &order, &btc, &m, &e, NOW_MS + 60_000)
            .is_ok()
    );
}

#[test]
fn a_refused_order_does_not_spend_a_rate_token() {
    let mut config = permissive(&["BTC"]);
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 300_000,
    };
    config.max_order_usd = d("25");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    for _ in 0..10 {
        assert!(matches!(
            f.evaluate(
                &intent("BTC", true, d("100"), d("1")),
                &btc,
                &market,
                &exposure(d("100000")),
            ),
            Err(Refusal::OrderNotional { .. })
        ));
    }
    assert!(
        f.evaluate(
            &intent("BTC", true, d("25"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok(),
        "the single token survived ten refusals"
    );
}

// ---- item 25: the loss circuit breaker ----------------------------------

#[test]
fn the_daily_loss_breaker_trips_the_kill_switch_and_the_next_order_is_paused() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-25");
    losing.agent.peak_equity_usd = d("1000");

    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &losing,
        )
        .expect_err("the budget is exhausted")
    {
        Refusal::LossLimit {
            scope,
            kind,
            observed_usd,
            limit_usd,
        } => {
            assert_eq!(scope, KillScope::agent("alpha"));
            assert_eq!(kind, LossKind::Daily);
            assert_eq!(observed_usd, d("25"));
            assert_eq!(limit_usd, d("25"));
        }
        other => panic!("expected the loss refusal, got {other}"),
    }

    // The switch is now engaged, so a healthy snapshot does not un-stop it.
    assert!(
        f.engine
            .kill_switch()
            .is_engaged(&KillScope::agent("alpha"))
    );
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        ),
        Err(Refusal::TradingPaused { .. })
    ));
}

/// Spec F's gauge, read through the engine: both scopes, because the
/// account-wide budget of item 25 is shared and a refusal an agent has never
/// received is the only other place it would have heard about it.
///
/// The numbers here are the shape of the failure the gauge exists to prevent:
/// the agent is at 20% of its own budget and would read that as room, while
/// the fleet is one dollar from a kill that stops it too.
#[test]
fn the_gauge_shows_the_shared_budget_an_agent_would_otherwise_never_see() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("100")),
        max_drawdown_usd: None,
    };
    let f = Fixture::new(config);
    f.engine
        .operator_set_account_limits(
            LossLimits {
                max_daily_loss_usd: Some(d("400")),
                max_drawdown_usd: None,
            },
            NOW_MS,
        )
        .expect("set account limits");
    acknowledge(&f.engine);

    let mut agent_account = account(d("980"));
    agent_account.realized_pnl_today_usd = d("-20");
    let mut fleet = account(d("4601"));
    fleet.realized_pnl_today_usd = d("-399");
    let rows = f.engine.loss_budget(
        &f.agent,
        &Exposure {
            account: vault(),
            agent: agent_account,
            fleet: Some(fleet),
        },
        NOW_MS,
    );

    assert_eq!(rows.len(), 2, "one row per configured budget: {rows:?}");
    assert_eq!(rows[0].scope, BudgetScope::Agent, "own budget first");
    assert_eq!(rows[0].utilization_pct, Some(d("20")));
    assert_eq!(rows[1].scope, BudgetScope::Account);
    assert_eq!(rows[1].utilization_pct, Some(d("99.75")));
    assert_eq!(rows[1].remaining_usd, d("1"));
    assert!(rows.iter().all(|row| !row.tripped));
}

/// The gauge is a read: it takes no intent, spends no rate token and cannot
/// hand back anything that signs. Without this, "put the breaker on the read
/// path" is one refactor away from being a second way to reach the signer
/// (`AGENTS.md` invariant 1).
#[test]
fn reading_the_gauge_spends_no_rate_budget_and_mints_no_proposal() {
    let mut budgeted = permissive(&["BTC"]);
    budgeted.loss = LossLimits {
        max_daily_loss_usd: Some(d("100")),
        max_drawdown_usd: None,
    };
    let exposure = exposure(d("1000"));

    // The rate cap is 1,000 orders per second. Fifty reads first; a token
    // spent by one of them would leave fewer than 999 for the order.
    let f = Fixture::new(budgeted.clone());
    for _ in 0..50 {
        assert_eq!(f.engine.loss_budget(&f.agent, &exposure, NOW_MS).len(), 1);
    }
    let cleared = f
        .engine
        .evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure,
            NOW_MS,
        )
        .expect("an order still clears");
    assert_eq!(
        cleared.clearance().utilization.order_tokens_remaining,
        d("999")
    );

    // In approval mode an evaluation mints a proposal for an operator. Fifty
    // gauge reads mint none, which is what "this is a read" has to mean for
    // the one engine call on the read path.
    budgeted.approval_required = true;
    let gated = Fixture::new(budgeted);
    for _ in 0..50 {
        assert_eq!(
            gated
                .engine
                .loss_budget(&gated.agent, &exposure, NOW_MS)
                .len(),
            1
        );
    }
    assert!(gated.engine.pending_proposals(NOW_MS).unwrap().is_empty());
}

/// An agent nobody registered has no guardrails, so it has no budget to
/// gauge — and no way to place an order either. An empty gauge is the honest
/// answer; a zero-limit one would read as "you have used all of nothing".
#[test]
fn an_unregistered_agent_has_no_gauge() {
    let f = Fixture::new(permissive(&["BTC"]));
    let rows = f
        .engine
        .loss_budget(&AgentId::new("nobody"), &exposure(d("1000")), NOW_MS);
    assert!(rows.is_empty(), "{rows:?}");
}

/// `preflight` says where an order would sit against each cap. The breaker
/// has fired on two budgets since it was written and this block reported one,
/// so the cap about to stop the account was the one it did not mention.
#[test]
fn a_preflight_reports_the_drawdown_budget_it_used_to_omit() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("100")),
        max_drawdown_usd: Some(d("500")),
    };
    let f = Fixture::new(config);

    let mut down = account(d("600"));
    down.peak_equity_usd = d("1000");
    down.realized_pnl_today_usd = d("-40");
    let verdict = f.preflight(
        &intent("BTC", true, d("100"), d("1")),
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", d("100"), NOW_MS),
        &Exposure {
            account: vault(),
            agent: down,
            fleet: None,
        },
    );

    assert!(verdict.would_clear);
    let used = verdict.utilization.expect("utilization on a clear verdict");
    assert_eq!(used.daily_loss_pct, Some(d("40")));
    // $400 of drawdown against a $500 budget — 80%, and previously silent.
    assert_eq!(used.drawdown_pct, Some(d("80")));
}

/// Spec F's `effective_cap = risk_budget / (2 * sigma_day)`, hand-computed:
/// $5 of risk at 2% daily volatility is `5 / 0.04` = $125 of position. The
/// point of the whole feature is the second half — the *same* budget on a
/// coin that moves four times as much buys a quarter of the size, with no
/// second configuration anywhere.
#[test]
fn the_same_risk_budget_buys_less_size_on_a_more_volatile_market() {
    let sized = |sigma: &str, sz: &str| {
        let mut config = permissive(&["BTC"]);
        config.risk.max_risk_usd = Some(d("5"));
        let f = Fixture::new(config);
        let market = MarketRef {
            sigma_day: Some(d(sigma)),
            ..MarketRef::fresh("BTC", d("100"), NOW_MS)
        };
        f.engine.evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d(sz)),
            &asset("BTC", 2, 40),
            &market,
            &exposure(d("100000")),
            NOW_MS,
        )
    };

    // 2% a day: $125 of position, so 1.25 units at $100 clears and 1.26 does
    // not. The cap is a ceiling a value may sit exactly on, like every other
    // size cap here.
    assert!(sized("0.02", "1.25").is_ok());
    assert!(matches!(
        sized("0.02", "1.26"),
        Err(Refusal::VolScaledPositionNotional { .. })
    ));

    // 8% a day, same budget: a quarter of the size, $31.25.
    assert!(sized("0.08", "0.31").is_ok());
    assert!(matches!(
        sized("0.08", "0.32"),
        Err(Refusal::VolScaledPositionNotional { .. })
    ));
}

/// The correction the day's sigma cannot make for itself.
///
/// `sigma_day` is measured over twenty-four hourly bars, so an hour of
/// tripled volatility moves it by one bar in twenty-four — the cap it derives
/// stays wide for most of a day after the market changed. `vol_ratio` is the
/// same measurement taken over the last hour, and multiplying by it is what
/// makes the cap notice.
///
/// Hand-computed: $5 of risk at 2% daily volatility is $125 of position, and
/// the same budget when the last hour is moving like three normal hours is
/// `5 / (2 * 0.02 * 3)` = $41.66…, so 0.41 units at $100 clears and 0.42 does
/// not.
#[test]
fn an_hour_moving_three_times_normal_buys_a_third_of_the_size() {
    let sized = |ratio: Option<Decimal>, sz: &str| {
        let mut config = permissive(&["BTC"]);
        config.risk.max_risk_usd = Some(d("5"));
        let f = Fixture::new(config);
        let market = MarketRef {
            sigma_day: Some(d("0.02")),
            vol_ratio: ratio,
            ..MarketRef::fresh("BTC", d("100"), NOW_MS)
        };
        f.engine.evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d(sz)),
            &asset("BTC", 2, 40),
            &market,
            &exposure(d("100000")),
            NOW_MS,
        )
    };

    // The day alone buys $125 — the size #31 cleared and this still does.
    assert!(sized(None, "1.25").is_ok());

    // The same day, with the last hour moving three times normal: $41.66.
    assert!(sized(Some(d("3")), "0.41").is_ok());
    assert!(matches!(
        sized(Some(d("3")), "0.42"),
        Err(Refusal::VolScaledPositionNotional { .. })
    ));

    // And the size the day alone allowed is now refused, which is the whole
    // point: nothing about the configuration changed, only the market.
    assert!(matches!(
        sized(Some(d("3")), "1.25"),
        Err(Refusal::VolScaledPositionNotional { .. })
    ));
}

/// **The clamp, which is the safety half of the correction.**
///
/// A ratio below one says the last hour was quieter than the day. Honouring
/// it would *widen* a guardrail on the strength of a sixty-bar sample — a
/// cap loosened by a statistic far noisier than the one it is correcting. An
/// unmeasured ratio lands on the same floor, and for the sharper reason: it
/// must leave the cap exactly where it was before this correction existed,
/// because a fetch that failed is not evidence that the market is calm.
#[test]
fn a_quiet_hour_and_an_unmeasured_one_both_leave_the_cap_where_the_day_puts_it() {
    let sized = |ratio: Option<Decimal>, sz: &str| {
        let mut config = permissive(&["BTC"]);
        config.risk.max_risk_usd = Some(d("5"));
        let f = Fixture::new(config);
        let market = MarketRef {
            sigma_day: Some(d("0.02")),
            vol_ratio: ratio,
            ..MarketRef::fresh("BTC", d("100"), NOW_MS)
        };
        f.engine.evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d(sz)),
            &asset("BTC", 2, 40),
            &market,
            &exposure(d("100000")),
            NOW_MS,
        )
    };

    // A tenth of a normal hour does not buy ten times the size: the cap is
    // still the day's $125, so 1.25 clears and 1.26 does not.
    for ratio in [Some(d("0.1")), Some(d("0.9")), Some(Decimal::ONE), None] {
        assert!(
            sized(ratio, "1.25").is_ok(),
            "ratio {ratio:?} should still buy the day's size"
        );
        assert!(
            matches!(
                sized(ratio, "1.26"),
                Err(Refusal::VolScaledPositionNotional { .. })
            ),
            "ratio {ratio:?} should not buy more than the day's size"
        );
    }
}

/// The refusal reports the measured volatility and the scale **separately**.
///
/// Folding them into one number would tell an agent its cap is $41 on a coin
/// with 6% daily volatility, which is false — the coin moves 2% a day and
/// happens to be moving now. The two facts call for different responses: one
/// is the asset and will not change, the other will pass within the hour.
#[test]
fn the_refusal_separates_the_days_volatility_from_the_hours_scale() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_risk_usd = Some(d("5"));
    let f = Fixture::new(config);
    let market = MarketRef {
        sigma_day: Some(d("0.02")),
        vol_ratio: Some(d("3")),
        ..MarketRef::fresh("BTC", d("100"), NOW_MS)
    };

    let verdict = f.engine.evaluate(
        &f.agent,
        &intent("BTC", true, d("100"), d("1.25")),
        &asset("BTC", 2, 40),
        &market,
        &exposure(d("100000")),
        NOW_MS,
    );

    match verdict {
        Err(Refusal::VolScaledPositionNotional {
            sigma_day_pct,
            vol_scale,
            effective_cap_usd,
            risk_budget_usd,
            ..
        }) => {
            assert_eq!(sigma_day_pct, d("2"), "the day's measured volatility");
            assert_eq!(vol_scale, d("3"), "what the hour multiplied it by");
            assert_eq!(risk_budget_usd, d("5"));
            // $5 / (2 * 0.02 * 3), to the precision the division carries.
            assert!(
                effective_cap_usd > d("41.66") && effective_cap_usd < d("41.67"),
                "cap was {effective_cap_usd}"
            );
        }
        other => panic!("expected a vol-scaled refusal, got {other:?}"),
    }
}

/// **The fail-closed branch.** A configured cap that cannot be computed is a
/// cap that is not enforced, which is the failure this module exists to
/// prevent. A missing sigma refuses; so does a zero one, which is not "an
/// asset that cannot move" but a measurement that failed — and which would
/// otherwise divide to an infinite cap.
#[test]
fn a_vol_scaled_cap_without_a_volatility_refuses_rather_than_sizing_blind() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_risk_usd = Some(d("5"));
    let f = Fixture::new(config);

    for sigma in [None, Some(Decimal::ZERO), Some(-d("0.02"))] {
        let market = MarketRef {
            sigma_day: sigma,
            ..MarketRef::fresh("BTC", d("100"), NOW_MS)
        };
        // Comfortably inside every cap at any sane volatility, and above the
        // venue's own minimum notional so nothing refuses it first.
        let verdict = f.engine.evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d("0.2")),
            &asset("BTC", 2, 40),
            &market,
            &exposure(d("100000")),
            NOW_MS,
        );
        assert!(
            matches!(
                verdict,
                Err(Refusal::Unevaluable(Unevaluable::MissingVolatility { .. }))
            ),
            "sigma {sigma:?} should refuse, got {verdict:?}"
        );
    }
}

/// An agent that configured no vol-scaled cap is unaffected, and in
/// particular is **not** refused for a missing volatility. Spec F calls this
/// an option; an option that fails closed when unset is not optional.
#[test]
fn no_vol_scaled_cap_means_no_volatility_is_needed() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .engine
        .evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d("50")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
        .expect("no vol-scaled cap, no volatility needed");
    assert_eq!(
        cleared.clearance().utilization.vol_scaled_position_pct,
        None
    );
}

/// The two caps compose as a minimum and the refusal names which one bound.
/// An agent told only "too big" cannot tell whether to ask for a larger
/// `max_position_usd` or a larger `max_risk_usd`, and the two are different
/// conversations with the operator.
#[test]
fn the_fixed_cap_still_binds_underneath_the_vol_scaled_one() {
    let mut config = permissive(&["BTC"]);
    // A generous risk budget: $50 at 2% would allow $1,250 of position.
    config.risk.max_risk_usd = Some(d("50"));
    config.max_position_usd = d("200");
    let f = Fixture::new(config);
    let market = MarketRef {
        sigma_day: Some(d("0.02")),
        ..MarketRef::fresh("BTC", d("100"), NOW_MS)
    };

    // $300 of position: inside the vol-scaled cap, past the fixed one.
    let refusal = f
        .engine
        .evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d("3")),
            &asset("BTC", 2, 40),
            &market,
            &exposure(d("100000")),
            NOW_MS,
        )
        .expect_err("the fixed cap binds");
    assert!(
        matches!(refusal, Refusal::PositionNotional { limit_usd, .. } if limit_usd == d("200")),
        "the tighter cap must name itself: {refusal:?}"
    );
}

/// **The cap moves, so trimming into it must not be refused.** Volatility
/// doubles and the cap halves: a position that was inside it this morning is
/// over it by noon with the agent having done nothing. If the check ignored
/// direction, the way out — selling some of it — would be refused for leaving
/// the position still over the cap, and only an all-at-once exit would clear.
/// Refusing a risk-reducing order can only raise risk.
///
/// This is the one place the vol-scaled cap behaves differently from the
/// fixed one, and the difference is earned: `max_position_usd` does not move
/// on its own, so an agent cannot be put over it by the market alone.
#[test]
fn a_position_the_market_pushed_over_the_cap_can_still_be_trimmed() {
    let mut config = permissive(&["BTC"]);
    // $5 of risk at 8% daily volatility: a $31.25 cap.
    config.risk.max_risk_usd = Some(d("5"));
    let f = Fixture::new(config);
    let market = MarketRef {
        sigma_day: Some(d("0.08")),
        ..MarketRef::fresh("BTC", d("100"), NOW_MS)
    };

    // Already holding $200 of it — four times over what the cap now allows,
    // which is what a volatility repricing looks like from the inside.
    let mut over = exposure(d("100000"));
    over.agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("2") });

    let order = |is_buy: bool, sz: &str| {
        f.engine.evaluate(
            &f.agent,
            &intent("BTC", is_buy, d("100"), d(sz)),
            &asset("BTC", 2, 40),
            &market,
            &over,
            NOW_MS,
        )
    };

    // Selling 1 leaves $100 — still far over the $31.25 cap, and allowed,
    // because it is less exposure than before.
    assert!(
        order(false, "1").is_ok(),
        "trimming an over-cap position must clear"
    );
    // Buying more is refused: that is the direction the cap is for.
    assert!(matches!(
        order(true, "0.2"),
        Err(Refusal::VolScaledPositionNotional { .. })
    ));
    // And flipping through to a larger short is not a reduction, however it
    // is dressed: $300 of short exposure is more risk, not less.
    assert!(matches!(
        order(false, "5"),
        Err(Refusal::VolScaledPositionNotional { .. })
    ));
}

/// The utilization block reports the vol-scaled cap too — L3's lesson, that
/// a block which omits a cap the engine fires on is how an agent walks into
/// a refusal it could have seen coming.
#[test]
fn a_preflight_reports_where_the_order_sits_against_the_vol_scaled_cap() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_risk_usd = Some(d("5"));
    let f = Fixture::new(config);
    let market = MarketRef {
        sigma_day: Some(d("0.02")),
        ..MarketRef::fresh("BTC", d("100"), NOW_MS)
    };

    // $100 of a $125 cap.
    let verdict = f.preflight(
        &intent("BTC", true, d("100"), d("1")),
        &asset("BTC", 2, 40),
        &market,
        &exposure(d("100000")),
    );
    assert!(verdict.would_clear);
    let used = verdict.utilization.expect("utilization on a clear verdict");
    assert_eq!(used.vol_scaled_position_pct, Some(d("80")));
}

/// A zero budget derives a zero cap, which refuses every order including a
/// risk-reducing one — so it is rejected where an operator types it rather
/// than at the moment an order needs a verdict, the same treatment every
/// other unevaluable setting gets.
#[test]
fn a_zero_risk_budget_is_refused_as_configuration_not_as_a_cap() {
    let f = Fixture::new(permissive(&["BTC"]));
    let mut config = permissive(&["BTC"]);
    config.risk.max_risk_usd = Some(Decimal::ZERO);
    assert!(matches!(
        f.engine
            .operator_set_guardrails(&f.agent, config, NOW_MS),
        Err(GuardrailError::InvalidConfig { field, .. }) if field == "risk.max_risk_usd"
    ));
}

#[test]
fn an_account_wide_breach_stops_every_agent() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_set_account_limits(
            LossLimits {
                max_daily_loss_usd: Some(d("400")),
                max_drawdown_usd: None,
            },
            NOW_MS,
        )
        .expect("set account limits");
    acknowledge(&f.engine);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    let mut fleet = account(d("4600"));
    fleet.realized_pnl_today_usd = d("-400");
    fleet.peak_equity_usd = d("5000");
    let with_fleet = Exposure {
        account: vault(),
        agent: account(d("1000")),
        fleet: Some(fleet),
    };

    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &with_fleet
        ),
        Err(Refusal::LossLimit {
            scope: KillScope::Global,
            ..
        })
    ));
    assert!(f.engine.kill_switch().is_engaged(&KillScope::Global));
}

/// Item 24's position cap is a cap on exposure, and a working order is
/// exposure. Five orders each landing exactly on the cap used to clear
/// one after another, because each was measured against a flat book.
#[test]
fn the_position_cap_counts_working_orders_so_it_cannot_be_split() {
    let mut config = permissive(&["BTC"]);
    config.max_position_usd = d("100");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    // The first order fills the cap on its own.
    let flat = exposure(d("100000"));
    assert!(f.evaluate(&order, &btc, &market, &flat).is_ok());

    // Once it is resting, every further one is refused — including the four
    // that the old engine admitted, which together held 5x the cap.
    let mut book = flat.clone();
    for n in 1..5 {
        book = with_resting(book, "BTC", Decimal::from(n), d("100"));
        match f
            .evaluate(&order, &btc, &market, &book)
            .expect_err("order {n} is past the cap once the book is counted")
        {
            Refusal::PositionNotional {
                observed_usd,
                resting_usd,
                limit_usd,
                ..
            } => {
                assert_eq!(observed_usd, Decimal::from(n + 1) * d("100"));
                assert_eq!(resting_usd, Decimal::from(n) * d("100"));
                assert_eq!(limit_usd, d("100"));
            }
            other => panic!("expected the position refusal, got {other}"),
        }
    }
}

/// Working orders on other symbols still consume the account's leverage.
#[test]
fn the_leverage_cap_counts_working_orders_on_other_symbols() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_leverage = 2;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));

    // $100 of new BTC exposure on $100 of equity is 1x on its own.
    assert!(
        f.evaluate(&order, &btc, &market, &exposure(d("100")))
            .is_ok()
    );

    // With $150 of ETH already working, the same order is 2.5x.
    let mut with_eth = exposure(d("100"));
    with_eth.agent.resting = Some(RestingExposure {
        buys: BTreeMap::from([("ETH".to_owned(), d("1.5"))]),
        sells: BTreeMap::new(),
        notional_usd: d("150"),
        notional_by_symbol: BTreeMap::from([("ETH".into(), d("150"))]),
        ..RestingExposure::default()
    });
    assert!(matches!(
        f.evaluate(&order, &btc, &market, &with_eth),
        Err(Refusal::Leverage { observed, .. }) if observed == d("2.5")
    ));
}

// ---- item 26: the kill switch -------------------------------------------

#[test]
fn leverage_replaces_the_exact_limit_notional_from_production_state() {
    use crate::state::{AccountState, Balances, Freshness as StateFreshness, OrderView};
    let order = |is_buy, limit_px| OrderView {
        symbol: "BTC".into(),
        oid: 1,
        cloid: None,
        is_buy,
        limit_px,
        reference_px: Some(limit_px),
        size: Decimal::ONE,
        original_size: Decimal::ONE,
        reduce_only: false,
        is_trigger: false,
        trigger_px: None,
        placed_ts_ms: NOW_MS,
    };
    let state = AccountState {
        contract_version: 1,
        network: "testnet",
        address: vault(),
        as_of_ms: NOW_MS,
        feed_age_ms: Some(0),
        feed: StateFreshness::Live,
        balances: Balances {
            equity_usd: d("110"),
            perps_account_value_usd: d("110"),
            spot_usdc_available: Decimal::ZERO,
            total_margin_used_usd: Decimal::ZERO,
            withdrawable_usd: d("110"),
        },
        margin_runway_h: None,
        loss_budget: vec![],
        positions: vec![],
        orders: vec![order(true, d("50")), order(false, d("110"))],
    };
    let mut exposure = crate::state::exposure_from(&state, Decimal::ZERO, None, true, MIDNIGHT_MS);
    let resting = exposure.agent.resting.as_ref().expect("working book");
    assert_eq!(resting.notional_usd, d("160"));
    assert_eq!(resting.notional_by_symbol["BTC"], d("160"));
    let mut config = permissive(&["BTC"]);
    config.risk.max_leverage = 1;
    let f = Fixture::new(config);
    let candidate = intent("BTC", true, d("100"), d("0.5"));
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    assert!(matches!(f.evaluate(&candidate, &btc, &market, &exposure),
        Err(Refusal::Leverage { position_notional_usd, .. }) if position_notional_usd == d("150")));

    // Missing attribution cannot earn subtraction credit via a mark fallback.
    exposure
        .agent
        .resting
        .as_mut()
        .expect("book")
        .notional_by_symbol
        .clear();
    assert!(matches!(f.evaluate(&candidate, &btc, &market, &exposure),
        Err(Refusal::Leverage { position_notional_usd, .. }) if position_notional_usd == d("310")));
}

#[test]
fn reductions_before_opening_fills_cannot_hide_a_position_flip() {
    for long in [true, false] {
        for candidate_is_reduction in [true, false] {
            for leverage_only in [false, true] {
                let mut config = permissive(&["BTC"]);
                config.max_position_usd = if leverage_only { d("1000") } else { d("100") };
                config.risk.max_leverage = 1;
                let f = Fixture::new(config);
                let sign = if long { Decimal::ONE } else { -Decimal::ONE };
                let opening = if candidate_is_reduction {
                    d("1.5")
                } else {
                    Decimal::ONE
                };
                let mut state = with_resting(exposure(d("100")), "BTC", -sign * opening, d("100"));
                state
                    .agent
                    .positions
                    .insert("BTC".into(), PositionSnapshot { szi: sign });
                state.agent.total_position_notional_usd = d("100");
                if !candidate_is_reduction {
                    let book = state.agent.resting.as_mut().expect("book");
                    let side = if long {
                        &mut book.reduce_sells
                    } else {
                        &mut book.reduce_buys
                    };
                    side.insert("BTC".into(), Decimal::ONE);
                }
                let mut candidate = intent(
                    "BTC",
                    !long,
                    d("100"),
                    if candidate_is_reduction {
                        Decimal::ONE
                    } else {
                        d("0.5")
                    },
                );
                candidate.reduce_only = candidate_is_reduction;
                let refusal = f
                    .evaluate(
                        &candidate,
                        &asset("BTC", 2, 40),
                        &MarketRef::fresh("BTC", d("100"), NOW_MS),
                        &state,
                    )
                    .expect_err("flip exceeds cap");
                if leverage_only {
                    assert!(
                        matches!(refusal, Refusal::Leverage { position_notional_usd, .. }
                        if position_notional_usd == d("150"))
                    );
                } else {
                    assert!(
                        matches!(refusal, Refusal::PositionNotional { observed_usd, .. }
                        if observed_usd == d("150"))
                    );
                }
            }
        }
    }
}

#[test]
fn genuine_reduce_only_orders_can_unwind_above_fixed_and_leverage_caps() {
    for long in [true, false] {
        let mut config = permissive(&["BTC"]);
        config.max_position_usd = d("50");
        config.risk.max_leverage = 1;
        let f = Fixture::new(config);
        let mut state = exposure(d("50"));
        state.agent.positions.insert(
            "BTC".into(),
            PositionSnapshot {
                szi: if long { Decimal::ONE } else { -Decimal::ONE },
            },
        );
        state.agent.total_position_notional_usd = d("100");
        for size in [d("0.5"), Decimal::ONE] {
            let mut candidate = intent("BTC", !long, d("100"), size);
            candidate.reduce_only = true;
            let cleared = f
                .evaluate(
                    &candidate,
                    &asset("BTC", 2, 40),
                    &MarketRef::fresh("BTC", d("100"), NOW_MS),
                    &state,
                )
                .expect("safe unwind");
            assert_eq!(cleared.clearance().utilization.leverage, d("2"));
            candidate.reduce_only = false;
            assert!(matches!(
                f.evaluate(
                    &candidate,
                    &asset("BTC", 2, 40),
                    &MarketRef::fresh("BTC", d("100"), NOW_MS),
                    &state
                ),
                Err(Refusal::PositionNotional { .. })
            ));
            candidate.reduce_only = true;
            candidate.is_buy = long;
            assert!(matches!(
                f.evaluate(
                    &candidate,
                    &asset("BTC", 2, 40),
                    &MarketRef::fresh("BTC", d("100"), NOW_MS),
                    &state
                ),
                Err(Refusal::PositionNotional { .. })
            ));
        }
    }
}

/// ES13 / D3: all symbols and both working sides consume the account cap.
/// Prices deliberately differ from the reference; sizes round to 0.20.
#[test]
fn open_exposure_counts_gross_cross_symbol_orders_at_rounded_reference_value() {
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let mut cases = 0;
    for btc_long in [false, true] {
        for eth_long in [false, true] {
            let mut state = exposure(d("1000"));
            state.agent.positions.insert(
                "BTC".into(),
                PositionSnapshot {
                    szi: if btc_long { d("1") } else { d("-1") },
                },
            );
            state.agent.positions.insert(
                "ETH".into(),
                PositionSnapshot {
                    szi: if eth_long { d("2") } else { d("-2") },
                },
            );
            state.agent.total_position_notional_usd = d("400");
            let book = state.agent.resting.as_mut().unwrap();
            book.buys.insert("BTC".into(), d("0.2"));
            book.sells.insert("BTC".into(), d("0.3"));
            book.buys.insert("ETH".into(), d("1"));
            book.notional_by_symbol.insert("BTC".into(), d("50"));
            book.notional_by_symbol.insert("ETH".into(), d("150"));
            book.notional_usd = d("200");
            for is_buy in [false, true] {
                for px in [d("50"), d("150")] {
                    for cap in [None, Some(d("619.99")), Some(d("620")), Some(d("620.01"))] {
                        cases += 1;
                        let mut config = permissive(&["BTC", "ETH"]);
                        config.risk.max_open_exposure_usd = cap;
                        let f = Fixture::without_keys(config.clone());
                        let candidate = intent("BTC", is_buy, px, d("0.204"));
                        let result = f.evaluate(&candidate, &btc, &market, &state);
                        if cap == Some(d("619.99")) {
                            assert!(
                                matches!(result, Err(Refusal::OpenExposure { observed_usd, limit_usd })
                                if observed_usd == d("620") && limit_usd == d("619.99"))
                            );
                        } else {
                            let cleared =
                                result.expect("equality or an unset cap permits the order");
                            assert_eq!(wire_sz(&wire_of(&cleared)), d("0.2"));
                            verify_every_predicate(
                                cases, &cleared, &config, &candidate, &btc, &market, &state, NOW_MS,
                            );
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 64);
}

#[test]
fn open_exposure_zero_cap_exempts_only_genuine_reductions() {
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let mut config = permissive(&["BTC"]);
    config.risk.max_open_exposure_usd = Some(Decimal::ZERO);
    let f = Fixture::without_keys(config.clone());
    for long in [false, true] {
        let mut state = exposure(d("1000"));
        state.agent.positions.insert(
            "BTC".into(),
            PositionSnapshot {
                szi: if long { d("1") } else { d("-1") },
            },
        );
        state.agent.total_position_notional_usd = d("100");
        for is_buy in [false, true] {
            for reduce_only in [false, true] {
                for size in [d("0.5"), d("1"), d("2")] {
                    let mut candidate = intent("BTC", is_buy, d("100"), size);
                    candidate.reduce_only = reduce_only;
                    let result = f.evaluate(&candidate, &btc, &market, &state);
                    if reduce_only && is_buy != long {
                        let cleared = result.expect("clipped RO unwind above a zero cap");
                        verify_every_predicate(
                            0, &cleared, &config, &candidate, &btc, &market, &state, NOW_MS,
                        );
                    } else {
                        let expected = d("100")
                            + if reduce_only {
                                Decimal::ZERO
                            } else {
                                size * d("100")
                            };
                        assert!(
                            matches!(result, Err(Refusal::OpenExposure { observed_usd, limit_usd })
                            if observed_usd == expected && limit_usd.is_zero()),
                            "long={long}, candidate={candidate:?}"
                        );
                    }
                }
            }
        }
    }
    let empty = exposure(d("1000"));
    let opening = intent("BTC", true, d("100"), d("1"));
    assert!(matches!(f.evaluate(&opening, &btc, &market, &empty),
        Err(Refusal::OpenExposure { observed_usd, limit_usd }) if observed_usd == d("100") && limit_usd.is_zero()));
    let mut no_position_ro = opening;
    no_position_ro.reduce_only = true;
    assert!(
        f.evaluate(&no_position_ro, &btc, &market, &empty).is_ok(),
        "RO adds no opening commitment even when the venue would have nothing to reduce"
    );
}

#[test]
fn open_exposure_ro_that_removes_an_offset_is_not_exempt_and_adds_no_notional() {
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    for long in [false, true] {
        let mut state = exposure(d("1000"));
        state.agent.positions.insert(
            "BTC".into(),
            PositionSnapshot {
                szi: if long { d("1") } else { d("-1") },
            },
        );
        state.agent.total_position_notional_usd = d("100");
        state = with_resting(state, "BTC", if long { d("-2") } else { d("2") }, d("100"));
        let mut candidate = intent("BTC", !long, d("100"), d("0.5"));
        candidate.reduce_only = true;
        for cap in [d("299.99"), d("300")] {
            let mut config = permissive(&["BTC"]);
            config.risk.max_open_exposure_usd = Some(cap);
            let f = Fixture::without_keys(config.clone());
            match f.evaluate(&candidate, &btc, &market, &state) {
                Err(Refusal::OpenExposure {
                    observed_usd,
                    limit_usd,
                }) => {
                    assert_eq!(observed_usd, d("300"));
                    assert_eq!(limit_usd, d("299.99"));
                    assert_eq!(cap, d("299.99"));
                }
                Ok(cleared) => {
                    assert_eq!(cap, d("300"));
                    verify_every_predicate(
                        0, &cleared, &config, &candidate, &btc, &market, &state, NOW_MS,
                    );
                }
                other => panic!("unexpected offset-removal result: {other:?}"),
            }
        }
    }
}

#[test]
fn open_exposure_fails_closed_on_missing_book_and_checked_arithmetic() {
    let mut config = permissive(&["BTC"]);
    config.risk.max_open_exposure_usd = Some(Decimal::MAX);
    let f = Fixture::without_keys(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let candidate = intent("BTC", true, d("100"), d("1"));
    let mut state = exposure(d("1000"));
    state
        .agent
        .positions
        .insert("BTC".into(), PositionSnapshot { szi: d("-1") });
    state.agent.total_position_notional_usd = d("100");
    state.agent.resting = None;
    for reduce_only in [false, true] {
        let mut candidate = candidate.clone();
        candidate.reduce_only = reduce_only;
        assert!(matches!(
            f.evaluate(&candidate, &btc, &market, &state),
            Err(Refusal::Unevaluable(Unevaluable::MissingRestingOrders))
        ));
    }
    for overflow in ["current_sum", "candidate_sum", "candidate_mark"] {
        let mut state = exposure(d("1000"));
        let mut candidate = candidate.clone();
        let mut market = market.clone();
        match overflow {
            "current_sum" => {
                state.agent.total_position_notional_usd = Decimal::MAX;
                state = with_resting(state, "ETH", d("1"), d("1"));
            }
            "candidate_sum" => state.agent.total_position_notional_usd = Decimal::MAX,
            "candidate_mark" => {
                market.reference_px = Some(Decimal::MAX);
                candidate.sz = d("2");
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                f.evaluate(&candidate, &btc, &market, &state),
                Err(Refusal::Unevaluable(Unevaluable::ArithmeticOverflow { .. }))
            ),
            "{overflow}"
        );
    }
}

#[test]
fn open_exposure_config_defaults_validation_and_sqlite_round_trip() {
    assert_eq!(RiskSettings::default().max_open_exposure_usd, None);
    let mut config = permissive(&["BTC"]);
    let mut legacy = serde_json::to_value(&config).unwrap();
    legacy["risk"]
        .as_object_mut()
        .unwrap()
        .remove("max_open_exposure_usd");
    let decoded: AgentGuardrails = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(decoded, config);
    config.risk.max_open_exposure_usd = Some(d("-0.01"));
    assert_eq!(
        config.validate().unwrap_err().0,
        "risk.max_open_exposure_usd"
    );
    config.risk.max_open_exposure_usd = Some(Decimal::ZERO);
    assert!(config.validate().is_ok());
    let f = Fixture::without_keys(config.clone());
    let mut invalid = config.clone();
    invalid.risk.max_open_exposure_usd = Some(d("-1"));
    assert!(
        f.engine
            .operator_set_guardrails(&f.agent, invalid, NOW_MS)
            .is_err()
    );
    assert_eq!(f.engine.guardrails(&f.agent), Some(config.clone()));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.db");
    let policy = open_test_policy(&path, true);
    let current = policy.current().unwrap();
    let mut next = current.state;
    next.guardrails.insert(AgentId::new("zero"), config.clone());
    config.risk.max_open_exposure_usd = Some(d("125.50"));
    next.guardrails
        .insert(AgentId::new("bounded"), config.clone());
    // Missing legacy fields are decoded above for explicit operator review,
    // never read as execution authority. The replacement is complete.
    next.guardrails
        .insert(AgentId::new("legacy"), decoded.clone());
    next.kill.release(&KillScope::Global);
    policy.replace(current.revision, next, NOW_MS).unwrap();
    drop(policy);
    let store = SqliteGuardrailStore::new(open_test_policy(&path, false));
    let loaded = store.load().unwrap().state;
    assert_eq!(loaded.guardrails[&AgentId::new("bounded")], config);
    assert_eq!(
        loaded.guardrails[&AgentId::new("zero")]
            .risk
            .max_open_exposure_usd,
        Some(Decimal::ZERO)
    );
    assert_eq!(loaded.guardrails[&AgentId::new("legacy")], decoded);
    let engine = GuardrailEngine::from_parts(
        Arc::new(store),
        Arc::new(NullAuditSink::new(["bounded", "zero", "legacy"].map(
            |agent| route_for(key_store(&[agent]).as_ref(), agent, vault(), None),
        ))),
        Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet)),
        Network::Testnet,
    )
    .unwrap();
    acknowledge(&engine);
    let candidate = intent("BTC", true, d("100"), d("2"));
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let state = exposure(d("1000"));
    assert!(
        matches!(engine.evaluate(&AgentId::new("bounded"), &candidate, &btc, &market, &state, NOW_MS),
        Err(Refusal::OpenExposure { observed_usd, limit_usd }) if observed_usd == d("200") && limit_usd == d("125.50"))
    );
    assert!(
        engine
            .evaluate(
                &AgentId::new("legacy"),
                &candidate,
                &btc,
                &market,
                &state,
                NOW_MS
            )
            .is_ok()
    );
}

#[test]
fn position_extrema_match_clipped_fill_permutations() {
    let mut config = permissive(&["BTC"]);
    config.max_position_usd = d("1000");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let mut cases = 0;
    let mut growing_reductions = 0;
    let mut safe_reductions = 0;
    // Three initial positions and all four side aggregates at sizes 0, 1, 2.
    for encoded in 0..243 {
        let mut digits = encoded;
        let mut values = [Decimal::ZERO; 5];
        for value in &mut values {
            *value = Decimal::from(digits % 3);
            digits /= 3;
        }
        let [p, buys, sells, rb, rs] = values;
        let p = p - Decimal::ONE;
        let mut state = exposure(d("10000"));
        state
            .agent
            .positions
            .insert("BTC".into(), PositionSnapshot { szi: p });
        state.agent.total_position_notional_usd = p.abs() * d("100");
        state.agent.resting = Some(RestingExposure {
            buys: BTreeMap::from([("BTC".into(), buys)]),
            sells: BTreeMap::from([("BTC".into(), sells)]),
            reduce_buys: BTreeMap::from([("BTC".into(), rb)]),
            reduce_sells: BTreeMap::from([("BTC".into(), rs)]),
            notional_usd: buys * d("50") + sells * d("110"),
            notional_by_symbol: BTreeMap::from([("BTC".into(), buys * d("50") + sells * d("110"))]),
        });
        for is_buy in [true, false] {
            for reduce_only in [true, false] {
                for sz in [d("0.5"), d("1.5")] {
                    let mut candidate = intent("BTC", is_buy, d("100"), sz);
                    candidate.reduce_only = reduce_only;
                    let signed = if is_buy { sz } else { -sz };
                    let fills = [
                        (buys, false),
                        (-sells, false),
                        (rb, true),
                        (-rs, true),
                        (signed, reduce_only),
                    ];
                    let before = reachable_worst(p, &fills[..4]);
                    let after = reachable_worst(p, &fills);
                    let verdict = f.preflight(&candidate, &btc, &market, &state);
                    assert!(verdict.would_clear, "case {encoded}: {:?}", verdict.refusal);
                    let used = verdict.utilization.expect("cleared utilization");
                    assert_eq!(
                        used.position_notional_pct,
                        Some(after * d("10")),
                        "case {encoded}: {fills:?}"
                    );
                    assert_eq!(used.leverage, after / d("100"));
                    cases += 1;
                    if reduce_only && p * signed < Decimal::ZERO {
                        if after > before {
                            growing_reductions += 1;
                        } else {
                            safe_reductions += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 1944);
    assert!(growing_reductions > 0 && safe_reductions > 0);
}

#[test]
fn opposite_resting_orders_never_create_position_headroom() {
    for is_buy in [true, false] {
        let mut config = permissive(&["BTC"]);
        config.max_position_usd = d("100");
        let f = Fixture::new(config);
        let mut state = exposure(d("10000"));
        state.agent.resting = Some(RestingExposure {
            buys: BTreeMap::from([("BTC".into(), d("1"))]),
            sells: BTreeMap::from([("BTC".into(), d("1"))]),
            notional_usd: d("200"),
            notional_by_symbol: BTreeMap::from([("BTC".into(), d("200"))]),
            ..RestingExposure::default()
        });
        assert!(matches!(f.evaluate(
            &intent("BTC", is_buy, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &state,
        ), Err(Refusal::PositionNotional { observed_usd, .. }) if observed_usd == d("200")));
    }
}

#[test]
fn a_failed_kill_release_keeps_the_live_engine_paused() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(FailingStore::default()),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    assert!(
        f.engine
            .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
            .is_err()
    );
    assert!(
        f.engine
            .operator_release_kill(&KillScope::Global, NOW_MS)
            .is_err()
    );
    assert!(f.engine.kill_switch().is_engaged(&KillScope::Global));
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("10000")),
        ),
        Err(Refusal::TradingPaused { .. })
    ));
    let effects = f.engine.take_pending_kill_effects();
    assert_eq!(effects.len(), 1);
    assert!(effects[0].cancel_for.contains(&f.agent));
}

#[test]
fn changing_policy_invalidates_an_order_already_cleared() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("10000")),
        )
        .expect("initial policy permits order");
    f.engine
        .operator_set_guardrails(&f.agent, permissive(&[]), NOW_MS)
        .expect("tighten policy");
    acknowledge(&f.engine);
    assert!(matches!(
        f.engine.sign_cleared(cleared, 1, None, || NOW_MS),
        Err(SignClearedError::Refused(Refusal::Unevaluable(
            Unevaluable::PolicyChanged
        )))
    ));
}

#[test]
fn the_kill_switch_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("testnet.db");
    let agent = AgentId::new("alpha");

    {
        let store: Arc<dyn GuardrailStore> =
            Arc::new(SqliteGuardrailStore::new(open_test_policy(&path, true)));
        let engine = GuardrailEngine::from_parts(
            store,
            Arc::new(NullAuditSink::new([alpha_route()])),
            key_store(&["alpha"]) as Arc<dyn KeyStore>,
            Network::Testnet,
        )
        .expect("engine");
        engine.register_agent(&agent, NOW_MS).expect("register");
        engine
            .operator_release_kill(&KillScope::Global, NOW_MS)
            .unwrap();
        engine
            .operator_set_guardrails(&agent, permissive(&["BTC"]), NOW_MS)
            .expect("set guardrails");
        acknowledge(&engine);
        let effect = engine
            .operator_engage_kill(
                KillScope::agent(agent.clone()),
                KillReason::Operator,
                NOW_MS + 7,
            )
            .expect("engage");
        assert!(effect.newly_engaged);
        assert!(effect.cancel_for.contains(&agent));
    }

    // A whole new process would see exactly this.
    let store: Arc<dyn GuardrailStore> =
        Arc::new(SqliteGuardrailStore::new(open_test_policy(&path, false)));
    let engine = GuardrailEngine::from_parts(
        store,
        Arc::new(NullAuditSink::new([alpha_route()])),
        key_store(&["alpha"]) as Arc<dyn KeyStore>,
        Network::Testnet,
    )
    .expect("engine");
    assert!(
        engine
            .kill_switch()
            .is_engaged(&KillScope::agent(agent.clone()))
    );
    let refusal = engine
        .evaluate(
            &agent,
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
        .expect_err("still paused after the restart");
    assert!(matches!(
        refusal,
        Refusal::TradingPaused {
            reason: KillReason::Operator,
            since_ms,
            ..
        } if since_ms == NOW_MS + 7
    ));

    // And the guardrails came back too, not D-c defaults.
    assert_eq!(
        engine.guardrails(&agent).map(|g| g.max_order_usd),
        Some(d("1000000"))
    );
}

#[test]
fn a_global_engagement_pauses_an_agent_with_no_engagement_of_its_own() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::FeedFailure, 1)
        .expect("engage");
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::TradingPaused {
            scope: KillScope::Global,
            ..
        })
    ));
    f.engine
        .operator_release_kill(&KillScope::Global, NOW_MS)
        .expect("release");
    acknowledge(&f.engine);
    assert!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .is_ok()
    );
}

#[test]
fn cancelling_still_clears_while_the_kill_switch_is_engaged() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, 1)
        .expect("engage");
    let cleared = f
        .engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 42 }],
            "kill switch",
            NOW_MS,
        )
        .expect("a cancel is risk-reducing and must clear while paused");
    assert!(matches!(
        cleared.action(),
        Action::Cancel { cancels } if cancels.len() == 1
    ));
    assert_eq!(cleared.clearance().kind, ClearedKind::Cancel { count: 1 });
}

/// Item 26 makes cancelling resting orders part of what engaging the switch
/// *does*, and item 25's whole point is stopping an agent that is grinding
/// the account down overnight. A trip that pauses new orders but leaves the
/// working ones live has not stopped anything.
#[test]
fn a_breaker_trip_queues_the_same_cancels_an_operator_engagement_would() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::new(config);
    assert!(f.engine.take_pending_kill_effects().is_empty());

    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-25");
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &losing,
        ),
        Err(Refusal::LossLimit { .. })
    ));

    let effects = f.engine.take_pending_kill_effects();
    assert_eq!(
        effects.len(),
        1,
        "the trip must name whose orders to cancel"
    );
    assert!(effects[0].newly_engaged);
    assert_eq!(effects[0].scope, KillScope::agent("alpha"));
    assert!(effects[0].cancel_for.contains(&f.agent));

    // Identical to what the operator pressing the same scope would produce.
    let manual = f
        .engine
        .operator_engage_kill(KillScope::agent("alpha"), KillReason::Operator, NOW_MS)
        .expect("engage");
    assert_eq!(effects[0].cancel_for, manual.cancel_for);

    // Drained, so a caller cannot double-issue the cancels.
    assert!(f.engine.take_pending_kill_effects().is_empty());
}

/// A `Global` trip has to be actionable without an agent id in hand.
#[test]
fn a_global_breaker_trip_names_every_agent() {
    let f = Fixture::new(permissive(&["BTC"]));
    let beta = AgentId::new("beta");
    f.engine.register_agent(&beta, NOW_MS).expect("register");
    f.engine
        .operator_set_account_limits(
            LossLimits {
                max_daily_loss_usd: Some(d("400")),
                max_drawdown_usd: None,
            },
            NOW_MS,
        )
        .expect("set account limits");
    acknowledge(&f.engine);

    let mut fleet = account(d("4600"));
    fleet.realized_pnl_today_usd = d("-400");
    let with_fleet = Exposure {
        account: vault(),
        agent: account(d("1000")),
        fleet: Some(fleet),
    };
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &with_fleet,
        ),
        Err(Refusal::LossLimit {
            scope: KillScope::Global,
            ..
        })
    ));
    let effects = f.engine.take_pending_kill_effects();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].scope, KillScope::Global);
    assert_eq!(effects[0].cancel_for, f.engine.agents());
    assert_eq!(
        effects[0].cancel_for,
        BTreeSet::from([f.agent.clone(), beta])
    );
}

/// A kill-switch write failure refuses the order, but must still hand the
/// caller the cancels: staying stopped and leaving the book live is the
/// worst of both.
#[test]
fn a_trip_whose_state_write_fails_still_queues_the_cancels() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::with(
        config,
        Arc::new(FailingStore::default()),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-30");
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &losing,
        ),
        Err(Refusal::Unevaluable(Unevaluable::StateWriteFailed { .. }))
    ));
    let effects = f.engine.take_pending_kill_effects();
    assert_eq!(effects.len(), 1);
    assert!(effects[0].cancel_for.contains(&f.agent));
}

// ---- item 27: the dead-man's switch -------------------------------------

#[test]
fn the_dead_man_switch_arms_while_an_agent_is_active() {
    let f = Fixture::new(permissive(&["BTC"]));
    assert_eq!(
        f.engine.dead_man_intent(&f.agent, NOW_MS, None),
        DeadManIntent::Hold
    );

    f.engine.set_agent_active(&f.agent, true);
    assert_eq!(f.engine.active_agents(), 1);
    let intent = f.engine.dead_man_intent(&f.agent, NOW_MS, None);
    let DeadManIntent::Arm { cancel_at_ms } = intent else {
        panic!("expected an arm, got {intent:?}");
    };
    let cleared = f
        .engine
        .clear_dead_man(&f.agent, intent, NOW_MS)
        .expect("arming clears")
        .expect("an arm produces an action");
    assert_eq!(
        cleared.action(),
        &Action::ScheduleCancel {
            time: Some(cancel_at_ms)
        }
    );

    f.engine.set_agent_active(&f.agent, false);
    assert_eq!(
        f.engine
            .dead_man_intent(&f.agent, NOW_MS, Some(cancel_at_ms)),
        DeadManIntent::Disarm
    );
    let disarm = f
        .engine
        .clear_dead_man(&f.agent, DeadManIntent::Disarm, NOW_MS)
        .expect("disarming clears")
        .expect("a disarm produces an action");
    assert_eq!(disarm.action(), &Action::ScheduleCancel { time: None });
    assert!(
        f.engine
            .clear_dead_man(&f.agent, DeadManIntent::Hold, NOW_MS)
            .expect("hold clears")
            .is_none()
    );
}

#[test]
fn a_schedule_cancel_inside_the_venue_minimum_is_refused() {
    let f = Fixture::new(permissive(&["BTC"]));
    assert!(matches!(
        f.engine
            .clear_schedule_cancel(&f.agent, Some(NOW_MS + DEAD_MAN_MIN_LEAD_MS - 1), NOW_MS),
        Err(Refusal::VenueRule(VenueRule::ScheduleCancelTooSoon { .. }))
    ));
    assert!(
        f.engine
            .clear_schedule_cancel(&f.agent, Some(NOW_MS + DEAD_MAN_MIN_LEAD_MS), NOW_MS)
            .is_ok()
    );
}

// ---- fail-closed --------------------------------------------------------

/// Every way the engine can be blind, and the refusal each one produces.
/// A new unevaluable input belongs in this table before it belongs in the
/// engine.
#[test]
fn every_unevaluable_input_refuses() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let good_market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let good_exposure = exposure(d("100000"));
    let order = intent("BTC", true, d("100"), d("1"));

    // Sanity: the baseline clears, so every refusal below is caused by the
    // one thing that was changed.
    assert!(
        f.evaluate(&order, &btc, &good_market, &good_exposure)
            .is_ok()
    );

    let stale_market = MarketRef {
        as_of_ms: NOW_MS - 2_001,
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &stale_market, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::StaleMarketData { .. }))
    ));

    let no_price = MarketRef {
        reference_px: None,
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &no_price, &good_exposure),
        Err(Refusal::Unevaluable(
            Unevaluable::MissingReferencePrice { .. }
        ))
    ));

    let zero_price = MarketRef {
        reference_px: Some(Decimal::ZERO),
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &zero_price, &good_exposure),
        Err(Refusal::Unevaluable(
            Unevaluable::MissingReferencePrice { .. }
        ))
    ));

    for quality in [
        FeedQuality::Warmup,
        FeedQuality::Degraded,
        FeedQuality::Unusable,
    ] {
        let degraded = MarketRef {
            quality,
            ..good_market.clone()
        };
        assert!(
            matches!(
                f.evaluate(&order, &btc, &degraded, &good_exposure),
                Err(Refusal::Unevaluable(Unevaluable::DegradedFeed { .. }))
            ),
            "{quality} must refuse"
        );
    }

    let diverged = MarketRef {
        mark_divergence_bps: Some(d("6")),
        mark_divergent_since_ms: Some(NOW_MS - 30_000),
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &diverged, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::MarkDivergence { .. }))
    ));
    // Inside tolerance, or not yet sustained, is not a refusal: §14.2
    // measured containment failing 37-39% of the time.
    let brief = MarketRef {
        mark_divergent_since_ms: Some(NOW_MS - 29_999),
        ..diverged.clone()
    };
    assert!(f.evaluate(&order, &btc, &brief, &good_exposure).is_ok());
    let small = MarketRef {
        mark_divergence_bps: Some(d("5")),
        ..diverged.clone()
    };
    assert!(f.evaluate(&order, &btc, &small, &good_exposure).is_ok());

    let backwards = MarketRef {
        as_of_ms: NOW_MS + 1,
        ..good_market.clone()
    };
    assert!(matches!(
        f.evaluate(&order, &btc, &backwards, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::ClockWentBackwards { .. }))
    ));

    let mut unreconciled = good_exposure.clone();
    unreconciled.agent.reconciled = false;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &unreconciled),
        Err(Refusal::Unevaluable(
            Unevaluable::UnreconciledAccount { .. }
        ))
    ));

    let mut no_book = good_exposure.clone();
    no_book.agent.resting = None;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &no_book),
        Err(Refusal::Unevaluable(Unevaluable::MissingRestingOrders))
    ));

    let mut stale_account = good_exposure.clone();
    stale_account.agent.as_of_ms = NOW_MS - 5_001;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &stale_account),
        Err(Refusal::Unevaluable(Unevaluable::StaleAccountState { .. }))
    ));

    let mut broke = good_exposure.clone();
    broke.agent.equity_usd = Decimal::ZERO;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &broke),
        Err(Refusal::Unevaluable(Unevaluable::NonPositiveEquity { .. }))
    ));

    let mut yesterday = good_exposure.clone();
    yesterday.agent.day_start_ms = MIDNIGHT_MS - 86_400_000;
    assert!(matches!(
        f.evaluate(&order, &btc, &good_market, &yesterday),
        Err(Refusal::Unevaluable(Unevaluable::LossWindowMismatch { .. }))
    ));

    let wrong_market = MarketRef::fresh("ETH", d("100"), NOW_MS);
    assert!(matches!(
        f.evaluate(&order, &btc, &wrong_market, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::InputMismatch { .. }))
    ));
    assert!(matches!(
        f.evaluate(&order, &asset("ETH", 2, 40), &good_market, &good_exposure),
        Err(Refusal::Unevaluable(Unevaluable::InputMismatch { .. }))
    ));

    let mut unnamed = order.clone();
    unnamed.reason = "   ".to_owned();
    assert!(matches!(
        f.evaluate(&unnamed, &btc, &good_market, &good_exposure),
        Err(Refusal::MissingReason)
    ));

    let unknown = AgentId::new("nobody");
    assert!(matches!(
        f.engine
            .evaluate(&unknown, &order, &btc, &good_market, &good_exposure, NOW_MS),
        Err(Refusal::Unevaluable(Unevaluable::UnknownAgent { .. }))
    ));
}

#[test]
fn account_wide_limits_with_no_fleet_snapshot_refuse() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_set_account_limits(
            LossLimits {
                max_daily_loss_usd: Some(d("500")),
                max_drawdown_usd: None,
            },
            NOW_MS,
        )
        .expect("set");
    acknowledge(&f.engine);
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::Unevaluable(Unevaluable::MissingFleetState))
    ));
}

/// `rust_decimal`'s operators panic on overflow, and an agent picks the price
/// and the size. An absurd order must be refused, not fatal.
#[test]
fn an_order_whose_arithmetic_overflows_is_refused_not_fatal() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let huge = Decimal::MAX.round_dp(0);
    let market = MarketRef::fresh("BTC", huge, NOW_MS);
    let refusal = f
        .evaluate(
            &intent("BTC", true, huge, huge),
            &btc,
            &market,
            &exposure(d("100")),
        )
        .expect_err("an unrepresentable order is refused");
    assert!(refusal.is_unevaluable() || matches!(refusal, Refusal::VenueRule(_)));

    // And a position snapshot large enough to overflow the post-fill maths.
    let mut vast = exposure(d("100"));
    vast.agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: huge });
    vast.agent.total_position_notional_usd = huge;
    let refusal = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &MarketRef::fresh("BTC", huge, NOW_MS),
            &vast,
        )
        .expect_err("an unrepresentable exposure is refused");
    assert!(refusal.is_unevaluable(), "got {refusal}");
}

#[test]
fn a_ledger_write_failure_refuses_the_order() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(test_store()),
        Arc::new(FailingSink {
            routes: TestRoutes::new([alpha_route()]),
        }),
    );
    match f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect_err("an unexplainable order does not happen")
    {
        Refusal::Unevaluable(Unevaluable::AuditWriteFailed { detail }) => {
            assert!(detail.contains("disk"));
        }
        other => panic!("expected the audit refusal, got {other}"),
    }
}

/// Fail-closed exists to stop new exposure. Applying it to the actions that
/// *remove* exposure inverts the property: a full ledger disk would take away
/// the operator's ability to stop trading, take the kill switch's own cancels
/// down with it, and stop `scheduleCancel` from being disarmed — all while
/// the orders it cannot cancel keep working.
#[test]
fn a_ledger_write_failure_never_blocks_a_cancel_or_the_dead_man_switch() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(test_store()),
        Arc::new(FailingSink {
            routes: TestRoutes::new([alpha_route()]),
        }),
    );

    f.engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 42 }],
            "the disk is full and the orders still have to go",
            NOW_MS,
        )
        .expect("a cancel is risk-reducing and must clear");

    f.engine
        .clear_cancel_by_cloid(
            &f.agent,
            vec![CancelByCloidWire {
                asset: 7,
                cloid: cloid(),
            }],
            "cancel by cloid after a timeout",
            NOW_MS,
        )
        .expect("cancel-by-cloid is the only safe move after a timeout");

    let disarm = f
        .engine
        .clear_dead_man(&f.agent, DeadManIntent::Disarm, NOW_MS)
        .expect("disarming clears")
        .expect("a disarm produces an action");
    assert_eq!(disarm.action(), &Action::ScheduleCancel { time: None });

    f.engine
        .clear_schedule_cancel(&f.agent, Some(NOW_MS + DEAD_MAN_MIN_LEAD_MS), NOW_MS)
        .expect("re-arming clears");

    // The order path is unchanged: an unexplainable *order* still does not
    // happen.
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        ),
        Err(Refusal::Unevaluable(Unevaluable::AuditWriteFailed { .. }))
    ));
}

/// Item 18 names guardrail trips, approval decisions and kill-switch changes
/// as ledger events. Without these rows an export cannot say why an order
/// refused yesterday cleared today.
#[test]
fn operator_actions_reach_the_ledger() {
    let sink = Arc::new(CountingSink::new([alpha_route()]));
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(test_store()),
        sink.clone() as Arc<dyn AuditSink>,
    );
    f.engine
        .operator_set_account_limits(LossLimits::UNSET, NOW_MS)
        .expect("set");
    acknowledge(&f.engine);
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        .expect("engage");
    f.engine
        .operator_release_kill(&KillScope::Global, NOW_MS)
        .expect("release");
    f.engine
        .operator_set_global_rate_budget(GlobalRateBudget::default(), NOW_MS)
        .expect("set budget");

    let actions = sink.operator_actions();
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OperatorAction::AgentRegistered { .. })),
        "{actions:?}"
    );
    // The guardrail row carries both sides, so the change is readable.
    let changed = actions
        .iter()
        .find_map(|a| match a {
            OperatorAction::GuardrailsChanged { before, after } => Some((before, after)),
            _ => None,
        })
        .expect("the fixture's own set_guardrails is recorded");
    assert_eq!(
        changed.0.as_deref().map(|c| c.max_order_usd),
        Some(DEFAULT_MAX_ORDER_USD)
    );
    assert_eq!(changed.1.max_order_usd, d("1000000"));
    assert!(actions.iter().any(|a| matches!(
        a,
        OperatorAction::KillEngaged {
            newly_engaged: true,
            ..
        }
    )));
    assert!(actions.iter().any(|a| matches!(
        a,
        OperatorAction::KillReleased {
            was_engaged: true,
            ..
        }
    )));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OperatorAction::AccountLimitsChanged { .. }))
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OperatorAction::GlobalRateBudgetChanged { .. }))
    );
}

/// An operator action that has already happened must not be reported as
/// failed because its row could not be written.
#[test]
fn an_operator_action_still_happens_when_its_ledger_row_fails() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(test_store()),
        Arc::new(FailingSink {
            routes: TestRoutes::new([alpha_route()]),
        }),
    );
    let effect = f
        .engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        .expect("the switch engages even when the ledger cannot record it");
    assert!(effect.newly_engaged);
    assert!(f.engine.kill_switch().is_engaged(&KillScope::Global));
}

// ---- item 10: the address-wide request budget ---------------------------

/// Item 10's budget is metered per address, so no per-agent cap bounds it.
/// An order may not draw it below the reserve; a cancel may, because item 10
/// says to always reserve headroom for risk-reducing actions and a budget
/// that refuses a cancel has spent that headroom on the wrong thing.
#[test]
fn the_global_budget_refuses_orders_at_the_reserve_but_never_a_cancel() {
    let f = Fixture::new(permissive(&["BTC"]));
    // Four requests, one of them reserved.
    f.engine
        .operator_set_global_rate_budget(
            GlobalRateBudget {
                rate: OrderRate {
                    count: 4,
                    per_ms: 4_000_000,
                },
                reserve: 1,
            },
            NOW_MS,
        )
        .expect("set budget");

    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let order = intent("BTC", true, d("100"), d("1"));
    for i in 0..3 {
        let cleared = f
            .evaluate(&order, &btc, &market, &exposure(d("100000")))
            .unwrap_or_else(|e| panic!("order {i} is inside the budget: {e}"));
        assert_eq!(
            cleared.clearance().utilization.global_tokens_remaining,
            Decimal::from(3 - i)
        );
    }
    match f
        .evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect_err("the fourth order would eat the reserve")
    {
        Refusal::GlobalRateBudget {
            tokens_available,
            reserve,
            ..
        } => {
            assert_eq!(tokens_available, Decimal::ONE);
            assert_eq!(reserve, 1);
        }
        other => panic!("expected the global budget refusal, got {other}"),
    }

    // The reserve is what the cancel path is for, and once it is gone the
    // cancel still clears rather than being throttled.
    for _ in 0..3 {
        f.engine
            .clear_cancel(
                &f.agent,
                vec![CancelWire { a: 7, o: 42 }],
                "winding down",
                NOW_MS,
            )
            .expect("a cancel is never refused by the request budget");
    }
}

// ---- item 30: the reason is untrusted text ------------------------------

#[test]
fn an_unbounded_or_control_laden_reason_is_refused_at_both_boundaries() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let with_reason = |reason: String| {
        let mut i = intent("BTC", true, d("100"), d("1"));
        i.reason = reason;
        i
    };

    assert!(
        f.evaluate(
            &with_reason("a".repeat(MAX_REASON_BYTES)),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok(),
        "exactly at the limit is allowed"
    );
    match f
        .evaluate(
            &with_reason("a".repeat(MAX_REASON_BYTES + 1)),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("one byte past the limit is refused")
    {
        Refusal::ReasonTooLong {
            len_bytes,
            max_bytes,
        } => {
            assert_eq!(len_bytes, MAX_REASON_BYTES + 1);
            assert_eq!(max_bytes, MAX_REASON_BYTES);
        }
        other => panic!("expected the length refusal, got {other}"),
    }

    // Newline and tab are the only control characters a sentence needs.
    assert!(
        f.evaluate(
            &with_reason("funding\nflipped\tnegative".to_owned()),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .is_ok()
    );
    match f
        .evaluate(
            &with_reason("\u{1b}[2J fake price row".to_owned()),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("an ANSI escape is not inert on a character grid")
    {
        Refusal::ReasonControlCharacter { at_byte, codepoint } => {
            assert_eq!(at_byte, 0);
            assert_eq!(codepoint, 0x1b);
        }
        other => panic!("expected the control-character refusal, got {other}"),
    }

    // The cancel path takes the same reason and must check it identically.
    assert!(matches!(
        f.engine.clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 42 }],
            &"a".repeat(MAX_REASON_BYTES + 1),
            NOW_MS,
        ),
        Err(Refusal::ReasonTooLong { .. })
    ));
}

#[test]
fn a_kill_switch_write_failure_refuses_rather_than_forgetting_the_trip() {
    let mut config = permissive(&["BTC"]);
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("25")),
        max_drawdown_usd: None,
    };
    let f = Fixture::with(
        config,
        Arc::new(FailingStore::default()),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    let mut losing = exposure(d("975"));
    losing.agent.realized_pnl_today_usd = d("-30");

    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &losing,
        ),
        Err(Refusal::Unevaluable(Unevaluable::StateWriteFailed { .. }))
    ));
    // The engagement still stands in memory: the conservative direction.
    assert!(
        f.engine
            .kill_switch()
            .is_engaged(&KillScope::agent("alpha"))
    );
}

#[test]
fn refusals_are_recorded_too() {
    let sink = Arc::new(CountingSink::new([alpha_route()]));
    let f = Fixture::with(
        permissive(&[]),
        Arc::new(test_store()),
        sink.clone() as Arc<dyn AuditSink>,
    );
    let _ = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", d("100"), NOW_MS),
        &exposure(d("100000")),
    );
    assert_eq!(sink.refused.load(Ordering::Relaxed), 1);
    assert_eq!(sink.cleared.load(Ordering::Relaxed), 0);
}

// ---- item 28: approval mode ---------------------------------------------

fn approval_route_fixture() -> (Fixture, Arc<CountingSink>, AuthorizedRoute) {
    let agent = AgentId::new("alpha");
    let route = AuthorizedRoute {
        network: Network::Testnet,
        binding_seq: 1,
        binding: RegistryBinding {
            agent: agent.clone(),
            container: exposure(d("100000")).account,
            vault_address: None,
            wallet: crate::keys::AgentWallet {
                generation: 0,
                address: oppen_hl::Address::from_bytes([19; 20]),
                approved_at_ms: NOW_MS - 1,
                valid_until_ms: NOW_MS + APPROVAL_TTL_MS,
            },
        },
    };
    let sink = Arc::new(CountingSink::new([route.clone()]));
    let engine = GuardrailEngine::from_parts(
        Arc::new(test_store()),
        sink.clone(),
        Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet)),
        Network::Testnet,
    )
    .unwrap();
    engine.register_agent(&agent, NOW_MS).unwrap();
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.max_position_usd = d("200");
    engine
        .operator_set_guardrails(&agent, config, NOW_MS)
        .unwrap();
    acknowledge(&engine);
    (Fixture { engine, agent }, sink, route)
}

#[test]
fn approval_route_binding_rejects_replacement_account_revision_and_wallet_changes() {
    for change in [
        "replacement_account",
        "binding_revision",
        "signer",
        "wallet_generation",
        "wallet_window",
        "vault",
    ] {
        let (f, sink, original) = approval_route_fixture();
        let btc = asset("BTC", 2, 40);
        let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
        let mut fresh_exposure = exposure(d("100000"));
        let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &fresh_exposure,
        ) else {
            panic!("expected a proposal");
        };
        let mut replacement = original.clone();
        match change {
            "replacement_account" => {
                replacement.binding_seq += 1;
                replacement.binding.container = oppen_hl::Address::from_bytes([23; 20]);
                replacement.binding.wallet.address = oppen_hl::Address::from_bytes([24; 20]);
                fresh_exposure.account = replacement.binding.container;
            }
            "binding_revision" => replacement.binding_seq += 1,
            "signer" => {
                replacement.binding.wallet.address = oppen_hl::Address::from_bytes([24; 20])
            }
            "wallet_generation" => replacement.binding.wallet.generation += 1,
            "wallet_window" => replacement.binding.wallet.valid_until_ms += 1,
            "vault" => replacement.binding.vault_address = Some(replacement.binding.container),
            _ => unreachable!(),
        }
        {
            let mut routes = sink.routes.0.lock().unwrap();
            assert_eq!(routes.remove(&f.agent), Some(original));
            routes.insert(f.agent.clone(), replacement);
        }
        let route_reads = sink.route_reads.load(Ordering::Relaxed);
        let result = f.engine.operator_approve_proposal(
            &approval_id,
            &btc,
            &market,
            &fresh_exposure,
            NOW_MS,
        );
        assert_eq!(
            sink.route_reads.load(Ordering::Relaxed),
            route_reads + 1,
            "{change}: compare and clearance must use one route lookup"
        );
        assert!(
            matches!(
                result,
                Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
            ),
            "{change}: {result:?}"
        );
        assert_eq!(sink.cleared.load(Ordering::Relaxed), 0, "{change}");
        assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
        assert!(matches!(
            f.engine.operator_approve_proposal(
                &approval_id,
                &btc,
                &market,
                &fresh_exposure,
                NOW_MS
            ),
            Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
        ));
    }
}

#[test]
fn approval_route_binding_unchanged_still_checks_fresh_exposure_and_kill() {
    for change in ["unchanged", "position", "kill"] {
        let (f, sink, route) = approval_route_fixture();
        let btc = asset("BTC", 2, 40);
        let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
        let mut fresh_exposure = exposure(d("100000"));
        let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &fresh_exposure,
        ) else {
            panic!("expected a proposal");
        };
        if change == "position" {
            fresh_exposure
                .agent
                .positions
                .insert("BTC".into(), PositionSnapshot { szi: d("2") });
            fresh_exposure.agent.total_position_notional_usd = d("200");
        } else if change == "kill" {
            f.engine
                .operator_engage_kill(
                    KillScope::agent(f.agent.clone()),
                    KillReason::Operator,
                    NOW_MS,
                )
                .unwrap();
        }
        let route_reads = sink.route_reads.load(Ordering::Relaxed);
        let result = f.engine.operator_approve_proposal(
            &approval_id,
            &btc,
            &market,
            &fresh_exposure,
            NOW_MS,
        );
        assert_eq!(sink.route_reads.load(Ordering::Relaxed), route_reads + 1);
        match change {
            "unchanged" => assert_eq!(result.unwrap().clearance().route, route),
            "position" => assert!(matches!(result, Err(Refusal::PositionNotional { .. }))),
            "kill" => assert!(matches!(result, Err(Refusal::TradingPaused { .. }))),
            _ => unreachable!(),
        }
        assert_eq!(
            sink.cleared.load(Ordering::Relaxed),
            usize::from(change == "unchanged")
        );
    }
}

#[test]
fn approval_is_the_last_check_and_is_not_charged_twice() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.max_order_usd = d("25");
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 300_000,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // An order that would be refused anyway is refused, not queued.
    assert!(matches!(
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        ),
        Err(Refusal::OrderNotional { .. })
    ));
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());

    // A good order becomes a proposal, and spends the single rate token.
    let order = intent("BTC", true, d("25"), d("1"));
    let Err(Refusal::ApprovalRequired {
        approval_id,
        expires_at_ms,
        ..
    }) = f.evaluate(&order, &btc, &market, &exposure(d("100000")))
    else {
        panic!("a good order under approval mode becomes a proposal");
    };
    assert_eq!(expires_at_ms, NOW_MS + APPROVAL_TTL_MS);
    let pending = f.engine.pending_proposals(NOW_MS).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id(), approval_id);
    assert_eq!(pending[0].intent(), &order);

    // The operator approves; the re-evaluation must not need a second token.
    assert!(
        f.engine
            .operator_approve_proposal(&approval_id, &btc, &market, &exposure(d("100000")), NOW_MS)
            .is_ok(),
        "an approved proposal does not pay the rate token twice"
    );
    // And it is spent: one approval, one evaluation.
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
    assert!(matches!(
        f.engine.operator_approve_proposal(
            &approval_id,
            &btc,
            &market,
            &exposure(d("100000")),
            NOW_MS
        ),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
}

#[test]
fn original_request_evidence_distinguishes_market_from_explicit_ioc() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let mut order = intent("BTC", true, d("100.5"), d("1"));
    order.kind = OrderKind::Limit { tif: Tif::Ioc };
    for kind in [
        RequestedOrderKind::Market {
            slippage_bps: d("50"),
        },
        RequestedOrderKind::Limit {
            limit_px: d("100.5"),
            tif: Tif::Ioc,
        },
    ] {
        order.original = Some(OriginalRequest {
            kind,
            reference_px: Some(d("100")),
            reference_at_ms: NOW_MS,
        });
        assert!(matches!(
            f.evaluate(&order, &btc, &market, &exposure(d("100000"))),
            Err(Refusal::ApprovalRequired { .. })
        ));
    }
    let pending = f.engine.pending_proposals(NOW_MS).unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].intent().px, pending[1].intent().px);
    assert_ne!(pending[0].intent().original, pending[1].intent().original);
}

#[test]
fn original_request_mismatch_never_mints_approval() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let mut order = intent("BTC", true, d("100.5"), d("1"));
    order.kind = OrderKind::Limit { tif: Tif::Ioc };
    for kind in [
        RequestedOrderKind::Limit {
            limit_px: d("101"),
            tif: Tif::Ioc,
        },
        RequestedOrderKind::Limit {
            limit_px: d("100.5"),
            tif: Tif::Gtc,
        },
        RequestedOrderKind::Market {
            slippage_bps: d("10"),
        },
        RequestedOrderKind::ClosePosition {
            position_size: d("1"),
            slippage_bps: d("50"),
        },
        RequestedOrderKind::StopMarket {
            trigger_px: d("100"),
            tpsl: Tpsl::Sl,
            slippage_bps: d("50"),
        },
    ] {
        order.original = Some(OriginalRequest {
            kind,
            reference_px: Some(d("100")),
            reference_at_ms: NOW_MS,
        });
        assert_eq!(
            f.evaluate(&order, &btc, &market, &exposure(d("100000")))
                .unwrap_err(),
            Refusal::Unevaluable(Unevaluable::OriginalRequestMismatch)
        );
    }
    order.original = Some(OriginalRequest {
        kind: RequestedOrderKind::Market {
            slippage_bps: Decimal::MAX,
        },
        reference_px: Some(Decimal::MAX),
        reference_at_ms: NOW_MS,
    });
    assert_eq!(
        f.evaluate(&order, &btc, &market, &exposure(d("100000")))
            .unwrap_err(),
        Refusal::Unevaluable(Unevaluable::OriginalRequestMismatch)
    );
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
}

/// The whole of finding 2: there must be no value a caller can build that
/// asserts it was approved. `OrderIntent` has no approval field, so the only
/// way to reach the approved path is an id the engine minted — and the engine
/// re-evaluates *its own* stored intent, never one the caller re-supplies.
#[test]
fn an_approval_cannot_be_asserted_by_the_caller_and_cannot_be_swapped() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 300_000,
    };
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);

    // A made-up id is not an approval.
    assert!(matches!(
        f.engine
            .operator_approve_proposal("a-1", &btc, &market, &exposure(d("100000")), NOW_MS),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));

    // A small order is proposed; approving it signs the small order, and
    // there is no parameter by which a larger one could be substituted.
    let small = intent("BTC", true, d("100"), d("1"));
    let Err(Refusal::ApprovalRequired { approval_id, .. }) =
        f.evaluate(&small, &btc, &market, &exposure(d("100000")))
    else {
        panic!("expected a proposal");
    };
    let cleared = f
        .engine
        .operator_approve_proposal(&approval_id, &btc, &market, &exposure(d("100000")), NOW_MS)
        .expect("the operator approves the proposal it was shown");
    match &cleared.clearance().kind {
        ClearedKind::Order { sz, .. } => assert_eq!(*sz, d("1")),
        other => panic!("expected an order clearance, got {other:?}"),
    }

    // The rate token was spent when the proposal was minted, so a second
    // fresh order is refused by the cap the old bypass skipped entirely.
    assert!(matches!(
        f.evaluate(&small, &btc, &market, &exposure(d("100000"))),
        Err(Refusal::OrderRate { .. })
    ));
}

/// Item 28: proposals carry a TTL and auto-expire.
#[test]
fn a_proposal_expires_and_cannot_be_approved_afterwards() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &btc,
        &market,
        &exposure(d("100000")),
    ) else {
        panic!("expected a proposal");
    };

    let expiry = NOW_MS + APPROVAL_TTL_MS;
    assert_eq!(f.engine.pending_proposals(expiry - 1).unwrap().len(), 1);
    assert!(f.engine.pending_proposals(expiry).unwrap().is_empty());
    let mut late_market = market.clone();
    late_market.as_of_ms = expiry;
    let mut late = exposure(d("100000"));
    late.agent.as_of_ms = expiry;
    assert!(matches!(
        f.engine
            .operator_approve_proposal(&approval_id, &btc, &late_market, &late, expiry),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
}

/// An approval is not a waiver. Every hard predicate runs again against the
/// state at approval time, which is what item 28's "re-priced at approval
/// time" means for the guardrails.
#[test]
fn an_approved_proposal_is_still_refused_if_the_world_moved() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.max_position_usd = d("200");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &btc,
        &market,
        &exposure(d("100000")),
    ) else {
        panic!("expected a proposal");
    };

    // By the time the operator looks, the agent already holds the cap.
    let mut loaded = exposure(d("100000"));
    loaded
        .agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("2") });
    loaded.agent.total_position_notional_usd = d("200");
    assert!(matches!(
        f.engine
            .operator_approve_proposal(&approval_id, &btc, &market, &loaded, NOW_MS),
        Err(Refusal::PositionNotional { .. })
    ));
}

#[test]
fn a_rejected_proposal_is_gone() {
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let Err(Refusal::ApprovalRequired { approval_id, .. }) = f.evaluate(
        &intent("BTC", true, d("100"), d("1")),
        &btc,
        &market,
        &exposure(d("100000")),
    ) else {
        panic!("expected a proposal");
    };
    assert!(
        f.engine
            .operator_reject_proposal(&approval_id, NOW_MS)
            .unwrap()
    );
    assert!(
        !f.engine
            .operator_reject_proposal(&approval_id, NOW_MS)
            .unwrap()
    );
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
}

// ---- what gets signed is what was checked -------------------------------

#[test]
fn the_cleared_action_carries_the_rounded_values_that_were_checked() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 5, 40);
    let market = MarketRef::fresh("BTC", d("41505.123"), NOW_MS);
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("41505.123"), d("0.0012345678")),
            &btc,
            &market,
            &exposure(d("1000000")),
        )
        .expect("clears");
    let wire = wire_of(&cleared);
    assert_eq!(wire.a, btc.index);
    assert_eq!(wire.p.as_str(), "41505");
    assert_eq!(wire.s.as_str(), "0.00123");
    match &cleared.clearance().kind {
        ClearedKind::Order {
            px,
            sz,
            notional_usd,
            ..
        } => {
            assert_eq!(*px, d("41505"));
            assert_eq!(*sz, d("0.00123"));
            assert_eq!(*notional_usd, d("41505") * d("0.00123"));
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }
}

/// Item 19 makes query-by-cloid the only safe move after a
/// `timeout_unknown_outcome` and item 9 reconciles by it, so the row that
/// says why an order was allowed has to carry the same cloid the wire order
/// does — otherwise the decision cannot be joined to the fill. R6 wants the
/// same of the decision-time book snapshot.
#[test]
fn the_clearance_carries_the_cloid_and_the_snapshot_it_was_decided_against() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let mut market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    market.snapshot = Some(MarketSnapshotRef {
        id: "snap-1".to_owned(),
        hash: "0xfeed".to_owned(),
    });
    let mut order = intent("BTC", true, d("100"), d("1"));
    order.cloid = Some(cloid());

    let cleared = f
        .evaluate(&order, &btc, &market, &exposure(d("100000")))
        .expect("clears");
    let wire = wire_of(&cleared);
    match &cleared.clearance().kind {
        ClearedKind::Order {
            cloid: recorded,
            snapshot_id,
            snapshot_hash,
            ..
        } => {
            assert_eq!(recorded.as_ref(), Some(&cloid()));
            assert_eq!(
                recorded.as_ref().map(|c| c.as_str()),
                wire.c.as_ref().map(|c| c.as_str()),
                "the ledger row and the wire order must name the same cloid"
            );
            assert_eq!(snapshot_id.as_deref(), Some("snap-1"));
            assert_eq!(snapshot_hash.as_deref(), Some("0xfeed"));
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }

    // Both are nullable today, and absence is absence rather than a
    // placeholder that could be mistaken for a real id.
    let bare = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &btc,
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    match &bare.clearance().kind {
        ClearedKind::Order {
            cloid,
            snapshot_id,
            snapshot_hash,
            ..
        } => {
            assert!(cloid.is_none());
            assert!(snapshot_id.is_none());
            assert!(snapshot_hash.is_none());
        }
        other => panic!("expected an order clearance, got {other:?}"),
    }
}

#[test]
fn a_venue_rule_breach_is_a_typed_subtype_not_a_string() {
    let f = Fixture::new(permissive(&["BTC"]));
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    // $9 is under the venue's $10 minimum.
    match f
        .evaluate(
            &intent("BTC", true, d("9"), d("1")),
            &btc,
            &market,
            &exposure(d("100000")),
        )
        .expect_err("the venue would reject this")
    {
        Refusal::VenueRule(VenueRule::MinNotional {
            notional_usd,
            minimum_usd,
        }) => {
            assert_eq!(notional_usd, d("9"));
            assert_eq!(minimum_usd, d("10"));
        }
        other => panic!("expected the min-notional subtype, got {other}"),
    }
}

/// Spec 19 close_position / D3: only a complete, opposite-side reduce-only
/// IOC can defer the local $10 check. This proves local policy, not live
/// venue acceptance. Expected sizes are explicit, independent of rounding code.
#[test]
fn tiny_full_ioc_close_minimum_exception_matrix() {
    let config = permissive(&["BTC"]);
    let f = Fixture::new(config.clone());
    let btc = asset("BTC", 2, 40);
    let market = MarketRef::fresh("BTC", d("100"), NOW_MS);
    let kinds = [
        OrderKind::Limit { tif: Tif::Ioc },
        OrderKind::Limit { tif: Tif::Gtc },
        OrderKind::Limit { tif: Tif::Alo },
        OrderKind::Trigger {
            is_market: true,
            trigger_px: d("100"),
            tpsl: Tpsl::Sl,
        },
        OrderKind::Trigger {
            is_market: false,
            trigger_px: d("100"),
            tpsl: Tpsl::Tp,
        },
    ];
    let mut cases = 0;
    let mut allowed = 0;
    for position in [
        None,
        Some("0"),
        Some("0.05"),
        Some("-0.05"),
        Some("0.055"),
        Some("-0.055"),
    ] {
        let mut state = exposure(d("1000"));
        if let Some(position) = position {
            let szi = d(position);
            state
                .agent
                .positions
                .insert("BTC".into(), PositionSnapshot { szi });
            state.agent.total_position_notional_usd = szi.abs() * d("100");
        }
        for is_buy in [false, true] {
            for reduce_only in [false, true] {
                for kind in &kinds {
                    for (requested, rounded) in [
                        ("0.04", "0.04"),
                        ("0.05", "0.05"),
                        ("0.0509", "0.05"),
                        ("0.06", "0.06"),
                    ] {
                        cases += 1;
                        let mut candidate = intent("BTC", is_buy, d("100"), d(requested));
                        candidate.reduce_only = reduce_only;
                        candidate.kind = kind.clone();
                        let expected_close = position
                            == Some(if is_buy { "-0.05" } else { "0.05" })
                            && reduce_only
                            && *kind == (OrderKind::Limit { tif: Tif::Ioc })
                            && rounded == "0.05";
                        let result = f.evaluate(&candidate, &btc, &market, &state);
                        let context = format!("position={position:?}, order={candidate:?}");
                        if expected_close {
                            let cleared =
                                result.unwrap_or_else(|error| panic!("{context}: {error}"));
                            let wire = wire_of(&cleared);
                            assert_eq!(wire_sz(&wire), d("0.05"), "{context}");
                            assert_eq!(wire_px(&wire) * wire_sz(&wire), d("5"));
                            assert_eq!(wire.b, is_buy);
                            assert!(wire.r);
                            assert!(matches!(
                                wire.t,
                                oppen_hl::wire::OrderType::Limit { tif: Tif::Ioc }
                            ));
                            verify_every_predicate(
                                cases, &cleared, &config, &candidate, &btc, &market, &state, NOW_MS,
                            );
                            // Without known-position context the same spec must
                            // retain the ordinary minimum, even with RO + IOC.
                            let ordinary = oppen_hl::order::OrderSpec {
                                is_buy,
                                px: candidate.px,
                                sz: candidate.sz,
                                kind: candidate.kind.clone(),
                                reduce_only,
                                cloid: None,
                            };
                            assert!(matches!(
                                ordinary.to_wire(&btc),
                                Err(oppen_hl::order::OrderError::Validation(
                                    oppen_hl::meta::ValidationError::MinNotional { .. }
                                ))
                            ));
                            allowed += 1;
                        } else {
                            match result.expect_err(&context) {
                                Refusal::VenueRule(VenueRule::MinNotional {
                                    notional_usd,
                                    minimum_usd,
                                }) => {
                                    assert_eq!(notional_usd, d("100") * d(rounded), "{context}");
                                    assert_eq!(minimum_usd, d("10"), "{context}");
                                }
                                other => panic!("{context}: expected min-notional, got {other:?}"),
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 480);
    assert_eq!(
        allowed, 4,
        "both position signs, exact and rounded full size"
    );
}

/// Spec 19 / D3: bypassing one local venue minimum cannot bypass an operator
/// limit, an unknown exposure, or another wire validation rule.
#[test]
fn tiny_full_ioc_close_still_obeys_every_other_guard() {
    for long in [false, true] {
        for case in [
            "stale_market",
            "stale_account",
            "unreconciled",
            "missing_book",
            "symbol",
            "order_cap",
            "missing_volatility",
            "loss",
            "kill_global",
            "kill_agent",
            "approval",
            "slippage",
            "rate",
            "delisted",
            "zero_price",
            "rounded_zero_size",
        ] {
            let mut config = permissive(&["BTC"]);
            let mut btc = asset("BTC", 2, 40);
            let mut market = MarketRef::fresh("BTC", d("100"), NOW_MS);
            let mut state = exposure(d("1000"));
            state.agent.positions.insert(
                "BTC".into(),
                PositionSnapshot {
                    szi: if long { d("0.05") } else { d("-0.05") },
                },
            );
            state.agent.total_position_notional_usd = d("5");
            let mut candidate = intent("BTC", !long, d("100"), d("0.05"));
            candidate.reduce_only = true;
            candidate.kind = OrderKind::Limit { tif: Tif::Ioc };
            match case {
                "stale_market" => market.as_of_ms = NOW_MS - 2_001,
                "stale_account" => state.agent.as_of_ms = NOW_MS - 5_001,
                "unreconciled" => state.agent.reconciled = false,
                "missing_book" => state.agent.resting = None,
                "symbol" => config.symbols.clear(),
                "order_cap" => config.max_order_usd = d("4"),
                "missing_volatility" => config.risk.max_risk_usd = Some(d("1")),
                "loss" => {
                    config.loss.max_daily_loss_usd = Some(d("4"));
                    state.agent.realized_pnl_today_usd = d("-5");
                }
                "approval" => config.approval_required = true,
                "slippage" => {
                    config.max_slippage_bps = Decimal::ZERO;
                    candidate.px = if long { d("99") } else { d("101") };
                }
                "rate" => {
                    config.order_rate = OrderRate {
                        count: 1,
                        per_ms: 300_000,
                    }
                }
                "delisted" => btc.info.is_delisted = true,
                "zero_price" => candidate.px = Decimal::ZERO,
                "rounded_zero_size" => candidate.sz = d("0.001"),
                "kill_global" | "kill_agent" => {}
                _ => unreachable!(),
            }
            let f = Fixture::new(config);
            if case == "kill_global" || case == "kill_agent" {
                let scope = if case == "kill_global" {
                    KillScope::Global
                } else {
                    KillScope::agent(f.agent.clone())
                };
                f.engine
                    .operator_engage_kill(scope, KillReason::Operator, NOW_MS)
                    .unwrap();
            }
            if case == "rate" {
                f.evaluate(&candidate, &btc, &market, &state)
                    .expect("first close spends the one token");
            }
            let refusal = f
                .evaluate(&candidate, &btc, &market, &state)
                .expect_err(&format!("{case}, long={long}"));
            let correct = match case {
                "stale_market" => matches!(
                    refusal,
                    Refusal::Unevaluable(Unevaluable::StaleMarketData { .. })
                ),
                "stale_account" => matches!(
                    refusal,
                    Refusal::Unevaluable(Unevaluable::StaleAccountState { .. })
                ),
                "unreconciled" => matches!(
                    refusal,
                    Refusal::Unevaluable(Unevaluable::UnreconciledAccount { .. })
                ),
                "missing_book" => matches!(
                    refusal,
                    Refusal::Unevaluable(Unevaluable::MissingRestingOrders)
                ),
                "symbol" => matches!(refusal, Refusal::SymbolNotAllowed { .. }),
                "order_cap" => {
                    matches!(refusal, Refusal::OrderNotional { observed_usd, limit_usd, .. } if observed_usd == d("5") && limit_usd == d("4"))
                }
                "missing_volatility" => matches!(
                    refusal,
                    Refusal::Unevaluable(Unevaluable::MissingVolatility { .. })
                ),
                "loss" => matches!(refusal, Refusal::LossLimit { .. }),
                "kill_global" | "kill_agent" => matches!(refusal, Refusal::TradingPaused { .. }),
                "approval" => matches!(refusal, Refusal::ApprovalRequired { .. }),
                "slippage" => matches!(refusal, Refusal::Slippage { .. }),
                "rate" => matches!(refusal, Refusal::OrderRate { .. }),
                "delisted" => matches!(refusal, Refusal::VenueRule(VenueRule::Delisted { .. })),
                "zero_price" => matches!(
                    refusal,
                    Refusal::VenueRule(VenueRule::NonPositivePrice { .. })
                ),
                "rounded_zero_size" => matches!(
                    refusal,
                    Refusal::VenueRule(VenueRule::NonPositiveSize { .. })
                ),
                _ => unreachable!(),
            };
            assert!(correct, "{case}, long={long}: wrong refusal {refusal:?}");
        }
    }
}

/// R4 calls a mainnet number that is actually a testnet number the worst bug
/// this product can ship, and D1 makes each agent's sub-account the unit of
/// capital segregation. Neither may be a signing parameter: the clearance
/// carries what it was evaluated against, and `sign_cleared` has no argument
/// by which a caller could change either.
#[test]
fn a_clearance_is_signed_for_the_network_and_sub_account_it_was_evaluated_for() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    assert_eq!(cleared.clearance().network, Network::Testnet);
    assert_eq!(cleared.clearance().vault_address, Some(vault()));

    let (request, _) = f
        .engine
        .sign_cleared(cleared, 1, None, || NOW_MS)
        .expect("signs");
    assert_eq!(request.vault_address(), Some(vault()));

    // The same order under a mainnet engine signs for mainnet — the network
    // travels with the engine, not with the call.
    let mainnet_keys = Arc::new(crate::keys::MemoryKeyStore::new(Network::Mainnet));
    mainnet_keys
        .create_agent_key(
            &AgentId::new("beta"),
            crate::keys::SecretText::new(format!("{:064x}", 9)),
            NOW_MS + 90 * 86_400_000,
            NOW_MS,
        )
        .expect("wallet");
    let other_vault = oppen_hl::Address::from_bytes([7; 20]);
    let route = route_for(
        mainnet_keys.as_ref(),
        "beta",
        other_vault,
        Some(other_vault),
    );
    let mainnet = GuardrailEngine::from_parts(
        Arc::new(test_store()),
        Arc::new(NullAuditSink::new([route])),
        mainnet_keys as Arc<dyn KeyStore>,
        Network::Mainnet,
    )
    .expect("engine");
    let other = AgentId::new("beta");
    mainnet.register_agent(&other, NOW_MS).expect("register");
    mainnet
        .operator_set_guardrails(&other, permissive(&["BTC"]), NOW_MS)
        .expect("set guardrails");
    acknowledge(&mainnet);
    let cleared = mainnet
        .evaluate(
            &other,
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &Exposure {
                account: other_vault,
                ..exposure(d("100000"))
            },
            NOW_MS,
        )
        .expect("clears");
    assert_eq!(cleared.clearance().network, Network::Mainnet);
    assert_eq!(cleared.clearance().vault_address, Some(other_vault));
}
/// Internally-tagged enums with newtype variants fail at *serialization*
/// time, not compile time, if the inner value is not a map. `Refusal`
/// wraps `VenueRule` and `Unevaluable` that way, so the taxonomy is
/// serialized here to prove the MCP layer can actually render it, with the
/// tag first and the fields in declaration order (`AGENTS.md` invariants 6
/// and 8).
#[test]
fn every_refusal_shape_serializes_deterministically() {
    let cases = vec![
        Refusal::MissingReason,
        Refusal::SymbolNotAllowed {
            symbol: "BTC".to_owned(),
            allowed: vec![],
        },
        Refusal::OrderNotional {
            symbol: "BTC".to_owned(),
            observed_usd: d("25.01"),
            limit_usd: d("25"),
        },
        Refusal::OrderRate {
            limit: 5,
            window_ms: 300_000,
            tokens_available: d("0.5"),
            retry_after_ms: 60_000,
        },
        Refusal::TradingPaused {
            scope: KillScope::agent("alpha"),
            since_ms: 1,
            reason: KillReason::LossLimit {
                kind: LossKind::Daily,
                observed_usd: d("25"),
                limit_usd: d("25"),
            },
        },
        Refusal::VenueRule(VenueRule::MinNotional {
            notional_usd: d("9"),
            minimum_usd: d("10"),
        }),
        Refusal::Unevaluable(Unevaluable::StaleMarketData {
            symbol: "BTC".to_owned(),
            age_ms: 3_000,
            max_age_ms: 2_000,
        }),
        Refusal::PositionNotional {
            symbol: "BTC".to_owned(),
            observed_usd: d("500"),
            resting_usd: d("400"),
            limit_usd: d("100"),
        },
        Refusal::GlobalRateBudget {
            tokens_available: d("100"),
            reserve: 100,
            retry_after_ms: 10_000,
        },
        Refusal::ApprovalRequired {
            symbol: "BTC".to_owned(),
            notional_usd: d("25"),
            approval_id: "alpha-1-1".to_owned(),
            expires_at_ms: 120_000,
        },
        Refusal::ReasonTooLong {
            len_bytes: 4_096,
            max_bytes: MAX_REASON_BYTES,
        },
        Refusal::ReasonControlCharacter {
            at_byte: 0,
            codepoint: 0x1b,
        },
        Refusal::Unevaluable(Unevaluable::MissingRestingOrders),
        Refusal::Unevaluable(Unevaluable::UnknownProposal {
            approval_id: "alpha-1-1".to_owned(),
        }),
    ];
    for refusal in &cases {
        let json = serde_json::to_string(refusal)
            .unwrap_or_else(|e| panic!("{refusal:?} does not serialize: {e}"));
        assert!(json.starts_with(r#"{"refusal":"#), "{json}");
        // Same value, same bytes, every time.
        assert_eq!(json, serde_json::to_string(refusal).expect("re-serialize"));
        // And the message a human reads is never empty.
        assert!(!refusal.to_string().is_empty());
    }
    assert_eq!(
        serde_json::to_string(&cases[2]).expect("serialize"),
        r#"{"refusal":"order_notional","symbol":"BTC","observed_usd":"25.01","limit_usd":"25"}"#
    );
}

#[test]
fn a_clearance_serializes_for_the_ledger() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    let json = serde_json::to_string(cleared.clearance()).expect("serializes");
    assert!(json.contains(r#""agent":"alpha""#), "{json}");
    assert!(json.contains(r#""cleared":"order""#), "{json}");
}

// ---- the property: no bypass --------------------------------------------

/// xorshift64*, so the case stream is identical on every machine and a
/// failure can be replayed from the seed alone.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        let i = self.below(xs.len() as u64) as usize;
        &xs[i]
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

/// The whole invariant, stated as a property.
///
/// For twenty-four thousand pseudo-random combinations of configuration, market
/// state, account state, clock and intent: if the engine produced a
/// [`Cleared`], then every predicate holds when re-derived here from the
/// action that would actually be signed. Nothing in this function calls back
/// into the engine's own arithmetic, so an error in the engine cannot hide by
/// being made twice.
///
/// The case stream is checked for balance at the end. A property test that
/// only ever produces refusals proves nothing, and one that only ever
/// produces clearances is not testing the guardrails.
#[test]
fn no_input_produces_a_signable_value_without_passing_every_predicate() {
    // The additional cap refuses more samples; keep the existing positive
    // coverage floor by extending the deterministic stream.
    const CASES: usize = 24_000;
    let symbols = ["BTC", "ETH", "SOL"];
    let sizes = ["0.25", "0.5", "1", "2", "5"];
    // Order prices are derived from the market reference, so the generated
    // slippage spans "passive" through "wildly aggressive" instead of being
    // dominated by unrelated price levels.
    let price_factors = ["0.5", "0.99", "1", "1.001", "1.002", "1.02", "1.5"];
    let caps = ["25", "100", "1000", "100000", "1000000"];
    let leverages = [2u32, 5, 40, 50];

    let mut rng = Rng(0x0DDB_1A5E_5BAD_5EED);
    // Keep the original input stream stable while sampling the new policy.
    let mut exposure_cap_rng = Rng(0xE513_0A11_CAFE_0042);
    let mut cleared_count = 0usize;
    let mut refused_count = 0usize;
    let mut unevaluable_count = 0usize;
    let mut capped_clearances = 0usize;
    let mut open_exposure_refusals = 0usize;

    for case in 0..CASES {
        let symbol = *rng.pick(&symbols);
        let allowed: BTreeSet<String> = symbols
            .iter()
            .filter(|_| !rng.chance(4))
            .map(|s| (*s).to_owned())
            .collect();
        let config = AgentGuardrails {
            symbols: allowed,
            max_order_usd: if rng.chance(20) {
                Decimal::ZERO
            } else {
                d(rng.pick(&caps))
            },
            max_position_usd: d(rng.pick(&caps)),
            max_slippage_bps: d(rng.pick(&["0", "5", "10", "20", "200", "10000", "10000"])),
            order_rate: OrderRate {
                count: 1 + rng.below(4) as u32,
                per_ms: 60_000,
            },
            reduce_only: rng.chance(6),
            risk: RiskSettings {
                max_leverage: *rng.pick(&leverages),
                margin_mode: MarginMode::Cross,
                max_open_exposure_usd: exposure_cap_rng
                    .chance(6)
                    .then(|| d(exposure_cap_rng.pick(&["0", "25", "100", "1000", "100000"]))),
                // Mostly unset, because that is the default and the fuzz
                // should spend most of its budget on the paths every agent
                // takes. When set, it is paired below with a sigma that is
                // itself sometimes absent and sometimes zero, so the
                // fail-closed branch is reached by the search rather than
                // only by the test written for it.
                max_risk_usd: rng
                    .chance(6)
                    .then(|| d(rng.pick(&["1", "40", "4000", "40000"]))),
            },
            loss: LossLimits {
                max_daily_loss_usd: rng.chance(2).then(|| d(rng.pick(&["25", "100", "1000"]))),
                max_drawdown_usd: rng.chance(3).then(|| d(rng.pick(&["200", "2000"]))),
            },
            approval_required: rng.chance(6),
            freshness: Freshness::default(),
            max_mark_divergence_bps: d("5"),
            mark_divergence_window_ms: 30_000,
        };

        let f = Fixture::without_keys(config.clone());
        let sz_decimals = rng.below(4) as u32;
        let asset = asset(symbol, sz_decimals, *rng.pick(&leverages));

        let now_ms = NOW_MS + rng.below(120_000);
        let base_px = d(rng.pick(&["50", "100", "150"]));
        let market = MarketRef {
            symbol: symbol.to_owned(),
            reference_px: if rng.chance(16) { None } else { Some(base_px) },
            as_of_ms: now_ms.saturating_sub(rng.below(2_200)),
            quality: *rng.pick(&[
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Ok,
                FeedQuality::Warmup,
                FeedQuality::Degraded,
                FeedQuality::Unusable,
            ]),
            mark_divergence_bps: rng.chance(4).then(|| d(rng.pick(&["1", "6", "40"]))),
            mark_divergent_since_ms: rng
                .chance(2)
                .then(|| now_ms.saturating_sub(rng.below(60_000))),
            snapshot: None,
            sigma_day: if rng.chance(5) {
                None
            } else {
                Some(d(rng.pick(&["0", "0.005", "0.02", "0.08", "0.5"])))
            },
            // Absent, below one, at one, and well above it — the four cases
            // the clamp has to tell apart. A ratio the search never takes
            // below one would leave the widening branch unexplored, which is
            // the direction being wrong costs money in.
            vol_ratio: if rng.chance(3) {
                None
            } else {
                Some(d(rng.pick(&["0.25", "0.9", "1", "1.4", "3", "12"])))
            },
        };

        let mut positions = BTreeMap::new();
        if rng.chance(2) {
            positions.insert(
                symbol.to_owned(),
                PositionSnapshot {
                    szi: d(rng.pick(&["-2", "-0.5", "0.5", "2"])),
                },
            );
        }
        let equity = if rng.chance(30) {
            Decimal::ZERO
        } else {
            d(rng.pick(&["100", "1000", "100000"]))
        };
        let agent_account = AccountSnapshot {
            as_of_ms: now_ms.saturating_sub(rng.below(5_400)),
            reconciled: !rng.chance(16),
            equity_usd: equity,
            peak_equity_usd: equity + d(rng.pick(&["0", "0", "100", "1000"])),
            realized_pnl_today_usd: d(rng.pick(&["-100", "-25", "0", "0", "0", "50"])),
            unrealized_pnl_usd: d(rng.pick(&["-50", "0", "0", "0", "10"])),
            day_start_ms: if rng.chance(16) {
                MIDNIGHT_MS - 86_400_000
            } else {
                MIDNIGHT_MS
            },
            total_position_notional_usd: d(rng.pick(&["0", "50", "500"])),
            positions,
            resting: if rng.chance(16) {
                None
            } else {
                let mut book = RestingExposure::none();
                if rng.chance(2) {
                    let szi = d(rng.pick(&["-2", "-0.5", "0.5", "2"]));
                    let limit_px = base_px * d(rng.pick(&price_factors));
                    book.notional_usd = szi.abs() * limit_px;
                    if szi >= Decimal::ZERO {
                        book.buys.insert(symbol.to_owned(), szi);
                    } else {
                        book.sells.insert(symbol.to_owned(), -szi);
                    }
                    if rng.chance(2) {
                        book.buys.insert(symbol.to_owned(), d("2"));
                        book.sells.insert(symbol.to_owned(), d("2"));
                        book.notional_usd = d("4") * limit_px;
                    }
                    book.notional_by_symbol
                        .insert(symbol.to_owned(), book.notional_usd);
                }
                if rng.chance(2) {
                    book.reduce_buys
                        .insert(symbol.to_owned(), d(rng.pick(&sizes)));
                    book.reduce_sells
                        .insert(symbol.to_owned(), d(rng.pick(&sizes)));
                }
                Some(book)
            },
        };
        let exposure = Exposure {
            account: vault(),
            agent: agent_account,
            fleet: None,
        };

        let mut order = intent(
            symbol,
            rng.chance(2),
            base_px * d(rng.pick(&price_factors)),
            d(rng.pick(&sizes)),
        );
        order.reduce_only = rng.chance(3);
        if rng.chance(5) {
            order.kind = OrderKind::Trigger {
                is_market: true,
                trigger_px: base_px * d(rng.pick(&price_factors)),
                tpsl: Tpsl::Sl,
            };
        }
        if rng.chance(25) {
            order.reason = String::new();
        }

        match f
            .engine
            .evaluate(&f.agent, &order, &asset, &market, &exposure, now_ms)
        {
            Err(refusal) => {
                refused_count += 1;
                if matches!(refusal, Refusal::OpenExposure { .. }) {
                    open_exposure_refusals += 1;
                }
                if refusal.is_unevaluable() {
                    unevaluable_count += 1;
                }
            }
            Ok(cleared) => {
                cleared_count += 1;
                if config.risk.max_open_exposure_usd.is_some() {
                    capped_clearances += 1;
                }
                verify_every_predicate(
                    case, &cleared, &config, &order, &asset, &market, &exposure, now_ms,
                );
            }
        }
    }

    assert!(
        capped_clearances > 0,
        "no clearance exercised the exposure cap oracle"
    );
    assert!(
        open_exposure_refusals > 0,
        "no generated case hit the exposure cap"
    );
    assert!(
        cleared_count > 1_000,
        "only {cleared_count} of {CASES} cases cleared; the property would be vacuous"
    );
    assert!(
        refused_count > 500,
        "only {refused_count} of {CASES} cases were refused; the generator is too permissive"
    );
    assert!(
        unevaluable_count > 100,
        "only {unevaluable_count} fail-closed refusals; the generator is not exercising them"
    );
}

/// Enumerate every subset and ordering of fills, clipping reduce-only at zero.
/// Partial fills cannot improve an extreme: each transition is monotone in
/// position, and monotone in its own size, so zero/full endpoints suffice.
fn reachable_worst(position: Decimal, fills: &[(Decimal, bool)]) -> Decimal {
    fn visit(position: Decimal, fills: &[(Decimal, bool)], used: usize) -> Decimal {
        let mut worst = position.abs();
        for (index, &(signed_size, reduce_only)) in fills.iter().enumerate() {
            if used & (1 << index) != 0 {
                continue;
            }
            let next = if !reduce_only {
                position + signed_size
            } else if position > Decimal::ZERO && signed_size < Decimal::ZERO {
                (position + signed_size).max(Decimal::ZERO)
            } else if position < Decimal::ZERO && signed_size > Decimal::ZERO {
                (position + signed_size).min(Decimal::ZERO)
            } else {
                position
            };
            worst = worst.max(visit(next, fills, used | (1 << index)));
        }
        worst
    }
    visit(position, fills, 0)
}

/// Re-derives every guardrail from the action that would be signed and the
/// inputs that were supplied, independently of the engine.
#[allow(clippy::too_many_arguments)]
fn verify_every_predicate(
    case: usize,
    cleared: &Cleared,
    config: &AgentGuardrails,
    order: &OrderIntent,
    asset: &Asset,
    market: &MarketRef,
    exposure: &Exposure,
    now_ms: u64,
) {
    let ctx = format!("case {case}");
    let wire = wire_of(cleared);
    let px = wire_px(&wire);
    let sz = wire_sz(&wire);
    let account = &exposure.agent;

    // The action is the intent, rounded — not something else entirely.
    assert_eq!(wire.a, asset.index, "{ctx}: wrong asset id");
    assert_eq!(wire.b, order.is_buy, "{ctx}: wrong side");
    assert_eq!(px, asset.round_price(order.px), "{ctx}: price drifted");
    assert_eq!(sz, asset.round_size(order.sz), "{ctx}: size drifted");
    match asset.validate_order(px, sz) {
        Ok(()) => {}
        Err(oppen_hl::meta::ValidationError::MinNotional { .. }) => {
            // Spec 19: the local minimum may defer only for the entire known
            // position. Check actual wire fields, never the engine's helper.
            let position = account.position_szi(&order.symbol);
            assert!(wire.r, "{ctx}: tiny opening order");
            assert!(
                matches!(wire.t, oppen_hl::wire::OrderType::Limit { tif: Tif::Ioc }),
                "{ctx}: tiny non-IOC"
            );
            assert!(!position.is_zero(), "{ctx}: tiny close without a position");
            assert_eq!(
                wire.b,
                position.is_sign_negative(),
                "{ctx}: wrong close side"
            );
            assert_eq!(sz, position.abs(), "{ctx}: tiny partial or oversized close");
        }
        Err(error) => panic!("{ctx}: local wire validation failed: {error}"),
    }

    // Reason, agent, approval.
    assert!(!order.reason.trim().is_empty(), "{ctx}: no reason");
    assert!(
        !config.approval_required,
        "{ctx}: approval mode bypassed — `evaluate` never clears under it"
    );

    // Inputs the engine must have established before measuring anything.
    assert!(market.quality.is_ok(), "{ctx}: cleared off a degraded feed");
    assert!(now_ms >= market.as_of_ms, "{ctx}: market from the future");
    assert!(
        now_ms - market.as_of_ms <= config.freshness.max_market_age_ms,
        "{ctx}: stale market"
    );
    assert!(account.reconciled, "{ctx}: unreconciled account");
    assert!(now_ms >= account.as_of_ms, "{ctx}: account from the future");
    assert!(
        now_ms - account.as_of_ms <= config.freshness.max_account_age_ms,
        "{ctx}: stale account"
    );
    assert!(account.equity_usd > Decimal::ZERO, "{ctx}: no equity");
    assert!(account.covers_day_of(now_ms), "{ctx}: wrong PnL window");
    let reference_px = market.reference_px.expect("a cleared order has a price");
    assert!(
        reference_px > Decimal::ZERO,
        "{ctx}: non-positive reference"
    );
    if let (Some(bps), Some(since)) = (market.mark_divergence_bps, market.mark_divergent_since_ms)
        && bps > config.max_mark_divergence_bps
    {
        assert!(
            now_ms < since || now_ms - since < config.mark_divergence_window_ms,
            "{ctx}: cleared through a sustained mark divergence"
        );
    }

    // The loss budgets.
    let loss = -account.day_pnl_usd();
    if let Some(limit) = config.loss.max_daily_loss_usd {
        assert!(loss < limit, "{ctx}: cleared past the daily budget");
    }
    if let Some(limit) = config.loss.max_drawdown_usd {
        let drawdown = (account.peak_equity_usd - account.equity_usd).max(Decimal::ZERO);
        assert!(drawdown < limit, "{ctx}: cleared past the drawdown budget");
    }

    // Item 24, one predicate at a time.
    assert!(
        config.symbols.contains(&order.symbol),
        "{ctx}: symbol off the allowlist"
    );
    assert!(
        px * sz <= config.max_order_usd,
        "{ctx}: order notional past the cap"
    );
    let position_szi = account.position_szi(&order.symbol);
    let resting = account
        .resting
        .as_ref()
        .expect("a cleared order has a resting book");
    let buys = resting.buys.get(&order.symbol).copied().unwrap_or_default();
    let sells = resting
        .sells
        .get(&order.symbol)
        .copied()
        .unwrap_or_default();
    let signed_sz = if order.is_buy { sz } else { -sz };
    let reduce_buys = resting
        .reduce_buys
        .get(&order.symbol)
        .copied()
        .unwrap_or_default();
    let reduce_sells = resting
        .reduce_sells
        .get(&order.symbol)
        .copied()
        .unwrap_or_default();
    let fills = [
        (buys, false),
        (-sells, false),
        (reduce_buys, true),
        (-reduce_sells, true),
        (signed_sz, order.reduce_only),
    ];
    let before = reachable_worst(position_szi, &fills[..4]);
    let after = reachable_worst(position_szi, &fills);
    let genuine_reduction =
        order.reduce_only && position_szi * signed_sz < Decimal::ZERO && after <= before;
    if let Some(limit) = config.risk.max_open_exposure_usd {
        let candidate_opening = if wire.r {
            Decimal::ZERO
        } else {
            sz * reference_px
        };
        let gross = account.total_position_notional_usd + resting.notional_usd + candidate_opening;
        assert!(
            genuine_reduction || gross <= limit,
            "{ctx}: gross open exposure {gross} exceeds cap {limit}"
        );
    }
    assert!(
        genuine_reduction || after * reference_px <= config.max_position_usd,
        "{ctx}: position notional past the cap"
    );
    // Spec F's vol-scaled cap, re-derived rather than re-read. Unlike every
    // other cap here it binds only when the order grows the position, so the
    // direction is part of the property and not an exemption from it.
    //
    // The sigma re-derived against is the day's *corrected by the hour*. That
    // matters more than it looks: the correction only ever tightens, so a
    // property that divided by the raw `sigma_day` would be satisfied by
    // every order the engine clears whether or not the scaling exists at all
    // — it would pass against the code as it stood before this line, which is
    // the definition of proving nothing.
    if let Some(budget) = config.risk.max_risk_usd {
        let sigma = market
            .sigma_day
            .expect("a cleared order under a vol-scaled cap has a volatility");
        assert!(
            sigma > Decimal::ZERO,
            "{ctx}: cleared against a non-positive volatility"
        );
        let scale = market.vol_ratio.unwrap_or(Decimal::ONE).max(Decimal::ONE);
        let reduces = genuine_reduction
            || (!order.reduce_only
                && after <= before
                && (position_szi + signed_sz).abs() < position_szi.abs());
        if !reduces {
            assert!(
                after.abs() * reference_px <= budget / (Decimal::from(2) * sigma * scale),
                "{ctx}: position past the vol-scaled cap"
            );
        }
    }
    // Sum the other symbols independently; do not repeat the engine's
    // total-minus-contribution arithmetic in the oracle.
    let other_working: Decimal = resting
        .notional_by_symbol
        .iter()
        .filter(|(symbol, _)| *symbol != &order.symbol)
        .map(|(_, usd)| *usd)
        .sum();
    let total_after = (account.total_position_notional_usd - position_szi.abs() * reference_px
        + other_working
        + after * reference_px)
        .max(Decimal::ZERO);
    assert!(
        genuine_reduction
            || total_after / account.equity_usd
                <= Decimal::from(config.risk.max_leverage.min(asset.info.max_leverage)),
        "{ctx}: leverage past the cap"
    );
    if config.reduce_only {
        assert!(order.reduce_only, "{ctx}: reduce-only flag missing");
        assert!(!position_szi.is_zero(), "{ctx}: nothing to reduce");
        assert_ne!(
            position_szi.is_sign_negative(),
            signed_sz.is_sign_negative(),
            "{ctx}: reduce-only order on the same side"
        );
        assert!(
            sz <= position_szi.abs(),
            "{ctx}: reduce-only order oversized"
        );
    }
    let slippage_reference = match &wire.t {
        oppen_hl::wire::OrderType::Trigger { trigger_px, .. } => {
            Decimal::from_str(trigger_px.as_str()).expect("trigger price parses")
        }
        oppen_hl::wire::OrderType::Limit { .. } => reference_px,
    };
    let adverse = if order.is_buy {
        px - slippage_reference
    } else {
        slippage_reference - px
    };
    let slippage_bps = if adverse <= Decimal::ZERO {
        Decimal::ZERO
    } else {
        adverse / slippage_reference * Decimal::from(10_000)
    };
    let slippage_limit = match order.max_slippage_bps {
        Some(agent_limit) => config.max_slippage_bps.min(agent_limit),
        None => config.max_slippage_bps,
    };
    assert!(
        slippage_bps <= slippage_limit,
        "{ctx}: slippage {slippage_bps} past the {slippage_limit} bps cap"
    );

    // The clearance describes what was checked.
    match &cleared.clearance().kind {
        ClearedKind::Order {
            symbol,
            px: cpx,
            sz: csz,
            notional_usd,
            ..
        } => {
            assert_eq!(symbol, &order.symbol, "{ctx}");
            assert_eq!(*cpx, px, "{ctx}");
            assert_eq!(*csz, sz, "{ctx}");
            assert_eq!(*notional_usd, px * sz, "{ctx}");
        }
        other => panic!("{ctx}: expected an order clearance, got {other:?}"),
    }
    assert_eq!(cleared.clearance().agent, AgentId::new("alpha"), "{ctx}");
}

/// The second property, and the one the first is structurally blind to.
///
/// [`no_input_produces_a_signable_value_without_passing_every_predicate`]
/// builds a fresh [`Fixture`] inside its loop, so every case starts with a
/// full token bucket and a clear kill switch: the order-rate cap and item
/// 26's pause are never exercised *across* cases, which is exactly where the
/// bucket over-refill and the approval bypass lived. This run hoists one
/// engine out of the loop and drives a sequence of intents against it on a
/// monotonically advancing clock.
///
/// The bucket is re-derived here from the carry arithmetic directly, not by
/// asking the engine, so an error in the token bucket cannot hide by being
/// made twice. Item 25's breaker and item 26's pause are re-derived the same
/// way: once anything has stopped this agent, nothing may clear again.
#[test]
fn a_sequence_against_one_engine_never_outruns_the_rate_cap_or_the_pause() {
    const STEPS: usize = 20_000;
    /// 3 orders per 30s. Small enough that the cap binds constantly, and
    /// `capacity_micro / per_ms` does not divide evenly — 3,000,000 / 30,000
    /// is exact, so use a window that is not a multiple: 29,999 ms.
    const RATE: OrderRate = OrderRate {
        count: 3,
        per_ms: 29_999,
    };
    const CAPACITY_MICRO: u128 = 3_000_000;

    let mut config = permissive(&["BTC"]);
    config.order_rate = RATE;
    config.loss = LossLimits {
        max_daily_loss_usd: Some(d("100")),
        max_drawdown_usd: None,
    };
    let f = Fixture::new(config.clone());
    // Item 10's budget must not be what refuses anything here.
    f.engine
        .operator_set_global_rate_budget(
            GlobalRateBudget {
                rate: OrderRate {
                    count: 1_000_000,
                    per_ms: 1,
                },
                reserve: 0,
            },
            NOW_MS,
        )
        .expect("set budget");

    let btc = asset("BTC", 2, 40);
    let mut rng = Rng(0x5EED_5EED_0BAD_F00D);

    // An independent model of the bucket: exact numerator carry, no
    // borrowing of the engine's arithmetic.
    let mut tokens_micro: u128 = CAPACITY_MICRO;
    let mut carry: u128 = 0;
    let mut last_ms = NOW_MS;
    let mut stopped_at_step: Option<usize> = None;
    let mut trips = 0usize;
    let mut releases = 0usize;

    let mut now_ms = NOW_MS;
    let mut cleared_count = 0usize;
    let mut rate_refusals = 0usize;
    let mut paused_refusals = 0usize;

    for step in 0..STEPS {
        now_ms += rng.below(3_000);
        // The operator looks at the trip after a while and releases it, so
        // the sequence keeps exercising the rate cap afterwards.
        if let Some(at) = stopped_at_step
            && step - at > 20
        {
            assert!(
                f.engine
                    .operator_release_kill(&KillScope::agent("alpha"), now_ms)
                    .expect("release"),
                "step {step}: the switch should still have been engaged"
            );
            acknowledge(&f.engine);
            stopped_at_step = None;
            releases += 1;
        }
        // Stay inside the UTC day the snapshot's `day_start_ms` names.
        if now_ms >= MIDNIGHT_MS + 86_400_000 {
            break;
        }
        let losing = stopped_at_step.is_none() && rng.chance(400);
        let mut exposure = exposure(d("100000"));
        exposure.agent.as_of_ms = now_ms;
        if losing {
            exposure.agent.realized_pnl_today_usd = d("-100");
        }
        let market = MarketRef::fresh("BTC", d("100"), now_ms);
        let order = intent("BTC", true, d("100"), d("1"));

        let outcome = f
            .engine
            .evaluate(&f.agent, &order, &btc, &market, &exposure, now_ms);

        // Advance the model's bucket to `now_ms` before judging the verdict.
        let numerator = u128::from(now_ms - last_ms) * CAPACITY_MICRO + carry;
        let per = u128::from(RATE.per_ms);
        let gain = numerator / per;
        carry = numerator % per;
        last_ms = now_ms;
        tokens_micro += gain;
        if tokens_micro >= CAPACITY_MICRO {
            tokens_micro = CAPACITY_MICRO;
            carry = 0;
        }

        match outcome {
            Ok(cleared) => {
                assert!(
                    stopped_at_step.is_none(),
                    "step {step}: cleared after the agent was stopped at {now_ms}ms"
                );
                assert!(
                    tokens_micro >= 1_000_000,
                    "step {step}: cleared with {tokens_micro} micro-tokens at {now_ms}ms — \
                     the rate cap was outrun"
                );
                tokens_micro -= 1_000_000;
                assert_eq!(
                    cleared.clearance().utilization.order_tokens_remaining,
                    Decimal::from(tokens_micro) / Decimal::from(1_000_000u64),
                    "step {step}: the engine and the model disagree about the budget"
                );
                cleared_count += 1;
            }
            Err(Refusal::OrderRate { .. }) => {
                assert!(
                    tokens_micro < 1_000_000,
                    "step {step}: refused with {tokens_micro} micro-tokens — a token was lost"
                );
                rate_refusals += 1;
            }
            Err(Refusal::LossLimit { .. }) => {
                assert!(losing, "step {step}: the breaker tripped on a healthy day");
                // Item 25: the trip engages the switch, and item 26 says
                // whose orders to cancel.
                let effects = f.engine.take_pending_kill_effects();
                assert_eq!(effects.len(), 1, "step {step}: no cancels queued");
                assert!(effects[0].cancel_for.contains(&f.agent));
                stopped_at_step = Some(step);
                trips += 1;
                // The refusal costs no token, so the model does not spend one.
            }
            Err(Refusal::TradingPaused { .. }) => {
                assert!(
                    stopped_at_step.is_some(),
                    "step {step}: paused without ever being stopped"
                );
                paused_refusals += 1;
            }
            Err(other) => panic!("step {step}: unexpected refusal {other}"),
        }
    }

    assert!(
        cleared_count > 100,
        "only {cleared_count} clearances; the sequence is not exercising the engine"
    );
    assert!(
        rate_refusals > 100,
        "only {rate_refusals} rate refusals; the cap is never binding"
    );
    assert!(
        paused_refusals > 10,
        "only {paused_refusals} paused refusals; item 26 is never exercised across cases"
    );
    assert!(
        trips > 5 && releases > 5,
        "{trips} trips and {releases} releases; item 25 is barely exercised"
    );
}

// ---- the P3 gate: no signer path without a guardrail check ---------------

fn account_at(equity: Decimal, now_ms: u64) -> AccountSnapshot {
    AccountSnapshot {
        as_of_ms: now_ms,
        ..account(equity)
    }
}

/// Spec item 7 puts a submit queue between the decision and the wire, so
/// "the switch was clear when we evaluated" is not the same statement as
/// "the switch is clear now". The gate re-reads the switch inside the signer,
/// so this order dies there.
#[test]
fn a_kill_switch_engaged_after_the_clearance_still_stops_the_signature() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears while the switch is open");

    f.engine
        .operator_engage_kill(
            KillScope::agent(f.agent.clone()),
            KillReason::Operator,
            NOW_MS + 1,
        )
        .expect("engage");

    match f.engine.sign_cleared(cleared, 1, None, || NOW_MS + 2) {
        Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::PolicyChanged))) => {
            assert!(
                f.engine
                    .kill_switch()
                    .is_engaged(&KillScope::agent(f.agent.clone()))
            );
        }
        other => panic!("a paused agent must not get a signature, got {other:?}"),
    }
}

/// D6 makes the chain the single record of why an order happened. A refusal
/// at the signer arrives *after* `evaluate` already wrote the clearance row,
/// so without its own row the export would show an order cleared and never
/// say why no fill followed — and item 18 names guardrail trips as events.
#[test]
fn a_refusal_at_the_signer_reaches_the_ledger() {
    let sink = Arc::new(CountingSink::new([alpha_route()]));
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(test_store()),
        sink.clone() as Arc<dyn AuditSink>,
    );
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");
    assert_eq!(sink.cleared.load(Ordering::Relaxed), 1);
    assert_eq!(sink.refused.load(Ordering::Relaxed), 0);

    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS + 1)
        .expect("engage");
    let err = f
        .engine
        .sign_cleared(cleared, 1, None, || NOW_MS + 2)
        .expect_err("the signer refuses");
    assert!(matches!(
        err,
        SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::PolicyChanged))
    ));

    // The clearance row still stands — it was true when it was written — and
    // the refusal that overtook it is now beside it.
    assert_eq!(sink.cleared.load(Ordering::Relaxed), 1);
    assert_eq!(sink.refused.load(Ordering::Relaxed), 1);
}

/// A ledger that cannot be written must not turn a refusal into a signature.
/// Same reading as the refusal branch of `record`: losing the row is bad, the
/// safe outcome already happened.
#[test]
fn a_failed_ledger_write_never_turns_a_signer_refusal_into_a_signature() {
    let f = Fixture::with(
        permissive(&["BTC"]),
        Arc::new(test_store()),
        Arc::new(NullAuditSink::new([alpha_route()])),
    );
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears");

    // A second engine over the same store, whose sink fails every write, and
    // which has never paired this agent.
    let broken = GuardrailEngine::from_parts(
        Arc::new(test_store()),
        Arc::new(FailingSink {
            routes: TestRoutes::new([alpha_route()]),
        }),
        key_store(&["alpha"]) as Arc<dyn KeyStore>,
        Network::Testnet,
    )
    .expect("engine");
    let err = broken
        .sign_cleared(cleared, 1, None, || NOW_MS)
        .expect_err("still refuses with the ledger down");
    assert!(matches!(
        err,
        SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::AuditWriteFailed { .. }))
    ));
}

/// The exemption, and only the exemption. Item 26 makes cancelling resting
/// orders part of what engaging the switch *does*, so a cancel that clears
/// while paused must also *sign* while paused — otherwise the switch stops
/// the cure along with the disease.
#[test]
fn a_cancel_still_signs_while_the_kill_switch_is_engaged() {
    let f = Fixture::new(permissive(&["BTC"]));
    f.engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, NOW_MS)
        .expect("engage");

    let cleared = f
        .engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 991 }],
            "stopping",
            NOW_MS,
        )
        .expect("a cancel clears while paused");
    let (request, _) = f
        .engine
        .sign_cleared(cleared, 1, None, || NOW_MS)
        .expect("and it signs while paused");
    assert!(matches!(request.action(), Action::Cancel { .. }));

    // The dead-man's switch is the other risk-reducing action. It is per
    // container (item 27), so it goes to the address it is protecting rather
    // than to a fleet-wide account that does not exist under D1.
    let cleared = f
        .engine
        .clear_schedule_cancel(&f.agent, Some(NOW_MS + 60_000), NOW_MS)
        .expect("the dead-man's switch clears while paused");
    let (request, _) = f
        .engine
        .sign_cleared(cleared, 2, None, || NOW_MS)
        .expect("and it signs");
    assert!(matches!(request.action(), Action::ScheduleCancel { .. }));
    assert_eq!(request.vault_address(), Some(vault()));
}

/// R4: one engine, one network, one database file. A clearance is a verdict
/// about testnet limits measured against testnet positions; signing it for
/// mainnet is the bug R4 calls the worst this product can ship. The gate is
/// the engine itself, so the wrong engine cannot sign it even though the
/// value type is the same.
#[test]
fn a_clearance_cannot_be_signed_through_another_networks_engine() {
    let f = Fixture::new(permissive(&["BTC"]));
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears on testnet");

    let mainnet_keys = Arc::new(crate::keys::MemoryKeyStore::new(Network::Mainnet));
    mainnet_keys
        .create_agent_key(
            &AgentId::new("alpha"),
            crate::keys::SecretText::new(format!("{:064x}", 1)),
            NOW_MS + 90 * 86_400_000,
            NOW_MS,
        )
        .expect("wallet");
    let mainnet = GuardrailEngine::from_parts(
        Arc::new(test_store()),
        Arc::new(NullAuditSink::new([route_for(
            mainnet_keys.as_ref(),
            "alpha",
            vault(),
            Some(vault()),
        )])),
        mainnet_keys as Arc<dyn KeyStore>,
        Network::Mainnet,
    )
    .expect("engine");
    // Same agent, same sub-account: only the network differs.
    mainnet
        .register_agent(&AgentId::new("alpha"), NOW_MS)
        .expect("register");

    match mainnet.sign_cleared(cleared, 1, None, || NOW_MS) {
        Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::WrongNetwork {
            expected,
            supplied,
        }))) => {
            assert_eq!(expected, Network::Mainnet);
            assert_eq!(supplied, Network::Testnet);
        }
        other => panic!("a testnet clearance must not sign for mainnet, got {other:?}"),
    }
}

/// D1 maps the agent roster 1:1 onto containers. An engine that has never
/// paired this agent has never measured a limit against its capital, so it
/// refuses rather than signing on someone else's behalf.
///
/// The identity checked is the clearance's own [`Clearance::agent`], not the
/// `vaultAddress` on the wire — which is why the same refusal is asserted for
/// a **top-level** container, where there is no `vaultAddress` at all (D1 V2)
/// and the address-derived version of this check saw nothing to reject.
#[test]
fn a_clearance_cannot_be_signed_through_an_engine_that_does_not_know_its_agent() {
    let stranger = |container| {
        let mut route = alpha_route();
        route.binding.vault_address = container;
        GuardrailEngine::from_parts(
            Arc::new(test_store()),
            Arc::new(NullAuditSink::new([route])),
            key_store(&["alpha"]) as Arc<dyn KeyStore>,
            Network::Testnet,
        )
        .expect("engine")
    };

    for container in [Some(vault()), None] {
        let f = Fixture::in_container(permissive(&["BTC"]), container);
        let cleared = f
            .evaluate(
                &intent("BTC", true, d("100"), d("1")),
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &exposure(d("100000")),
            )
            .expect("clears");
        assert_eq!(cleared.clearance().vault_address, container);

        match stranger(container).sign_cleared(cleared, 1, None, || NOW_MS) {
            Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::PolicyChanged))) => {}
            other => panic!("an unknown agent must not be signed for, got {other:?}"),
        }
    }
}

/// **The seal on `AGENTS.md` invariant 1, asserted on the type system.**
///
/// `GuardrailEngine` is public and `ExchangeRequest::sign_checked` is public.
/// While the engine also implemented `PreSignCheck`, those three facts
/// composed into a complete bypass: a caller wrote
///
/// ```text
/// ExchangeRequest::sign_checked(&key, whatever_action, nonce, Some(vault), None, network, &engine)
/// ```
///
/// and the real guardrail engine signed an action it had never evaluated. The
/// gate is handed the assembled wire request and nothing else, so it had no
/// way to tell that action from one of its own — the engine was its own
/// rubber stamp.
///
/// The fix is that the shipped checker is a private type inside `engine.rs`,
/// so that call no longer compiles. A compile error is not observable from a
/// test that has to compile, so the property is asserted the only way it can
/// be: **`GuardrailEngine` does not implement the trait.** The probe is the
/// autoref-specialization shape `impls!` uses — the inherent method applies
/// only when the bound holds, and the blanket trait method answers otherwise.
///
/// The second assertion is the one that keeps this honest. A probe that
/// always answered `false` would pass whatever the engine does, so a type
/// that *does* implement the trait is put through the same probe.
#[test]
fn the_guardrail_engine_is_not_a_checker_a_caller_can_hand_to_the_signer() {
    use oppen_hl::exchange::{PreSign, PreSignCheck};
    use std::marker::PhantomData;

    struct Probe<T>(PhantomData<T>);

    trait NotAChecker {
        fn implements_pre_sign_check(&self) -> bool {
            false
        }
    }
    impl<T> NotAChecker for Probe<T> {}
    impl<T: PreSignCheck> Probe<T> {
        fn implements_pre_sign_check(&self) -> bool {
            true
        }
    }

    /// The kind of no-op gate the module doc admits anyone may still write.
    /// Here it is only the probe's positive control.
    struct RubberStamp;
    impl PreSignCheck for RubberStamp {
        type Refusal = Refusal;
        fn check(&self, _request: PreSign<'_>) -> Result<(), Refusal> {
            Ok(())
        }
    }

    assert!(
        Probe::<RubberStamp>(PhantomData).implements_pre_sign_check(),
        "the probe cannot see a PreSignCheck impl at all; the assertion below proves nothing"
    );
    assert!(
        !Probe::<GuardrailEngine>(PhantomData).implements_pre_sign_check(),
        "GuardrailEngine implements PreSignCheck, so a caller can pass it to \
         ExchangeRequest::sign_checked with any Action it likes and be handed a signature \
         for an order the engine never evaluated (AGENTS.md invariant 1)"
    );
}

/// **The gate's identity comes off the clearance, never off `vaultAddress`.**
///
/// The revised D1 (V2) gives each agent its own *top-level* venue account on
/// Hyperliquid, and a top-level account sends no `vaultAddress` at all. The
/// gate used to re-derive the agent by scanning the registry for that field
/// and read an absent one as "the master account", where only a *global*
/// engagement speaks. So an operator stopping exactly this agent — the
/// narrowest, most deliberate use of the switch there is — did not stop the
/// only container the agent has, and the order was signed while it was
/// paused.
#[test]
fn an_agent_scoped_kill_stops_a_top_level_containers_signature() {
    let f = Fixture::in_container(permissive(&["BTC"]), None);
    let cleared = f
        .evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears while the switch is open");
    assert_eq!(
        cleared.clearance().vault_address,
        None,
        "a top-level container carries no vaultAddress"
    );
    assert_eq!(cleared.clearance().agent, f.agent);

    f.engine
        .operator_engage_kill(
            KillScope::agent(f.agent.clone()),
            KillReason::Operator,
            NOW_MS + 1,
        )
        .expect("engage");

    match f.engine.sign_cleared(cleared, 1, None, || NOW_MS + 2) {
        Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::PolicyChanged))) => {
            assert!(
                f.engine
                    .kill_switch()
                    .is_engaged(&KillScope::agent(f.agent.clone()))
            );
        }
        other => panic!("a paused agent's top-level container must not sign, got {other:?}"),
    }
}
/// **A clearance is a verdict about the market it was measured against, and
/// it expires.**
///
/// `sign_cleared` took `now_ms` and never compared it to
/// `Clearance::evaluated_at_ms`, so a `Cleared` held across spec item 7's
/// submit queue could be signed against caps evaluated on an arbitrarily old
/// book — an hour later, a day later. The budget is the agent's own
/// `freshness.max_market_age_ms`: the operator has already stated there how
/// stale a view of the market may be before this agent must not act on it,
/// and a decision taken on that view is no fresher than the view was.
#[test]
fn an_order_clearance_expires_between_the_evaluation_and_the_signature() {
    let config = permissive(&["BTC"]);
    let budget_ms = config.freshness.max_market_age_ms;
    let f = Fixture::new(config);
    let clear = || {
        f.evaluate(
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
        )
        .expect("clears")
    };

    match f
        .engine
        .sign_cleared(clear(), 1, None, || NOW_MS + budget_ms + 1)
    {
        Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::StaleClearance {
            age_ms,
            max_age_ms,
        }))) => {
            assert_eq!(age_ms, budget_ms + 1);
            assert_eq!(max_age_ms, budget_ms);
        }
        other => panic!("a stale clearance must not be signed, got {other:?}"),
    }

    // The boundary is inclusive, so the refusal above is the age and not an
    // off-by-one that would refuse every order at the limit.
    f.engine
        .sign_cleared(clear(), 2, None, || NOW_MS + budget_ms)
        .expect("a clearance exactly at the budget still signs");

    // A clock that moved backwards is unevaluable, not "zero milliseconds
    // old": saturating the age would make a rewound clock the one way to sign
    // anything, however old.
    let later = f
        .engine
        .evaluate(
            &f.agent,
            &intent("BTC", true, d("100"), d("1")),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS + 1),
            &exposure(d("100000")),
            NOW_MS + 1,
        )
        .unwrap();
    match f.engine.sign_cleared(later, 3, None, || NOW_MS) {
        Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::ClockWentBackwards {
            now_ms,
            last_ms,
        }))) => {
            assert_eq!(now_ms, NOW_MS);
            assert_eq!(last_ms, NOW_MS + 1);
        }
        other => panic!("a rewound clock must not sign, got {other:?}"),
    }

    // The exemption, and the reason for it: a cancel was never priced off a
    // market tick, and item 10 keeps risk-reducing actions unblockable. An
    // hour-old cancel is still the right thing to send.
    let cancel = f
        .engine
        .clear_cancel(
            &f.agent,
            vec![CancelWire { a: 7, o: 991 }],
            "stopping",
            NOW_MS,
        )
        .expect("a cancel clears");
    f.engine
        .sign_cleared(cancel, 4, None, || NOW_MS + 3_600_000)
        .expect("a cancel does not go stale");
}

/// **The signing key is not a parameter, so it cannot be the wrong one.**
///
/// Under the revised D1 (V2) a Hyperliquid container is a *top-level* account
/// that sends no `vaultAddress`, so the venue reads the account off the
/// signature: the agent key **is** the container. While `sign_cleared` took a
/// key, the identity the engine went to such lengths to bind into the
/// clearance — the sub-account, the network — was the one that is `None` on
/// every v1 container, and the one that decides where the order actually
/// lands was supplied by the caller. Agent X's clearance signed with agent
/// Y's key executed on Y's capital under X's caps, X's equity and X's kill
/// switch.
///
/// The exploit cannot be written any more: there is no parameter to pass. So
/// the property is asserted from the other side — the signature is a function
/// of the *agent named by the clearance*, and of the wallet that agent holds:
///
/// 1. two agents on one engine, given the identical order and nonce, get
///    different signatures (with a caller-supplied key both were the caller's
///    single key, and the two were byte-identical);
/// 2. an engine whose store gives `alpha` `beta`'s wallet signs `alpha`'s
///    clearance with `beta`'s signature — so the key is read from the named
///    agent's record and not from anywhere else.
#[test]
fn a_clearance_is_signed_by_the_wallet_of_the_agent_it_names() {
    let sign_for = |keys: Arc<crate::keys::MemoryKeyStore>, agent: &str| -> String {
        let engine = GuardrailEngine::from_parts(
            Arc::new(test_store()),
            Arc::new(NullAuditSink::new([route_for(
                keys.as_ref(),
                agent,
                vault(),
                None,
            )])),
            keys as Arc<dyn KeyStore>,
            Network::Testnet,
        )
        .expect("engine");
        let agent = AgentId::new(agent);
        // Top-level containers: no `vaultAddress`, so nothing but the key
        // distinguishes where this order lands.
        engine.register_agent(&agent, NOW_MS).expect("register");
        engine
            .operator_set_guardrails(&agent, permissive(&["BTC"]), NOW_MS)
            .expect("rails");
        acknowledge(&engine);
        let cleared = engine
            .evaluate(
                &agent,
                &intent("BTC", true, d("100"), d("1")),
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &exposure(d("100000")),
                NOW_MS,
            )
            .expect("clears");
        assert_eq!(cleared.clearance().agent, agent);
        assert_eq!(cleared.clearance().vault_address, None);
        let (request, _) = engine
            .sign_cleared(cleared, 1, None, || NOW_MS)
            .expect("signs");
        request.signature().to_hex()
    };

    let pair = key_store(&["alpha", "beta"]);
    assert_ne!(
        signer_of(&pair, "alpha"),
        signer_of(&pair, "beta"),
        "the fixture must give the two agents different wallets"
    );
    let alpha = sign_for(pair.clone(), "alpha");
    let beta = sign_for(pair.clone(), "beta");
    assert_ne!(
        alpha, beta,
        "the same order and nonce for two agents produced one signature, so the key \
         did not follow the clearance's agent"
    );

    // Give `alpha` the wallet `beta` had. If the key is read from the named
    // agent's record, alpha's signature becomes beta's.
    let swapped = Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet));
    swapped
        .create_agent_key(
            &AgentId::new("alpha"),
            crate::keys::SecretText::new(format!("{:064x}", 2)),
            NOW_MS + 90 * 86_400_000,
            NOW_MS,
        )
        .expect("wallet");
    assert_eq!(signer_of(&swapped, "alpha"), signer_of(&pair, "beta"));
    assert_eq!(
        sign_for(swapped, "alpha"),
        beta,
        "the signature did not follow the wallet in the named agent's record"
    );
}
/// **One proposal is one approval.**
///
/// [`GuardrailEngine::operator_approve_proposal`] read the proposal under one
/// lock and removed it under another, with a full evaluation and a ledger
/// write in between. Two callers approving one id both walked out with a
/// [`Cleared`] — and `Mode::Approved` charges neither an order-rate token, so
/// a cap of one order per hour signed two.
///
/// The window is held open deliberately rather than raced for: the sink stops
/// the first clearance row until the second approval has been attempted, so
/// the interleaving is certain on every run.
#[test]
fn one_proposal_authorises_exactly_one_approval() {
    /// Parks the first *clearance* row until the test releases it. Two
    /// barriers rather than one so the test can wait for the parking to have
    /// happened before it acts, which is what makes the interleaving certain
    /// rather than a race the spawn order decides.
    struct HoldFirstClearance {
        routes: TestRoutes,
        parked: std::sync::Barrier,
        release: std::sync::Barrier,
        held: std::sync::atomic::AtomicBool,
    }
    impl AuditSink for HoldFirstClearance {
        fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute, Refusal> {
            self.routes.get(agent)
        }
        fn before_sign(
            &self,
            clearance: &Clearance,
            wallet: &crate::keys::AgentWallet,
            signer: oppen_hl::Address,
        ) -> Result<Box<dyn SigningPermit + '_>, Refusal> {
            self.routes.permit(clearance, wallet, signer)
        }

        fn record(&self, entry: &AuditEntry<'_>) -> Result<(), AuditError> {
            if matches!(entry.outcome, AuditOutcome::Cleared(_))
                && !self.held.swap(true, Ordering::SeqCst)
            {
                self.parked.wait();
                self.release.wait();
            }
            Ok(())
        }
    }

    let sink = Arc::new(HoldFirstClearance {
        routes: TestRoutes::new([alpha_route()]),
        parked: std::sync::Barrier::new(2),
        release: std::sync::Barrier::new(2),
        held: std::sync::atomic::AtomicBool::new(false),
    });
    let mut config = permissive(&["BTC"]);
    config.approval_required = true;
    config.order_rate = OrderRate {
        count: 1,
        per_ms: 3_600_000,
    };
    let engine = Arc::new(
        GuardrailEngine::from_parts(
            Arc::new(test_store()),
            sink.clone() as Arc<dyn AuditSink>,
            key_store(&["alpha"]) as Arc<dyn KeyStore>,
            Network::Testnet,
        )
        .expect("engine"),
    );
    let alpha = AgentId::new("alpha");
    engine.register_agent(&alpha, NOW_MS).expect("register");
    engine
        .operator_set_guardrails(&alpha, config, NOW_MS)
        .expect("rails");
    acknowledge(&engine);

    let Err(Refusal::ApprovalRequired { approval_id, .. }) = engine.evaluate(
        &alpha,
        &intent("BTC", true, d("100"), d("1")),
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", d("100"), NOW_MS),
        &exposure(d("100000")),
        NOW_MS,
    ) else {
        panic!("expected a queued proposal");
    };

    let approve = |engine: &GuardrailEngine| {
        engine.operator_approve_proposal(
            &approval_id,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
    };
    let first = std::thread::spawn({
        let engine = engine.clone();
        let id = approval_id.clone();
        move || {
            engine.operator_approve_proposal(
                &id,
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &exposure(d("100000")),
                NOW_MS,
            )
        }
    });
    // Wait until that approval is parked inside its ledger write, which is
    // after it has taken (or read) the proposal and before it has recorded
    // anything. That is the widest this window ever is.
    sink.parked.wait();
    let second = approve(&engine);
    sink.release.wait();
    let first = first.join().expect("the first approval thread");

    assert!(first.is_ok(), "the first approval must succeed");
    assert!(
        matches!(
            second,
            Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
        ),
        "a second approval of one proposal minted a second clearance past a cap of \
         one order per hour, got {second:?}"
    );
    assert!(engine.pending_proposals(NOW_MS).unwrap().is_empty());
}

/// **The dead-man's switch is armed per container.**
///
/// Item 27: "armed per container, N times, not once… no container is covered
/// by another's arming". The intent was computed from the count of active
/// agents fleet-wide and the clearance named no agent at all, so one active
/// agent armed on behalf of a roster and every other container's resting
/// orders were left unwatched while the console reported them covered.
#[test]
fn the_dead_man_switch_is_armed_per_container() {
    let engine = GuardrailEngine::from_parts(
        Arc::new(test_store()),
        Arc::new(NullAuditSink::new([alpha_route()])),
        key_store(&["alpha", "beta"]) as Arc<dyn KeyStore>,
        Network::Testnet,
    )
    .expect("engine");
    let alpha = AgentId::new("alpha");
    let beta = AgentId::new("beta");
    engine.register_agent(&alpha, NOW_MS).expect("alpha");
    engine.register_agent(&beta, NOW_MS).expect("beta");
    engine.set_agent_active(&alpha, true);

    assert!(matches!(
        engine.dead_man_intent(&alpha, NOW_MS, None),
        DeadManIntent::Arm { .. }
    ));
    assert_eq!(
        engine.dead_man_intent(&beta, NOW_MS, None),
        DeadManIntent::Hold,
        "beta's container is not covered by alpha's arming"
    );

    // The arm carries alpha's container, so the request goes to the address it
    // is meant to protect.
    let cleared = engine
        .clear_schedule_cancel(&alpha, Some(NOW_MS + 60_000), NOW_MS)
        .expect("arms");
    assert_eq!(cleared.clearance().agent, alpha);
    assert_eq!(cleared.clearance().vault_address, Some(vault()));

    // And an agent the engine has never paired cannot arm anything.
    assert!(matches!(
        engine.clear_schedule_cancel(&AgentId::new("ghost"), Some(NOW_MS + 60_000), NOW_MS),
        Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
    ));
}

/// **The other clearance that goes stale, and the one that does not.**
///
/// The freshness check was scoped to [`ClearedKind::Order`], on the reasoning
/// that only an order is priced off a market tick. An *arm* of the dead-man's
/// switch is not priced off a tick but it is priced off the clock: it clears
/// against the venue's five-second minimum lead measured at evaluation, and
/// item 7's submit queue eats into that lead. Held long enough the arm is one
/// the venue rejects — and `deadman.rs` says silently failing to arm is the
/// worst outcome there, so it is refused loudly and re-armed with a fresh
/// lead. A *disarm* carries no time and never goes stale.
#[test]
fn an_arm_whose_lead_has_run_out_is_refused_at_the_signer() {
    let f = Fixture::new(permissive(&["BTC"]));
    let arm = || {
        f.engine
            .clear_schedule_cancel(&f.agent, Some(NOW_MS + 60_000), NOW_MS)
            .expect("arms")
    };

    // 56 s later the lead is 4 s, inside the venue's 5 s minimum.
    match f.engine.sign_cleared(arm(), 1, None, || NOW_MS + 56_000) {
        Err(SignClearedError::Refused(Refusal::VenueRule(VenueRule::ScheduleCancelTooSoon {
            cancel_at_ms,
            earliest_ms,
        }))) => {
            assert_eq!(cancel_at_ms, NOW_MS + 60_000);
            assert_eq!(earliest_ms, NOW_MS + 56_000 + DEAD_MAN_MIN_LEAD_MS);
        }
        other => panic!("an arm the venue would reject must not be signed, got {other:?}"),
    }
    // Exactly at the minimum still signs, so the refusal above is the lead and
    // not an off-by-one.
    f.engine
        .sign_cleared(arm(), 2, None, || NOW_MS + 55_000)
        .expect("an arm still clear of the minimum signs");

    let disarm = f
        .engine
        .clear_schedule_cancel(&f.agent, None, NOW_MS)
        .expect("disarms");
    f.engine
        .sign_cleared(disarm, 3, None, || NOW_MS + 3_600_000)
        .expect("a disarm carries no deadline and never goes stale");
}

/// **The residual the caps carry, and the obligation that closes it.**
///
/// A cleared order is exposure the agent has committed to, and it is not in
/// [`RestingExposure`] until the venue acknowledges it. Nothing in this engine
/// can know when that happened, so five clearances taken against the same flat
/// book each pass the position cap on their own and sign to $125 against a
/// $100 cap — inside the freshness window and inside the order-rate cap, so
/// neither of those bounds it either. Only the component that sent them knows
/// what is outstanding: spec item 7's submit queue.
///
/// So the cap is exactly as good as the working book the caller reports, and
/// this pins that it *is* good when the obligation is met — the caller folds
/// what it has sent and not yet seen acknowledged into
/// [`AccountSnapshot::resting`], and the order that would breach is refused
/// naming the resting notional it counted. Inventing an in-engine substitute
/// was rejected: every approximation either double-counts an order once the
/// venue's snapshot catches up — spurious refusals, and a cap the caller
/// cannot predict — or leaks entries for clearances that were never signed.
#[test]
fn the_position_cap_holds_when_the_caller_reports_what_it_has_in_flight() {
    let mut config = permissive(&["BTC"]);
    config.max_order_usd = d("25");
    config.max_position_usd = d("100");
    let f = Fixture::new(config);
    let btc = asset("BTC", 2, 40);

    // Four orders of $25 fill the $100 cap exactly, each one reported into the
    // working book before the next is asked for.
    let mut in_flight = Decimal::ZERO;
    for n in 1..=4 {
        let exp = with_resting(exposure(d("100000")), "BTC", in_flight, d("100"));
        f.engine
            .evaluate(
                &f.agent,
                &intent("BTC", true, d("100"), d("0.25")),
                &btc,
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &exp,
                NOW_MS,
            )
            .unwrap_or_else(|e| panic!("order {n} of $25 is inside a $100 cap, got {e:?}"));
        in_flight += d("0.25");
    }

    let exp = with_resting(exposure(d("100000")), "BTC", in_flight, d("100"));
    match f.engine.evaluate(
        &f.agent,
        &intent("BTC", true, d("100"), d("0.25")),
        &btc,
        &MarketRef::fresh("BTC", d("100"), NOW_MS),
        &exp,
        NOW_MS,
    ) {
        Err(Refusal::PositionNotional {
            observed_usd,
            resting_usd,
            limit_usd,
            ..
        }) => {
            assert_eq!(observed_usd, d("125.00"));
            assert_eq!(resting_usd, d("100.00"));
            assert_eq!(limit_usd, d("100"));
        }
        other => panic!("the fifth $25 order breaches a $100 cap, got {other:?}"),
    }
}

/// **The P3 gate stated as a property.**
///
/// Thousands of generated intents driven at one engine with an advancing
/// clock, with the kill switch pressed and released *between* each
/// evaluation and its signature so the two do not always see the same world.
/// Three things are asserted on every case:
///
/// 1. A refusal produces no [`Cleared`], so `sign_cleared` cannot be called.
///    That half is enforced by the compiler, not by this loop — the `Err`
///    arm below has no signing call in it because none can be written.
/// 2. A signature is produced only when the gate passed, and a pause between
///    the evaluation and the signature refuses at the signer.
/// 3. Signatures never outnumber the clearances the ledger recorded.
///
/// **What this proves.** No input in the generated space reaches the signer
/// through `oppen-core` without exactly one evaluation, and a signature
/// always has a ledger row behind it.
///
/// **What it cannot prove.** That nothing else signs. `sign_unchecked` is
/// `pub` for spec item 33's manual path and a foreign crate can implement a
/// no-op [`PreSignCheck`]; see
/// [`no_call_site_in_oppen_core_reaches_the_signer_unchecked`] for the
/// grep-shaped half of the argument, and the module doc for the honest
/// statement of the residual.
#[test]
fn no_generated_intent_reaches_the_signer_without_an_evaluation() {
    const CASES: u64 = 3_000;
    let sink = Arc::new(CountingSink::new([alpha_route()]));
    let mut config = permissive(&["BTC", "ETH"]);
    config.max_order_usd = d("5000");
    config.max_position_usd = d("50000");
    config.max_slippage_bps = d("500");
    // Loose enough that most cases clear, tight enough that the cap still
    // bites: one token per 750 ms against a case every 900 ms.
    config.order_rate = OrderRate {
        count: 4,
        per_ms: 3_000,
    };
    let f = Fixture::with(
        config,
        Arc::new(test_store()),
        sink.clone() as Arc<dyn AuditSink>,
    );

    let mut rng = Rng(0x0000_0000_5161_1ED0);
    let mut signed = 0u64;
    let mut refused_at_signer = 0u64;
    let mut refused_before_signer = 0u64;
    let mut nonce = 1u64;

    for case in 0..CASES {
        // 45 minutes in total, so `MIDNIGHT_MS` stays the correct UTC day
        // start and the loss window never mismatches.
        let now_ms = NOW_MS + case * 900;
        if rng.chance(3) {
            f.engine
                .operator_release_kill(&KillScope::Global, now_ms)
                .expect("release");
            acknowledge(&f.engine);
        }

        // "DOGE" is off the allowlist and the wide sizes and prices push
        // past the notional and slippage caps, so both branches are fed.
        let symbol = *rng.pick(&["BTC", "BTC", "ETH", "DOGE"]);
        let px = d(rng.pick(&["95", "100", "101", "180"]));
        let sz = d(rng.pick(&["0.1", "1", "9", "900"]));
        let mut order = intent(symbol, rng.chance(2), px, sz);
        order.reason = format!("case {case}");
        let market = MarketRef::fresh(symbol, d("100"), now_ms);
        let instrument = asset(symbol, 2, 40);
        let exposure = Exposure {
            account: vault(),
            agent: account_at(d("1000000"), now_ms),
            fleet: None,
        };

        let cleared =
            match f
                .engine
                .evaluate(&f.agent, &order, &instrument, &market, &exposure, now_ms)
            {
                Ok(cleared) => cleared,
                Err(_) => {
                    // No `Cleared` exists, so there is nothing to sign. This arm
                    // deliberately contains no signing call: `sign_cleared` takes
                    // a `Cleared` by value and there is no public constructor,
                    // so the bypass is not something this test has to police —
                    // it is something the compiler refuses to compile.
                    refused_before_signer += 1;
                    continue;
                }
            };

        // The gap spec item 7's submit queue lives in.
        if rng.chance(8) {
            f.engine
                .operator_engage_kill(KillScope::Global, KillReason::Operator, now_ms)
                .expect("engage");
        }
        let paused = f.engine.kill_switch().blocking(&f.agent).is_some();

        match f.engine.sign_cleared(cleared, nonce, None, || now_ms) {
            Ok((request, clearance)) => {
                assert!(!paused, "case {case}: a paused agent got a signature");
                assert_eq!(request.nonce(), nonce, "case {case}");
                assert_eq!(request.vault_address(), Some(vault()), "case {case}");
                assert_eq!(clearance.network, Network::Testnet, "case {case}");
                assert!(
                    matches!(clearance.kind, ClearedKind::Order { .. }),
                    "case {case}"
                );
                signed += 1;
            }
            Err(SignClearedError::Refused(Refusal::Unevaluable(Unevaluable::PolicyChanged))) => {
                assert!(
                    paused,
                    "case {case}: refused at the signer with the switch open"
                );
                refused_at_signer += 1;
            }
            Err(other) => panic!("case {case}: unexpected {other}"),
        }
        nonce += 1;
    }

    let recorded_clearances = sink.cleared.load(Ordering::Relaxed) as u64;
    let recorded_refusals = sink.refused.load(Ordering::Relaxed) as u64;

    // The invariant: a signature always has exactly one recorded evaluation
    // behind it, and some evaluations end without one.
    assert_eq!(
        recorded_clearances,
        signed + refused_at_signer,
        "every clearance was either signed or refused at the signer"
    );
    // Item 18 and D6: a refusal at the signer lands in the ledger too, or an
    // export would show a clearance with no fill and no explanation.
    assert_eq!(
        recorded_refusals,
        refused_before_signer + refused_at_signer,
        "every refusal, wherever it happened, has a row"
    );
    assert_eq!(signed + refused_at_signer + refused_before_signer, CASES);
    assert!(
        signed < recorded_clearances,
        "the signer gate never refused anything; the property is untested"
    );

    // Both branches have to be genuinely populated or the assertions above
    // are vacuous.
    assert!(signed > 500, "only {signed} signatures");
    assert!(
        refused_before_signer > 500,
        "only {refused_before_signer} refusals before the signer"
    );
    assert!(
        refused_at_signer > 20,
        "only {refused_at_signer} refusals at the signer"
    );
}

/// The other half of `AGENTS.md` invariant 1, which no type can express:
/// **`oppen-core` must not call the ungated signer at all.**
///
/// `oppen_hl::ExchangeRequest::sign_unchecked` and
/// `AgentKey::sign_l1_action` stay `pub` because spec item 33's manual
/// escape hatch needs them from `oppen-hl`'s own example. Nothing in this
/// crate may use them: every signature here goes through
/// [`GuardrailEngine::sign_cleared`], which passes the engine as the gate.
///
/// The needles are built at runtime so this file does not match itself, and
/// comment lines are skipped so the doc comments that name the hazard are
/// not mistaken for call sites. The check is a grep, and a grep is exactly
/// as strong as the claim being made: a bypass cannot happen by accident,
/// and a deliberate one is visible.
#[test]
fn no_call_site_in_oppen_core_reaches_the_signer_unchecked() {
    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .expect("the crate's own src directory is readable")
            .map(|e| e.expect("directory entry").path())
            .collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    let needles = [
        format!("sign_{}", "unchecked"),
        format!("sign_{}", "l1_action"),
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(files.len() > 5, "the source walk found almost nothing");

    let mut offenders = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("source file is utf-8");
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for needle in &needles {
                if code.contains(needle.as_str()) {
                    offenders.push(format!("{}:{}: {}", path.display(), n + 1, code));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "oppen-core must reach the signer only through GuardrailEngine::sign_cleared \
         (AGENTS.md invariant 1). Ungated call sites:\n{}",
        offenders.join("\n")
    );
}

// ======================= ADVERSARIAL ATTACK PROBES =======================

// The literal bypass — `ExchangeRequest::sign_checked(.., &engine)` — is not a
// test here because it DOES NOT COMPILE, which is the point. `rustc` reports:
//
//     error[E0277]: the trait bound `GuardrailEngine: PreSignCheck` is not
//     satisfied --> the trait `PreSignCheck` is not implemented for
//     `engine::GuardrailEngine`
//
// The seal is the absence of that impl: `PreSignGate` is private to engine.rs
// and only `sign_cleared` constructs one, so a caller has no checker to hand
// the signer. `the_guardrail_engine_is_not_a_checker_a_caller_can_hand_to_the_signer`
// asserts that absence from inside the crate, where it can be checked without
// a compile-fail harness.
