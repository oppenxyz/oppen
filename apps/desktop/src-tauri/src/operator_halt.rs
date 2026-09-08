//! Operator-only halt ownership; cancellation remains the server's job.

use std::sync::{Arc, Mutex};

use oppen_core::guardrail::{GuardrailEngine, KillReason, KillScope};
use oppen_mcp::auth::Binding;
use oppen_mcp::server::{Pairings, SupervisionControl, SupervisionStatus};
use serde::Serialize;

use crate::mcp_runtime::{SharedStatus, status_lock};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HaltPhase {
    #[default]
    Idle,
    Persisting,
    Persisted,
    Uncertain,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CancellationPhase {
    #[default]
    NotRequested,
    Pending,
    Retrying,
    Acknowledged,
    Unavailable,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct HaltStatus {
    pub phase: HaltPhase,
    pub cancellation: CancellationPhase,
    pub requested_at_ms: Option<u64>,
    pub durable_revision: Option<u64>,
    pub error: Option<String>,
    pub cancellation_error: Option<String>,
}

#[derive(Default)]
struct Admission {
    closed: bool,
    requested: bool,
    baseline: Option<u64>,
    stop_engaged: bool,
}

pub(crate) struct HaltControl {
    engine: Arc<GuardrailEngine>,
    binding: Binding,
    pairings: Pairings,
    supervision: SupervisionControl,
    status: SharedStatus,
    admission: Mutex<Admission>,
    task: tokio::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl HaltControl {
    pub(crate) fn new(
        engine: Arc<GuardrailEngine>,
        binding: Binding,
        pairings: Pairings,
        supervision: SupervisionControl,
        status: SharedStatus,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            binding,
            pairings,
            supervision,
            status,
            admission: Mutex::new(Admission::default()),
            task: tokio::sync::Mutex::new(None),
        })
    }

    fn admission(&self) -> std::sync::MutexGuard<'_, Admission> {
        self.admission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn request(self: &Arc<Self>, binding: &Binding) -> Result<(), String> {
        let mut admission = self.admission();
        if admission.closed {
            return Err("operator halt admission is closed".into());
        }
        if *binding != self.binding {
            return Err("operator halt requires the supervised agent and account".into());
        }
        if admission.requested {
            return Ok(());
        }
        let mut task = self
            .task
            .try_lock()
            .map_err(|_| "operator halt is draining")?;
        let at_ms = u64::try_from(oppen_core::ledger::now_ms())
            .map_err(|_| "operator halt timestamp is unavailable")?;
        admission.requested = true;
        status_lock(&self.status).halt = HaltStatus {
            phase: HaltPhase::Persisting,
            requested_at_ms: Some(at_ms),
            ..HaltStatus::default()
        };
        let owner = self.clone();
        *task = Some(tauri::async_runtime::spawn(async move {
            owner.execute(at_ms).await;
        }));
        Ok(())
    }

    async fn execute(self: &Arc<Self>, at_ms: u64) {
        let engine = self.engine.clone();
        let agent = self.binding.agent.clone();
        let mutation = tauri::async_runtime::spawn_blocking(move || {
            engine
                .operator_engage_kill(KillScope::Agent { agent }, KillReason::Operator, at_ms)
                .map_err(|error| error.to_string())
        })
        .await;
        // Every normal return, including a persistence error, follows the
        // engine's per-agent emergency engagement. Only explicit release can
        // remove that overlay; this owner exposes no such capability. A panic
        // is not evidence that engagement was reached.
        self.admission().stop_engaged = mutation.is_ok();
        let persisted = mutation
            .map_err(|error| format!("operator halt mutation task: {error}"))
            .and_then(|result| result);
        {
            let mut status = status_lock(&self.status);
            status.halt.phase = if persisted.is_ok() {
                HaltPhase::Persisted
            } else {
                HaltPhase::Uncertain
            };
            // KillEffect does not carry the committed revision. A later cache
            // read is not a receipt for this mutation.
            status.halt.error = persisted.err();
            status.halt.cancellation = CancellationPhase::Pending;
        }
        let mut updates = self.supervision.status();
        let baseline = self.supervision.request_sweep();
        self.admission().baseline = Some(baseline);
        loop {
            let sweep = updates.borrow_and_update().clone();
            if self.observe(&sweep) {
                break;
            }
            if updates.changed().await.is_err() {
                self.unavailable("cancellation supervisor ended before a qualifying sweep");
                break;
            }
        }
    }

    /// Only later completed attempts qualify. A successful sweep may skip a
    /// target, so retain and check the same engine and pairing coverage too.
    pub(crate) fn observe(&self, sweep: &SupervisionStatus) -> bool {
        let (baseline, stop_engaged) = {
            let admission = self.admission();
            let Some(baseline) = admission.baseline else {
                return false;
            };
            (baseline, admission.stop_engaged)
        };
        if sweep.completed_sequence <= baseline {
            return false;
        }
        let covered = stop_engaged
            && self.engine.cancellation_needed(&self.binding.agent)
            && self
                .pairings
                .try_read()
                .is_ok_and(|store| store.supports_binding(&self.binding));
        let mut status = status_lock(&self.status);
        if status.halt.cancellation == CancellationPhase::Acknowledged {
            return true;
        }
        if let Some(error) = &sweep.last_error {
            status.halt.cancellation = CancellationPhase::Retrying;
            status.halt.cancellation_error = Some(error.clone());
        } else if !covered {
            status.halt.cancellation = CancellationPhase::Unavailable;
            status.halt.cancellation_error =
                Some("bound-agent cancellation coverage is unavailable".into());
        } else {
            status.halt.cancellation = CancellationPhase::Acknowledged;
            status.halt.cancellation_error = None;
        }
        true
    }

    fn unavailable(&self, detail: &str) {
        let mut status = status_lock(&self.status);
        if status.halt.cancellation != CancellationPhase::Acknowledged {
            status.halt.cancellation = CancellationPhase::Unavailable;
            status.halt.cancellation_error = Some(detail.into());
        }
    }

    pub(crate) fn supervision_ended(&self) {
        if self.admission().requested {
            self.unavailable("cancellation supervisor has stopped");
        }
    }

    pub(crate) async fn close_and_drain(&self) -> Result<(), String> {
        self.admission().closed = true;
        // Borrow the retained handle: dropping a drain waiter must not detach
        // the mutation or allow another waiter to declare it drained.
        let mut task = self.task.lock().await;
        if let Some(task) = task.as_mut()
            && let Err(error) = task.await
        {
            let mut status = status_lock(&self.status);
            status.halt.phase = HaltPhase::Uncertain;
            status.halt.error = Some(format!("operator halt task: {error}"));
            status.halt.cancellation = CancellationPhase::Unavailable;
        }
        *task = None;
        let status = status_lock(&self.status);
        if status.halt.phase == HaltPhase::Idle {
            return Ok(());
        }
        if status.halt.phase != HaltPhase::Persisted {
            return Err("operator halt durability is uncertain".into());
        }
        if status.halt.cancellation != CancellationPhase::Acknowledged {
            return Err("operator halt cancellation was not acknowledged before shutdown".into());
        }
        Ok(())
    }
}
