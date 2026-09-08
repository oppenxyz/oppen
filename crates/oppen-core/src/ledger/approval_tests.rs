use std::sync::{
    Barrier, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::guardrail::{AgentGuardrails, LegacyPolicyReview, PersistedState, RequestedOrderKind};
use crate::keys::{AgentWallet, HmacKey};
use crate::ledger::{Anchor, HeadAnchor, RegistryBinding, RegistryJournal};

const NOW: u64 = 1_800_000_000_000;

struct Fixture {
    dir: TempDir,
    ledger: Arc<Ledger>,
    policy: Arc<PolicyJournal>,
    journal: ApprovalJournal,
    route: AuthorizedRoute,
}

fn key() -> Arc<HmacKey> {
    Arc::new(HmacKey::from_bytes([63; 32]))
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
        Self::with_ledger(dir, ledger)
    }

    fn with_ledger(dir: TempDir, ledger: Arc<Ledger>) -> Self {
        let registry = Arc::new(RegistryJournal::open(ledger.clone(), key()).unwrap());
        let route = registry
            .grant(
                RegistryBinding {
                    agent: AgentId::new("approval-agent"),
                    container: Address::from_bytes([1; 20]),
                    vault_address: None,
                    wallet: AgentWallet {
                        generation: 0,
                        address: Address::from_bytes([2; 20]),
                        approved_at_ms: NOW - 10,
                        valid_until_ms: NOW + 86_400_000,
                    },
                },
                NOW - 10,
            )
            .unwrap();
        let policy = Arc::new(PolicyJournal::new(registry));
        let review =
            LegacyPolicyReview::open(dir.path().join("legacy.db"), Network::Testnet, NOW - 5)
                .unwrap();
        let mut state = PersistedState::paused(NOW - 5);
        state
            .guardrails
            .insert(route.binding.agent.clone(), AgentGuardrails::default());
        policy.initialize(&review, state, NOW - 5).unwrap();
        let journal = ApprovalJournal::new(policy.clone());
        Self {
            dir,
            ledger,
            policy,
            journal,
            route,
        }
    }

    fn candidate(&self, id: u128, at_ms: u64) -> Candidate {
        Candidate {
            agent: self.route.binding.agent.clone(),
            route: self.route.clone(),
            policy_revision: self.policy.current().unwrap().revision,
            at_ms,
            intent: OrderIntent {
                symbol: "BTC".into(),
                is_buy: true,
                px: 100.into(),
                sz: Decimal::ONE,
                kind: OrderKind::Limit { tif: Tif::Gtc },
                reduce_only: false,
                cloid: Some(Cloid::parse(&format!("0x{id:032x}")).unwrap()),
                grouping: Grouping::Na,
                builder: None,
                max_slippage_bps: None,
                reason: "operator-reviewed\nagent claim\tverbatim".into(),
                original: None,
            },
        }
    }

    fn reopened(&self) -> ApprovalJournal {
        let ledger = Arc::new(Ledger::open(self.dir.path(), Network::Testnet).unwrap());
        ApprovalJournal::new(Arc::new(PolicyJournal::new(Arc::new(
            RegistryJournal::open(ledger, key()).unwrap(),
        ))))
    }

    fn refusal_receipt(&self, proposal: &Proposal, at_ms: u64) -> Appended {
        let refusal = Refusal::MissingReason;
        self.ledger.append(&NewEvent {
            kind: EventKind::Refusal, ts_ms: at_ms as i64, agent_id: Some(proposal.agent().as_str()),
            payload: &json!({"refusal": refusal.to_string(), "refusal_detail": refusal, "reason": proposal.order_intent().unwrap().reason}), snapshot: None,
        }).unwrap()
    }
}

#[test]
fn mint_reopen_claim_once_and_dropped_permit_stays_consumed() {
    let f = Fixture::new();
    assert!(f.journal.pending(NOW).unwrap().is_empty());
    let proposal = f.journal.mint(f.candidate(1, NOW)).unwrap();
    assert_eq!(proposal.expires_at_ms(), NOW + APPROVAL_TTL_MS);
    assert_eq!(
        f.reopened().pending(NOW + 1).unwrap(),
        vec![proposal.clone()]
    );
    let claim = f.journal.claim(proposal.id(), NOW + 2).unwrap().unwrap();
    assert_eq!(claim.proposal(), &proposal);
    drop(claim);
    let reopened = f.reopened();
    assert!(reopened.claim(proposal.id(), NOW + 3).unwrap().is_none());
    assert!(reopened.pending(NOW + 3).unwrap().is_empty());
    assert!(!reopened.reject(proposal.id(), NOW + 3).unwrap());
    assert!(f.journal.mint(f.candidate(1, NOW + 3)).is_err());
}

#[test]
fn identical_mint_preserves_id_expiry_and_changed_intent_conflicts_for_lifetime() {
    let f = Fixture::new();
    let first = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let head = f.ledger.chain_head().unwrap();
    assert_eq!(first, f.journal.mint(f.candidate(1, NOW + 50)).unwrap());
    assert_eq!(head, f.ledger.chain_head().unwrap());
    let mut changed = f.candidate(1, NOW + 51);
    changed.intent.sz += Decimal::ONE;
    assert!(matches!(
        f.journal.mint(changed),
        Err(ApprovalError::Conflict { .. })
    ));
    assert!(f.journal.reject(first.id(), NOW + 52).unwrap());
    let after = f.ledger.chain_head().unwrap();
    assert!(!f.journal.reject(first.id(), NOW + 53).unwrap());
    assert!(!f.journal.reject("absent", NOW + 53).unwrap());
    assert_eq!(after, f.ledger.chain_head().unwrap());
    assert!(f.journal.mint(f.candidate(1, NOW + 54)).is_err());
    assert!(f.reopened().pending(NOW + 54).unwrap().is_empty());
}

#[test]
fn expiry_boundary_is_durable_and_cannot_extend_or_claim() {
    for operation in ["pending", "claim", "reject"] {
        let f = Fixture::new();
        let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
        assert_eq!(f.journal.pending(p.expires_at_ms() - 1).unwrap().len(), 1);
        let before = f.ledger.chain_head().unwrap().seq;
        match operation {
            "pending" => assert!(f.journal.pending(p.expires_at_ms()).unwrap().is_empty()),
            "claim" => assert!(
                f.journal
                    .claim(p.id(), p.expires_at_ms())
                    .unwrap()
                    .is_none()
            ),
            _ => assert!(!f.journal.reject(p.id(), p.expires_at_ms()).unwrap()),
        }
        assert_eq!(f.ledger.chain_head().unwrap().seq, before + 1);
        assert!(
            f.reopened()
                .claim(p.id(), p.expires_at_ms() + 1)
                .unwrap()
                .is_none()
        );
        assert!(
            f.journal
                .mint(f.candidate(1, p.expires_at_ms() + 1))
                .is_err()
        );
        assert_eq!(f.ledger.chain_head().unwrap().seq, before + 1);
    }
}

#[test]
fn simultaneous_independent_handles_issue_only_one_claim() {
    let f = Fixture::new();
    let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let a = f.reopened();
    let b = f.reopened();
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = [a, b]
        .into_iter()
        .map(|journal| {
            let barrier = barrier.clone();
            let id = p.id().to_owned();
            std::thread::spawn(move || {
                barrier.wait();
                journal.claim(&id, NOW + 1).unwrap().is_some()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert!(f.journal.pending(NOW + 2).unwrap().is_empty());
}

#[test]
fn candidate_validation_and_current_authority_fail_without_append() {
    let f = Fixture::new();
    let head = f.ledger.chain_head().unwrap();
    for mutation in 0..8 {
        let mut c = f.candidate(1, NOW);
        match mutation {
            0 => c.intent.cloid = None,
            1 => c.agent = AgentId::new("bad agent"),
            2 => c.route.binding.container = Address::ZERO,
            3 => c.policy_revision += 1,
            4 => c.route.network = Network::Mainnet,
            5 => c.route.binding.wallet.address = Address::from_bytes([3; 20]),
            6 => c.intent.reason = " ".into(),
            _ => c.at_ms = u64::MAX,
        }
        assert!(f.journal.mint(c).is_err(), "mutation {mutation}");
        assert_eq!(f.ledger.chain_head().unwrap(), head);
    }
    let old = f.candidate(1, NOW);
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails.get_mut(&old.agent).unwrap().max_order_usd = 7.into();
    f.policy.replace(current.revision, next, NOW).unwrap();
    assert!(matches!(
        f.journal.mint(old),
        Err(ApprovalError::Conflict { .. })
    ));
    let c = f.candidate(1, NOW + 1);
    f.policy.registry().retire(&f.route, NOW + 1).unwrap();
    assert!(f.journal.mint(c).is_err());
}

#[test]
fn normalized_intent_roundtrip_preserves_trigger_builder_and_explicit_options() {
    let f = Fixture::new();
    let mut c = f.candidate(7, NOW);
    c.intent.kind = OrderKind::Trigger {
        is_market: false,
        trigger_px: 99.into(),
        tpsl: Tpsl::Sl,
    };
    c.intent.reduce_only = true;
    c.intent.max_slippage_bps = Some(Decimal::from(10));
    let expected = c.intent.clone();
    let p = f.journal.mint(c).unwrap();
    assert_eq!(
        f.reopened().pending(NOW + 1).unwrap()[0]
            .order_intent()
            .unwrap(),
        &expected
    );
    let event = f
        .ledger
        .get_events(0, 100)
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == EventKind::ApprovalProposed)
        .unwrap();
    let raw = event.payload.unwrap();
    let mut missing = raw.clone();
    missing["envelope"]["operation"]["proposal"]["intent"]
        .as_object_mut()
        .unwrap()
        .remove("builder");
    let parsed: Signed = serde_json::from_value(missing.clone()).unwrap();
    assert_ne!(serde_json::to_value(parsed).unwrap(), missing);
    let mut unknown = raw;
    unknown["envelope"]["operation"]["proposal"]["intent"]["future_flag"] = json!(true);
    assert!(serde_json::from_value::<Signed>(unknown).is_err());
    assert_eq!(p.order_intent().unwrap(), &expected);
}

#[test]
fn refused_finish_consumes_without_constructing_clearance_and_reopen_is_empty() {
    let f = Fixture::new();
    let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let claim = f.journal.claim(p.id(), NOW + 1).unwrap().unwrap();
    let receipt = f.refusal_receipt(&p, NOW + 2);
    f.journal
        .finish(claim, &Err(Refusal::MissingReason), Some(&receipt), NOW + 2)
        .unwrap();
    assert!(f.reopened().pending(NOW + 3).unwrap().is_empty());
    assert!(f.journal.claim(p.id(), NOW + 3).unwrap().is_none());
    let guard = f.ledger.lock().unwrap();
    assert!(matches!(
        f.journal
            .replay(&guard)
            .unwrap()
            .values()
            .next()
            .unwrap()
            .state,
        State::Disposed
    ));
}

#[test]
fn permits_cannot_be_used_by_another_journal_owner() {
    let f = Fixture::new();
    let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let claim = f.journal.claim(p.id(), NOW + 1).unwrap().unwrap();
    assert!(matches!(
        f.reopened()
            .finish(claim, &Err(Refusal::MissingReason), None, NOW + 2),
        Err(ApprovalError::Conflict { .. })
    ));
    assert!(f.journal.claim(p.id(), NOW + 3).unwrap().is_none());
}

#[test]
fn malformed_or_redacted_consumed_history_is_not_an_empty_queue() {
    for redacted in [true, false] {
        let f = Fixture::new();
        let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
        f.journal.reject(p.id(), NOW + 1).unwrap();
        let guard = f.ledger.lock().unwrap();
        if redacted {
            guard
                .execute(
                    "UPDATE events SET payload = NULL WHERE kind='approval_proposed'",
                    [],
                )
                .unwrap();
        } else {
            guard.execute("UPDATE events SET payload = '{\"version\":1,\"version\":1}' WHERE kind='approval_proposed'", []).unwrap();
        }
        drop(guard);
        assert!(f.journal.pending(NOW + 2).is_err());
        assert!(f.journal.claim(p.id(), NOW + 2).is_err());
        assert!(f.journal.reject(p.id(), NOW + 2).is_err());
        assert!(
            f.journal
                .mint(f.candidate_without_read(2, NOW + 2))
                .is_err()
        );
    }
}

impl Fixture {
    fn candidate_without_read(&self, id: u128, at_ms: u64) -> Candidate {
        Candidate {
            agent: self.route.binding.agent.clone(),
            intent: OrderIntent {
                symbol: "BTC".into(),
                is_buy: true,
                px: 100.into(),
                sz: Decimal::ONE,
                kind: OrderKind::Limit { tif: Tif::Gtc },
                reduce_only: false,
                cloid: Some(Cloid::parse(&format!("0x{id:032x}")).unwrap()),
                grouping: Grouping::Na,
                builder: None,
                max_slippage_bps: None,
                reason: "synthetic".into(),
                original: None,
            },
            route: self.route.clone(),
            policy_revision: 3,
            at_ms,
        }
    }
}

#[derive(Debug, Default)]
struct AnchorControl {
    head: Mutex<Option<Anchor>>,
    calls: AtomicUsize,
    fail_call: AtomicUsize,
    fail_all: AtomicBool,
}
impl HeadAnchor for Arc<AnchorControl> {
    fn load(&self) -> crate::ledger::Result<Option<Anchor>> {
        Ok(self.head.lock().unwrap().clone())
    }
    fn store(&self, head: &Anchor) -> crate::ledger::Result<()> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_all.load(Ordering::SeqCst) || self.fail_call.load(Ordering::SeqCst) == call {
            return Err(LedgerError::Io(std::io::Error::other(
                "synthetic publication failure",
            )));
        }
        *self.head.lock().unwrap() = Some(head.clone());
        Ok(())
    }
}

fn controlled() -> (Fixture, Arc<AnchorControl>) {
    let dir = TempDir::new().unwrap();
    let control = Arc::new(AnchorControl::default());
    let ledger = Arc::new(
        Ledger::open_anchored(
            &dir.path().join("ledger.db"),
            Network::Testnet,
            Some(Box::new(control.clone())),
        )
        .unwrap(),
    );
    (Fixture::with_ledger(dir, ledger), control)
}

fn fail_next_commit_publication(control: &AnchorControl) {
    // An approval write first publishes its verified predecessor, then its new row.
    control
        .fail_call
        .store(control.calls.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
}

#[test]
fn mint_publication_retry_preserves_expiry_and_blocks_further_lag() {
    let (f, control) = controlled();
    let before = f.ledger.chain_head().unwrap();
    fail_next_commit_publication(&control);
    assert!(f.journal.mint(f.candidate(1, NOW)).is_err());
    assert_eq!(f.ledger.chain_head().unwrap().seq, before.seq + 1);
    control.fail_all.store(true, Ordering::SeqCst);
    assert!(f.journal.mint(f.candidate(2, NOW + 1)).is_err());
    assert!(f.journal.mint(f.candidate(1, NOW + 1)).is_err());
    assert_eq!(f.ledger.chain_head().unwrap().seq, before.seq + 1);
    control.fail_all.store(false, Ordering::SeqCst);
    let p = f.journal.mint(f.candidate(1, NOW + 2)).unwrap();
    assert_eq!(p.expires_at_ms(), NOW + APPROVAL_TTL_MS);
    assert_eq!(
        control.head.lock().unwrap().as_ref().unwrap().seq,
        before.seq + 1
    );
}

#[test]
fn committed_claim_publication_error_never_reissues_a_permit() {
    let (f, control) = controlled();
    let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
    fail_next_commit_publication(&control);
    assert!(f.journal.claim(p.id(), NOW + 1).is_err());
    let head = f.ledger.chain_head().unwrap();
    control.fail_all.store(true, Ordering::SeqCst);
    assert!(f.journal.claim(p.id(), NOW + 2).is_err());
    control.fail_all.store(false, Ordering::SeqCst);
    assert!(f.journal.claim(p.id(), NOW + 3).unwrap().is_none());
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert_eq!(*control.head.lock().unwrap(), Some(head));
}

#[test]
fn committed_rejection_and_finish_errors_are_publication_only_recoverable() {
    for reject in [true, false] {
        let (f, control) = controlled();
        let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
        let claim = if reject {
            None
        } else {
            f.journal.claim(p.id(), NOW + 1).unwrap()
        };
        let receipt = claim.as_ref().map(|_| f.refusal_receipt(&p, NOW + 2));
        fail_next_commit_publication(&control);
        if let Some(claim) = claim {
            assert!(
                f.journal
                    .finish(
                        claim,
                        &Err(Refusal::MissingReason),
                        receipt.as_ref(),
                        NOW + 2
                    )
                    .is_err()
            );
        } else {
            assert!(f.journal.reject(p.id(), NOW + 2).is_err());
        }
        control.fail_all.store(true, Ordering::SeqCst);
        assert!(f.journal.reject(p.id(), NOW + 3).is_err());
        assert!(f.journal.pending(NOW + 3).is_err());
        control.fail_all.store(false, Ordering::SeqCst);
        assert!(!f.journal.reject(p.id(), NOW + 4).unwrap());
        assert!(f.journal.pending(NOW + 4).unwrap().is_empty());
        assert!(f.journal.claim(p.id(), NOW + 4).unwrap().is_none());
        assert_eq!(
            *control.head.lock().unwrap(),
            Some(f.ledger.chain_head().unwrap())
        );
    }
}

#[test]
fn signed_fields_and_invalid_mac_are_rejected_by_record_validation() {
    let f = Fixture::new();
    f.journal.mint(f.candidate(1, NOW)).unwrap();
    let source = f
        .ledger
        .get_events(0, 100)
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == EventKind::ApprovalProposed)
        .unwrap();
    let original = source.payload.as_ref().unwrap();
    assert!(
        f.journal
            .decode(
                &source,
                &crate::ledger::hash::canonical_json(original).unwrap()
            )
            .is_ok()
    );
    for pointer in [
        "/mac",
        "/envelope/network",
        "/envelope/seq",
        "/envelope/prev_hash",
        "/envelope/at_ms",
        "/envelope/operation/proposal/intent/px",
        "/envelope/operation/proposal/intent/reason",
        "/envelope/operation/proposal/policy/hash",
        "/envelope/operation/proposal/route_hash",
        "/envelope/operation/proposal/expires_at_ms",
    ] {
        let mut event = source.clone();
        let payload = event.payload.as_mut().unwrap();
        let field = payload.pointer_mut(pointer).unwrap();
        *field = if field.is_number() {
            json!(9)
        } else if pointer == "/mac" {
            json!("0".repeat(64))
        } else if pointer.ends_with("/network") {
            json!("mainnet")
        } else {
            json!("altered")
        };
        let raw = crate::ledger::hash::canonical_json(payload).unwrap();
        assert!(f.journal.decode(&event, &raw).is_err(), "{pointer}");
    }
    // Raw source, not a Value roundtrip: duplicate fields must not disappear.
    let raw = crate::ledger::hash::canonical_json(original).unwrap();
    let duplicate = raw.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
    assert_ne!(duplicate, raw);
    assert!(f.journal.decode(&source, &duplicate).is_err());
    let duplicate_nested = raw.replacen("\"is_buy\":true", "\"is_buy\":true,\"is_buy\":true", 1);
    assert_ne!(duplicate_nested, raw);
    assert!(f.journal.decode(&source, &duplicate_nested).is_err());
}

#[test]
fn valid_retention_redaction_still_blocks_all_approval_replay() {
    let f = Fixture::new();
    let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let seq = f.ledger.chain_head().unwrap().seq;
    f.journal.reject(p.id(), NOW + 1).unwrap();
    f.ledger
        .redact(seq, "synthetic retention", (NOW + 2) as i64)
        .unwrap();
    assert!(f.ledger.verify().unwrap().is_intact());
    assert!(f.journal.pending(NOW + 3).is_err());
    assert!(f.journal.claim(p.id(), NOW + 3).is_err());
    assert!(f.journal.reject(p.id(), NOW + 3).is_err());
}

#[test]
fn missing_and_mismatched_refusal_receipts_leave_claim_consumed() {
    for mismatch in 0..4 {
        let f = Fixture::new();
        let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
        let claim = f.journal.claim(p.id(), NOW + 1).unwrap().unwrap();
        let mut receipt = f.refusal_receipt(&p, NOW + 2);
        if mismatch == 1 {
            receipt.hash = "0".repeat(64);
        }
        if mismatch == 2 {
            receipt.seq = f.route.binding_seq;
        }
        let outcome = if mismatch == 3 {
            Err(Refusal::OrderNotional {
                symbol: "BTC".into(),
                observed_usd: 100.into(),
                limit_usd: 15.into(),
            })
        } else {
            Err(Refusal::MissingReason)
        };
        let before = f.ledger.chain_head().unwrap();
        assert!(
            f.journal
                .finish(
                    claim,
                    &outcome,
                    (mismatch != 0).then_some(&receipt),
                    NOW + 3
                )
                .is_err()
        );
        assert_eq!(f.ledger.chain_head().unwrap(), before);
        assert!(f.journal.claim(p.id(), NOW + 4).unwrap().is_none());
    }
}

impl Fixture {
    fn evaluation_engine(&self) -> crate::guardrail::GuardrailEngine {
        use crate::guardrail::{GuardrailEngine, KillScope};
        let current = self.policy.current().unwrap();
        let mut state = current.state;
        state.guardrails.insert(
            self.route.binding.agent.clone(),
            AgentGuardrails {
                symbols: ["BTC".to_owned()].into(),
                max_order_usd: 100.into(),
                max_position_usd: 100.into(),
                approval_required: false,
                ..AgentGuardrails::default()
            },
        );
        self.policy.replace(current.revision, state, NOW).unwrap();
        // Evaluation uses authenticated wallet metadata; this empty synthetic store
        // never reads or creates a signing key, and these tests never call sign.
        let engine = GuardrailEngine::new(
            self.policy.clone(),
            Arc::new(crate::keys::MemoryKeyStore::new(Network::Testnet)),
            crate::feed::test_session(oppen_hl::Address::from_bytes([1; 20])),
        )
        .unwrap();
        engine
            .operator_release_kill(&KillScope::Global, NOW)
            .unwrap();
        let observation = engine.policy_observation().unwrap();
        engine
            .operator_acknowledge_policy(observation, NOW)
            .unwrap();
        engine
    }

    fn context(
        &self,
        at_ms: u64,
    ) -> (
        oppen_hl::meta::Asset,
        crate::guardrail::MarketRef,
        crate::guardrail::Exposure,
    ) {
        use crate::guardrail::{
            AccountSnapshot, Exposure, FeedQuality, MarketRef, RestingExposure,
        };
        let asset = oppen_hl::meta::Asset {
            index: 0,
            info: oppen_hl::types::AssetInfo {
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
            reference_px: Some(100.into()),
            as_of_ms: at_ms,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
            sigma_day: None,
            vol_ratio: None,
        };
        let exposure = Exposure {
            feed_stamp: Some(
                crate::feed::test_session(oppen_hl::Address::from_bytes([1; 20])).stamp(),
            ),
            account: self.route.binding.container,
            agent: AccountSnapshot {
                as_of_ms: at_ms,
                reconciled: true,
                equity_usd: 1000.into(),
                peak_equity_usd: 1000.into(),
                realized_pnl_today_usd: Decimal::ZERO,
                unrealized_pnl_usd: Decimal::ZERO,
                day_start_ms: at_ms / 86_400_000 * 86_400_000,
                total_position_notional_usd: Decimal::ZERO,
                positions: Default::default(),
                resting: Some(RestingExposure::default()),
            },
            fleet: None,
        };
        (asset, market, exposure)
    }

    fn evaluate(
        &self,
        engine: &crate::guardrail::GuardrailEngine,
        intent: &OrderIntent,
        at_ms: u64,
    ) -> Cleared {
        let (asset, market, exposure) = self.context(at_ms);
        engine
            .evaluate(
                &self.route.binding.agent,
                intent,
                &asset,
                &market,
                &exposure,
                at_ms,
            )
            .unwrap()
    }
}

#[test]
fn approved_finish_links_actual_engine_intent_and_replay_survives_reopen() {
    let f = Fixture::new();
    let engine = f.evaluation_engine();
    let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let claim = f.journal.claim(p.id(), NOW + 1).unwrap().unwrap();
    let cleared = f.evaluate(&engine, p.order_intent().unwrap(), NOW + 2);
    let head = f.ledger.chain_head().unwrap();
    let receipt = Appended {
        seq: head.seq,
        hash: head.hash,
    };
    f.journal
        .finish(claim, &Ok(cleared), Some(&receipt), NOW + 2)
        .unwrap();
    assert!(f.reopened().pending(NOW + 3).unwrap().is_empty());
    let event = f
        .ledger
        .get_events(receipt.seq, 10)
        .unwrap()
        .events
        .pop()
        .unwrap();
    assert_eq!(event.kind, EventKind::ApprovalDisposed);
    assert_eq!(
        event.payload.as_ref().unwrap()["envelope"]["operation"]["outcome"]["intent"],
        json!({"seq": receipt.seq, "hash": receipt.hash})
    );
    f.ledger
        .redact(receipt.seq, "synthetic retention", (NOW + 4) as i64)
        .unwrap();
    assert!(f.ledger.verify().unwrap().is_intact());
    assert!(f.journal.pending(NOW + 5).is_err());
}

#[test]
fn reviewed_finish_rejects_a_different_action_even_with_identical_clearance_economics() {
    let f = Fixture::new();
    let engine = f.evaluation_engine();
    let proposal = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let evidence = f.journal.prepare(proposal.id(), NOW).unwrap().unwrap();
    let evaluated = f.evaluate(&engine, proposal.order_intent().unwrap(), NOW);
    let review = ReviewCommitment::new(
        &evidence,
        proposal.order_intent().unwrap(),
        evaluated.action().clone(),
        evaluated.clearance(),
        NOW,
        NOW,
    )
    .unwrap();
    let claim = f
        .journal
        .claim_review(evidence, review, NOW + 1)
        .unwrap()
        .unwrap();
    let mut different = proposal.order_intent().unwrap().clone();
    different.kind = OrderKind::Limit { tif: Tif::Ioc };
    let cleared = f.evaluate(&engine, &different, NOW + 2);
    assert_eq!(evaluated.clearance().kind, cleared.clearance().kind);
    assert_ne!(evaluated.action(), cleared.action());
    let head = f.ledger.chain_head().unwrap();
    let receipt = Appended {
        seq: head.seq,
        hash: head.hash.clone(),
    };
    assert!(matches!(
        f.journal
            .finish(claim, &Ok(cleared), Some(&receipt), NOW + 2),
        Err(ApprovalError::Conflict { .. })
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert!(f.reopened().pending(NOW + 3).unwrap().is_empty());
}

#[test]
fn reviewed_disposition_digest_binds_entire_commitment_to_actual_receipt() {
    let f = Fixture::new();
    let engine = f.evaluation_engine();
    let proposal = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let (asset, market, exposure) = f.context(NOW);
    let retained = engine
        .operator_prepare_proposal(proposal.id(), &asset, &market, &exposure, NOW)
        .unwrap();
    let cleared = engine
        .operator_confirm_review(retained, &asset, &market, &exposure, NOW + 2)
        .unwrap();
    let events = f.ledger.get_events(0, 100).unwrap().events;
    let claim = events
        .iter()
        .find(|event| event.kind == EventKind::ApprovalClaimed)
        .unwrap();
    let review: ReviewCommitment = serde_json::from_value(
        claim.payload.as_ref().unwrap()["envelope"]["operation"]["review"].clone(),
    )
    .unwrap();
    let intent = events
        .iter()
        .find(|event| event.kind == EventKind::OrderIntent)
        .unwrap();
    let receipt = Appended {
        seq: intent.seq,
        hash: intent.hash.clone(),
    };
    assert_eq!(
        cleared.clearance().approval_review_digest.as_ref(),
        Some(&review.digest().unwrap())
    );
    assert_eq!(
        intent.payload.as_ref().unwrap()["approval_review_digest"],
        review.digest().unwrap()
    );
    let raw = events.last().unwrap().payload.as_ref().unwrap();
    let actual_digest = raw["envelope"]["operation"]["outcome"]["review_receipt"]
        .as_str()
        .unwrap();
    assert_eq!(
        actual_digest,
        review.receipt_digest(&Link::appended(&receipt)).unwrap()
    );
    for field in ["action", "reference", "policy", "deadline", "receipt"] {
        let mut changed = review.clone();
        let mut link = Link::appended(&receipt);
        let ReviewCommitment::Order(order) = &mut changed else {
            panic!("order commitment")
        };
        match field {
            "action" => {
                if let Action::Order { orders, .. } = &mut order.action {
                    orders[0].t = oppen_hl::wire::OrderType::Limit { tif: Tif::Ioc };
                }
            }
            "reference" => order.reference_px += Decimal::ONE,
            "policy" => order.policy.hash = "0".repeat(64),
            "deadline" => order.expires_at_ms += 1,
            _ => link.hash = "0".repeat(64),
        }
        assert_ne!(
            actual_digest,
            changed.receipt_digest(&link).unwrap(),
            "{field}"
        );
    }
    assert!(f.reopened().pending(NOW + 3).unwrap().is_empty());
}

#[test]
fn reviewed_finish_refuses_missing_or_wrong_commitment_in_actual_intent_receipt() {
    for wrong_digest in [false, true] {
        let f = Fixture::new();
        let engine = f.evaluation_engine();
        let proposal = f.journal.mint(f.candidate(1, NOW)).unwrap();
        let evidence = f.journal.prepare(proposal.id(), NOW).unwrap().unwrap();
        let evaluated = f.evaluate(&engine, proposal.order_intent().unwrap(), NOW);
        let review = ReviewCommitment::new(
            &evidence,
            proposal.order_intent().unwrap(),
            evaluated.action().clone(),
            evaluated.clearance(),
            NOW,
            NOW,
        )
        .unwrap();
        let claim = f
            .journal
            .claim_review(evidence, review, NOW + 1)
            .unwrap()
            .unwrap();
        let cleared = f.evaluate(&engine, proposal.order_intent().unwrap(), NOW + 2);
        let mut payload = serde_json::to_value(cleared.clearance()).unwrap();
        payload["reason"] = json!(proposal.order_intent().unwrap().reason);
        assert!(payload.get("approval_review_digest").is_none());
        if wrong_digest {
            payload["approval_review_digest"] = json!("0".repeat(64));
        }
        let receipt = f
            .ledger
            .record_intent(&crate::ledger::NewIntent {
                agent_id: proposal.agent().as_str(),
                ts_ms: (NOW + 2) as i64,
                payload: &payload,
                snapshot: None,
            })
            .unwrap();
        let receipt = Appended {
            seq: receipt.seq(),
            hash: receipt.hash().to_owned(),
        };
        let head = f.ledger.chain_head().unwrap();
        assert!(
            matches!(f.journal.finish(claim, &Ok(cleared), Some(&receipt), NOW + 2),
            Err(ApprovalError::Unavailable { detail }) if detail.contains("reviewed commitment"))
        );
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        assert!(f.reopened().pending(NOW + 3).unwrap().is_empty());
    }
}

#[test]
fn legacy_claim_and_disposition_omit_review_fields_and_preserve_canonical_bytes() {
    let f = Fixture::new();
    let proposal = f.journal.mint(f.candidate(1, NOW)).unwrap();
    let claim = f.journal.claim(proposal.id(), NOW + 1).unwrap().unwrap();
    let receipt = f.refusal_receipt(&proposal, NOW + 2);
    f.journal
        .finish(claim, &Err(Refusal::MissingReason), Some(&receipt), NOW + 2)
        .unwrap();
    let before = f.ledger.get_events(0, 100).unwrap().events;
    for event in before.iter().filter(|event| {
        matches!(
            event.kind,
            EventKind::ApprovalClaimed | EventKind::ApprovalDisposed
        )
    }) {
        let payload = event.payload.as_ref().unwrap();
        let operation = &payload["envelope"]["operation"];
        assert!(operation.get("review").is_none());
        assert!(
            operation
                .get("outcome")
                .is_none_or(|outcome| outcome.get("review_receipt").is_none())
        );
        let raw = super::super::hash::canonical_json(payload).unwrap();
        let decoded = f.journal.decode(event, &raw).unwrap();
        assert_eq!(
            super::super::hash::canonical_json(&serde_json::to_value(decoded).unwrap()).unwrap(),
            raw
        );
    }
    assert!(f.reopened().pending(NOW + 3).unwrap().is_empty());
    assert_eq!(f.ledger.get_events(0, 100).unwrap().events, before);
}

#[test]
fn cancellation_mint_refuses_manual_targets_without_authenticated_acceptance() {
    use crate::guardrail::{CancelContext, CancelTarget};
    {
        let f = Fixture::new();
        let engine = f.evaluation_engine();
        let intent = CancelIntent {
            reason: "explicit cancellation".into(),
            targets: vec![CancelTarget {
                symbol: "BTC".into(),
                asset_index: 0,
                oid: 1,
                cloid: None,
                is_buy: false,
                limit_px: 100.into(),
                sz: Decimal::ONE,
                orig_sz: Decimal::ONE,
                timestamp: NOW - 1,
                order_type: "Limit".into(),
                tif: Some(oppen_hl::wire::Tif::Gtc),
                reduce_only: true,
                is_trigger: false,
                trigger_px: None,
                trigger_condition: None,
                is_position_tpsl: false,
            }],
        };
        let head = f.ledger.chain_head().unwrap();
        assert!(
            f.journal
                .mint_cancel(
                    f.route.binding.agent.clone(),
                    intent.clone(),
                    f.route.clone(),
                    f.policy.current().unwrap().revision,
                    NOW
                )
                .is_err()
        );
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        let context = CancelContext {
            account: f.route.binding.container,
            observed_at_ms: NOW,
            targets: intent.targets.clone(),
        };
        assert!(
            engine
                .evaluate_cancel(&f.route.binding.agent, &intent, &context, NOW)
                .is_err()
        );
        assert!(f.reopened().pending(NOW).unwrap().is_empty());
    }
}

#[test]
fn v14_absent_cancellation_provenance_and_tif_preserve_historical_bytes() {
    let f = Fixture::new();
    let target = json!({
        "symbol":"BTC", "asset_index":0, "oid":1, "cloid":null, "is_buy":false,
        "limit_px":"100", "sz":"1", "orig_sz":"1", "timestamp":NOW,
        "order_type":"Limit", "reduce_only":true, "is_trigger":false,
        "trigger_px":null, "trigger_condition":null, "is_position_tpsl":false
    });
    let intent = json!({"targets":[target], "reason":"historical cancellation"});
    let historical = json!({
        "agent": f.route.binding.agent, "intent":intent, "route": f.route,
        "route_hash":"historical-route-hash", "policy":{"seq":2,"hash":"historical-policy-hash"},
        "expires_at_ms": NOW + APPROVAL_TTL_MS
    });
    let raw = super::super::hash::canonical_json(&historical).unwrap();
    let proposal: Proposed = serde_json::from_str(&raw).unwrap();
    assert!(proposal.cancel_provenance.is_none());
    assert_eq!(
        super::super::hash::canonical_json(&serde_json::to_value(proposal).unwrap()).unwrap(),
        raw
    );
    let review = json!({
        "proposal_root":{"seq":3,"hash":"historical-proposal-hash"}, "route": f.route,
        "candidate":intent, "action":{"type":"cancel","cancels":[{"a":0,"o":1}]},
        "policy":{"seq":2,"hash":"historical-policy-hash"}, "reviewed_at_ms":NOW,
        "observed_at_ms":NOW, "expires_at_ms":NOW + APPROVAL_TTL_MS
    });
    let raw = super::super::hash::canonical_json(&review).unwrap();
    let decoded: ReviewCommitment = serde_json::from_str(&raw).unwrap();
    assert!(
        matches!(&decoded, ReviewCommitment::Cancel(cancel) if cancel.provenance.is_none() && cancel.candidate.targets[0].tif.is_none())
    );
    assert_eq!(
        super::super::hash::canonical_json(&serde_json::to_value(decoded).unwrap()).unwrap(),
        raw
    );
    assert!(
        f.ledger
            .events_of_kind(EventKind::ApprovalClaimed)
            .unwrap()
            .is_empty(),
        "decoding historical evidence does not claim or adopt it"
    );
}

#[test]
fn v12_typed_order_wrapper_preserves_v11_intent_and_review_bytes() {
    let f = Fixture::new();
    let engine = f.evaluation_engine();
    let candidate = f.candidate(1, NOW);
    let original = NormalizedIntent::from_intent(&candidate.intent).unwrap();
    assert_eq!(
        serde_json::to_vec(&original).unwrap(),
        serde_json::to_vec(&StoredIntent::Order(original.clone())).unwrap()
    );
    let proposal = f.journal.mint(candidate).unwrap();
    let evidence = f.journal.prepare(proposal.id(), NOW).unwrap().unwrap();
    let evaluated = f.evaluate(&engine, proposal.order_intent().unwrap(), NOW);
    let review = ReviewCommitment::new(
        &evidence,
        proposal.order_intent().unwrap(),
        evaluated.action().clone(),
        evaluated.clearance(),
        NOW,
        NOW,
    )
    .unwrap();
    let ReviewCommitment::Order(old) = &review else {
        panic!("order commitment")
    };
    assert_eq!(
        serde_json::to_vec(old).unwrap(),
        serde_json::to_vec(&review).unwrap()
    );
    let bytes = serde_json::to_vec(&review).unwrap();
    let replayed: ReviewCommitment = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(replayed.digest().unwrap(), review.digest().unwrap());
    assert_eq!(serde_json::to_vec(&replayed).unwrap(), bytes);
}

#[test]
fn approved_finish_rejects_missing_wrong_hash_and_nonmatching_actual_payload() {
    for mismatch in 0..4 {
        let f = Fixture::new();
        let engine = f.evaluation_engine();
        let p = f.journal.mint(f.candidate(1, NOW)).unwrap();
        let claim = f.journal.claim(p.id(), NOW + 1).unwrap().unwrap();
        let cleared = f.evaluate(&engine, p.order_intent().unwrap(), NOW + 2);
        let head = f.ledger.chain_head().unwrap();
        let mut receipt = Appended {
            seq: head.seq,
            hash: head.hash,
        };
        if mismatch == 1 {
            receipt.hash = "0".repeat(64);
        }
        if mismatch >= 2 {
            let mut payload = serde_json::to_value(cleared.clearance()).unwrap();
            payload["reason"] = json!(p.order_intent().unwrap().reason);
            if mismatch == 2 {
                payload["evaluated_at_ms"] = json!(NOW + 1);
            } else {
                payload["kind"]["cloid"] =
                    json!(Cloid::parse("0x00000000000000000000000000000009").unwrap());
            }
            let recorded = f
                .ledger
                .record_intent(&crate::ledger::NewIntent {
                    agent_id: p.agent().as_str(),
                    ts_ms: (NOW + 2) as i64,
                    payload: &payload,
                    snapshot: None,
                })
                .unwrap();
            receipt = Appended {
                seq: recorded.seq(),
                hash: recorded.hash().to_owned(),
            };
        }
        let before = f.ledger.chain_head().unwrap();
        assert!(
            f.journal
                .finish(
                    claim,
                    &Ok(cleared),
                    (mismatch != 0).then_some(&receipt),
                    NOW + 3
                )
                .is_err()
        );
        assert_eq!(f.ledger.chain_head().unwrap(), before);
        assert!(f.journal.claim(p.id(), NOW + 4).unwrap().is_none());
    }
}

fn sourced_candidate(f: &Fixture, kind: RequestedOrderKind) -> Candidate {
    let mut candidate = f.candidate(1, NOW);
    candidate.intent.px = 101.into();
    candidate.intent.kind = OrderKind::Limit { tif: Tif::Ioc };
    candidate.intent.max_slippage_bps = Some(100.into());
    match &kind {
        RequestedOrderKind::Limit { limit_px, tif } => {
            candidate.intent.px = *limit_px;
            candidate.intent.kind = OrderKind::Limit { tif: *tif };
        }
        RequestedOrderKind::Market { .. } => {}
        RequestedOrderKind::StopMarket {
            trigger_px, tpsl, ..
        } => {
            candidate.intent.kind = OrderKind::Trigger {
                is_market: true,
                trigger_px: *trigger_px,
                tpsl: *tpsl,
            };
        }
        RequestedOrderKind::ClosePosition { position_size, .. } => {
            candidate.intent.is_buy = *position_size < Decimal::ZERO;
            candidate.intent.sz = position_size.abs();
            candidate.intent.reduce_only = true;
        }
    }
    candidate.intent.original = Some(OriginalRequest {
        kind,
        reference_px: Some(100.into()),
        reference_at_ms: NOW,
    });
    candidate
}

#[test]
fn original_request_variants_survive_signed_replay_and_claim() {
    for kind in [
        RequestedOrderKind::Limit {
            limit_px: 101.into(),
            tif: Tif::Ioc,
        },
        RequestedOrderKind::Market {
            slippage_bps: 100.into(),
        },
        RequestedOrderKind::StopMarket {
            trigger_px: 100.into(),
            tpsl: Tpsl::Sl,
            slippage_bps: 100.into(),
        },
        RequestedOrderKind::ClosePosition {
            position_size: -Decimal::ONE,
            slippage_bps: 100.into(),
        },
    ] {
        let f = Fixture::new();
        let mut candidate = sourced_candidate(&f, kind);
        if matches!(
            candidate.intent.original.as_ref().unwrap().kind,
            RequestedOrderKind::Limit { .. }
        ) {
            candidate.intent.original.as_mut().unwrap().reference_px = None;
        }
        let expected = candidate.intent.clone();
        let proposal = f.journal.mint(candidate).unwrap();
        let reopened = f.reopened();
        let pending = reopened.pending(NOW + 1).unwrap();
        assert_eq!(pending[0].order_intent().unwrap(), &expected);
        let claim = reopened.claim(proposal.id(), NOW + 2).unwrap().unwrap();
        assert_eq!(
            claim.proposal().order_intent().unwrap().original,
            expected.original
        );
    }
}

#[test]
fn same_normalized_ioc_retains_source_but_rejects_kind_slippage_and_repricing() {
    let f = Fixture::new();
    let market = RequestedOrderKind::Market {
        slippage_bps: 100.into(),
    };
    let proposal = f
        .journal
        .mint(sourced_candidate(&f, market.clone()))
        .unwrap();
    let head = f.ledger.chain_head().unwrap();
    let mut retry = sourced_candidate(&f, market.clone());
    retry.at_ms += 1;
    assert_eq!(f.journal.mint(retry).unwrap(), proposal);
    let mut limit = sourced_candidate(
        &f,
        RequestedOrderKind::Limit {
            limit_px: 101.into(),
            tif: Tif::Ioc,
        },
    );
    let mut without_source = proposal.order_intent().unwrap().clone();
    without_source.original = None;
    let saved_source = limit.intent.original.take();
    assert_eq!(limit.intent, without_source);
    limit.intent.original = saved_source;
    assert!(matches!(
        f.journal.mint(limit),
        Err(ApprovalError::Conflict { .. })
    ));
    let mut changed_reference = sourced_candidate(&f, market.clone());
    changed_reference
        .intent
        .original
        .as_mut()
        .unwrap()
        .reference_at_ms -= 1;
    assert_eq!(f.journal.mint(changed_reference).unwrap(), proposal);
    let mut repriced = sourced_candidate(&f, market.clone());
    repriced.intent.px = 102.into();
    assert!(matches!(
        f.journal.mint(repriced),
        Err(ApprovalError::Conflict { .. })
    ));
    let changed_slippage = sourced_candidate(
        &f,
        RequestedOrderKind::Market {
            slippage_bps: 101.into(),
        },
    );
    assert!(matches!(
        f.journal.mint(changed_slippage),
        Err(ApprovalError::Conflict { .. })
    ));
    let mut unknown = sourced_candidate(&f, market);
    unknown.intent.original = None;
    assert!(matches!(
        f.journal.mint(unknown),
        Err(ApprovalError::Conflict { .. })
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
}

#[test]
fn explicit_limit_retry_keeps_original_quote_and_expiry_across_new_ticks() {
    let f = Fixture::new();
    let kind = RequestedOrderKind::Limit {
        limit_px: 101.into(),
        tif: Tif::Ioc,
    };
    let proposal = f.journal.mint(sourced_candidate(&f, kind.clone())).unwrap();
    let head = f.ledger.chain_head().unwrap();
    let mut retry = sourced_candidate(&f, kind.clone());
    retry.at_ms = NOW + 10;
    let source = retry.intent.original.as_mut().unwrap();
    source.reference_px = Some(102.into());
    source.reference_at_ms = NOW + 10;
    assert_eq!(f.journal.mint(retry).unwrap(), proposal);
    assert_eq!(
        f.reopened().pending(NOW + 11).unwrap(),
        vec![proposal.clone()]
    );
    assert_eq!(proposal.expires_at_ms(), NOW + APPROVAL_TTL_MS);
    assert_eq!(
        proposal
            .order_intent()
            .unwrap()
            .original
            .as_ref()
            .unwrap()
            .reference_px,
        Some(100.into())
    );
    assert_eq!(
        proposal
            .order_intent()
            .unwrap()
            .original
            .as_ref()
            .unwrap()
            .reference_at_ms,
        NOW
    );
    assert_eq!(f.ledger.chain_head().unwrap(), head);

    let changed_limit = sourced_candidate(
        &f,
        RequestedOrderKind::Limit {
            limit_px: 102.into(),
            tif: Tif::Ioc,
        },
    );
    assert!(matches!(
        f.journal.mint(changed_limit),
        Err(ApprovalError::Conflict { .. })
    ));
    let mut changed_slippage = sourced_candidate(&f, kind.clone());
    changed_slippage.intent.max_slippage_bps = Some(101.into());
    assert!(matches!(
        f.journal.mint(changed_slippage),
        Err(ApprovalError::Conflict { .. })
    ));
    let current = f.policy.current().unwrap();
    let mut next = current.state;
    next.guardrails
        .get_mut(&f.route.binding.agent)
        .unwrap()
        .max_order_usd = 7.into();
    f.policy.replace(current.revision, next, NOW + 12).unwrap();
    let updated = f.ledger.chain_head().unwrap();
    assert!(matches!(
        f.journal.mint(sourced_candidate(&f, kind)),
        Err(ApprovalError::Conflict { .. })
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), updated);
}

#[test]
fn historical_v1_absent_original_preserves_bytes_and_never_infers_market() {
    let f = Fixture::new();
    let mut historical = sourced_candidate(
        &f,
        RequestedOrderKind::Market {
            slippage_bps: 100.into(),
        },
    );
    historical.intent.original = None;
    let proposal = f.journal.mint(historical).unwrap();
    let head = f.ledger.chain_head().unwrap();
    let event = row(&f.ledger.lock().unwrap(), head.seq).unwrap();
    let payload = event.payload.as_ref().unwrap();
    assert_eq!(payload["envelope"]["version"], 1);
    assert!(
        payload
            .pointer("/envelope/operation/proposal/intent/original")
            .is_none()
    );
    let raw = crate::ledger::hash::canonical_json(payload).unwrap();
    let decoded = f.journal.decode(&event, &raw).unwrap();
    assert_eq!(
        crate::ledger::hash::canonical_json(&serde_json::to_value(decoded).unwrap()).unwrap(),
        raw
    );
    let reopened = f.reopened();
    let pending = reopened.pending(NOW + 1).unwrap();
    assert_eq!(pending, vec![proposal]);
    assert!(pending[0].order_intent().unwrap().original.is_none());
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    assert!(matches!(
        f.journal.mint(sourced_candidate(
            &f,
            RequestedOrderKind::Market {
                slippage_bps: 100.into()
            }
        )),
        Err(ApprovalError::Conflict { .. })
    ));
}

#[test]
fn source_must_match_intent_and_is_authenticated_not_display_only() {
    let f = Fixture::new();
    let mut mismatched = sourced_candidate(
        &f,
        RequestedOrderKind::Limit {
            limit_px: 101.into(),
            tif: Tif::Ioc,
        },
    );
    mismatched.intent.px = 102.into();
    let head = f.ledger.chain_head().unwrap();
    assert!(matches!(
        f.journal.mint(mismatched),
        Err(ApprovalError::Unavailable { .. })
    ));
    assert_eq!(f.ledger.chain_head().unwrap(), head);
    f.journal
        .mint(sourced_candidate(
            &f,
            RequestedOrderKind::Market {
                slippage_bps: 100.into(),
            },
        ))
        .unwrap();
    let head = f.ledger.chain_head().unwrap();
    let mut event = row(&f.ledger.lock().unwrap(), head.seq).unwrap();
    let payload = event.payload.as_mut().unwrap();
    payload["envelope"]["operation"]["proposal"]["intent"]["original"]["reference_at_ms"] =
        json!(NOW - 1);
    let raw = crate::ledger::hash::canonical_json(payload).unwrap();
    assert!(
        matches!(f.journal.decode(&event, &raw), Err(ApprovalError::Unavailable { detail }) if detail == "approval MAC mismatch")
    );
    let payload = event.payload.as_mut().unwrap();
    payload["envelope"]["operation"]["proposal"]["intent"]
        .as_object_mut()
        .unwrap()
        .remove("original");
    let raw = crate::ledger::hash::canonical_json(payload).unwrap();
    assert!(
        matches!(f.journal.decode(&event, &raw), Err(ApprovalError::Unavailable { detail }) if detail == "approval MAC mismatch")
    );
}
