//! Operator-only halt ownership; cancellation remains the server's job.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oppen_core::guardrail::{GuardrailEngine, KillReason, KillScope, PendingOperatorKill};
use oppen_mcp::auth::Binding;
use oppen_mcp::server::{Pairings, SupervisionControl, SupervisionStatus};
use serde::Serialize;

use crate::mcp_runtime::{SharedStatus, status_lock};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

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
    pub owner_id: String,
    pub stop_generation: u64,
    pub released_stop_generation: Option<u64>,
    pub released_engine_stop_generation: Option<u64>,
    pub previous: Option<HaltReceipt>,
    pub phase: HaltPhase,
    pub cancellation: CancellationPhase,
    pub requested_at_ms: Option<u64>,
    pub durable_revision: Option<u64>,
    pub error: Option<String>,
    pub cancellation_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HaltReceipt {
    pub stop_generation: u64,
    pub phase: HaltPhase,
    pub cancellation: CancellationPhase,
    pub requested_at_ms: Option<u64>,
    pub error: Option<String>,
    pub cancellation_error: Option<String>,
}

impl HaltStatus {
    fn receipt(&self) -> HaltReceipt {
        HaltReceipt {
            stop_generation: self.stop_generation,
            phase: self.phase,
            cancellation: self.cancellation,
            requested_at_ms: self.requested_at_ms,
            error: self.error.clone(),
            cancellation_error: self.cancellation_error.clone(),
        }
    }
}

#[derive(Default)]
struct Admission {
    closed: bool,
    requested: bool,
    generation: u64,
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
    #[cfg(test)]
    rearm_entered: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
}

impl HaltControl {
    pub(crate) fn new(
        engine: Arc<GuardrailEngine>,
        binding: Binding,
        pairings: Pairings,
        supervision: SupervisionControl,
        status: SharedStatus,
    ) -> Arc<Self> {
        let owner =
            NEXT_OWNER.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1));
        let exhausted = owner.is_err();
        status_lock(&status).halt.owner_id =
            owner.map_or_else(|_| "exhausted".into(), |id| id.to_string());
        Arc::new(Self {
            engine,
            binding,
            pairings,
            supervision,
            status,
            admission: Mutex::new(Admission {
                closed: exhausted,
                ..Admission::default()
            }),
            task: tokio::sync::Mutex::new(None),
            #[cfg(test)]
            rearm_entered: Mutex::new(None),
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
        let mut task = self
            .task
            .try_lock()
            .map_err(|_| "operator halt is draining")?;
        let at_ms = u64::try_from(oppen_core::ledger::now_ms())
            .map_err(|_| "operator halt timestamp is unavailable")?;
        // Fence release synchronously, even when an earlier HALT worker is retained.
        let pending = self.engine.begin_operator_kill(
            KillScope::Agent {
                agent: binding.agent.clone(),
            },
            KillReason::Operator,
            at_ms,
        );
        let generation = pending.stop_generation();
        admission.requested = true;
        admission.generation = generation;
        admission.baseline = None;
        admission.stop_engaged = true;
        let mut status = status_lock(&self.status);
        let previous = (status.halt.phase != HaltPhase::Idle).then(|| status.halt.receipt());
        let owner_id = status.halt.owner_id.clone();
        status.halt = HaltStatus {
            owner_id,
            stop_generation: generation,
            previous,
            phase: HaltPhase::Persisting,
            requested_at_ms: Some(at_ms),
            ..HaltStatus::default()
        };
        status.orders_inhibited = true;
        drop(status);
        let previous = task.take();
        let owner = self.clone();
        *task = Some(tauri::async_runtime::spawn(async move {
            // The newest handle owns the complete chain, including dropped IPC observers.
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            owner.execute(pending, generation).await;
        }));
        Ok(())
    }

    async fn execute(self: &Arc<Self>, pending: PendingOperatorKill, generation: u64) {
        let engine = self.engine.clone();
        let mutation = tauri::async_runtime::spawn_blocking(move || {
            engine
                .persist_operator_kill(pending)
                .map_err(|error| error.to_string())
        })
        .await;
        let persisted = mutation
            .map_err(|error| format!("operator halt mutation task: {error}"))
            .and_then(|result| result);
        {
            let admission = self.admission();
            let mut status = status_lock(&self.status);
            if admission.generation != generation {
                if let Some(previous) = &mut status.halt.previous
                    && previous.stop_generation == generation
                {
                    previous.phase = if persisted.is_ok() {
                        HaltPhase::Persisted
                    } else {
                        HaltPhase::Uncertain
                    };
                    previous.error = persisted.err();
                }
                return;
            }
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
        {
            let mut admission = self.admission();
            if admission.generation != generation {
                return;
            }
            admission.baseline = Some(baseline);
        }
        loop {
            let sweep = updates.borrow_and_update().clone();
            if self.observe_generation(generation, &sweep) {
                break;
            }
            if updates.changed().await.is_err() {
                self.unavailable(
                    generation,
                    "cancellation supervisor ended before a qualifying sweep",
                );
                break;
            }
        }
    }

    /// Only later completed attempts qualify. A successful sweep may skip a
    /// target, so retain and check the same engine and pairing coverage too.
    pub(crate) fn observe(&self, sweep: &SupervisionStatus) -> bool {
        let generation = self.admission().generation;
        self.observe_generation(generation, sweep)
    }

    fn observe_generation(&self, generation: u64, sweep: &SupervisionStatus) -> bool {
        let (baseline, stop_engaged) = {
            let admission = self.admission();
            if admission.generation != generation {
                return true;
            }
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
        let admission = self.admission();
        if admission.generation != generation {
            return true;
        }
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

    fn unavailable(&self, generation: u64, detail: &str) {
        let admission = self.admission();
        if admission.generation != generation {
            return;
        }
        let mut status = status_lock(&self.status);
        if status.halt.cancellation != CancellationPhase::Acknowledged {
            status.halt.cancellation = CancellationPhase::Unavailable;
            status.halt.cancellation_error = Some(detail.into());
        }
    }

    pub(crate) fn supervision_ended(&self) {
        let (requested, generation) = {
            let admission = self.admission();
            (admission.requested, admission.generation)
        };
        if requested {
            self.unavailable(generation, "cancellation supervisor has stopped");
        }
    }

    pub(crate) fn work_terminal(&self) -> bool {
        self.task
            .try_lock()
            .is_ok_and(|task| task.as_ref().is_none_or(|task| task.inner().is_finished()))
    }

    pub(crate) fn release_confirmed(&self, receipt: &oppen_core::guardrail::KillReleaseReceipt) {
        let admission = self.admission();
        if admission.closed
            || !admission.requested
            || admission.generation > receipt.reviewed_stop_generation
            || !self.work_terminal()
            || Some(self.engine.policy_status().stop_generation)
                != receipt.reviewed_stop_generation.checked_add(1)
            || self.engine.kill_switch() != Default::default()
        {
            return;
        }
        #[cfg(test)]
        if let Some((entered, resume)) = self.rearm_entered.lock().unwrap().take() {
            let _ = entered.send(());
            let _ = resume.recv();
        }
        let mut status = status_lock(&self.status);
        status.halt.released_stop_generation = Some(admission.generation);
        status.halt.released_engine_stop_generation =
            receipt.reviewed_stop_generation.checked_add(1);
    }

    #[cfg(test)]
    pub(crate) fn watch_rearm(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (send, recv) = tokio::sync::oneshot::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        *self.rearm_entered.lock().unwrap() = Some((send, wait));
        (recv, resume)
    }

    #[cfg(test)]
    pub(crate) fn last_sweep(&self) -> SupervisionStatus {
        self.supervision.status().borrow().clone()
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
