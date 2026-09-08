//! Native work shares the listener's admission and execution drain.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::auth::{Binding, SessionAuthority};
use crate::outcome::ToolError;
use crate::tools::Gateway;

use super::{ExecutionOwner, ExecutionTracker, Pairings, SigningAdmission};

pub(super) struct OperatorOwner {
    gateway: Gateway,
    pairings: Pairings,
    pub(super) lifecycle: Arc<OperatorLifecycle>,
}

// Server teardown retains the drain, not an otherwise-unused token store.
pub(super) struct OperatorLifecycle {
    started: AtomicBool,
    closed: AtomicBool,
    pub(super) shutdown: CancellationToken,
    pub(super) execution: watch::Sender<()>,
}

impl OperatorLifecycle {
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Obtained from the actual listener; never constructible from a gateway clone.
#[derive(Clone)]
pub struct OperatorControl {
    pub(super) owner: Arc<OperatorOwner>,
}

#[derive(Clone)]
pub(crate) struct OperatorWork {
    owner: Arc<OperatorOwner>,
    pub(crate) authority: SessionAuthority,
    pub(crate) tracker: ExecutionTracker,
}

/// Retains the selected pairing and the core-created candidate, never signing authority.
pub struct OperatorReview {
    owner: Arc<OperatorOwner>,
    authority: SessionAuthority,
    review: oppen_core::guardrail::ApprovalReview,
}

impl OperatorReview {
    pub fn display(&self) -> &oppen_core::guardrail::ApprovalReviewDisplay {
        self.review.display()
    }

    pub fn pairing_id(&self) -> crate::auth::PairingId {
        self.authority.id
    }
}

impl OperatorControl {
    pub(super) fn new(gateway: Gateway, pairings: Pairings) -> Self {
        Self {
            owner: Arc::new(OperatorOwner {
                gateway,
                pairings,
                lifecycle: Arc::new(OperatorLifecycle {
                    started: AtomicBool::new(false),
                    closed: AtomicBool::new(false),
                    shutdown: CancellationToken::new(),
                    execution: watch::channel(()).0,
                }),
            }),
        }
    }

    pub(super) fn start(&self, gateway: &Gateway, pairings: &Pairings) -> std::io::Result<()> {
        if self.owner.lifecycle.closed.load(Ordering::SeqCst)
            || !self.owner.gateway.same_owner(gateway)
            || !Arc::ptr_eq(&self.owner.pairings, pairings)
        {
            return Err(std::io::Error::other(
                "listener execution owner changed or closed",
            ));
        }
        self.owner
            .lifecycle
            .started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| std::io::Error::other("listener already started"))?;
        Ok(())
    }

    /// Close native admission; keep the cancellation supervisor alive until
    /// its owner finishes halt cleanup and stops the listener itself.
    pub fn close(&self) {
        self.owner.lifecycle.close();
    }

    pub async fn prepare(
        &self,
        binding: &Binding,
        proposal_id: &str,
    ) -> Result<OperatorReview, rmcp::ErrorData> {
        let work = self.admit(binding, None)?;
        let review = self
            .owner
            .gateway
            .operator_prepare(&work, proposal_id)
            .await?;
        work.check_live()?;
        Ok(OperatorReview {
            owner: self.owner.clone(),
            authority: work.authority.clone(),
            review,
        })
    }

    pub async fn confirm(
        &self,
        retained: OperatorReview,
    ) -> Result<serde_json::Value, rmcp::ErrorData> {
        let correlation = match retained.review.display() {
            oppen_core::guardrail::ApprovalReviewDisplay::Order(display) => serde_json::json!({
                "cloid": display.cloid.as_str(), "proposal_id": display.proposal_id,
            }),
            oppen_core::guardrail::ApprovalReviewDisplay::Cancel(display) => serde_json::json!({
                "action": "cancel", "targets": display.targets, "proposal_id": display.proposal_id,
            }),
        };
        let failure = |error: ToolError| {
            let mut error: rmcp::ErrorData = error.into();
            if let Some(data) = error
                .data
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                if let Some(correlation) = correlation.as_object() {
                    data.extend(correlation.clone());
                }
                data.insert("retryable".into(), serde_json::Value::Bool(false));
            }
            error
        };
        if !Arc::ptr_eq(&self.owner, &retained.owner) {
            return Err(failure(ToolError::unavailable(
                "operator admission",
                "review belongs to another listener",
            )));
        }
        let work = self
            .admit(retained.authority.binding(), Some(&retained.authority))
            .map_err(&failure)?;
        let reply = self
            .owner
            .gateway
            .operator_confirm(&work, retained.review)
            .await
            .map_err(&failure)?;
        serde_json::to_value(reply)
            .map_err(|error| failure(ToolError::unavailable("operator result", error)))
    }

    fn admit(
        &self,
        binding: &Binding,
        pinned: Option<&SessionAuthority>,
    ) -> Result<OperatorWork, ToolError> {
        // Register before observing closure. If close wins, this receiver
        // admits no work; if admission wins, drain already sees its obligation.
        let execution = self.owner.lifecycle.execution.subscribe();
        if !self.owner.lifecycle.started.load(Ordering::SeqCst)
            || self.owner.lifecycle.closed.load(Ordering::SeqCst)
            || self.owner.lifecycle.shutdown.is_cancelled()
        {
            return Err(ToolError::unavailable(
                "operator admission",
                "listener is not accepting work",
            ));
        }
        let pairings = self
            .owner
            .pairings
            .try_read()
            .map_err(|error| ToolError::unavailable("pairing authority", error))?;
        let authority = match pinned {
            Some(authority) if authority.binding() == binding => {
                pairings
                    .check_authority(authority)
                    .map_err(|error| ToolError::unavailable("pairing authority", error))?;
                authority.clone()
            }
            Some(_) => {
                return Err(ToolError::unavailable(
                    "pairing authority",
                    "review binding changed",
                ));
            }
            None => pairings
                .operator_authority(binding)
                .map_err(|error| ToolError::unavailable("pairing authority", error))?,
        };
        let tracker = ExecutionTracker {
            _execution: execution,
            _owner: ExecutionOwner::Native {
                _authority: authority.clone(),
            },
        };
        Ok(OperatorWork {
            owner: self.owner.clone(),
            authority,
            tracker,
        })
    }
}

impl OperatorWork {
    pub(crate) fn binding(&self) -> &Binding {
        self.authority.binding()
    }

    pub(crate) fn check_live(&self) -> Result<(), ToolError> {
        self.signing_admission().map(drop)
    }

    pub(crate) fn signing_admission(&self) -> Result<SigningAdmission<'_>, ToolError> {
        let pairings = self
            .owner
            .pairings
            .try_read()
            .map_err(|error| ToolError::unavailable("pairing authority", error))?;
        pairings
            .check_authority(&self.authority)
            .map_err(|error| ToolError::unavailable("pairing authority", error))?;
        if self.owner.lifecycle.closed.load(Ordering::SeqCst)
            || self.owner.lifecycle.shutdown.is_cancelled()
        {
            return Err(ToolError::unavailable(
                "operator admission",
                "listener stopped before signing",
            ));
        }
        Ok(SigningAdmission {
            _pairings: pairings,
        })
    }
}
