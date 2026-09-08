use super::*;

#[path = "approval_review_tests.rs"]
mod approval_review_tests;

use std::path::Path;
use std::sync::atomic::AtomicU64;

use crate::keys::{HmacKey, MemoryKeyStore, SecretText};
use crate::ledger::{
    Anchor, Event, EventKind, FileAnchor, HeadAnchor, Ledger, LedgerError, PolicyJournal,
    RegistryJournal,
};
use tempfile::TempDir;

#[derive(Debug)]
struct FailSequenceAnchor {
    inner: FileAnchor,
    fail_seq: Arc<AtomicU64>,
    failures: Arc<AtomicUsize>,
}

impl HeadAnchor for FailSequenceAnchor {
    fn load(&self) -> Result<Option<Anchor>, LedgerError> {
        self.inner.load()
    }

    fn store(&self, anchor: &Anchor) -> Result<(), LedgerError> {
        // Permit publication repair of the old head; fail only the new commit.
        if anchor.seq != 0 && anchor.seq == self.fail_seq.load(Ordering::SeqCst) {
            self.failures.fetch_add(1, Ordering::SeqCst);
            return Err(LedgerError::Io(std::io::Error::other(
                "synthetic approval publication failure",
            )));
        }
        self.inner.store(anchor)
    }
}

struct DurableFixture {
    dir: TempDir,
    engine: GuardrailEngine,
    policy: Arc<PolicyJournal>,
    ledger: Arc<Ledger>,
    keys: Arc<MemoryKeyStore>,
    fail_seq: Arc<AtomicU64>,
    failures: Arc<AtomicUsize>,
}

fn authority_key() -> Arc<HmacKey> {
    Arc::new(HmacKey::from_bytes([31; 32]))
}

fn open_approval_ledger(
    path: &Path,
    fail_seq: Arc<AtomicU64>,
    failures: Arc<AtomicUsize>,
) -> Arc<Ledger> {
    Arc::new(
        Ledger::open_anchored(
            path,
            Network::Testnet,
            Some(Box::new(FailSequenceAnchor {
                inner: FileAnchor::beside(path),
                fail_seq,
                failures,
            })),
        )
        .unwrap(),
    )
}

impl DurableFixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("approval.db");
        let fail_seq = Arc::new(AtomicU64::new(0));
        let failures = Arc::new(AtomicUsize::new(0));
        let ledger = open_approval_ledger(&path, fail_seq.clone(), failures.clone());
        let keys = key_store(&["alpha"]);
        let registry = RegistryJournal::open(ledger.clone(), authority_key()).unwrap();
        registry
            .grant(
                route_for(keys.as_ref(), "alpha", vault(), Some(vault())).binding,
                NOW_MS,
            )
            .unwrap();
        let policy = initialize_policy(registry, &dir.path().join("absent-legacy.db"));
        let current = policy.current().unwrap();
        let mut next = current.state;
        assert!(next.kill.release(&KillScope::Global));
        let mut config = permissive(&["BTC"]);
        config.approval_required = true;
        next.guardrails.insert(AgentId::new("alpha"), config);
        policy.replace(current.revision, next, NOW_MS).unwrap();
        let engine = GuardrailEngine::new(policy.clone(), keys.clone()).unwrap();
        assert!(engine.policy_status().admission_inhibited);
        acknowledge(&engine);
        Self {
            dir,
            engine,
            policy,
            ledger,
            keys,
            fail_seq,
            failures,
        }
    }

    fn reopen(self) -> Self {
        let Self {
            dir,
            engine,
            policy,
            ledger,
            keys,
            fail_seq,
            failures,
        } = self;
        drop(engine);
        drop(policy);
        drop(ledger);
        let ledger = open_approval_ledger(
            &dir.path().join("approval.db"),
            fail_seq.clone(),
            failures.clone(),
        );
        let registry = RegistryJournal::open(ledger.clone(), authority_key()).unwrap();
        let policy = Arc::new(PolicyJournal::new(Arc::new(registry)));
        let engine = GuardrailEngine::new(policy.clone(), keys.clone()).unwrap();
        Self {
            dir,
            engine,
            policy,
            ledger,
            keys,
            fail_seq,
            failures,
        }
    }

    fn evaluate(&self, order: &OrderIntent) -> Result<Cleared, Refusal> {
        self.engine.evaluate(
            &AgentId::new("alpha"),
            order,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
    }

    fn propose(&self) -> Proposal {
        let order = approval_order();
        let Err(Refusal::ApprovalRequired {
            approval_id,
            expires_at_ms,
            ..
        }) = self.evaluate(&order)
        else {
            panic!("a recorded production proposal must return ApprovalRequired");
        };
        let pending = self.engine.pending_proposals(NOW_MS).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id(), approval_id);
        assert_eq!(pending[0].expires_at_ms(), expires_at_ms);
        assert_eq!(pending[0].intent(), &order);
        pending[0].clone()
    }

    fn approve(&self, id: &str) -> Result<Cleared, Refusal> {
        self.engine.operator_approve_proposal(
            id,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &exposure(d("100000")),
            NOW_MS,
        )
    }

    fn events(&self, kind: EventKind) -> Vec<Event> {
        self.ledger.events_of_kind(kind).unwrap()
    }
}

fn approval_order() -> OrderIntent {
    let mut order = intent("BTC", true, d("100"), d("1"));
    order.cloid = Some(Cloid::parse("0x00000000000000000000000000000001").unwrap());
    order
}

fn assert_approval_authority(result: Result<Cleared, Refusal>) {
    assert!(
        matches!(
            result,
            Err(Refusal::Unevaluable(Unevaluable::ApprovalAuthority { .. }))
        ),
        "journal failure must not report a pending approval or produce clearance: {result:?}"
    );
}

fn assert_consumed(f: &DurableFixture, id: &str) {
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
    assert!(matches!(
        f.approve(id),
        Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
    ));
}

#[test]
fn production_pending_reopens_with_same_identity_intent_and_expiry_without_activation() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let head = f.ledger.chain_head().unwrap();
    let f = f.reopen();
    assert!(f.engine.policy_status().admission_inhibited);
    assert_eq!(f.engine.policy_status().acknowledgment, None);
    assert_eq!(
        f.engine.pending_proposals(NOW_MS).unwrap(),
        vec![proposal.clone()]
    );
    assert_eq!(
        f.ledger.chain_head().unwrap(),
        head,
        "opening and listing must not activate or append"
    );
    assert!(f.evaluate(&approval_order()).is_err());
    assert!(f.events(EventKind::OrderIntent).is_empty());
    assert_eq!(f.events(EventKind::ApprovalProposed).len(), 1);
    assert_eq!(
        f.engine
            .pending_proposals(proposal.expires_at_ms() - 1)
            .unwrap(),
        vec![proposal.clone()]
    );
    assert!(
        f.engine
            .pending_proposals(proposal.expires_at_ms())
            .unwrap()
            .is_empty()
    );
    assert!(f.events(EventKind::OrderIntent).is_empty());
}

#[test]
fn production_rejection_is_durable_and_consumes_the_proposal_once() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    assert!(
        f.engine
            .operator_reject_proposal(proposal.id(), NOW_MS)
            .unwrap()
    );
    assert!(
        !f.engine
            .operator_reject_proposal(proposal.id(), NOW_MS)
            .unwrap()
    );
    let disposed = f.events(EventKind::ApprovalDisposed);
    assert_eq!(disposed.len(), 1);
    assert!(f.events(EventKind::OrderIntent).is_empty());
    let f = f.reopen();
    acknowledge(&f.engine);
    assert_consumed(&f, proposal.id());
    assert!(
        !f.engine
            .operator_reject_proposal(proposal.id(), NOW_MS)
            .unwrap()
    );
    assert_eq!(f.events(EventKind::ApprovalDisposed), disposed);
    assert!(f.events(EventKind::OrderIntent).is_empty());
}

#[test]
fn production_approval_consumes_once_and_records_claim_intent_and_disposition() {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let cleared = f.approve(proposal.id()).unwrap();
    let intents = f.events(EventKind::OrderIntent);
    let claims = f.events(EventKind::ApprovalClaimed);
    let disposed = f.events(EventKind::ApprovalDisposed);
    assert_eq!(intents.len(), 1);
    assert_eq!(claims.len(), 1);
    assert_eq!(disposed.len(), 1);
    assert!(claims[0].seq < intents[0].seq && intents[0].seq < disposed[0].seq);
    assert_eq!(intents[0].agent_id.as_deref(), Some("alpha"));
    let mut expected = serde_json::to_value(cleared.clearance()).unwrap();
    expected
        .as_object_mut()
        .unwrap()
        .insert("reason".into(), proposal.intent().reason.clone().into());
    assert_eq!(intents[0].payload.as_ref().unwrap(), &expected);
    let proposed = f.events(EventKind::ApprovalProposed);
    let root = serde_json::json!({ "seq": proposed[0].seq, "hash": proposed[0].hash });
    let claim_link = serde_json::json!({ "seq": claims[0].seq, "hash": claims[0].hash });
    let intent_link = serde_json::json!({ "seq": intents[0].seq, "hash": intents[0].hash });
    let claimed = claims[0].payload.as_ref().unwrap();
    assert_eq!(claimed.pointer("/envelope/root"), Some(&root));
    assert_eq!(claimed.pointer("/envelope/previous"), Some(&root));
    let approved = disposed[0].payload.as_ref().unwrap();
    assert_eq!(approved.pointer("/envelope/root"), Some(&root));
    assert_eq!(approved.pointer("/envelope/previous"), Some(&claim_link));
    assert_eq!(
        approved
            .pointer("/envelope/operation/outcome/disposition")
            .and_then(serde_json::Value::as_str),
        Some("approved")
    );
    assert_eq!(
        approved.pointer("/envelope/operation/outcome/intent"),
        Some(&intent_link)
    );
    drop(cleared);
    assert_consumed(&f, proposal.id());
    let f = f.reopen();
    acknowledge(&f.engine);
    assert_consumed(&f, proposal.id());
    assert_eq!(f.events(EventKind::OrderIntent), intents);
    assert_eq!(f.events(EventKind::ApprovalClaimed), claims);
    assert_eq!(f.events(EventKind::ApprovalDisposed), disposed);
}

#[test]
fn production_fresh_exposure_refusal_records_terminal_disposition_and_consumes_proposal() {
    let f = DurableFixture::new();
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails
        .get_mut(&AgentId::new("alpha"))
        .unwrap()
        .max_position_usd = d("200");
    f.policy.replace(current.revision, next, NOW_MS).unwrap();
    acknowledge(&f.engine);
    let proposal = f.propose();
    let previous_refusals = f.events(EventKind::Refusal).len();
    let mut loaded = exposure(d("100000"));
    loaded
        .agent
        .positions
        .insert("BTC".to_owned(), PositionSnapshot { szi: d("2") });
    loaded.agent.total_position_notional_usd = d("200");
    let refusal = f
        .engine
        .operator_approve_proposal(
            proposal.id(),
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &loaded,
            NOW_MS,
        )
        .expect_err("fresh exposure must still enforce the position limit");
    assert!(
        matches!(&refusal, Refusal::PositionNotional { symbol, observed_usd, limit_usd, .. }
        if symbol == "BTC" && *observed_usd == d("300") && *limit_usd == d("200")),
        "durable finish must preserve the original typed refusal: {refusal:?}"
    );
    let refusals = f.events(EventKind::Refusal);
    assert_eq!(refusals.len(), previous_refusals + 1);
    let audit = refusals.last().unwrap();
    assert_eq!(audit.agent_id.as_deref(), Some("alpha"));
    assert_eq!(
        audit.payload.as_ref().unwrap().get("refusal_detail"),
        Some(&serde_json::to_value(&refusal).unwrap())
    );
    let claims = f.events(EventKind::ApprovalClaimed);
    let disposed = f.events(EventKind::ApprovalDisposed);
    assert_eq!(claims.len(), 1);
    assert_eq!(disposed.len(), 1);
    assert!(claims[0].seq < audit.seq && audit.seq < disposed[0].seq);
    let terminal = disposed[0].payload.as_ref().unwrap();
    assert_eq!(
        terminal
            .pointer("/envelope/operation/outcome/disposition")
            .and_then(serde_json::Value::as_str),
        Some("refused")
    );
    assert_eq!(
        terminal
            .pointer("/envelope/operation/outcome/detail")
            .and_then(serde_json::Value::as_str),
        Some(refusal.to_string().as_str())
    );
    assert_consumed(&f, proposal.id());
    assert!(f.events(EventKind::OrderIntent).is_empty());
    let f = f.reopen();
    acknowledge(&f.engine);
    assert_consumed(&f, proposal.id());
    assert_eq!(f.events(EventKind::ApprovalDisposed), disposed);
    assert!(f.events(EventKind::OrderIntent).is_empty());
}

#[test]
fn production_approval_refuses_retired_or_reassigned_registry_route() {
    for reassign in [false, true] {
        let f = DurableFixture::new();
        let proposal = f.propose();
        let registry = RegistryJournal::open(f.ledger.clone(), authority_key()).unwrap();
        let route = registry.route_for_agent(&AgentId::new("alpha")).unwrap();
        assert!(registry.retire(&route, NOW_MS).unwrap());
        let result = if reassign {
            f.keys
                .rotate_agent_key(
                    &AgentId::new("alpha"),
                    SecretText::new(format!("{:064x}", 2)),
                    NOW_MS + 90 * 86_400_000,
                    NOW_MS,
                )
                .unwrap();
            let account =
                oppen_hl::Address::parse("0x2222222222222222222222222222222222222222").unwrap();
            let binding = route_for(f.keys.as_ref(), "alpha", account, Some(account)).binding;
            registry.grant(binding, NOW_MS).unwrap();
            let mut current_exposure = exposure(d("100000"));
            current_exposure.account = account;
            f.engine.operator_approve_proposal(
                proposal.id(),
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), NOW_MS),
                &current_exposure,
                NOW_MS,
            )
        } else {
            f.approve(proposal.id())
        };
        assert!(
            matches!(
                result,
                Err(Refusal::Unevaluable(Unevaluable::RouteAuthority { .. }))
            ),
            "the proposal must remain bound to its original registry route: {result:?}"
        );
        assert!(f.events(EventKind::OrderIntent).is_empty());
        let disposed = f.events(EventKind::ApprovalDisposed);
        assert_eq!(disposed.len(), 1);
        assert_eq!(
            disposed[0]
                .payload
                .as_ref()
                .unwrap()
                .pointer("/envelope/operation/outcome/disposition")
                .and_then(serde_json::Value::as_str),
            Some("refused")
        );
        assert_consumed(&f, proposal.id());
        let f = f.reopen();
        acknowledge(&f.engine);
        assert_consumed(&f, proposal.id());
        assert_eq!(f.events(EventKind::ApprovalDisposed), disposed);
        assert!(f.events(EventKind::OrderIntent).is_empty());
    }
}

#[test]
fn production_proposal_requires_cloid_and_failed_insert_never_returns_pending() {
    let f = DurableFixture::new();
    assert_approval_authority(f.evaluate(&intent("BTC", true, d("100"), d("1"))));
    assert!(f.events(EventKind::ApprovalProposed).is_empty());
    let connection = rusqlite::Connection::open(f.dir.path().join("approval.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_approval_proposal BEFORE INSERT ON events
        WHEN NEW.kind = 'approval_proposed' BEGIN SELECT RAISE(ABORT, 'synthetic proposal write failure'); END;").unwrap();
    assert_approval_authority(f.evaluate(&approval_order()));
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
    assert!(f.events(EventKind::ApprovalProposed).is_empty());
    assert!(f.events(EventKind::OrderIntent).is_empty());
    connection
        .execute_batch("DROP TRIGGER fail_approval_proposal;")
        .unwrap();
    drop(connection);
    let f = f.reopen();
    assert!(f.engine.pending_proposals(NOW_MS).unwrap().is_empty());
}

#[test]
fn production_claim_publication_failure_returns_no_clearance_and_reopen_cannot_reapprove() {
    publication_failure_does_not_reapprove(false);
}

#[test]
fn production_mint_publication_failure_has_no_followon_refusal_and_retry_recovers_original_proposal()
 {
    let f = DurableFixture::new();
    let before = f.ledger.chain_head().unwrap();
    let refusals = f.events(EventKind::Refusal);
    f.fail_seq.store(before.seq + 1, Ordering::SeqCst);
    let order = approval_order();
    assert_approval_authority(f.evaluate(&order));
    assert!(f.failures.load(Ordering::SeqCst) > 0);
    assert_eq!(f.ledger.chain_head().unwrap().seq, before.seq + 1);
    assert_eq!(
        f.events(EventKind::Refusal),
        refusals,
        "failed publication must not be hidden by another append"
    );
    let proposed = f.events(EventKind::ApprovalProposed);
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].seq, before.seq + 1);
    let original_id = format!("approval-testnet-{}", proposed[0].seq);
    let original_expiry = proposed[0]
        .payload
        .as_ref()
        .unwrap()
        .pointer("/envelope/operation/proposal/expires_at_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap();
    assert_eq!(original_expiry, NOW_MS + APPROVAL_TTL_MS);
    assert_eq!(
        FileAnchor::beside(&f.dir.path().join("approval.db"))
            .load()
            .unwrap(),
        Some(before)
    );
    f.fail_seq.store(0, Ordering::SeqCst);
    let retry_at = NOW_MS + 1;
    let mut current_exposure = exposure(d("100000"));
    current_exposure.agent.as_of_ms = retry_at;
    let result = f.engine.evaluate(
        &AgentId::new("alpha"),
        &order,
        &asset("BTC", 2, 40),
        &MarketRef::fresh("BTC", d("100"), retry_at),
        &current_exposure,
        retry_at,
    );
    let Err(Refusal::ApprovalRequired {
        approval_id,
        expires_at_ms,
        ..
    }) = result
    else {
        panic!("matching-cloid retry must recover the committed proposal: {result:?}");
    };
    assert_eq!(approval_id, original_id);
    assert_eq!(
        expires_at_ms, original_expiry,
        "retry must not renew the approval TTL"
    );
    assert_eq!(f.events(EventKind::ApprovalProposed), proposed);
    let pending = f.engine.pending_proposals(retry_at).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id(), original_id);
    assert_eq!(pending[0].expires_at_ms(), original_expiry);
    assert_eq!(pending[0].intent(), &order);
    assert!(f.events(EventKind::OrderIntent).is_empty());
}

#[test]
fn production_approved_disposition_publication_failure_returns_no_clearance_and_cannot_resign() {
    publication_failure_does_not_reapprove(true);
}

fn publication_failure_does_not_reapprove(disposition: bool) {
    let f = DurableFixture::new();
    let proposal = f.propose();
    let before = f.ledger.chain_head().unwrap();
    // Approval appends claim, actual order-intent receipt, then disposition.
    let target = before.seq + if disposition { 3 } else { 1 };
    f.fail_seq.store(target, Ordering::SeqCst);
    assert_approval_authority(f.approve(proposal.id()));
    assert!(
        f.failures.load(Ordering::SeqCst) > 0,
        "the targeted publication must actually fail"
    );
    let committed = f.ledger.chain_head().unwrap();
    assert_eq!(
        committed.seq, target,
        "failure must occur after the targeted row commits"
    );
    assert_eq!(
        FileAnchor::beside(&f.dir.path().join("approval.db"))
            .load()
            .unwrap()
            .unwrap()
            .seq,
        target - 1
    );
    assert_eq!(f.events(EventKind::ApprovalClaimed).len(), 1);
    assert_eq!(
        f.events(EventKind::ApprovalDisposed).len(),
        usize::from(disposition)
    );
    assert_eq!(
        f.events(EventKind::OrderIntent).len(),
        usize::from(disposition)
    );
    f.fail_seq.store(0, Ordering::SeqCst);
    let f = f.reopen();
    assert!(f.engine.policy_status().admission_inhibited);
    acknowledge(&f.engine);
    let intents = f.events(EventKind::OrderIntent);
    let claims = f.events(EventKind::ApprovalClaimed);
    assert_consumed(&f, proposal.id());
    assert_eq!(
        f.events(EventKind::OrderIntent),
        intents,
        "reopening cannot mint another signable order"
    );
    assert_eq!(f.events(EventKind::ApprovalClaimed), claims);
}

#[test]
fn production_reserved_approval_expires_before_signature_and_stays_consumed() {
    reserved_approval_deadline(false, false);
}

#[test]
fn production_reserved_approval_deadline_is_sampled_after_key_loading() {
    reserved_approval_deadline(true, false);
}

#[test]
fn production_reserved_approval_deadline_is_sampled_after_ledger_authority_lock() {
    reserved_approval_deadline(true, true);
}

fn reserved_approval_deadline(wait_for_keys: bool, wait_for_authority: bool) {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::ledger::SubmissionResolution;

    for reduce_only in [false, true] {
        let mut f = DurableFixture::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut order = approval_order();
        order.reduce_only = reduce_only;
        let mut loaded = exposure(d("100000"));
        if reduce_only {
            loaded
                .agent
                .positions
                .insert("BTC".into(), PositionSnapshot { szi: -Decimal::ONE });
            loaded.agent.total_position_notional_usd = d("100");
        }
        let result = f.engine.evaluate(
            &AgentId::new("alpha"),
            &order,
            &asset("BTC", 2, 40),
            &MarketRef::fresh("BTC", d("100"), NOW_MS),
            &loaded,
            NOW_MS,
        );
        let Err(Refusal::ApprovalRequired {
            approval_id,
            expires_at_ms,
            ..
        }) = result
        else {
            panic!("expected durable proposal, reduce_only={reduce_only}: {result:?}");
        };
        assert_eq!(expires_at_ms, NOW_MS + APPROVAL_TTL_MS);
        let approved_at = expires_at_ms - 1;
        let proposed = f.engine.pending_proposals(NOW_MS).unwrap();
        f = f.reopen();
        assert_eq!(f.engine.pending_proposals(approved_at).unwrap(), proposed);
        let keys = Arc::new(WaitingKeys {
            inner: f.keys.clone(),
            wait: wait_for_keys.then(|| (entered_tx, Mutex::new(release_rx))),
        });
        f.engine = GuardrailEngine::new(f.policy.clone(), keys).unwrap();
        acknowledge(&f.engine);
        let wallet = f
            .keys
            .agent_wallet(&AgentId::new("alpha"))
            .unwrap()
            .unwrap();
        assert!(wallet.approved_at_ms <= approved_at && wallet.valid_until_ms > expires_at_ms);
        loaded.agent.as_of_ms = approved_at;
        let cleared = f
            .engine
            .operator_approve_proposal(
                &approval_id,
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), approved_at),
                &loaded,
                approved_at,
            )
            .unwrap();
        assert_eq!(cleared.clearance().evaluated_at_ms, expires_at_ms - 1);
        let claims = f.events(EventKind::ApprovalClaimed);
        let intents = f.events(EventKind::OrderIntent);
        let dispositions = f.events(EventKind::ApprovalDisposed);
        assert_eq!(claims.len(), 1);
        assert_eq!(intents.len(), 1);
        assert_eq!(dispositions.len(), 1);
        assert!(claims[0].seq < intents[0].seq && intents[0].seq < dispositions[0].seq);
        let submissions = f.engine.submissions().unwrap();
        let before = submissions.state(vault()).unwrap();
        let receipt = submissions
            .begin(vault(), cleared.clearance(), before.revision, approved_at)
            .unwrap();
        assert_eq!(
            submissions
                .state(vault())
                .unwrap()
                .pending
                .as_ref()
                .unwrap()
                .cloid(),
            receipt.cloid()
        );

        let clock = AtomicU64::new(if wait_for_keys {
            approved_at
        } else {
            expires_at_ms
        });
        let samples = AtomicUsize::new(0);
        let previous_refusals = f.events(EventKind::Refusal).len();
        let result = std::thread::scope(|scope| {
            let coordination = wait_for_authority.then(|| {
                let file = std::fs::File::options()
                    .read(true)
                    .write(true)
                    .open(f.dir.path().join("approval.db.lock"))
                    .unwrap();
                file.lock().unwrap();
                file
            });
            let (done_tx, done_rx) = mpsc::channel();
            let engine = &f.engine;
            let clock = &clock;
            let samples = &samples;
            scope.spawn(move || {
                let result = engine.sign_cleared(cleared, 1, None, || {
                    samples.fetch_add(1, Ordering::SeqCst);
                    clock.load(Ordering::SeqCst)
                });
                done_tx.send(result).unwrap();
            });
            if wait_for_keys {
                entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if wait_for_authority {
                    release_tx.send(()).unwrap();
                }
                let waiting = done_rx.recv_timeout(Duration::from_millis(50));
                let samples_while_blocked = samples.load(Ordering::SeqCst);
                clock.store(expires_at_ms, Ordering::SeqCst);
                drop(coordination);
                if !wait_for_authority {
                    release_tx.send(()).unwrap();
                }
                assert!(matches!(waiting, Err(mpsc::RecvTimeoutError::Timeout)));
                assert_eq!(
                    samples_while_blocked, 0,
                    "deadline must follow key loading and authority locking"
                );
            }
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap()
        });
        assert!(
            matches!(result,
                Err(SignClearedError::Refused(Refusal::Unevaluable(
                    Unevaluable::ApprovalExpired { expires_at_ms: expiry, now_ms }
                ))) if expiry == expires_at_ms && now_ms == expires_at_ms
            ),
            "expired durable approval returned a signature or wrong refusal: {result:?}"
        );
        assert_eq!(samples.load(Ordering::SeqCst), 1);
        let refusals = f.events(EventKind::Refusal);
        assert_eq!(refusals.len(), previous_refusals + 1);
        let audit = refusals.last().unwrap();
        assert_eq!(audit.ts_ms, expires_at_ms as i64);
        assert_eq!(audit.agent_id.as_deref(), Some("alpha"));
        assert_eq!(
            audit.payload.as_ref().unwrap().get("refusal_detail"),
            Some(
                &serde_json::to_value(Refusal::Unevaluable(Unevaluable::ApprovalExpired {
                    expires_at_ms,
                    now_ms: expires_at_ms,
                }))
                .unwrap()
            )
        );
        assert_eq!(
            submissions
                .state(vault())
                .unwrap()
                .pending
                .as_ref()
                .unwrap()
                .cloid(),
            receipt.cloid()
        );
        submissions
            .resolve(
                &receipt,
                SubmissionResolution::NotSent {
                    detail: "synthetic signing gate refused expired approval".into(),
                },
                expires_at_ms,
            )
            .unwrap();
        assert!(submissions.state(vault()).unwrap().pending.is_none());
        drop(submissions);

        let f = f.reopen();
        assert!(
            f.engine
                .pending_proposals(expires_at_ms + 1)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            f.engine.operator_approve_proposal(
                &approval_id,
                &asset("BTC", 2, 40),
                &MarketRef::fresh("BTC", d("100"), expires_at_ms + 1),
                &loaded,
                expires_at_ms + 1,
            ),
            Err(Refusal::Unevaluable(Unevaluable::UnknownProposal { .. }))
        ));
        assert!(
            f.engine
                .submissions()
                .unwrap()
                .state(vault())
                .unwrap()
                .pending
                .is_none()
        );
        assert_eq!(f.events(EventKind::ApprovalClaimed), claims);
        assert_eq!(f.events(EventKind::OrderIntent), intents);
        assert_eq!(f.events(EventKind::ApprovalDisposed), dispositions);
        assert!(f.ledger.verify().unwrap().is_intact());
    }
}
