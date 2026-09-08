//! Process-local evidence between socket receipt and completed consumption.
//! This cannot observe frames that have not reached the WS parser.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use tokio::sync::mpsc;

use super::WsEvent;

#[derive(Debug, Clone)]
pub struct IngressObservation(Arc<()>);

impl PartialEq for IngressObservation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for IngressObservation {}

#[derive(Debug)]
struct State {
    epoch: IngressObservation,
    pending: usize,
    outstanding: usize,
    failed: bool,
    completed: bool,
    admitted: usize,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    released: Condvar,
}

#[derive(Debug, Clone)]
pub struct IngressMonitor(Arc<Shared>);

#[derive(Debug, Clone)]
pub struct IngressStatus {
    pub pending: usize,
    pub failure: Option<String>,
    pub completed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum IngressError {
    #[error("ingress evidence was abandoned or its lock poisoned")]
    Failed,
    #[error("account ingress is pending")]
    Pending,
    #[error("ingress observation changed")]
    Changed,
    #[error("ingress consumer is closed or not fully drained")]
    Closed,
}

/// Held only across synchronous admission/crypto or the first transport poll.
/// Never retain this guard across an await or publication I/O.
#[derive(Debug)]
pub struct IngressAdmissionGuard {
    monitor: IngressMonitor,
}

impl Drop for IngressAdmissionGuard {
    fn drop(&mut self) {
        let mut state = self
            .monitor
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.admitted -= 1;
        if std::thread::panicking() {
            state.failed = true;
        }
        self.monitor.0.released.notify_all();
    }
}

impl IngressMonitor {
    pub fn same_monitor(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    // An owned permit avoids borrowing a mutex inside the caller's guarded
    // state. Mutations wait for synchronous admitted work; reads do not.
    fn mutation(&self) -> MutexGuard<'_, State> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.admitted != 0 {
            state = self
                .0
                .released
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        state
    }

    pub fn status(&self) -> IngressStatus {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        IngressStatus {
            pending: state.pending,
            failure: (state.failed || self.0.state.is_poisoned())
                .then(|| IngressError::Failed.to_string()),
            completed: state.completed,
        }
    }

    /// Capturing an observation is not admission; pending/failed state is
    /// checked by `admit`, including when the observation itself is current.
    pub fn observation(&self) -> IngressObservation {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .epoch
            .clone()
    }

    pub fn admit(
        &self,
        observation: &IngressObservation,
    ) -> Result<IngressAdmissionGuard, IngressError> {
        let mut state = self.0.state.lock().map_err(|_| IngressError::Failed)?;
        if state.failed {
            return Err(IngressError::Failed);
        }
        if state.completed {
            return Err(IngressError::Closed);
        }
        if state.pending != 0 {
            return Err(IngressError::Pending);
        }
        if !Arc::ptr_eq(&state.epoch.0, &observation.0) {
            return Err(IngressError::Changed);
        }
        state.admitted += 1;
        Ok(IngressAdmissionGuard {
            monitor: self.clone(),
        })
    }

    pub fn replacement_eligible(&self) -> bool {
        self.0
            .state
            .lock()
            .is_ok_and(|s| s.completed && !s.failed && s.outstanding == 0)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("event ingress channel is closed")]
pub struct EventSendError;

#[derive(Debug, Clone)]
pub struct EventSender {
    tx: mpsc::Sender<FrameEnvelope>,
    monitor: IngressMonitor,
}

#[derive(Debug)]
pub struct EventReceiver {
    rx: mpsc::Receiver<FrameEnvelope>,
    monitor: IngressMonitor,
    eof: bool,
    completed: bool,
}

/// Non-cloneable processing obligation. Dequeue is not acknowledgment.
#[derive(Debug)]
pub struct FrameEnvelope {
    event: WsEvent,
    received_at_ms: u64,
    obligation: Obligation,
}

#[derive(Debug)]
struct Obligation {
    monitor: IngressMonitor,
    account: bool,
    acknowledged: bool,
}

impl Drop for Obligation {
    fn drop(&mut self) {
        let mut state = self.monitor.mutation();
        if !self.acknowledged {
            state.failed = true;
        }
        state.outstanding -= 1;
        if self.account {
            state.pending -= 1;
        }
    }
}

impl FrameEnvelope {
    pub fn event(&self) -> &WsEvent {
        &self.event
    }
    pub fn received_at_ms(&self) -> u64 {
        self.received_at_ms
    }

    /// Only after every required durable/state effect has completed.
    pub fn acknowledge(mut self) {
        self.obligation.acknowledged = true;
    }
}

pub fn event_channel(capacity: usize) -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel(capacity);
    let monitor = IngressMonitor(Arc::new(Shared {
        state: Mutex::new(State {
            epoch: IngressObservation(Arc::new(())),
            pending: 0,
            outstanding: 0,
            failed: false,
            completed: false,
            admitted: 0,
        }),
        released: Condvar::new(),
    }));
    (
        EventSender {
            tx,
            monitor: monitor.clone(),
        },
        EventReceiver {
            rx,
            monitor,
            eof: false,
            completed: false,
        },
    )
}

impl EventSender {
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Registration runs at invocation, not at the future's first poll. A
    /// capacity wait, canceled send, or never-polled future cannot hide work.
    pub fn send(
        &self,
        event: WsEvent,
        received_at_ms: u64,
    ) -> impl std::future::Future<Output = Result<(), EventSendError>> + Send + '_ {
        let account = match &event {
            WsEvent::UserFills { .. }
            | WsEvent::OrderUpdates { .. }
            | WsEvent::Disconnected(_)
            | WsEvent::Reconnected(_)
            | WsEvent::SubscriptionQuarantined { .. }
            | WsEvent::VenueError { .. }
            | WsEvent::MessageDropped { .. } => true,
            WsEvent::ActiveAssetCtx { .. }
            | WsEvent::Bbo { .. }
            | WsEvent::Trades { .. }
            | WsEvent::Candle(_)
            | WsEvent::L2Book(_) => false,
        };
        let envelope = {
            let mut state = self
                .monitor
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if state.completed {
                state.failed = true;
                None
            } else {
                state.outstanding += 1;
                if account {
                    state.pending += 1;
                    state.epoch = IngressObservation(Arc::new(()));
                }
                // Publish account work before waiting for the existing permit.
                // New admissions cannot overtake it. A failed monitor still
                // delivers evidence; only admission stays permanently closed.
                while state.admitted != 0 {
                    state = self
                        .monitor
                        .0
                        .released
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
                Some(FrameEnvelope {
                    event,
                    received_at_ms,
                    obligation: Obligation {
                        monitor: self.monitor.clone(),
                        account,
                        acknowledged: false,
                    },
                })
            }
        };
        async move {
            let envelope = envelope.ok_or(EventSendError)?;
            self.tx.send(envelope).await.map_err(|_| EventSendError)
        }
    }
}

impl EventReceiver {
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }

    pub fn monitor(&self) -> IngressMonitor {
        self.monitor.clone()
    }

    pub async fn recv(&mut self) -> Option<FrameEnvelope> {
        let frame = self.rx.recv().await;
        if frame.is_none() {
            self.eof = true;
        }
        frame
    }

    pub fn blocking_recv(&mut self) -> Option<FrameEnvelope> {
        let frame = self.rx.blocking_recv();
        if frame.is_none() {
            self.eof = true;
        }
        frame
    }

    pub fn try_recv(&mut self) -> Result<FrameEnvelope, mpsc::error::TryRecvError> {
        let frame = self.rx.try_recv();
        if matches!(frame, Err(mpsc::error::TryRecvError::Disconnected)) {
            self.eof = true;
        }
        frame
    }

    pub fn close(&mut self) {
        self.rx.close();
    }

    /// Called by the owner after the consumer has actually completed, not
    /// merely after dequeue/EOF. There is no recovery/reset for failed ingress.
    pub fn complete(mut self) -> Result<(), IngressError> {
        {
            let mut state = self.monitor.mutation();
            if state.failed || self.monitor.0.state.is_poisoned() {
                return Err(IngressError::Failed);
            }
            if !self.eof || state.outstanding != 0 || self.rx.sender_strong_count() != 0 {
                return Err(IngressError::Closed);
            }
            state.completed = true;
        }
        self.completed = true;
        Ok(())
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        if !self.completed {
            self.monitor.mutation().failed = true;
        }
    }
}

#[cfg(test)]
#[path = "ingress_tests.rs"]
mod tests;
