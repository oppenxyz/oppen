//! Retained native kill release. No venue operations, activation or pilot mutation.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oppen_core::guardrail::{
    GuardrailEngine, KillReleaseDisplay, KillReleaseError, KillReleaseReceipt,
    KillReleaseResolution, KillReleaseReview, KillScope, KillSwitch, PolicyStatus, Refusal,
    Unevaluable,
};
use oppen_mcp::auth::Binding;
use oppen_mcp::server::Pairings;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::operator_halt::HaltControl;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Idle,
    Reviewing,
    ReviewReady,
    Confirming,
    Released,
    Reconciling,
    Refused,
    Uncertain,
    Closed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Operation {
    Review { scope: KillScope },
    Confirm { review_id: String },
    Discard { review_id: String },
    Reconcile { operation_id: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ReleaseError {
    Refused {
        detail: String,
    },
    Uncertain {
        operation_id: Option<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReviewView {
    pub id: String,
    pub display: KillReleaseDisplay,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AbsenceProof {
    WorkerTerminal,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Resolution {
    Committed {
        receipt: Box<KillReleaseReceipt>,
        current_policy_revision: u64,
        current_persisted_kill: KillSwitch,
        current_effective_kill: KillSwitch,
        current_stop_generation: u64,
    },
    NotCommitted {
        operation_id: String,
        proof: AbsenceProof,
    },
    Unknown {
        operation_id: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReleaseStatus {
    pub owner_id: String,
    pub agent: String,
    pub account: String,
    pub operation_seq: u64,
    pub last_operation: Option<Operation>,
    pub phase: Phase,
    pub review: Option<ReviewView>,
    pub receipt: Option<KillReleaseReceipt>,
    pub resolution: Option<Resolution>,
    pub error: Option<ReleaseError>,
    pub policy_status: PolicyStatus,
    /// Cached effective stops, not a freshly verified persisted observation.
    pub cached_effective_kill: KillSwitch,
}

struct State {
    status: ReleaseStatus,
    review: Option<KillReleaseReview>,
    terminal_operations: BTreeSet<String>,
    recovery_required: bool,
    unresolved_operation: Option<String>,
}

enum Work {
    Review(KillScope),
    Confirm(Box<KillReleaseReview>),
    Reconcile(String),
}
enum Outcome {
    Reviewed(Box<KillReleaseReview>),
    Released(KillReleaseReceipt),
    Reconciled(KillReleaseResolution),
}

pub(crate) struct ReleaseControl {
    engine: Arc<GuardrailEngine>,
    binding: Binding,
    // Retain the actual exclusive runtime pairing owner, not an active agent permission.
    _pairings: Pairings,
    halt: Arc<HaltControl>,
    stop: CancellationToken,
    closed: AtomicBool,
    state: Mutex<State>,
    task: tokio::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

fn now_ms() -> u64 {
    u64::try_from(oppen_core::ledger::now_ms()).unwrap_or(u64::MAX)
}

impl ReleaseControl {
    pub(crate) fn new(
        engine: Arc<GuardrailEngine>,
        binding: Binding,
        pairings: Pairings,
        halt: Arc<HaltControl>,
        stop: CancellationToken,
    ) -> Result<Arc<Self>, String> {
        let id = NEXT_OWNER
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| "release owner IDs exhausted")?;
        let status = ReleaseStatus {
            owner_id: id.to_string(),
            agent: binding.agent.to_string(),
            account: binding.account.to_string(),
            operation_seq: 0,
            last_operation: None,
            phase: Phase::Idle,
            review: None,
            receipt: None,
            resolution: None,
            error: None,
            policy_status: engine.policy_status(),
            cached_effective_kill: engine.kill_switch(),
        };
        Ok(Arc::new(Self {
            engine,
            binding,
            _pairings: pairings,
            halt,
            stop,
            closed: AtomicBool::new(false),
            state: Mutex::new(State {
                status,
                review: None,
                terminal_operations: BTreeSet::new(),
                recovery_required: false,
                unresolved_operation: None,
            }),
            task: tokio::sync::Mutex::new(None),
        }))
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "release owner state poisoned".into())
    }

    fn live(&self) -> Result<(), Refusal> {
        if self.closed.load(Ordering::SeqCst) || self.stop.is_cancelled() {
            return Err(Unevaluable::PolicyAuthority {
                detail: "native release owner closed".into(),
            }
            .into());
        }
        Ok(())
    }

    fn binding(&self, binding: &Binding) -> Result<(), String> {
        if binding != &self.binding {
            return Err("release requires the supervised identity".into());
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self, binding: &Binding) -> Result<ReleaseStatus, String> {
        self.binding(binding)?;
        let mut status = self.state()?.status.clone();
        status.policy_status = self.engine.policy_status();
        status.cached_effective_kill = self.engine.kill_switch();
        Ok(status)
    }

    pub(crate) fn review(
        self: &Arc<Self>,
        binding: &Binding,
        scope: KillScope,
    ) -> Result<ReleaseStatus, String> {
        if matches!(&scope, KillScope::Agent { agent } if agent != &binding.agent) {
            return Err("agent release must name the supervised agent".into());
        }
        if !self.halt.work_terminal() {
            return Err("prior HALT work is still owned; review after it drains".into());
        }
        self.start(binding, None, Operation::Review { scope })
    }

    pub(crate) fn confirm(
        self: &Arc<Self>,
        binding: &Binding,
        owner: &str,
        review_id: String,
    ) -> Result<ReleaseStatus, String> {
        self.start(binding, Some(owner), Operation::Confirm { review_id })
    }

    pub(crate) fn reconcile(
        self: &Arc<Self>,
        binding: &Binding,
        owner: &str,
        operation_id: String,
    ) -> Result<ReleaseStatus, String> {
        self.start(binding, Some(owner), Operation::Reconcile { operation_id })
    }

    fn start(
        self: &Arc<Self>,
        binding: &Binding,
        expected_owner: Option<&str>,
        operation: Operation,
    ) -> Result<ReleaseStatus, String> {
        self.binding(binding)?;
        let mut state = self.state()?;
        self.live().map_err(|error| error.to_string())?;
        if expected_owner.is_some_and(|owner| owner != state.status.owner_id) {
            return Err("release owner changed".into());
        }
        let mut task = self.task.try_lock().map_err(|_| "release owner draining")?;
        if task
            .as_ref()
            .is_some_and(|task| !task.inner().is_finished())
        {
            return Err("release work remains active; absence is not terminal proof".into());
        }
        if state.recovery_required && !matches!(operation, Operation::Reconcile { .. }) {
            return Err("release outcome requires read-only reconciliation or restart".into());
        }
        if state.recovery_required
            && let Operation::Reconcile { operation_id } = &operation
            && state.unresolved_operation.as_ref() != Some(operation_id)
        {
            return Err(
                "only the exact unresolved release operation can be reconciled in this owner"
                    .into(),
            );
        }
        let next = state
            .status
            .operation_seq
            .checked_add(1)
            .ok_or("release operation IDs exhausted")?;
        let (work, phase, confirming_id) = match &operation {
            Operation::Review { scope } => {
                if state.review.is_some() {
                    return Err("discard or confirm the retained release review".into());
                }
                (Work::Review(scope.clone()), Phase::Reviewing, None)
            }
            Operation::Confirm { review_id } => {
                if state
                    .status
                    .review
                    .as_ref()
                    .is_none_or(|review| review.id != *review_id)
                {
                    return Err("release review changed or already consumed".into());
                }
                let review = state
                    .review
                    .take()
                    .ok_or("release review already consumed")?;
                let operation_id = review.display().operation_id.clone();
                (
                    Work::Confirm(Box::new(review)),
                    Phase::Confirming,
                    Some(operation_id),
                )
            }
            Operation::Reconcile { operation_id } => {
                if state.review.is_some() {
                    return Err("discard the idle release review before reconciliation".into());
                }
                (
                    Work::Reconcile(operation_id.clone()),
                    Phase::Reconciling,
                    None,
                )
            }
            Operation::Discard { .. } => return Err("discard is not background work".into()),
        };
        state.status.operation_seq = next;
        state.status.last_operation = Some(operation);
        state.status.phase = phase;
        state.status.review = None;
        state.status.error = None;
        state.status.receipt = None;
        state.status.resolution = None;
        let owner = self.clone();
        *task = Some(tauri::async_runtime::spawn(async move {
            let worker = owner.clone();
            let result = tauri::async_runtime::spawn_blocking(move || worker.execute(work)).await;
            owner.finish(
                next,
                confirming_id,
                result.map_err(|error| error.to_string()),
            );
        }));
        Ok(state.status.clone())
    }

    fn execute(&self, work: Work) -> Result<Outcome, KillReleaseError> {
        self.live().map_err(|error| KillReleaseError::Refused {
            detail: error.to_string(),
        })?;
        match work {
            Work::Review(scope) => self
                .engine
                .review_kill_release(scope, &now_ms)
                .map(|review| Outcome::Reviewed(Box::new(review))),
            Work::Confirm(review) => self
                .engine
                .confirm_kill_release(*review, &now_ms, &|| self.live())
                .map(Outcome::Released),
            Work::Reconcile(id) => Ok(Outcome::Reconciled(self.engine.reconcile_kill_release(&id))),
        }
    }

    fn finish(
        &self,
        seq: u64,
        confirming_id: Option<String>,
        result: Result<Result<Outcome, KillReleaseError>, String>,
    ) {
        if let Ok(Ok(Outcome::Released(receipt))) = &result
            && self.live().is_ok()
        {
            self.halt.release_confirmed(receipt);
        }
        let Ok(mut state) = self.state() else {
            self.closed.store(true, Ordering::SeqCst);
            return;
        };
        if let Some(id) = &confirming_id {
            state.terminal_operations.insert(id.clone());
        }
        if state.status.operation_seq != seq {
            return;
        }
        match result {
            Ok(Ok(Outcome::Reviewed(review))) if self.live().is_ok() => {
                state.status.review = Some(ReviewView {
                    id: seq.to_string(),
                    display: review.display().clone(),
                });
                state.review = Some(*review);
                state.status.phase = Phase::ReviewReady;
            }
            Ok(Ok(Outcome::Reviewed(_))) => state.status.phase = Phase::Closed,
            Ok(Ok(Outcome::Released(receipt))) => {
                state.status.receipt = Some(receipt);
                state.status.phase = Phase::Released;
            }
            Ok(Ok(Outcome::Reconciled(resolution))) => {
                let resolution = match resolution {
                    KillReleaseResolution::Committed {
                        receipt,
                        current_policy_revision,
                        current_persisted_kill,
                        current_effective_kill,
                        current_stop_generation,
                    } => {
                        state.recovery_required = false;
                        state.unresolved_operation = None;
                        state.status.receipt = Some((*receipt).clone());
                        state.status.phase = Phase::Released;
                        Resolution::Committed {
                            receipt,
                            current_policy_revision,
                            current_persisted_kill,
                            current_effective_kill,
                            current_stop_generation,
                        }
                    }
                    KillReleaseResolution::Absent { operation_id } => {
                        if state.terminal_operations.contains(&operation_id) {
                            state.recovery_required = false;
                            state.unresolved_operation = None;
                            state.status.phase = Phase::Idle;
                            Resolution::NotCommitted {
                                operation_id,
                                proof: AbsenceProof::WorkerTerminal,
                            }
                        } else {
                            state.recovery_required = true;
                            state.unresolved_operation = Some(operation_id.clone());
                            state.status.phase = Phase::Uncertain;
                            Resolution::Unknown { operation_id,
                                detail: "absence has no known terminal native confirmation; another core owner may still commit".into() }
                        }
                    }
                    KillReleaseResolution::Unknown {
                        operation_id,
                        detail,
                    } => {
                        state.recovery_required = true;
                        state.unresolved_operation = Some(operation_id.clone());
                        state.status.phase = Phase::Uncertain;
                        Resolution::Unknown {
                            operation_id,
                            detail,
                        }
                    }
                };
                state.status.resolution = Some(resolution);
            }
            Ok(Err(KillReleaseError::Refused { detail })) => {
                state.status.phase = Phase::Refused;
                state.status.error = Some(ReleaseError::Refused { detail });
            }
            Ok(Err(KillReleaseError::Uncertain {
                operation_id,
                detail,
            })) => {
                state.recovery_required = true;
                state.unresolved_operation = Some(operation_id.clone());
                state.status.phase = Phase::Uncertain;
                state.status.error = Some(ReleaseError::Uncertain {
                    operation_id: Some(operation_id),
                    detail,
                });
            }
            Err(detail) => {
                state.recovery_required = true;
                state.unresolved_operation = confirming_id.clone();
                state.status.phase = Phase::Uncertain;
                state.status.error = Some(ReleaseError::Uncertain {
                    operation_id: confirming_id,
                    detail,
                });
            }
        }
        if self.closed.load(Ordering::SeqCst) && state.status.phase != Phase::Uncertain {
            state.status.phase = Phase::Closed;
        }
    }

    pub(crate) fn discard(
        &self,
        binding: &Binding,
        owner: &str,
        id: &str,
    ) -> Result<ReleaseStatus, String> {
        self.binding(binding)?;
        let mut state = self.state()?;
        self.live().map_err(|error| error.to_string())?;
        if owner != state.status.owner_id
            || state
                .status
                .review
                .as_ref()
                .is_none_or(|review| review.id != id)
        {
            return Err("release review owner changed".into());
        }
        state.status.operation_seq = state
            .status
            .operation_seq
            .checked_add(1)
            .ok_or("release operation IDs exhausted")?;
        state.status.last_operation = Some(Operation::Discard {
            review_id: id.into(),
        });
        state.review = None;
        state.status.review = None;
        state.status.phase = Phase::Idle;
        Ok(state.status.clone())
    }

    pub(crate) fn close(&self) -> Result<(), String> {
        self.closed.store(true, Ordering::SeqCst);
        let mut state = self.state()?;
        state.review = None;
        state.status.review = None;
        if state.status.phase != Phase::Uncertain {
            state.status.phase = Phase::Closed;
        }
        Ok(())
    }

    pub(crate) async fn close_and_drain(&self) -> Result<(), String> {
        let closed = self.close();
        let mut task = self.task.lock().await;
        let joined = if let Some(task) = task.as_mut() {
            task.await.map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        *task = None;
        joined?;
        closed?;
        if self.state()?.recovery_required {
            return Err("kill release requires verified outcome reconciliation".into());
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn state_available(&self) -> bool {
        self.state.try_lock().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn task_finished(&self) -> bool {
        self.task
            .try_lock()
            .is_ok_and(|task| task.as_ref().is_none_or(|task| task.inner().is_finished()))
    }

    pub(crate) fn in_progress(&self) -> bool {
        self.state().is_ok_and(|state| {
            matches!(
                state.status.phase,
                Phase::Reviewing | Phase::Confirming | Phase::Reconciling
            )
        })
    }
}
