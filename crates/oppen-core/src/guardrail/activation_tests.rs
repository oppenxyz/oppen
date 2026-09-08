use super::*;

#[path = "kill_release_tests.rs"]
mod kill_release_tests;
#[path = "pilot_consent_tests.rs"]
mod pilot_consent_tests;
use crate::feed::{FeedSession, TestIngress};
use crate::guardrail::{LegacyPolicyReview, MarginMode};
use crate::keys::{HmacKey, MemoryKeyStore, SecretText};
use crate::ledger::{
    Anchor, FileAnchor, HeadAnchor, Ledger, LedgerError, PilotJournal, RegistryBinding,
    RegistryJournal,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tempfile::TempDir;

const NOW: u64 = 1_788_998_400_000;

type Hook = Arc<Mutex<Option<(u64, Box<dyn FnOnce() + Send>)>>>;

struct TestAnchor {
    inner: FileAnchor,
    fail: Arc<AtomicBool>,
    hook: Hook,
}
impl std::fmt::Debug for TestAnchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestAnchor").finish_non_exhaustive()
    }
}
impl HeadAnchor for TestAnchor {
    fn load(&self) -> Result<Option<Anchor>, LedgerError> {
        self.inner.load()
    }
    fn store(&self, anchor: &Anchor) -> Result<(), LedgerError> {
        let hook = {
            let mut held = self.hook.lock().unwrap();
            if held.as_ref().is_some_and(|(seq, _)| *seq == anchor.seq) {
                held.take()
            } else {
                None
            }
        };
        if let Some((_, hook)) = hook {
            hook();
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(LedgerError::Io(std::io::Error::other(
                "activation fixture anchor failure",
            )));
        }
        self.inner.store(anchor)
    }
}

struct Fixture {
    engine: Arc<GuardrailEngine>,
    ledger: Arc<Ledger>,
    registry: Arc<RegistryJournal>,
    policy: Arc<PolicyJournal>,
    pilot: PilotJournal,
    keys: Arc<MemoryKeyStore>,
    agent: AgentId,
    account: Address,
    feed: Arc<FeedSession>,
    fail: Arc<AtomicBool>,
    hook: Hook,
    _ingress: TestIngress,
    _dir: TempDir,
}

impl Fixture {
    fn new(consent: bool, sub_account: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("activation.db");
        let fail = Arc::new(AtomicBool::new(false));
        let hook: Hook = Arc::new(Mutex::new(None));
        let ledger = Arc::new(
            Ledger::open_anchored(
                &path,
                Network::Testnet,
                Some(Box::new(TestAnchor {
                    inner: FileAnchor::beside(&path),
                    fail: fail.clone(),
                    hook: hook.clone(),
                })),
            )
            .unwrap(),
        );
        let keys = Arc::new(MemoryKeyStore::new(Network::Testnet));
        let agent = AgentId::from("activation-agent");
        let account = Address::from_bytes([11; 20]);
        let wallet = keys
            .create_agent_key(
                &agent,
                SecretText::new(format!("{:064x}", 1)),
                NOW + 86_400_000,
                NOW,
            )
            .unwrap();
        let registry = Arc::new(
            RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([31; 32]))).unwrap(),
        );
        registry
            .grant(
                RegistryBinding {
                    agent: agent.clone(),
                    container: account,
                    vault_address: sub_account.then_some(account),
                    wallet,
                },
                NOW,
            )
            .unwrap();
        let policy = Arc::new(PolicyJournal::new(registry.clone()));
        let legacy =
            LegacyPolicyReview::open(dir.path().join("absent.db"), Network::Testnet, NOW).unwrap();
        let mut initial = PersistedState::paused(NOW);
        initial.guardrails.insert(
            agent.clone(),
            AgentGuardrails {
                symbols: ["TEST".to_owned()].into(),
                max_order_usd: Decimal::from(15),
                max_position_usd: Decimal::from(25),
                approval_required: false,
                risk: crate::guardrail::RiskSettings {
                    max_leverage: 1,
                    margin_mode: MarginMode::Cross,
                    max_open_exposure_usd: Some(Decimal::from(25)),
                    max_risk_usd: None,
                },
                ..AgentGuardrails::default()
            },
        );
        policy.initialize(&legacy, initial, NOW).unwrap();
        let feed = Arc::new(FeedSession::new());
        feed.bind(Network::Testnet, account).unwrap();
        let ingress = crate::feed::test_ingress(&feed);
        feed.reconciled(&feed.stamp(), NOW);
        let engine = Arc::new(
            GuardrailEngine::new_supervised_alpha(policy.clone(), keys.clone(), feed.clone())
                .unwrap(),
        );
        engine
            .operator_release_kill(&KillScope::Global, NOW)
            .unwrap();
        let pilot = PilotJournal::new(registry.clone());
        if consent {
            pilot.authorize(agent.clone(), account, NOW).unwrap();
        }
        Self {
            engine,
            ledger,
            registry,
            policy,
            pilot,
            keys,
            agent,
            account,
            feed,
            fail,
            hook,
            _ingress: ingress,
            _dir: dir,
        }
    }

    fn evidence(&self) -> ActivationEvidence {
        let route = self.registry.route_for_agent(&self.agent).unwrap();
        let user = if route.binding.vault_address.is_some() {
            Address::from_bytes([12; 20])
        } else {
            self.account
        };
        ActivationEvidence {
            read_started_at_ms: NOW, read_completed_at_ms: NOW,
            perps: serde_json::from_value(serde_json::json!({
                "marginSummary": {"accountValue":"100","totalNtlPos":"0","totalRawUsd":"100","totalMarginUsed":"0"},
                "crossMarginSummary": {"accountValue":"100","totalNtlPos":"0","totalRawUsd":"100","totalMarginUsed":"0"},
                "crossMaintenanceMarginUsed":"0", "withdrawable":"100", "assetPositions":[], "time": NOW
            })).unwrap(),
            spot: SpotClearinghouseState { balances: vec![] }, orders: vec![],
            reference_prices: ReferencePrices::default(),
            account_role: if route.binding.vault_address.is_some() { UserRole::SubAccount { master: user } } else { UserRole::User },
            signer_role: UserRole::Agent { user },
            extra_agents: vec![ExtraAgent { name: "display only".into(), address: route.binding.wallet.address, valid_until: NOW + 86_400_000 }],
        }
    }

    fn review(&self) -> ActivationReview {
        let observation = self
            .engine
            .begin_activation_review(&self.agent, self.account)
            .unwrap();
        self.engine
            .review_activation(observation, self.evidence(), &|| NOW)
            .unwrap()
    }

    fn fresh_evidence(&self, at_ms: u64) -> ActivationEvidence {
        let mut evidence = self.evidence();
        evidence.read_started_at_ms = at_ms;
        evidence.read_completed_at_ms = at_ms;
        evidence.perps.time = at_ms;
        evidence
    }

    fn at_next_audit(&self, hook: impl FnOnce() + Send + 'static) {
        let next = self.ledger.chain_head().unwrap().seq + 1;
        *self.hook.lock().unwrap() = Some((next, Box::new(hook)));
    }

    fn reopen(self) -> Self {
        let Self {
            engine,
            ledger,
            registry,
            policy,
            pilot,
            keys,
            agent,
            account,
            feed,
            fail,
            hook,
            _ingress,
            _dir,
        } = self;
        let old_ledger = Arc::downgrade(&ledger);
        drop(engine);
        drop(pilot);
        drop(policy);
        drop(registry);
        drop(ledger);
        drop(_ingress);
        drop(feed);
        assert!(
            old_ledger.upgrade().is_none(),
            "physical reopen must release every ledger owner"
        );
        let path = _dir.path().join("activation.db");
        let ledger = Arc::new(
            Ledger::open_anchored(
                &path,
                Network::Testnet,
                Some(Box::new(TestAnchor {
                    inner: FileAnchor::beside(&path),
                    fail: fail.clone(),
                    hook: hook.clone(),
                })),
            )
            .unwrap(),
        );
        let registry = Arc::new(
            RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([31; 32]))).unwrap(),
        );
        let policy = Arc::new(PolicyJournal::new(registry.clone()));
        let pilot = PilotJournal::new(registry.clone());
        let feed = Arc::new(FeedSession::new());
        feed.bind(Network::Testnet, account).unwrap();
        let ingress = crate::feed::test_ingress(&feed);
        feed.reconciled(&feed.stamp(), NOW);
        let engine = Arc::new(
            GuardrailEngine::new_supervised_alpha(policy.clone(), keys.clone(), feed.clone())
                .unwrap(),
        );
        Self {
            engine,
            ledger,
            registry,
            policy,
            pilot,
            keys,
            agent,
            account,
            feed,
            fail,
            hook,
            _ingress: ingress,
            _dir,
        }
    }

    // Historical synthetic order evidence, using the same audited intent and
    // authenticated reservation APIs as ledger/pilot_tests. No signer is invoked.
    fn historical_order(&self, id: u8, is_buy: bool) -> crate::ledger::SubmissionReceipt {
        let route = self.registry.route_for_agent(&self.agent).unwrap();
        let clearance = Clearance {
            approval_review_digest: None,
            agent: self.agent.clone(),
            vault_address: route.binding.vault_address,
            route,
            network: Network::Testnet,
            policy_revision: self.policy.current().unwrap().revision,
            evaluated_at_ms: NOW,
            kind: ClearedKind::Order {
                symbol: "TEST".into(),
                is_buy,
                px: Decimal::from(100),
                sz: Decimal::new(15, 2),
                notional_usd: Decimal::from(15),
                reduce_only: !is_buy,
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
                order_tokens_remaining: Decimal::ONE,
                global_tokens_remaining: Decimal::ONE,
            },
        };
        self.engine
            .sink
            .record(&AuditEntry {
                agent: Some(&self.agent),
                at_ms: NOW,
                reason: "synthetic historical order",
                outcome: AuditOutcome::Cleared(&clearance),
            })
            .unwrap();
        let journal = self.engine.submissions().unwrap();
        let revision = journal.state(self.account).unwrap().revision;
        journal
            .begin(self.account, &clearance, revision, NOW)
            .unwrap()
    }

    fn observe_order(&self, receipt: &crate::ledger::SubmissionReceipt, id: u8, status: &str) {
        self.engine
            .submissions()
            .unwrap()
            .resolve(
                receipt,
                crate::ledger::SubmissionResolution::Observed {
                    oid: u64::from(id),
                    status: status.into(),
                },
                NOW,
            )
            .unwrap();
    }

    fn historical_fill(&self, id: u8, tid: u64, is_buy: bool, size: &str, pnl: &str, fee: &str) {
        let payload = serde_json::json!({
            "account": self.account, "tid":tid, "ts_ms":NOW, "oid":u64::from(id),
            "cloid":Cloid::from_bytes([id; 16]), "coin":"TEST",
            "side":if is_buy { "buy" } else { "sell" }, "px":"100", "sz":size,
            "closed_pnl":pnl, "fee":fee, "fee_token":"USDC"
        });
        assert!(
            self.ledger
                .record_fill(&crate::ledger::NewFill {
                    account: &self.account.to_string(),
                    tid,
                    ts_ms: NOW as i64,
                    agent_id: Some(self.agent.as_str()),
                    payload: &payload,
                })
                .unwrap()
                .is_some()
        );
    }

    fn consent_count(&self) -> usize {
        self.ledger
            .get_events(0, 1000)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == crate::ledger::EventKind::PilotAuthorized)
            .count()
    }
}

#[test]
fn activation_is_explicit_scoped_audited_and_restart_requires_fresh_review() {
    let f = Fixture::new(true, false);
    let baseline = f.pilot.state(f.account).unwrap().unwrap();
    assert!(f.engine.policy_status().admission_inhibited);
    assert!(
        f.engine
            .operator_acknowledge_policy(f.engine.policy_observation().unwrap(), NOW)
            .is_err()
    );
    let review = f.review();
    assert_eq!(review.display().pilot, baseline);
    assert_eq!(review.display().remaining_committed_usd, Decimal::from(150));
    assert!(f.engine.policy_status().admission_inhibited);
    let before = f.ledger.chain_head().unwrap();
    let receipt = f
        .engine
        .confirm_activation(review, f.evidence(), &|| NOW, &|| Ok(()))
        .unwrap();
    assert_eq!(receipt.audit_seq, before.seq + 1);
    assert_eq!(receipt.audit_hash, f.ledger.chain_head().unwrap().hash);
    assert!(!f.engine.policy_status().admission_inhibited);
    let state = f.engine.state();
    assert!(
        state
            .check_scoped_acknowledgment(&receipt.route, true)
            .is_ok()
    );
    let mut foreign = receipt.route.clone();
    foreign.binding.container = Address::from_bytes([99; 20]);
    assert!(state.check_scoped_acknowledgment(&foreign, true).is_err());
    drop(state);
    let reopened =
        GuardrailEngine::new_supervised_alpha(f.policy.clone(), f.keys.clone(), f.feed.clone())
            .unwrap();
    assert!(reopened.policy_status().admission_inhibited);
    assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), baseline);
}

#[test]
fn missing_consent_wrong_account_and_foreign_engine_are_refused() {
    let missing = Fixture::new(false, false);
    assert!(
        missing
            .engine
            .begin_activation_review(&missing.agent, missing.account)
            .is_err()
    );
    let f = Fixture::new(true, false);
    assert!(
        f.engine
            .begin_activation_review(&f.agent, Address::from_bytes([22; 20]))
            .is_err()
    );
    let foreign = Fixture::new(true, false);
    assert!(
        foreign
            .engine
            .confirm_activation(f.review(), f.evidence(), &|| NOW, &|| Ok(()))
            .is_err()
    );
    assert!(foreign.engine.policy_status().admission_inhibited);
}

#[test]
fn replacement_and_reconnect_never_revive_an_old_review() {
    let f = Fixture::new(true, false);
    let old = f.review();
    let _replacement = f.review();
    assert!(
        f.engine
            .confirm_activation(old, f.evidence(), &|| NOW, &|| Ok(()))
            .is_err()
    );
    let old = f.review();
    let stamp = f.feed.unreconciled();
    f.feed.reconciled(&stamp, NOW);
    assert!(
        f.engine
            .confirm_activation(old, f.evidence(), &|| NOW, &|| Ok(()))
            .is_err()
    );
    f.engine
        .confirm_activation(f.review(), f.evidence(), &|| NOW, &|| Ok(()))
        .unwrap();
}

#[test]
fn raw_roles_approval_expiry_and_duplicates_are_checked() {
    for case in 0..6 {
        let f = Fixture::new(true, case == 5);
        let observation = f
            .engine
            .begin_activation_review(&f.agent, f.account)
            .unwrap();
        let mut evidence = f.evidence();
        match case {
            0 => evidence.account_role = UserRole::Vault,
            1 => {
                evidence.signer_role = UserRole::Agent {
                    user: Address::from_bytes([55; 20]),
                }
            }
            2 => evidence.extra_agents[0].valid_until = NOW,
            3 => evidence.extra_agents.push(evidence.extra_agents[0].clone()),
            4 => evidence.extra_agents.clear(),
            5 => {}
            _ => unreachable!(),
        }
        let result = f.engine.review_activation(observation, evidence, &|| NOW);
        assert_eq!(result.is_ok(), case == 5);
    }
}

#[test]
fn policy_and_route_changes_between_review_and_confirmation_refuse() {
    for retire in [false, true] {
        let f = Fixture::new(true, false);
        let review = f.review();
        let evidence = f.evidence();
        if retire {
            f.registry.retire(&review.display().route, NOW).unwrap();
        } else {
            let mut config = review.display().policy.clone();
            config.approval_required = true;
            f.engine
                .operator_set_guardrails(&f.agent, config, NOW)
                .unwrap();
        }
        assert!(
            f.engine
                .confirm_activation(review, evidence, &|| NOW, &|| Ok(()))
                .is_err()
        );
        assert!(f.engine.policy_status().admission_inhibited);
    }
}

#[test]
fn ingress_during_account_reads_and_after_audit_prevents_acknowledgment() {
    let f = Fixture::new(true, false);
    let observation = f
        .engine
        .begin_activation_review(&f.agent, f.account)
        .unwrap();
    f.feed.unreconciled();
    assert!(
        f.engine
            .review_activation(observation, f.evidence(), &|| NOW)
            .is_err()
    );
    f.feed.reconciled(&f.feed.stamp(), NOW);
    let review = f.review();
    let feed = f.feed.clone();
    f.at_next_audit(move || {
        feed.unreconciled();
    });
    assert!(
        f.engine
            .confirm_activation(review, f.evidence(), &|| NOW, &|| Ok(()))
            .is_err()
    );
    assert!(f.engine.policy_status().admission_inhibited);
}

#[test]
fn final_clock_is_sampled_after_audit_publication_wait() {
    let f = Fixture::new(true, false);
    let review = f.review();
    let deadline = NOW + review.display().policy.freshness.max_market_age_ms;
    let clock = Arc::new(AtomicU64::new(NOW));
    let advance = clock.clone();
    f.at_next_audit(move || advance.store(deadline, Ordering::SeqCst));
    assert!(
        f.engine
            .confirm_activation(
                review,
                f.evidence(),
                &|| clock.load(Ordering::SeqCst),
                &|| Ok(())
            )
            .is_err()
    );
    assert!(f.engine.policy_status().admission_inhibited);
}

#[test]
fn local_stop_and_pairing_revocation_during_audit_cannot_enable() {
    for local_stop in [false, true] {
        let f = Fixture::new(true, false);
        let review = f.review();
        let allowed = Arc::new(AtomicBool::new(true));
        let revoke = allowed.clone();
        let engine = f.engine.clone();
        f.at_next_audit(move || {
            if local_stop {
                engine.state().inhibit();
            } else {
                revoke.store(false, Ordering::SeqCst);
            }
        });
        assert!(
            f.engine
                .confirm_activation(review, f.evidence(), &|| NOW, &|| {
                    if allowed.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(refused("pairing revoked"))
                    }
                })
                .is_err()
        );
        assert!(f.engine.policy_status().admission_inhibited);
    }
}

#[test]
fn audit_failure_and_callback_panic_leave_admission_inhibited() {
    let f = Fixture::new(true, false);
    let review = f.review();
    f.fail.store(true, Ordering::SeqCst);
    assert!(
        f.engine
            .confirm_activation(review, f.evidence(), &|| NOW, &|| Ok(()))
            .is_err()
    );
    assert!(f.engine.policy_status().admission_inhibited);
    f.fail.store(false, Ordering::SeqCst);
    let review = f.review();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        f.engine
            .confirm_activation(review, f.evidence(), &|| NOW, &|| {
                panic!("synthetic observer panic")
            })
    }));
    assert!(result.is_err());
    assert!(f.engine.policy_status().admission_inhibited);
}

#[test]
fn human_review_delay_accepts_fresh_evidence_and_current_feed_tick() {
    let f = Fixture::new(true, false);
    let review = f.review();
    assert_eq!(review.display().expires_at_ms, NOW + 60_000);
    let at = NOW + 17_000;
    f.feed.reconciled(&f.feed.stamp(), at);
    let receipt = f
        .engine
        .confirm_activation(review, f.fresh_evidence(at), &|| at, &|| Ok(()))
        .unwrap();
    assert_eq!(receipt.acknowledged_at_ms, at);
    assert!(!f.engine.policy_status().admission_inhibited);
}

#[test]
fn stale_confirmation_evidence_and_human_deadline_both_refuse() {
    for human_expired in [false, true] {
        let f = Fixture::new(true, false);
        let review = f.review();
        let at = NOW + if human_expired { 60_000 } else { 17_000 };
        f.feed.reconciled(&f.feed.stamp(), at);
        let evidence = if human_expired {
            f.fresh_evidence(at)
        } else {
            f.evidence()
        };
        assert!(
            f.engine
                .confirm_activation(review, evidence, &|| at, &|| Ok(()))
                .is_err()
        );
        assert!(f.engine.policy_status().admission_inhibited);
    }
}

fn position_evidence(f: &Fixture, at_ms: u64, mark: &str) -> ActivationEvidence {
    let mut evidence = f.fresh_evidence(at_ms);
    evidence.perps.asset_positions = vec![
        serde_json::from_value(serde_json::json!({
            "type":"oneWay", "position": {
                "coin":"TEST", "szi":"0.1", "entryPx":"100", "positionValue":"10",
                "unrealizedPnl":"0", "returnOnEquity":"0", "liquidationPx":null,
                "marginUsed":"10", "maxLeverage":10, "leverage":{"type":"cross","value":1},
                "cumFunding":{"allTime":"0","sinceOpen":"0","sinceChange":"0"}
            }
        }))
        .unwrap(),
    ];
    let ctxs: oppen_hl::types::MetaAndAssetCtxs = serde_json::from_value(serde_json::json!([
        {"universe":[{"name":"TEST","szDecimals":2,"maxLeverage":10}]},
        [{"funding":"0","openInterest":"100","prevDayPx":"100","dayNtlVlm":"100",
            "premium":"0","oraclePx":mark,"markPx":mark,"midPx":mark,"impactPxs":[mark,mark]}]
    ]))
    .unwrap();
    evidence.reference_prices = ctxs.reference_pxs();
    evidence
}

#[test]
fn quote_and_unrealized_pnl_refresh_is_allowed_but_material_positions_are_not() {
    for case in 0..5 {
        let f = Fixture::new(true, false);
        let observation = f
            .engine
            .begin_activation_review(&f.agent, f.account)
            .unwrap();
        let review = f
            .engine
            .review_activation(observation, position_evidence(&f, NOW, "100"), &|| NOW)
            .unwrap();
        let at = NOW + 10_000;
        f.feed.reconciled(&f.feed.stamp(), at);
        let mut fresh = position_evidence(&f, at, "101");
        fresh.perps.asset_positions[0].position.unrealized_pnl = Decimal::new(1, 1);
        fresh.perps.asset_positions[0].position.position_value = Decimal::new(101, 1);
        fresh.perps.margin_summary.account_value = Decimal::new(1001, 1);
        match case {
            0 => {}
            1 => fresh.perps.asset_positions[0].position.szi = Decimal::new(11, 2),
            2 => fresh.perps.asset_positions[0].position.entry_px = Some(Decimal::from(99)),
            3 => fresh.perps.asset_positions[0].position.leverage.value = 2,
            4 => fresh.perps.asset_positions[0].position.leverage.kind = "isolated".into(),
            _ => unreachable!(),
        }
        let result = f
            .engine
            .confirm_activation(review, fresh, &|| at, &|| Ok(()));
        assert_eq!(result.is_ok(), case == 0, "case {case}: {result:?}");
    }
}

#[test]
fn protective_order_changes_and_missing_marks_refuse() {
    for changed in [false, true] {
        let f = Fixture::new(true, false);
        let mut evidence = position_evidence(&f, NOW, "100");
        evidence.orders = vec![serde_json::from_value(serde_json::json!({
            "coin":"TEST","side":"A","limitPx":"100","sz":"0.1","origSz":"0.1",
            "oid":1,"timestamp":NOW,"orderType":"Limit","tif":"Gtc","reduceOnly":true,
            "isTrigger":false,"triggerPx":null,"triggerCondition":null,"isPositionTpsl":false,"cloid":null
        })).unwrap()];
        let mut fresh = position_evidence(&f, NOW, "100");
        fresh.orders = evidence.orders.clone();
        let observation = f
            .engine
            .begin_activation_review(&f.agent, f.account)
            .unwrap();
        let review = f
            .engine
            .review_activation(observation, evidence, &|| NOW)
            .unwrap();
        if changed {
            fresh.orders[0].reduce_only = false;
        } else {
            fresh.reference_prices = ReferencePrices::default();
        }
        assert!(
            f.engine
                .confirm_activation(review, fresh, &|| NOW, &|| Ok(()))
                .is_err()
        );
    }
}

#[test]
fn only_bound_confirmation_allows_a_real_guarded_order_signature() {
    use crate::guardrail::FeedQuality;
    use oppen_hl::types::AssetInfo;
    use oppen_hl::wire::Tif;
    let f = Fixture::new(true, false);
    let intent = OrderIntent {
        original: None,
        symbol: "TEST".into(),
        is_buy: true,
        px: Decimal::from(100),
        sz: Decimal::new(1, 1),
        kind: OrderKind::Limit { tif: Tif::Gtc },
        reduce_only: false,
        cloid: Some(Cloid::parse("0x00000000000000000000000000000001").unwrap()),
        grouping: Grouping::Na,
        builder: None,
        max_slippage_bps: None,
        reason: "synthetic activation proof".into(),
    };
    let asset = Asset {
        index: 0,
        info: AssetInfo {
            name: "TEST".into(),
            sz_decimals: 2,
            max_leverage: 10,
            margin_table_id: 0,
            is_delisted: false,
            only_isolated: false,
        },
    };
    let market = MarketRef {
        symbol: "TEST".into(),
        reference_px: Some(Decimal::from(100)),
        as_of_ms: NOW,
        quality: FeedQuality::Ok,
        mark_divergence_bps: None,
        mark_divergent_since_ms: None,
        snapshot: None,
        sigma_day: None,
        vol_ratio: None,
    };
    let review = f.review();
    let mut exposure =
        crate::state::exposure_from(&review.display().account, Decimal::ZERO, None, true, NOW);
    exposure.feed_stamp = Some(f.feed.stamp());
    assert!(
        !f.engine
            .preflight(&f.agent, &intent, &asset, &market, &exposure, NOW)
            .would_clear
    );
    f.engine
        .confirm_activation(review, f.evidence(), &|| NOW, &|| Ok(()))
        .unwrap();
    let cleared = f
        .engine
        .evaluate(&f.agent, &intent, &asset, &market, &exposure, NOW)
        .unwrap();
    let journal = f.engine.submissions().unwrap();
    let revision = journal.state(f.account).unwrap().revision;
    let reservation = journal
        .begin(f.account, cleared.clearance(), revision, NOW)
        .unwrap();
    let _signed = f
        .engine
        .sign_submission_authorized(
            cleared,
            &journal,
            &reservation,
            NOW,
            None,
            || NOW,
            || Ok(()),
        )
        .unwrap();
    let events = f.ledger.get_events(0, 100).unwrap().events;
    assert!(
        events
            .iter()
            .any(|event| event.kind == crate::ledger::EventKind::OrderIntent)
    );
    assert!(f.ledger.verify().unwrap().is_intact());
    assert_eq!(
        f.pilot.state(f.account).unwrap().unwrap().reserved_usd,
        Decimal::from(10)
    );
}

fn ordinary_row(f: &Fixture) -> u64 {
    f.ledger
        .append(&crate::ledger::NewEvent {
            kind: crate::ledger::EventKind::OperatorAction,
            ts_ms: NOW as i64,
            agent_id: Some(f.agent.as_str()),
            payload: &serde_json::json!({"reason":"ordinary fixture audit claim"}),
            snapshot: None,
        })
        .unwrap()
        .seq
}

fn corrupt_ordinary_row(path: &std::path::Path, seq: u64) {
    let writer = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        writer
            .execute(
                "UPDATE events SET payload = ?1 WHERE seq = ?2",
                rusqlite::params![r#"{"reason":"changed ordinary audit claim"}"#, seq],
            )
            .unwrap(),
        1
    );
}

#[test]
fn raw_wal_corruption_during_verified_read_is_rechecked_before_audit_append() {
    let f = Fixture::new(true, false);
    let seq = ordinary_row(&f);
    let review = f.review();
    let evidence = f.evidence();
    let before = f.ledger.chain_head().unwrap();
    let path = f._dir.path().join("activation.db");
    let (start, ready) = std::sync::mpsc::channel();
    let (done, completed) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        corrupt_ordinary_row(&path, seq);
        done.send(()).unwrap();
    });
    // Model preemption after verification. The competing writer bypasses only
    // application coordination, not SQLite, and alters no consent/authority row.
    let first = AtomicBool::new(true);
    let result = f.engine.confirm_activation(review, evidence, &|| NOW, &|| {
        if first.swap(false, Ordering::SeqCst) {
            start.send(()).unwrap();
            completed
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }
        Ok(())
    });
    writer.join().unwrap();
    assert!(result.is_err());
    assert_eq!(
        f.ledger.chain_head().unwrap(),
        before,
        "no request appended from the old snapshot"
    );
    assert!(f.engine.policy_status().admission_inhibited);
}

#[test]
fn raw_wal_corruption_during_anchor_publication_is_rechecked_before_acknowledgment() {
    let f = Fixture::new(true, false);
    let seq = ordinary_row(&f);
    let review = f.review();
    let evidence = f.evidence();
    let before = f.ledger.chain_head().unwrap();
    let path = f._dir.path().join("activation.db");
    f.at_next_audit(move || {
        std::thread::spawn(move || corrupt_ordinary_row(&path, seq))
            .join()
            .unwrap();
    });
    let result = f
        .engine
        .confirm_activation(review, evidence, &|| NOW, &|| Ok(()));
    assert!(result.is_err());
    assert_eq!(
        f.ledger.chain_head().unwrap().seq,
        before.seq + 1,
        "request audit committed, not acknowledgment"
    );
    assert!(f.engine.policy_status().admission_inhibited);
}

#[test]
fn fresh_rest_does_not_override_missing_or_stale_account_feed_tick() {
    let f = Fixture::new(true, false);
    let review = f.review();
    let at = NOW + 6_000;
    assert!(
        f.engine
            .confirm_activation(review, f.fresh_evidence(at), &|| at, &|| Ok(()))
            .is_err()
    );
    assert!(f.engine.policy_status().admission_inhibited);

    let observation = f
        .engine
        .begin_activation_review(&f.agent, f.account)
        .unwrap();
    assert!(validate_evidence(&observation, &f.evidence(), NOW, None).is_err());
}

#[test]
fn account_feed_tick_aging_during_publication_refuses_even_while_rest_is_fresh() {
    let f = Fixture::new(true, false);
    let review = f.review();
    let at = NOW + 4_000;
    let clock = Arc::new(AtomicU64::new(at));
    let advance = clock.clone();
    f.at_next_audit(move || advance.store(NOW + 5_001, Ordering::SeqCst));
    let result = f.engine.confirm_activation(
        review,
        f.fresh_evidence(at),
        &|| clock.load(Ordering::SeqCst),
        &|| Ok(()),
    );
    assert!(result.is_err());
    assert!(f.engine.policy_status().admission_inhibited);
}

#[test]
fn physical_reopen_preserves_nonzero_usage_fees_canceled_reservation_and_original_baseline() {
    let f = Fixture::new(true, false);
    let original = f.pilot.state(f.account).unwrap().unwrap();
    let receipt = f.historical_order(1, true);
    f.observe_order(&receipt, 1, "canceled");
    f.historical_fill(1, 1, true, "0.05", "0", "0.1");
    let expected = f.pilot.state(f.account).unwrap().unwrap();
    assert_eq!(expected.executed_usd, Decimal::from(5));
    assert_eq!(expected.reserved_usd, Decimal::from(10));
    assert_eq!(expected.net_realized_pnl_usd, Decimal::new(-1, 1));
    assert_eq!(expected.baseline, original.baseline);
    assert_eq!(expected.authorized_at_ms, original.authorized_at_ms);
    assert!(expected.halt.is_none());
    let before = f.ledger.chain_head().unwrap();
    let f = f.reopen();
    assert_eq!(f.ledger.chain_head().unwrap(), before);
    assert!(f.engine.policy_status().admission_inhibited);
    assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), expected);
    let mut evidence = position_evidence(&f, NOW, "100");
    let position = &mut evidence.perps.asset_positions[0].position;
    position.szi = Decimal::new(5, 2);
    position.position_value = Decimal::from(5);
    position.margin_used = Decimal::from(5);
    let mut fresh = position_evidence(&f, NOW, "100");
    fresh.perps = evidence.perps.clone();
    let observation = f
        .engine
        .begin_activation_review(&f.agent, f.account)
        .unwrap();
    let review = f
        .engine
        .review_activation(observation, evidence, &|| NOW)
        .unwrap();
    assert_eq!(review.display().pilot, expected);
    assert_eq!(review.display().remaining_committed_usd, Decimal::from(135));
    f.engine
        .confirm_activation(review, fresh, &|| NOW, &|| Ok(()))
        .unwrap();
    assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), expected);
    assert_eq!(f.consent_count(), 1);
    assert!(!f.engine.policy_status().admission_inhibited);
}

#[test]
fn physical_reopen_executed_and_loss_stops_cannot_be_activated_or_reset() {
    for loss in [false, true] {
        let f = Fixture::new(true, false);
        let baseline = f.pilot.state(f.account).unwrap().unwrap().baseline;
        if loss {
            let receipt = f.historical_order(1, true);
            f.observe_order(&receipt, 1, "filled");
            f.historical_fill(1, 1, true, "0.05", "0", "5");
            // The remaining fill can improve current net PnL, never erase the stop.
            f.historical_fill(1, 2, true, "0.10", "10", "0");
        } else {
            for id in 1..=10 {
                let is_buy = id % 2 != 0;
                let receipt = f.historical_order(id, is_buy);
                f.observe_order(&receipt, id, "filled");
                f.historical_fill(id, u64::from(id), is_buy, "0.15", "0", "0");
            }
        }
        let expected = f.pilot.state(f.account).unwrap().unwrap();
        let expected_metric = if loss {
            crate::guardrail::PilotMetric::RealizedLoss
        } else {
            crate::guardrail::PilotMetric::ExecutedNotional
        };
        assert!(
            matches!(&expected.halt, Some(crate::ledger::PilotStop::Exhausted { metric, .. }) if *metric == expected_metric)
        );
        assert_eq!(expected.baseline, baseline);
        let f = f.reopen();
        let before = f.ledger.chain_head().unwrap();
        assert!(
            f.engine
                .begin_activation_review(&f.agent, f.account)
                .is_err()
        );
        assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), expected);
        assert_eq!(f.ledger.chain_head().unwrap(), before);
        assert_eq!(f.consent_count(), 1);
        assert!(f.engine.policy_status().admission_inhibited);
    }
}

#[test]
fn physical_reopen_committed_capacity_and_pending_liability_refuse_activation() {
    for pending in [false, true] {
        let f = Fixture::new(true, false);
        if pending {
            let _receipt = f.historical_order(1, true);
        } else {
            for id in 1..=10 {
                let receipt = f.historical_order(id, true);
                f.observe_order(&receipt, id, "canceled");
            }
        }
        let expected = f.pilot.state(f.account).unwrap().unwrap();
        assert_eq!(expected.executed_usd, Decimal::ZERO);
        assert_eq!(
            expected.reserved_usd,
            Decimal::from(if pending { 15 } else { 150 })
        );
        let f = f.reopen();
        assert_eq!(
            f.engine
                .submissions()
                .unwrap()
                .state(f.account)
                .unwrap()
                .pending
                .is_some(),
            pending
        );
        let before = f.ledger.chain_head().unwrap();
        let result = f
            .engine
            .begin_activation_review(&f.agent, f.account)
            .and_then(|observation| {
                f.engine
                    .review_activation(observation, f.evidence(), &|| NOW)
            });
        assert!(result.is_err());
        assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), expected);
        assert_eq!(f.ledger.chain_head().unwrap(), before);
        assert_eq!(f.consent_count(), 1);
        assert!(f.engine.policy_status().admission_inhibited);
    }
}

#[test]
fn physical_reopen_unknown_accounting_remains_unknown_and_inhibited() {
    let f = Fixture::new(true, false);
    let receipt = f.historical_order(1, true);
    f.engine
        .submissions()
        .unwrap()
        .resolve(
            &receipt,
            crate::ledger::SubmissionResolution::NotSent {
                detail: "synthetic definite-not-sent observation".into(),
            },
            NOW,
        )
        .unwrap();
    // Contradictory ordinary fill evidence, not a modified consent/authority row.
    f.historical_fill(1, 1, true, "0.15", "0", "0.1");
    assert!(f.pilot.state(f.account).is_err());
    let f = f.reopen();
    let before = f.ledger.chain_head().unwrap();
    let status = f.pilot.status(f.account).unwrap().unwrap();
    assert!(matches!(
        status.accounting,
        crate::ledger::PilotAccounting::Unavailable { .. }
    ));
    assert!(status.halt.is_some());
    assert!(
        f.engine
            .begin_activation_review(&f.agent, f.account)
            .is_err()
    );
    assert!(f.pilot.state(f.account).is_err());
    assert_eq!(f.ledger.chain_head().unwrap(), before);
    assert_eq!(f.consent_count(), 1);
    assert!(f.engine.policy_status().admission_inhibited);
}

#[test]
fn ordinary_ledger_churn_stales_review_and_confirmation_but_fresh_review_succeeds() {
    for before_review in [false, true] {
        let f = Fixture::new(true, false);
        let pilot = f.pilot.state(f.account).unwrap().unwrap();
        if before_review {
            let observation = f
                .engine
                .begin_activation_review(&f.agent, f.account)
                .unwrap();
            ordinary_row(&f);
            let head = f.ledger.chain_head().unwrap();
            assert!(
                f.engine
                    .review_activation(observation, f.evidence(), &|| NOW)
                    .is_err()
            );
            assert_eq!(f.ledger.chain_head().unwrap(), head);
        } else {
            let review = f.review();
            ordinary_row(&f);
            let head = f.ledger.chain_head().unwrap();
            assert!(
                f.engine
                    .confirm_activation(review, f.evidence(), &|| NOW, &|| Ok(()))
                    .is_err()
            );
            assert_eq!(f.ledger.chain_head().unwrap(), head);
        }
        assert!(f.engine.policy_status().admission_inhibited);
        let review = f.review();
        f.engine
            .confirm_activation(review, f.evidence(), &|| NOW, &|| Ok(()))
            .unwrap();
        assert!(!f.engine.policy_status().admission_inhibited);
        assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), pilot);
        assert_eq!(f.consent_count(), 1);
        assert!(f.ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn physical_reopen_after_uncertain_audit_does_not_infer_acknowledgment() {
    let f = Fixture::new(true, false);
    let expected = f.pilot.state(f.account).unwrap().unwrap();
    let review = f.review();
    let evidence = f.evidence();
    let before = f.ledger.chain_head().unwrap();
    let fail = f.fail.clone();
    f.at_next_audit(move || fail.store(true, Ordering::SeqCst));
    assert!(
        f.engine
            .confirm_activation(review, evidence, &|| NOW, &|| Ok(()))
            .is_err()
    );
    assert_eq!(
        f.ledger.chain_head().unwrap().seq,
        before.seq + 1,
        "request committed before publication failure"
    );
    assert!(f.engine.policy_status().admission_inhibited);
    f.fail.store(false, Ordering::SeqCst);
    let f = f.reopen();
    assert!(f.engine.policy_status().admission_inhibited);
    assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), expected);
    let review = f.review();
    f.engine
        .confirm_activation(review, f.evidence(), &|| NOW, &|| Ok(()))
        .unwrap();
    assert_eq!(f.consent_count(), 1);
    assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), expected);
}

#[test]
fn final_verified_sqlite_permit_excludes_raw_writer_until_acknowledgment_returns() {
    let f = Fixture::new(true, false);
    let review = f.review();
    let evidence = f.evidence();
    let path = f._dir.path().join("activation.db");
    let writer_path = path.clone();
    let (start, ready) = std::sync::mpsc::channel();
    let (done, completed) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let connection = rusqlite::Connection::open(writer_path).unwrap();
        connection.busy_timeout(std::time::Duration::ZERO).unwrap();
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let result = connection.execute_batch("BEGIN IMMEDIATE");
        let blocked = result
            .as_ref()
            .err()
            .and_then(rusqlite::Error::sqlite_error_code)
            == Some(rusqlite::ErrorCode::DatabaseBusy);
        if result.is_ok() {
            connection.execute_batch("ROLLBACK").unwrap();
        }
        done.send(blocked).unwrap();
    });
    let checks = std::cell::Cell::new(0);
    let result = f.engine.confirm_activation(review, evidence, &|| NOW, &|| {
        checks.set(checks.get() + 1);
        if checks.get() == 2 {
            // Deterministic preemption at the post-publication boundary. The
            // other thread requests only a writer slot and changes no rows.
            start.send(()).unwrap();
            assert!(
                completed
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
            );
        }
        Ok(())
    });
    writer.join().unwrap();
    result.unwrap();
    assert_eq!(checks.get(), 2);
    assert!(!f.engine.policy_status().admission_inhibited);
    let writer = rusqlite::Connection::open(path).unwrap();
    writer.busy_timeout(std::time::Duration::ZERO).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert!(f.ledger.verify().unwrap().is_intact());
}

fn assert_no_order_evidence(f: &Fixture) {
    assert!(
        f.ledger
            .get_events(0, 1000)
            .unwrap()
            .events
            .iter()
            .all(|event| {
                !matches!(
                    event.kind,
                    crate::ledger::EventKind::OrderIntent
                        | crate::ledger::EventKind::SubmissionSigned
                        | crate::ledger::EventKind::SubmissionAccepted
                )
            })
    );
}

#[test]
fn actual_operator_kill_and_release_during_review_never_revive_old_review() {
    for global in [false, true] {
        let f = Fixture::new(true, false);
        let scope = if global {
            KillScope::Global
        } else {
            KillScope::Agent {
                agent: f.agent.clone(),
            }
        };
        let pilot = f.pilot.state(f.account).unwrap().unwrap();
        let review = f.review();
        let generation = review.display().stop_generation;
        let evidence = f.evidence();
        let effect = f
            .engine
            .operator_engage_kill(scope.clone(), KillReason::Operator, NOW)
            .unwrap();
        assert_eq!(effect.scope, scope);
        assert!(f.engine.policy_status().stop_generation > generation);
        assert!(
            f.policy
                .current()
                .unwrap()
                .state
                .kill
                .blocking(&f.agent)
                .is_some()
        );
        assert!(f.engine.operator_release_kill(&scope, NOW).unwrap());
        assert!(
            f.policy
                .current()
                .unwrap()
                .state
                .kill
                .blocking(&f.agent)
                .is_none()
        );
        assert!(f.engine.policy_status().admission_inhibited);
        assert!(
            f.engine
                .confirm_activation(review, evidence, &|| NOW, &|| Ok(()))
                .is_err()
        );
        assert!(f.engine.policy_status().acknowledgment.is_none());
        assert_no_order_evidence(&f);
        f.engine
            .confirm_activation(f.review(), f.evidence(), &|| NOW, &|| Ok(()))
            .unwrap();
        assert!(!f.engine.policy_status().admission_inhibited);
        assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), pilot);
        assert_eq!(f.consent_count(), 1);
        assert_no_order_evidence(&f);
        assert!(f.ledger.verify().unwrap().is_intact());
    }
}

#[test]
fn actual_operator_kill_during_audit_refuses_before_persistence_then_release_needs_fresh_review() {
    for global in [false, true] {
        let f = Fixture::new(true, false);
        let scope = if global {
            KillScope::Global
        } else {
            KillScope::Agent {
                agent: f.agent.clone(),
            }
        };
        let pilot = f.pilot.state(f.account).unwrap().unwrap();
        let review = f.review();
        let generation = review.display().stop_generation;
        let evidence = f.evidence();
        let before = f.ledger.chain_head().unwrap();
        let (start, ready) = std::sync::mpsc::channel();
        let killer = f.engine.clone();
        let kill_scope = scope.clone();
        let worker = std::thread::spawn(move || {
            ready
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            killer.operator_engage_kill(kill_scope, KillReason::Operator, NOW)
        });
        let observed = Arc::new(AtomicBool::new(false));
        let observed_in_hook = observed.clone();
        let engine = f.engine.clone();
        f.at_next_audit(move || {
            start.send(()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if engine.policy_status().stop_generation > generation {
                    observed_in_hook.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::yield_now();
            }
            // Engage has performed its real local stop and now waits for the
            // confirmation's mutation lock. Do not join it inside publication.
        });
        let confirmation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f.engine
                .confirm_activation(review, evidence, &|| NOW, &|| Ok(()))
        }));
        // Even an unexpected assertion panic releases confirmation's guards
        // before the worker is joined, so the fixture cannot strand persistence.
        let effect = worker.join().unwrap().unwrap();
        assert!(
            observed.load(Ordering::SeqCst),
            "real local stop must precede publication completion"
        );
        assert_eq!(effect.scope, scope);
        assert!(matches!(
            confirmation.unwrap(),
            Err(Refusal::Unevaluable(
                Unevaluable::ActivationAuthority { .. }
            ))
        ));
        let request = f.ledger.get_events(before.seq, 100).unwrap().events;
        assert!(
            request
                .iter()
                .any(|event| event.payload.as_ref().is_some_and(|payload| {
                    payload["action"] == "activation_acknowledgment_requested"
                })),
            "the race must occur after the request audit committed"
        );
        assert!(f.engine.policy_status().acknowledgment.is_none());
        assert!(
            f.policy
                .current()
                .unwrap()
                .state
                .kill
                .blocking(&f.agent)
                .is_some()
        );
        assert_no_order_evidence(&f);
        assert!(f.engine.operator_release_kill(&scope, NOW).unwrap());
        assert!(
            f.policy
                .current()
                .unwrap()
                .state
                .kill
                .blocking(&f.agent)
                .is_none()
        );
        assert!(f.engine.policy_status().admission_inhibited);
        assert!(f.engine.policy_status().acknowledgment.is_none());
        f.engine
            .confirm_activation(f.review(), f.evidence(), &|| NOW, &|| Ok(()))
            .unwrap();
        assert!(!f.engine.policy_status().admission_inhibited);
        assert_eq!(f.pilot.state(f.account).unwrap().unwrap(), pilot);
        assert_eq!(f.consent_count(), 1);
        assert_no_order_evidence(&f);
        assert!(f.ledger.verify().unwrap().is_intact());
    }
}
