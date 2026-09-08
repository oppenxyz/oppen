//! Native retained activation reviews on the listener's existing engine.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oppen_core::guardrail::{
    ActivationDisplay, ActivationEvidence, ActivationReceipt, ActivationReview, GuardrailEngine,
    PolicyStatus, Refusal, Unevaluable,
};
use oppen_core::ledger::AuthorizedRoute;
use oppen_hl::types::UserRole;
use oppen_hl::{InfoClient, Network};
use oppen_mcp::auth::Binding;
use oppen_mcp::server::{ActivationAdmission, OperatorControl};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Idle,
    Reviewing,
    ReviewReady,
    Confirming,
    Acknowledged,
    Refused,
    Uncertain,
    Closed,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKind {
    Refusal,
    Worker,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActivationError {
    pub kind: ErrorKind,
    pub detail: String,
    pub refusal: Option<Refusal>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReviewView {
    pub id: String,
    pub display: ActivationDisplay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ActivationOperation {
    Review,
    Confirm { review_id: String },
    Discard { review_id: String },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActivationStatus {
    pub owner_id: String,
    pub agent: String,
    pub account: String,
    pub phase: Phase,
    pub operation_seq: u64,
    pub last_operation: Option<ActivationOperation>,
    pub review: Option<ReviewView>,
    pub receipt: Option<ActivationReceipt>,
    pub error: Option<ActivationError>,
    /// Local observation, not fresh authority or an order-admission guarantee.
    pub policy_status: PolicyStatus,
}

struct Retained {
    review: ActivationReview,
    admission: ActivationAdmission,
}
enum WorkResult {
    Reviewed(Box<Retained>),
    Acknowledged(ActivationReceipt),
}
struct State {
    status: ActivationStatus,
    retained: Option<Retained>,
}

pub(crate) struct ActivationControl {
    engine: Arc<GuardrailEngine>,
    binding: Binding,
    operator: OperatorControl,
    info: InfoClient,
    stop: CancellationToken,
    closed: AtomicBool,
    state: Mutex<State>,
    task: tokio::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

fn refused(detail: impl ToString) -> Refusal {
    Unevaluable::PolicyAuthority {
        detail: detail.to_string(),
    }
    .into()
}

fn now_ms() -> u64 {
    u64::try_from(oppen_core::ledger::now_ms()).unwrap_or(u64::MAX)
}

impl ActivationControl {
    #[cfg(test)]
    pub(crate) fn task_finished(&self) -> bool {
        self.task
            .try_lock()
            .is_ok_and(|task| task.as_ref().is_none_or(|task| task.inner().is_finished()))
    }
    pub(crate) fn new(
        engine: Arc<GuardrailEngine>,
        binding: Binding,
        operator: OperatorControl,
        info: InfoClient,
        stop: CancellationToken,
    ) -> Result<Arc<Self>, String> {
        let id = NEXT_OWNER
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| "activation owner IDs exhausted")?;
        let status = ActivationStatus {
            owner_id: id.to_string(),
            agent: binding.agent.to_string(),
            account: binding.account.to_string(),
            phase: Phase::Idle,
            operation_seq: 0,
            last_operation: None,
            review: None,
            receipt: None,
            error: None,
            policy_status: engine.policy_status(),
        };
        Ok(Arc::new(Self {
            engine,
            binding,
            operator,
            info,
            stop,
            closed: AtomicBool::new(false),
            state: Mutex::new(State {
                status,
                retained: None,
            }),
            task: tokio::sync::Mutex::new(None),
        }))
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "activation owner state poisoned".into())
    }

    fn live(&self) -> Result<(), Refusal> {
        if self.closed.load(Ordering::SeqCst) || self.stop.is_cancelled() {
            return Err(refused("activation owner closed"));
        }
        Ok(())
    }

    fn binding(&self, binding: &Binding) -> Result<(), String> {
        if binding != &self.binding {
            return Err("activation requires the supervised identity".into());
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self, binding: &Binding) -> Result<ActivationStatus, String> {
        self.binding(binding)?;
        let mut status = self.state()?.status.clone();
        status.policy_status = self.engine.policy_status();
        Ok(status)
    }

    pub(crate) fn request_review(
        self: &Arc<Self>,
        binding: &Binding,
    ) -> Result<ActivationStatus, String> {
        self.start(binding, None)
    }

    pub(crate) fn confirm(
        self: &Arc<Self>,
        binding: &Binding,
        owner: &str,
        review: &str,
    ) -> Result<ActivationStatus, String> {
        self.start(binding, Some((owner, review)))
    }

    fn start(
        self: &Arc<Self>,
        binding: &Binding,
        confirm: Option<(&str, &str)>,
    ) -> Result<ActivationStatus, String> {
        self.binding(binding)?;
        self.live().map_err(|error| error.to_string())?;
        let mut state = self.state()?;
        self.live().map_err(|error| error.to_string())?;
        let mut task = self
            .task
            .try_lock()
            .map_err(|_| "activation owner draining")?;
        if task
            .as_ref()
            .is_some_and(|task| !task.inner().is_finished())
        {
            return Err("activation work is already running".into());
        }
        if matches!(state.status.phase, Phase::Uncertain | Phase::Closed) {
            return Err("activation owner requires restart".into());
        }
        let next_seq = state
            .status
            .operation_seq
            .checked_add(1)
            .ok_or("activation operation IDs exhausted")?;
        let retained = if let Some((owner, review)) = confirm {
            if owner != state.status.owner_id
                || state
                    .status
                    .review
                    .as_ref()
                    .is_none_or(|view| view.id != review)
            {
                return Err("activation review owner changed".into());
            }
            Some(
                state
                    .retained
                    .take()
                    .ok_or("activation review already consumed")?,
            )
        } else {
            if state.retained.is_some() {
                return Err("discard or confirm the retained review first".into());
            }
            None
        };
        state.status.operation_seq = next_seq;
        state.status.last_operation = Some(match confirm {
            Some((_, review_id)) => ActivationOperation::Confirm {
                review_id: review_id.to_owned(),
            },
            None => ActivationOperation::Review,
        });
        let id = next_seq.to_string();
        state.status.phase = if confirm.is_some() {
            Phase::Confirming
        } else {
            Phase::Reviewing
        };
        state.status.review = None;
        state.status.receipt = None;
        state.status.error = None;
        let owner = self.clone();
        *task = Some(tauri::async_runtime::spawn(async move {
            let executor = tokio::runtime::Handle::current();
            let work = owner.clone();
            let result =
                tauri::async_runtime::spawn_blocking(move || work.execute(retained, &executor))
                    .await;
            owner.finish(id, result.map_err(|error| error.to_string()));
        }));
        Ok(state.status.clone())
    }

    fn execute(
        &self,
        retained: Option<Retained>,
        executor: &tokio::runtime::Handle,
    ) -> Result<WorkResult, Refusal> {
        self.live()?;
        if let Some(Retained { review, admission }) = retained {
            drop(admission.check().map_err(refused)?);
            let evidence = executor.block_on(self.gather(&review.display().route))?;
            let held = RefCell::new(None);
            let authorize = || {
                drop(held.borrow_mut().take());
                self.live()?;
                *held.borrow_mut() = Some(admission.check().map_err(refused)?);
                Ok(())
            };
            // The guard remains alive until the synchronous acknowledgment returns.
            let receipt = self
                .engine
                .confirm_activation(review, evidence, &now_ms, &authorize)?;
            drop(held);
            Ok(WorkResult::Acknowledged(receipt))
        } else {
            let admission = self
                .operator
                .activation_admission(&self.binding)
                .map_err(refused)?;
            let observation = self
                .engine
                .begin_activation_review(&self.binding.agent, self.binding.account)?;
            let evidence = executor.block_on(self.gather(observation.route()))?;
            self.live()?;
            drop(admission.check().map_err(refused)?);
            let review = self
                .engine
                .review_activation(observation, evidence, &now_ms)?;
            self.live()?;
            Ok(WorkResult::Reviewed(Box::new(Retained {
                review,
                admission,
            })))
        }
    }

    async fn gather(&self, route: &AuthorizedRoute) -> Result<ActivationEvidence, Refusal> {
        self.live()?;
        if route.network != Network::Testnet
            || route.binding.agent != self.binding.agent
            || route.binding.container != self.binding.account
        {
            return Err(refused(
                "activation route differs from supervised testnet identity",
            ));
        }
        let read_started_at_ms = now_ms();
        let account = route.binding.container;
        let (perps, spot, orders, market, account_role, signer_role) = tokio::try_join!(
            self.info.clearinghouse_state(account),
            self.info.spot_clearinghouse_state(account),
            self.info.frontend_open_orders(account),
            self.info.meta_and_asset_ctxs(),
            self.info.user_role(account),
            self.info.user_role(route.binding.wallet.address),
        )
        .map_err(refused)?;
        if !market.is_aligned() {
            return Err(refused("misaligned activation market evidence"));
        }
        let approval_user = match (&account_role, route.binding.vault_address) {
            (UserRole::User, None) => account,
            (UserRole::SubAccount { master }, Some(vault)) if vault == account => *master,
            _ => {
                return Err(refused(
                    "account role does not identify the reviewed approval user",
                ));
            }
        };
        let extra_agents = self
            .info
            .extra_agents(approval_user)
            .await
            .map_err(refused)?;
        self.live()?;
        Ok(ActivationEvidence {
            read_started_at_ms,
            read_completed_at_ms: now_ms(),
            perps,
            spot,
            orders,
            reference_prices: market.reference_pxs(),
            account_role,
            signer_role,
            extra_agents,
        })
    }

    fn finish(&self, id: String, result: Result<Result<WorkResult, Refusal>, String>) {
        let Ok(mut state) = self.state() else {
            self.closed.store(true, Ordering::SeqCst);
            return;
        };
        match result {
            Ok(Ok(WorkResult::Reviewed(retained))) if self.live().is_ok() => {
                state.status.review = Some(ReviewView {
                    id,
                    display: retained.review.display().clone(),
                });
                state.retained = Some(*retained);
                state.status.phase = Phase::ReviewReady;
            }
            Ok(Ok(WorkResult::Reviewed(_))) => state.status.phase = Phase::Closed,
            Ok(Ok(WorkResult::Acknowledged(receipt))) => {
                state.status.receipt = Some(receipt);
                state.status.phase = Phase::Acknowledged;
            }
            Ok(Err(refusal)) => {
                state.status.phase = if matches!(
                    refusal,
                    Refusal::Unevaluable(Unevaluable::AuditWriteFailed { .. })
                ) {
                    self.closed.store(true, Ordering::SeqCst);
                    Phase::Uncertain
                } else {
                    Phase::Refused
                };
                state.status.error = Some(ActivationError {
                    kind: ErrorKind::Refusal,
                    detail: refusal.to_string(),
                    refusal: Some(refusal),
                });
            }
            Err(detail) => {
                self.closed.store(true, Ordering::SeqCst);
                state.status.phase = Phase::Uncertain;
                state.status.error = Some(ActivationError {
                    kind: ErrorKind::Worker,
                    detail,
                    refusal: None,
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
        review: &str,
    ) -> Result<ActivationStatus, String> {
        self.binding(binding)?;
        let mut state = self.state()?;
        self.live().map_err(|error| error.to_string())?;
        if state.status.owner_id != owner
            || state
                .status
                .review
                .as_ref()
                .is_none_or(|view| view.id != review)
        {
            return Err("activation review owner changed".into());
        }
        let next_seq = state
            .status
            .operation_seq
            .checked_add(1)
            .ok_or("activation operation IDs exhausted")?;
        let retained = state.retained.take();
        state.status.operation_seq = next_seq;
        state.status.last_operation = Some(ActivationOperation::Discard {
            review_id: review.to_owned(),
        });
        state.status.review = None;
        state.status.phase = Phase::Idle;
        let status = state.status.clone();
        drop(state);
        drop(retained);
        Ok(status)
    }

    pub(crate) fn close(&self) -> Result<(), String> {
        self.closed.store(true, Ordering::SeqCst);
        let mut state = self.state()?;
        let retained = state.retained.take();
        state.status.review = None;
        if state.status.phase != Phase::Uncertain {
            state.status.phase = Phase::Closed;
        }
        drop(state);
        drop(retained);
        Ok(())
    }

    pub(crate) async fn close_and_drain(&self) -> Result<(), String> {
        let closed = self.close();
        let mut task = self.task.lock().await;
        if let Some(task) = task.as_mut() {
            task.await.map_err(|error| error.to_string())?;
        }
        *task = None;
        closed?;
        if self.state()?.status.phase == Phase::Uncertain {
            return Err("activation worker requires recovery".into());
        }
        Ok(())
    }
}
