//! ES38: operator review removes one kill scope, never order inhibition.

use super::*;
use crate::ledger::PilotState;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillReleaseMember {
    pub route: AuthorizedRoute,
    pub pilot: PilotState,
}

/// Display data, not a deserializable release capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillReleaseDisplay {
    pub operation_id: String,
    pub network: Network,
    pub scope: KillScope,
    pub persisted_engagement: Option<Engagement>,
    pub local_engagement: Option<Engagement>,
    pub policy_revision: u64,
    pub stop_generation: u64,
    pub affected: Vec<KillReleaseMember>,
    pub remaining_kill: KillSwitch,
    pub reviewed_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug)]
pub struct KillReleaseReview {
    owner: Arc<()>,
    request: crate::ledger::release::ReleaseRequest,
}
impl KillReleaseReview {
    pub fn display(&self) -> &KillReleaseDisplay {
        &self.request.display
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KillReleaseReceipt {
    pub operation_id: String,
    pub network: Network,
    pub scope: KillScope,
    pub reviewed_stop_generation: u64,
    pub policy_revision: u64,
    pub recorded_at_ms: u64,
    pub seq: u64,
    pub hash: String,
    pub remaining_kill: KillSwitch,
}

#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KillReleaseError {
    #[error("kill release refused: {detail}")]
    Refused { detail: String },
    #[error("kill release {operation_id} outcome uncertain: {detail}")]
    Uncertain {
        operation_id: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KillReleaseResolution {
    Committed {
        receipt: Box<KillReleaseReceipt>,
        current_policy_revision: u64,
        current_persisted_kill: KillSwitch,
        current_effective_kill: KillSwitch,
        current_stop_generation: u64,
    },
    /// Snapshot absence only. Native must prove actual work terminality before
    /// displaying NotCommitted; dropped IPC does not establish terminality.
    Absent { operation_id: String },
    Unknown {
        operation_id: String,
        detail: String,
    },
}

pub(crate) fn refused(detail: impl ToString) -> KillReleaseError {
    KillReleaseError::Refused {
        detail: detail.to_string(),
    }
}

impl GuardrailEngine {
    /// Synchronous ledger I/O; no feed, signer, acknowledgment or venue action.
    pub fn review_kill_release(
        &self,
        scope: KillScope,
        clock: &dyn Fn() -> u64,
    ) -> Result<KillReleaseReview, KillReleaseError> {
        let _mutation = self.mutation_lock().map_err(refused)?;
        self.state().inhibit();
        let authority = self.release_authority()?;
        let permit = authority.release_permit()?;
        let snapshot = permit.observe(&scope)?;
        let mut bytes = [0u8; 32];
        crate::keys::os_entropy(&mut bytes).map_err(refused)?;
        let mut state = self.state();
        state.publish(snapshot.policy.clone()).map_err(refused)?;
        let local_engagement = state.emergency.get(&scope).cloned();
        let persisted_engagement = snapshot.policy.state.kill.engagement(&scope).cloned();
        if local_engagement.is_none() && persisted_engagement.is_none() {
            return Err(refused("requested scope is not engaged"));
        }
        if state.stop_generation == u64::MAX {
            return Err(refused("stop generation exhausted"));
        }
        let now_ms = clock();
        if local_engagement
            .iter()
            .chain(persisted_engagement.iter())
            .any(|e| e.engaged_at_ms > now_ms)
        {
            return Err(refused("review clock precedes engagement"));
        }
        let expires_at_ms = now_ms
            .checked_add(60_000)
            .ok_or_else(|| refused("release deadline overflow"))?;
        let mut remaining_kill = state.effective_kill();
        remaining_kill.release(&scope);
        Ok(KillReleaseReview {
            owner: self.submission_owner.clone(),
            request: crate::ledger::release::ReleaseRequest {
                head: snapshot.head,
                before: snapshot.policy.state,
                display: KillReleaseDisplay {
                    operation_id: hex::encode(bytes),
                    network: self.network,
                    scope,
                    persisted_engagement,
                    local_engagement,
                    policy_revision: snapshot.policy.revision,
                    stop_generation: state.stop_generation,
                    affected: snapshot.affected,
                    remaining_kill,
                    reviewed_at_ms: now_ms,
                    expires_at_ms,
                },
            },
        })
    }

    /// Synchronous retained mutation. Uncertainty never authorizes an automatic retry.
    pub fn confirm_kill_release(
        &self,
        review: KillReleaseReview,
        clock: &dyn Fn() -> u64,
        final_authorize: &dyn Fn() -> Result<(), Refusal>,
    ) -> Result<KillReleaseReceipt, KillReleaseError> {
        let _mutation = self.mutation_lock().map_err(refused)?;
        let request = &review.request;
        if !Arc::ptr_eq(&review.owner, &self.submission_owner) {
            return Err(refused("kill release review belongs to another engine"));
        }
        let mut permit = self.release_authority()?.release_permit()?;
        permit.check(request)?;
        {
            let mut state = self.state();
            check_local(&state, request)?;
            final_authorize().map_err(refused)?;
            let at_ms = clock();
            check_time(request, at_ms)?;
            // Keep the old stop locally even if a durable removal commits but
            // anchor publication fails. Only verified success removes this overlay.
            if let Some(engagement) = request
                .display
                .local_engagement
                .as_ref()
                .or(request.display.persisted_engagement.as_ref())
            {
                state
                    .emergency
                    .entry(request.display.scope.clone())
                    .or_insert_with(|| engagement.clone());
            }
            state.acknowledged = None;
            state.activation_scope = None;
        }
        let uncertain = |error: String| KillReleaseError::Uncertain {
            operation_id: request.display.operation_id.clone(),
            detail: error,
        };
        let (receipt, version) = permit.commit(request, &|| {
            let state = self.state();
            if state.stop_generation != request.display.stop_generation {
                return Err(refused("new local stop arrived before release write"));
            }
            final_authorize().map_err(refused)?;
            let at_ms = clock();
            check_time(request, at_ms)?;
            Ok(at_ms)
        })?;
        let mut state = self.state();
        // Publication may have overlapped a local HALT, even while its durable
        // worker waits for this mutation lock. It wins and its overlay survives.
        if state.stop_generation != request.display.stop_generation {
            return Err(uncertain("new local stop arrived during release".into()));
        }
        final_authorize().map_err(|e| uncertain(e.to_string()))?;
        check_time(request, clock()).map_err(|e| uncertain(e.to_string()))?;
        state
            .publish(version)
            .map_err(|e| uncertain(e.to_string()))?;
        state.emergency.remove(&request.display.scope);
        state.kill_incarnations.remove(&request.display.scope);
        state.inhibit();
        Ok(receipt)
    }

    /// Read-only. Absence is never a claim that a retained task cannot still commit.
    pub fn reconcile_kill_release(&self, operation_id: &str) -> KillReleaseResolution {
        let read = self
            .release_authority()
            .and_then(|authority| authority.release_resolution(operation_id));
        match read {
            Ok(Some((receipt, current))) => {
                let state = self.state();
                let mut effective = current.state.kill.clone();
                for (scope, engagement) in &state.emergency {
                    effective.engage(scope.clone(), engagement.clone());
                }
                KillReleaseResolution::Committed {
                    receipt: Box::new(receipt),
                    current_policy_revision: current.revision,
                    current_persisted_kill: current.state.kill,
                    current_effective_kill: effective,
                    current_stop_generation: state.stop_generation,
                }
            }
            Ok(None) => KillReleaseResolution::Absent {
                operation_id: operation_id.into(),
            },
            Err(error) => KillReleaseResolution::Unknown {
                operation_id: operation_id.into(),
                detail: error.to_string(),
            },
        }
    }

    fn release_authority(&self) -> Result<&PolicyJournal, KillReleaseError> {
        if self.network != Network::Testnet {
            return Err(refused("reviewed release is testnet-only"));
        }
        self.activation_authority
            .as_deref()
            .ok_or_else(|| refused("release requires authenticated policy authority"))
    }
}

fn check_time(
    request: &crate::ledger::release::ReleaseRequest,
    at_ms: u64,
) -> Result<(), KillReleaseError> {
    if at_ms < request.display.reviewed_at_ms || at_ms >= request.display.expires_at_ms {
        return Err(refused("kill release review expired or clock rolled back"));
    }
    Ok(())
}

fn check_local(
    state: &EngineState,
    request: &crate::ledger::release::ReleaseRequest,
) -> Result<(), KillReleaseError> {
    if state.stop_generation != request.display.stop_generation
        || state.stop_generation == u64::MAX
        || state.policy_revision != request.display.policy_revision
        || state.emergency.get(&request.display.scope) != request.display.local_engagement.as_ref()
    {
        return Err(refused(
            "reviewed stop generation, policy or local engagement changed",
        ));
    }
    Ok(())
}
