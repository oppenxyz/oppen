//! ES28: retained operator queue reads and rejection, never approval or signing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oppen_core::guardrail::{GuardrailEngine, OriginalRequest, Proposal};
use oppen_mcp::auth::Binding;
use serde::Serialize;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueuePhase {
    Idle,
    Refreshing,
    Rejecting,
    Ready,
    Unavailable,
    RecoveryRequired,
    Closed,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PendingApprovalView {
    pub id: String,
    pub agent: String,
    pub account: String,
    pub symbol: String,
    pub is_buy: bool,
    pub px: String,
    pub sz: String,
    pub reduce_only: bool,
    pub reason: String,
    pub expires_at_ms: u64,
    pub original: Option<OriginalRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecisionOutcome {
    Rejected,
    NotPending,
    Uncertain,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalDecision {
    pub proposal_id: String,
    pub outcome: DecisionOutcome,
    pub at_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ApprovalQueueStatus {
    pub owner_id: String,
    pub agent: String,
    pub account: String,
    pub phase: QueuePhase,
    pub observed_at_ms: Option<u64>,
    pub pending: Vec<PendingApprovalView>,
    pub decision: Option<ApprovalDecision>,
    pub error: Option<String>,
}

struct Admission {
    closed: bool,
    busy: bool,
    status: ApprovalQueueStatus,
}

pub(crate) struct ApprovalQueueControl {
    engine: Arc<GuardrailEngine>,
    binding: Binding,
    admission: Mutex<Admission>,
    task: tokio::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

enum Operation {
    Refresh,
    Reject(String),
}

struct WorkResult {
    at_ms: u64,
    pending: Result<Vec<PendingApprovalView>, String>,
    decision: Option<ApprovalDecision>,
}

fn scoped(proposal: &Proposal, binding: &Binding) -> bool {
    proposal.agent() == &binding.agent && proposal.account() == binding.account
}

fn views(proposals: Vec<Proposal>, binding: &Binding) -> Vec<PendingApprovalView> {
    proposals
        .into_iter()
        .filter(|proposal| scoped(proposal, binding))
        .map(|proposal| {
            let intent = proposal.intent();
            PendingApprovalView {
                id: proposal.id().to_owned(),
                agent: binding.agent.to_string(),
                account: binding.account.to_string(),
                symbol: intent.symbol.clone(),
                is_buy: intent.is_buy,
                px: intent.px.to_string(),
                sz: intent.sz.to_string(),
                reduce_only: intent.reduce_only,
                reason: intent.reason.clone(),
                expires_at_ms: proposal.expires_at_ms(),
                original: intent.original.clone(),
            }
        })
        .collect()
}

fn read(
    engine: &GuardrailEngine,
    binding: &Binding,
    at_ms: u64,
) -> Result<Vec<PendingApprovalView>, String> {
    engine
        .pending_proposals(at_ms)
        .map(|pending| views(pending, binding))
        .map_err(|error| error.to_string())
}

fn perform(
    engine: &GuardrailEngine,
    binding: &Binding,
    operation: Operation,
    at_ms: u64,
) -> WorkResult {
    let Operation::Reject(proposal_id) = operation else {
        return WorkResult {
            at_ms,
            pending: read(engine, binding, at_ms),
            decision: None,
        };
    };
    let result = engine
        .pending_proposals(at_ms)
        .map_err(|error| error.to_string())
        .and_then(
            |pending| match pending.iter().find(|proposal| proposal.id() == proposal_id) {
                Some(proposal) if !scoped(proposal, binding) => {
                    Err("proposal does not belong to the supervised agent/account".into())
                }
                Some(_) => engine
                    .operator_reject_proposal(&proposal_id, at_ms)
                    .map_err(|error| error.to_string()),
                None => Ok(false),
            },
        );
    let decision = match result {
        Ok(rejected) => ApprovalDecision {
            proposal_id,
            outcome: if rejected {
                DecisionOutcome::Rejected
            } else {
                DecisionOutcome::NotPending
            },
            at_ms,
            error: None,
        },
        Err(error) => ApprovalDecision {
            proposal_id,
            outcome: DecisionOutcome::Uncertain,
            at_ms,
            error: Some(error),
        },
    };
    WorkResult {
        at_ms,
        pending: read(engine, binding, at_ms),
        decision: Some(decision),
    }
}

impl ApprovalQueueControl {
    pub(crate) fn new(engine: Arc<GuardrailEngine>, binding: Binding) -> Result<Arc<Self>, String> {
        let owner_id = NEXT_OWNER
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "approval queue owner IDs exhausted")?
            .to_string();
        let status = ApprovalQueueStatus {
            owner_id,
            agent: binding.agent.to_string(),
            account: binding.account.to_string(),
            phase: QueuePhase::Idle,
            observed_at_ms: None,
            pending: Vec::new(),
            decision: None,
            error: None,
        };
        Ok(Arc::new(Self {
            engine,
            binding,
            admission: Mutex::new(Admission {
                closed: false,
                busy: false,
                status,
            }),
            task: tokio::sync::Mutex::new(None),
        }))
    }

    fn admission(&self) -> Result<std::sync::MutexGuard<'_, Admission>, String> {
        self.admission
            .lock()
            .map_err(|_| "approval queue owner is poisoned; controlled recovery required".into())
    }

    fn check_binding(&self, binding: &Binding) -> Result<(), String> {
        if binding != &self.binding {
            return Err("approval queue requires the exact supervised agent/account".into());
        }
        Ok(())
    }

    pub(crate) fn status(&self, binding: &Binding) -> Result<ApprovalQueueStatus, String> {
        self.check_binding(binding)?;
        Ok(self.admission()?.status.clone())
    }

    pub(crate) fn refresh(
        self: &Arc<Self>,
        binding: &Binding,
    ) -> Result<ApprovalQueueStatus, String> {
        self.launch(
            binding,
            None,
            Operation::Refresh,
            |engine, binding, operation| {
                let at_ms = u64::try_from(oppen_core::ledger::now_ms())
                    .map_err(|error| error.to_string())?;
                Ok(perform(&engine, &binding, operation, at_ms))
            },
        )
    }

    pub(crate) fn reject(
        self: &Arc<Self>,
        binding: &Binding,
        owner_id: &str,
        proposal_id: String,
    ) -> Result<ApprovalQueueStatus, String> {
        self.launch(
            binding,
            Some(owner_id),
            Operation::Reject(proposal_id),
            |engine, binding, operation| {
                let at_ms = u64::try_from(oppen_core::ledger::now_ms())
                    .map_err(|error| error.to_string())?;
                Ok(perform(&engine, &binding, operation, at_ms))
            },
        )
    }

    fn launch(
        self: &Arc<Self>,
        binding: &Binding,
        owner_id: Option<&str>,
        operation: Operation,
        work: impl FnOnce(Arc<GuardrailEngine>, Binding, Operation) -> Result<WorkResult, String>
        + Send
        + 'static,
    ) -> Result<ApprovalQueueStatus, String> {
        self.check_binding(binding)?;
        let mut admission = self.admission()?;
        if admission.closed {
            return Err("approval queue admission is closed".into());
        }
        if owner_id.is_some_and(|id| id != admission.status.owner_id) {
            return Err("approval queue owner changed; refresh before rejecting".into());
        }
        if admission.busy {
            return Err("approval queue work is still running".into());
        }
        let mut task = self
            .task
            .try_lock()
            .map_err(|_| "approval queue is draining")?;
        if task
            .as_ref()
            .is_some_and(|task| !task.inner().is_finished())
        {
            return Err("approval queue task has not finished".into());
        }
        let admitted_at_ms =
            u64::try_from(oppen_core::ledger::now_ms()).map_err(|error| error.to_string())?;
        let rejected_id = match &operation {
            Operation::Reject(id) => Some(id.clone()),
            Operation::Refresh => None,
        };
        admission.busy = true;
        admission.status.phase = if rejected_id.is_some() {
            QueuePhase::Rejecting
        } else {
            QueuePhase::Refreshing
        };
        admission.status.error = None;
        let initial = admission.status.clone();
        let owner = self.clone();
        *task = Some(tauri::async_runtime::spawn(async move {
            let engine = owner.engine.clone();
            let binding = owner.binding.clone();
            let result =
                tauri::async_runtime::spawn_blocking(move || work(engine, binding, operation))
                    .await;
            let Ok(mut admission) = owner.admission() else {
                return;
            };
            match result {
                Ok(Ok(result)) => {
                    let decision_error = result
                        .decision
                        .as_ref()
                        .and_then(|decision| decision.error.clone());
                    if let Some(decision) = result.decision {
                        admission.status.decision = Some(decision);
                    }
                    match result.pending {
                        Ok(pending) => {
                            admission.status.pending = pending;
                            admission.status.observed_at_ms = Some(result.at_ms);
                            admission.status.phase = if decision_error.is_some() {
                                QueuePhase::Unavailable
                            } else {
                                QueuePhase::Ready
                            };
                            admission.status.error = decision_error;
                        }
                        Err(error) => {
                            admission.status.phase = QueuePhase::Unavailable;
                            admission.status.error = Some(error);
                        }
                    }
                }
                other => {
                    let (error, panic) = match other {
                        Ok(Err(error)) => (error, false),
                        Err(error) => (format!("approval queue worker: {error}"), true),
                        Ok(Ok(_)) => unreachable!(),
                    };
                    if let Some(proposal_id) = rejected_id {
                        admission.status.decision = Some(ApprovalDecision {
                            proposal_id,
                            outcome: DecisionOutcome::Uncertain,
                            at_ms: admitted_at_ms,
                            error: Some(error.clone()),
                        });
                    }
                    admission.status.phase = if panic {
                        QueuePhase::RecoveryRequired
                    } else {
                        QueuePhase::Unavailable
                    };
                    admission.status.error = Some(error);
                    admission.closed |= panic;
                }
            }
            if admission.closed && admission.status.phase != QueuePhase::RecoveryRequired {
                admission.status.phase = QueuePhase::Closed;
            }
            admission.busy = false;
        }));
        Ok(initial)
    }

    pub(crate) fn close(&self) -> Result<(), String> {
        let mut admission = self.admission()?;
        admission.closed = true;
        if admission.status.phase != QueuePhase::RecoveryRequired {
            admission.status.phase = QueuePhase::Closed;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn blocked_refresh(
        self: &Arc<Self>,
        binding: &Binding,
        entered: tokio::sync::oneshot::Sender<()>,
        released: std::sync::mpsc::Receiver<()>,
    ) -> Result<ApprovalQueueStatus, String> {
        self.launch(
            binding,
            None,
            Operation::Refresh,
            move |engine, binding, operation| {
                let _ = entered.send(());
                released.recv().map_err(|error| error.to_string())?;
                let at_ms = u64::try_from(oppen_core::ledger::now_ms())
                    .map_err(|error| error.to_string())?;
                Ok(perform(&engine, &binding, operation, at_ms))
            },
        )
    }

    pub(crate) async fn close_and_drain(&self) -> Result<(), String> {
        let closed = self.close();
        // Borrow, never take, so a dropped waiter cannot detach admitted work.
        let mut task = self.task.lock().await;
        if let Some(task) = task.as_mut() {
            task.await
                .map_err(|error| format!("approval queue owner: {error}"))?;
        }
        *task = None;
        closed?;
        let admission = self.admission()?;
        if admission.status.phase == QueuePhase::RecoveryRequired {
            return Err("approval queue requires controlled recovery after worker panic".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::mpsc;
    use std::time::Duration;

    use oppen_core::guardrail::{
        AccountSnapshot, AgentGuardrails, AgentId, Exposure, FeedQuality, KillScope,
        LegacyPolicyReview, MarketRef, OrderIntent, PersistedState, Refusal, RestingExposure,
    };
    use oppen_core::keys::{EntryName, HmacKey, KeyStore, KeyStoreError, SecretText};
    use oppen_core::ledger::{
        Anchor, FileAnchor, HeadAnchor, Ledger, LedgerError, PolicyJournal, RegistryBinding,
        RegistryJournal,
    };
    use oppen_hl::meta::Asset;
    use oppen_hl::order::OrderKind;
    use oppen_hl::types::AssetInfo;
    use oppen_hl::wire::{Cloid, Grouping, Tif};
    use oppen_hl::{Address, Network};

    #[derive(Default)]
    struct FixtureKeys(Mutex<HashMap<EntryName, String>>);

    impl KeyStore for FixtureKeys {
        fn network(&self) -> Network {
            Network::Testnet
        }

        fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(entry)
                .cloned()
                .map(SecretText::new))
        }

        fn write(&self, entry: &EntryName, secret: &str) -> Result<(), KeyStoreError> {
            self.0
                .lock()
                .unwrap()
                .insert(entry.clone(), secret.to_owned());
            Ok(())
        }

        fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError> {
            self.0.lock().unwrap().remove(entry);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FailAnchor {
        inner: FileAnchor,
        fail_seq: Arc<AtomicU64>,
    }
    impl HeadAnchor for FailAnchor {
        fn load(&self) -> Result<Option<Anchor>, LedgerError> {
            self.inner.load()
        }
        fn store(&self, anchor: &Anchor) -> Result<(), LedgerError> {
            if anchor.seq != 0 && anchor.seq == self.fail_seq.load(Ordering::SeqCst) {
                return Err(LedgerError::Io(std::io::Error::other(
                    "synthetic publication failure",
                )));
            }
            self.inner.store(anchor)
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        engine: Arc<GuardrailEngine>,
        ledger: Arc<Ledger>,
        fail_seq: Arc<AtomicU64>,
        alpha: Binding,
        beta: Binding,
        at_ms: u64,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("approval.db");
            let fail_seq = Arc::new(AtomicU64::new(0));
            let ledger = Arc::new(
                Ledger::open_anchored(
                    &path,
                    Network::Testnet,
                    Some(Box::new(FailAnchor {
                        inner: FileAnchor::beside(&path),
                        fail_seq: fail_seq.clone(),
                    })),
                )
                .unwrap(),
            );
            let at_ms = u64::try_from(oppen_core::ledger::now_ms()).unwrap();
            let keys = Arc::new(FixtureKeys::default());
            let registry = Arc::new(
                RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([31; 32])))
                    .unwrap(),
            );
            let alpha = Binding {
                agent: AgentId::new("alpha"),
                account: Address::parse("0x1111111111111111111111111111111111111111").unwrap(),
            };
            let beta = Binding {
                agent: AgentId::new("beta"),
                account: Address::parse("0x2222222222222222222222222222222222222222").unwrap(),
            };
            let mut state = PersistedState::paused(at_ms);
            for (index, binding) in [&alpha, &beta].into_iter().enumerate() {
                keys.create_agent_key(
                    &binding.agent,
                    SecretText::new(format!("{:064x}", index + 1)),
                    at_ms + 86_400_000,
                    at_ms,
                )
                .unwrap();
                registry
                    .grant(
                        RegistryBinding {
                            agent: binding.agent.clone(),
                            container: binding.account,
                            vault_address: Some(binding.account),
                            wallet: keys.agent_wallet(&binding.agent).unwrap().unwrap(),
                        },
                        at_ms,
                    )
                    .unwrap();
                let mut config = AgentGuardrails::default();
                config.symbols.insert("TEST".into());
                config.max_order_usd = 1000.into();
                config.max_position_usd = 1000.into();
                state.guardrails.insert(binding.agent.clone(), config);
            }
            let policy = Arc::new(PolicyJournal::new(registry));
            let legacy = LegacyPolicyReview::open(
                dir.path().join("absent-legacy.db"),
                Network::Testnet,
                at_ms,
            )
            .unwrap();
            policy.initialize(&legacy, state, at_ms).unwrap();
            let engine = Arc::new(GuardrailEngine::new(policy, keys).unwrap());
            engine
                .operator_release_kill(&KillScope::Global, at_ms)
                .unwrap();
            engine
                .operator_acknowledge_policy(engine.policy_observation().unwrap(), at_ms)
                .unwrap();
            Self {
                _dir: dir,
                engine,
                ledger,
                fail_seq,
                alpha,
                beta,
                at_ms,
            }
        }

        fn propose(&self, binding: &Binding, id: u8) -> String {
            let intent = OrderIntent {
                symbol: "TEST".into(),
                is_buy: true,
                px: 100.into(),
                sz: 1.into(),
                kind: OrderKind::Limit { tif: Tif::Gtc },
                reduce_only: false,
                cloid: Some(Cloid::from_bytes([id; 16])),
                grouping: Grouping::Na,
                builder: None,
                max_slippage_bps: None,
                reason: "<script>inert claim</script>".into(),
                original: None,
            };
            let asset = Asset {
                index: 0,
                info: AssetInfo {
                    name: "TEST".into(),
                    sz_decimals: 2,
                    max_leverage: 50,
                    margin_table_id: 0,
                    is_delisted: false,
                    only_isolated: false,
                },
            };
            let market = MarketRef {
                symbol: "TEST".into(),
                reference_px: Some(100.into()),
                as_of_ms: self.at_ms,
                quality: FeedQuality::Ok,
                mark_divergence_bps: None,
                mark_divergent_since_ms: None,
                snapshot: None,
                sigma_day: None,
                vol_ratio: None,
            };
            let exposure = Exposure {
                account: binding.account,
                fleet: None,
                agent: AccountSnapshot {
                    as_of_ms: self.at_ms,
                    reconciled: true,
                    equity_usd: 100000.into(),
                    peak_equity_usd: 100000.into(),
                    realized_pnl_today_usd: 0.into(),
                    unrealized_pnl_usd: 0.into(),
                    day_start_ms: self.at_ms / 86_400_000 * 86_400_000,
                    total_position_notional_usd: 0.into(),
                    positions: BTreeMap::new(),
                    resting: Some(RestingExposure {
                        buys: BTreeMap::new(),
                        sells: BTreeMap::new(),
                        reduce_buys: BTreeMap::new(),
                        reduce_sells: BTreeMap::new(),
                        notional_by_symbol: BTreeMap::new(),
                        notional_usd: 0.into(),
                    }),
                },
            };
            match self.engine.evaluate(
                &binding.agent,
                &intent,
                &asset,
                &market,
                &exposure,
                self.at_ms,
            ) {
                Err(Refusal::ApprovalRequired { approval_id, .. }) => approval_id,
                other => panic!("expected a durable proposal: {other:?}"),
            }
        }

        fn queue(&self) -> Arc<ApprovalQueueControl> {
            ApprovalQueueControl::new(self.engine.clone(), self.alpha.clone()).unwrap()
        }
    }

    async fn settled(queue: &ApprovalQueueControl) -> ApprovalQueueStatus {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                {
                    let state = queue.admission().unwrap();
                    if !state.busy
                        && queue.task.try_lock().is_ok_and(|task| {
                            task.as_ref().is_none_or(|task| task.inner().is_finished())
                        })
                    {
                        return state.status.clone();
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("queue work must complete")
    }

    #[tokio::test]
    async fn cache_is_idle_until_explicit_refresh_and_filters_exact_scope_without_activation() {
        let f = Fixture::new();
        let alpha = f.propose(&f.alpha, 1);
        f.propose(&f.beta, 2);
        let queue = f.queue();
        let head = f.ledger.chain_head().unwrap();
        let initial = queue.status(&f.alpha).unwrap();
        assert_eq!(initial.phase, QueuePhase::Idle);
        assert!(initial.pending.is_empty());
        assert_eq!(initial.observed_at_ms, None);
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        let acknowledgment = f.engine.policy_status().acknowledgment;
        assert_eq!(
            queue.refresh(&f.alpha).unwrap().phase,
            QueuePhase::Refreshing
        );
        let status = settled(&queue).await;
        assert_eq!(status.phase, QueuePhase::Ready);
        assert_eq!(status.pending.len(), 1);
        assert_eq!(status.pending[0].id, alpha);
        assert_eq!(status.pending[0].agent, "alpha");
        assert_eq!(status.pending[0].account, f.alpha.account.to_string());
        assert_eq!(status.pending[0].reason, "<script>inert claim</script>");
        assert_eq!(status.pending[0].px, "100");
        assert_eq!(f.engine.policy_status().acknowledgment, acknowledgment);
        assert_eq!(f.ledger.chain_head().unwrap(), head);
        queue.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn reject_checks_fresh_scope_and_owner_then_records_rejected_or_not_pending() {
        let f = Fixture::new();
        let alpha = f.propose(&f.alpha, 1);
        let beta = f.propose(&f.beta, 2);
        let queue = f.queue();
        let replacement = f.queue();
        let owner = queue.status(&f.alpha).unwrap().owner_id;
        assert_ne!(owner, replacement.status(&f.alpha).unwrap().owner_id);
        assert!(queue.status(&f.beta).is_err());
        assert!(queue.refresh(&f.beta).is_err());
        assert!(queue.reject(&f.beta, &owner, beta.clone()).is_err());
        assert!(replacement.reject(&f.alpha, &owner, alpha.clone()).is_err());
        queue.reject(&f.alpha, &owner, beta.clone()).unwrap();
        let foreign = settled(&queue).await;
        assert_eq!(
            foreign.decision.unwrap().outcome,
            DecisionOutcome::Uncertain
        );
        assert_eq!(f.engine.pending_proposals(f.at_ms).unwrap().len(), 2);
        queue.reject(&f.alpha, &owner, alpha.clone()).unwrap();
        let rejected = settled(&queue).await;
        assert_eq!(
            rejected.decision.unwrap().outcome,
            DecisionOutcome::Rejected
        );
        assert!(rejected.pending.is_empty());
        queue.reject(&f.alpha, &owner, alpha).unwrap();
        assert_eq!(
            settled(&queue).await.decision.unwrap().outcome,
            DecisionOutcome::NotPending
        );
        let pending = f.engine.pending_proposals(f.at_ms).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id(), beta);
        queue.close_and_drain().await.unwrap();
        replacement.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn read_failure_retains_old_queue_and_uncertain_rejection_stays_uncertain_after_fresh_empty_read()
     {
        let f = Fixture::new();
        let id = f.propose(&f.alpha, 1);
        let queue = f.queue();
        queue.refresh(&f.alpha).unwrap();
        let initial = settled(&queue).await;
        let head = f.ledger.chain_head().unwrap();
        f.fail_seq.store(head.seq, Ordering::SeqCst);
        queue.refresh(&f.alpha).unwrap();
        let failed = settled(&queue).await;
        assert_eq!(failed.phase, QueuePhase::Unavailable);
        assert_eq!(failed.pending[0].id, id);
        assert_eq!(failed.observed_at_ms, initial.observed_at_ms);
        f.fail_seq.store(head.seq + 1, Ordering::SeqCst);
        queue
            .reject(&f.alpha, &initial.owner_id, id.clone())
            .unwrap();
        let uncertain = settled(&queue).await;
        assert_eq!(uncertain.phase, QueuePhase::Unavailable);
        assert_eq!(
            uncertain.decision.unwrap().outcome,
            DecisionOutcome::Uncertain
        );
        assert_eq!(f.ledger.chain_head().unwrap().seq, head.seq + 1);
        assert_eq!(uncertain.pending[0].id, id);
        f.fail_seq.store(0, Ordering::SeqCst);
        queue.refresh(&f.alpha).unwrap();
        let fresh = settled(&queue).await;
        assert_eq!(fresh.phase, QueuePhase::Ready);
        assert!(fresh.pending.is_empty());
        assert_eq!(fresh.decision.unwrap().outcome, DecisionOutcome::Uncertain);
        queue.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn blocked_rejection_survives_dropped_ipc_and_drain_waiter_without_overlapping_work() {
        let f = Fixture::new();
        let id = f.propose(&f.alpha, 1);
        let queue = f.queue();
        let owner = queue.status(&f.alpha).unwrap().owner_id;
        let (entered, entering) = tokio::sync::oneshot::channel();
        let (release, released) = mpsc::channel();
        let at_ms = f.at_ms;
        drop(
            queue
                .launch(
                    &f.alpha,
                    Some(&owner),
                    Operation::Reject(id.clone()),
                    move |engine, binding, operation| {
                        let _ = entered.send(());
                        released.recv().unwrap();
                        Ok(perform(&engine, &binding, operation, at_ms))
                    },
                )
                .unwrap(),
        );
        entering.await.unwrap();
        assert!(queue.refresh(&f.alpha).is_err());
        assert!(queue.reject(&f.alpha, &owner, id.clone()).is_err());
        let draining = tokio::spawn({
            let queue = queue.clone();
            async move { queue.close_and_drain().await }
        });
        while !queue.admission().unwrap().closed {
            tokio::task::yield_now().await;
        }
        assert!(!draining.is_finished());
        draining.abort();
        assert!(draining.await.unwrap_err().is_cancelled());
        assert!(queue.admission().unwrap().busy);
        assert!(queue.refresh(&f.alpha).is_err());
        release.send(()).unwrap();
        queue.close_and_drain().await.unwrap();
        let status = queue.status(&f.alpha).unwrap();
        assert_eq!(status.phase, QueuePhase::Closed);
        assert_eq!(status.decision.unwrap().outcome, DecisionOutcome::Rejected);
        assert!(f.engine.pending_proposals(f.at_ms).unwrap().is_empty());
        assert!(!f.engine.operator_reject_proposal(&id, f.at_ms).unwrap());
    }

    #[tokio::test]
    async fn worker_panic_retains_evidence_and_closes_mutations_for_controlled_recovery() {
        let f = Fixture::new();
        let id = f.propose(&f.alpha, 1);
        let queue = f.queue();
        queue.refresh(&f.alpha).unwrap();
        let initial = settled(&queue).await;
        queue
            .launch(
                &f.alpha,
                Some(&initial.owner_id),
                Operation::Reject(id.clone()),
                |_, _, _| panic!("synthetic worker panic"),
            )
            .unwrap();
        let status = settled(&queue).await;
        assert_eq!(status.phase, QueuePhase::RecoveryRequired);
        assert_eq!(status.pending[0].id, id);
        assert_eq!(status.decision.unwrap().outcome, DecisionOutcome::Uncertain);
        assert!(queue.refresh(&f.alpha).is_err());
        assert!(queue.reject(&f.alpha, &initial.owner_id, id).is_err());
        assert!(queue.close_and_drain().await.is_err());
        assert_eq!(
            queue.status(&f.alpha).unwrap().phase,
            QueuePhase::RecoveryRequired
        );
    }
}
