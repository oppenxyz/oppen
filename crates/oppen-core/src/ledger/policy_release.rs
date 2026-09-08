//! A reviewed release is one MAC-covered policy row, not a second journal.

use super::*;
use crate::guardrail::{
    KillReleaseDisplay, KillReleaseError, KillReleaseMember, KillReleaseReceipt, KillScope,
};
use crate::ledger::{Anchor, LedgerSigningPermit};
use std::collections::{BTreeMap, BTreeSet};

type ReleaseResult<T> = std::result::Result<T, KillReleaseError>;

fn refused(error: impl ToString) -> KillReleaseError {
    KillReleaseError::Refused {
        detail: error.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseRequest {
    pub head: Anchor,
    pub before: PersistedState,
    pub display: KillReleaseDisplay,
}

pub(crate) struct ReleaseSnapshot {
    pub head: Anchor,
    pub policy: PolicyVersion,
    pub affected: Vec<KillReleaseMember>,
}

pub(crate) struct ReleasePermit<'a> {
    held: LedgerSigningPermit<'a>,
    policy: &'a PolicyJournal,
}

impl PolicyJournal {
    pub(crate) fn release_permit(&self) -> ReleaseResult<ReleasePermit<'_>> {
        let guard = self.registry.ledger().lock().map_err(refused)?;
        Ok(ReleasePermit {
            held: LedgerSigningPermit::new(guard).map_err(refused)?,
            policy: self,
        })
    }

    pub(crate) fn release_resolution(
        &self,
        operation_id: &str,
    ) -> ReleaseResult<Option<(KillReleaseReceipt, PolicyVersion)>> {
        if !valid_id(operation_id) {
            return Err(refused("invalid release operation identity"));
        }
        let permit = self.release_permit()?;
        let connection = &permit.held.guard;
        let current = self.current_in(connection).map_err(refused)?;
        // A verified database row with an unpublished anchor remains uncertain.
        let (seq, hash) = crate::ledger::head(connection).map_err(refused)?;
        let published = self
            .registry
            .ledger()
            .anchor
            .as_ref()
            .ok_or_else(|| refused("missing release anchor"))?
            .load()
            .map_err(refused)?;
        if published != Some(Anchor { seq, hash }) {
            return Err(refused("release head publication is not confirmed"));
        }
        let mut statement = connection
            .prepare(
                "SELECT payload, seq, hash FROM events WHERE kind = 'policy_replaced' ORDER BY seq",
            )
            .map_err(refused)?;
        let mut rows = statement.query([]).map_err(refused)?;
        while let Some(row) = rows.next().map_err(refused)? {
            let raw: String = row.get(0).map_err(refused)?;
            let signed: Signed = serde_json::from_str(&raw).map_err(refused)?;
            if let Operation::KillReleased { request } = &signed.envelope.operation
                && request.display.operation_id == operation_id
            {
                return Ok(Some((
                    receipt(
                        request,
                        signed.envelope.seq,
                        row.get(2).map_err(refused)?,
                        signed.envelope.at_ms,
                        &signed.envelope.state,
                    ),
                    current,
                )));
            }
        }
        Ok(None)
    }
}

impl ReleasePermit<'_> {
    pub(crate) fn observe(&self, scope: &KillScope) -> ReleaseResult<ReleaseSnapshot> {
        observe(self.policy, &self.held.guard, scope).map_err(refused)
    }

    pub(crate) fn check(&self, request: &ReleaseRequest) -> ReleaseResult<()> {
        check(&self.observe(&request.display.scope)?, request).map_err(refused)
    }

    pub(crate) fn commit(
        &mut self,
        request: &ReleaseRequest,
        final_gate: &dyn Fn() -> ReleaseResult<u64>,
    ) -> ReleaseResult<(KillReleaseReceipt, PolicyVersion)> {
        self.held.guard.execute_batch("ROLLBACK").map_err(refused)?;
        self.held.transaction_active = false;
        let tx = self
            .held
            .guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(refused)?;
        let observed = observe(self.policy, &tx, &request.display.scope).map_err(refused)?;
        check(&observed, request).map_err(refused)?;
        let previous = self
            .policy
            .replay(&tx)
            .map_err(refused)?
            .ok_or_else(|| refused("policy missing at release"))?;
        let mut next = request.before.clone();
        next.kill.release(&request.display.scope);
        let at_ms = final_gate()?;
        validate_state(&next, at_ms).map_err(refused)?;
        let envelope = Envelope {
            version: 1,
            network: self.policy.network(),
            seq: request
                .head
                .seq
                .checked_add(1)
                .ok_or_else(|| refused("release sequence exhausted"))?,
            prev_hash: request.head.hash.clone(),
            at_ms,
            previous_policy: Some(previous.link()),
            operation: Operation::KillReleased {
                request: Box::new(request.clone()),
            },
            state: next.clone(),
        };
        validate_transition(request, &previous.version, &envelope).map_err(refused)?;
        let (version, appended) = self
            .policy
            .append_in_tx(&tx, Some(previous.link()), envelope.operation, next, at_ms)
            .map_err(refused)?;
        let uncertain = |error: String| KillReleaseError::Uncertain {
            operation_id: request.display.operation_id.clone(),
            detail: error,
        };
        tx.commit().map_err(|e| uncertain(e.to_string()))?;
        self.policy
            .registry
            .ledger()
            .note_head(&appended)
            .map_err(|e| uncertain(e.to_string()))?;

        // A new write snapshot prevents an uncoordinated WAL writer from changing
        // verified history between publication and the local stop-generation gate.
        self.held
            .guard
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| uncertain(e.to_string()))?;
        self.held.transaction_active = true;
        let final_state = self
            .observe(&request.display.scope)
            .map_err(|e| uncertain(e.to_string()))?;
        if final_state.head
            != (Anchor {
                seq: appended.seq,
                hash: appended.hash.clone(),
            })
            || final_state.policy != version
            || final_state.affected != request.display.affected
        {
            return Err(uncertain(
                "release authority changed during publication".into(),
            ));
        }
        Ok((
            receipt(request, appended.seq, appended.hash, at_ms, &version.state),
            version,
        ))
    }
}

fn observe(
    policy: &PolicyJournal,
    connection: &Connection,
    scope: &KillScope,
) -> Result<ReleaseSnapshot> {
    let version = policy.current_in(connection)?;
    let routes = policy
        .registry
        .active_routes_in(connection)
        .map_err(|e| unavailable(e.to_string()))?;
    let mut by_agent = BTreeMap::new();
    let mut accounts = BTreeSet::new();
    for route in routes {
        if route.network != Network::Testnet
            || !accounts.insert(*route.binding.container.as_bytes())
            || by_agent
                .insert(route.binding.agent.clone(), route)
                .is_some()
        {
            return Err(unavailable("ambiguous or non-testnet release membership"));
        }
    }
    let agents = match scope {
        KillScope::Global => {
            let policy_agents: BTreeSet<_> = version.state.guardrails.keys().cloned().collect();
            if policy_agents.is_empty() || policy_agents != by_agent.keys().cloned().collect() {
                return Err(unavailable(
                    "global release membership is empty or unresolved",
                ));
            }
            policy_agents
        }
        KillScope::Agent { agent } => {
            if !version.state.guardrails.contains_key(agent) {
                return Err(unavailable("release agent has no authenticated policy"));
            }
            BTreeSet::from([agent.clone()])
        }
    };
    let mut affected = Vec::new();
    for agent in agents {
        let route = by_agent
            .remove(&agent)
            .ok_or_else(|| unavailable("release route missing or retired"))?;
        let pilot = crate::ledger::pilot::release_state_in(&policy.registry, connection, &route)
            .map_err(|e| unavailable(e.to_string()))?;
        affected.push(KillReleaseMember { route, pilot });
    }
    let (seq, hash) = crate::ledger::head(connection)?;
    Ok(ReleaseSnapshot {
        head: Anchor { seq, hash },
        policy: version,
        affected,
    })
}

fn check(snapshot: &ReleaseSnapshot, request: &ReleaseRequest) -> Result<()> {
    if snapshot.head != request.head
        || snapshot.policy.state != request.before
        || snapshot.policy.revision != request.display.policy_revision
        || snapshot.affected != request.display.affected
    {
        return Err(unavailable(
            "reviewed release checkpoint or authority changed",
        ));
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn validate_transition(
    request: &ReleaseRequest,
    previous: &PolicyVersion,
    envelope: &Envelope,
) -> Result<()> {
    let display = &request.display;
    let mut expected = request.before.clone();
    expected.kill.release(&display.scope);
    if !valid_id(&display.operation_id)
        || display.network != Network::Testnet
        || display.network != envelope.network
        || display.policy_revision != previous.revision
        || request.before != previous.state
        || expected != envelope.state
        || request.head.seq.checked_add(1) != Some(envelope.seq)
        || request.head.hash != envelope.prev_hash
        || display.persisted_engagement.as_ref() != request.before.kill.engagement(&display.scope)
        || (display.persisted_engagement.is_none() && display.local_engagement.is_none())
        || display.stop_generation == u64::MAX
        || display.reviewed_at_ms.checked_add(60_000) != Some(display.expires_at_ms)
        || envelope.at_ms < display.reviewed_at_ms
        || envelope.at_ms >= display.expires_at_ms
        || display.affected.is_empty()
    {
        return Err(unavailable("invalid correlated kill release transition"));
    }
    let mut agents = BTreeSet::new();
    for member in &display.affected {
        if member.route.network != display.network
            || !agents.insert(member.route.binding.agent.clone())
            || member.pilot.agent != member.route.binding.agent
            || member.pilot.account != member.route.binding.container
            || member
                .pilot
                .halt
                .as_ref()
                .is_some_and(|h| !matches!(h, crate::ledger::PilotStop::AwaitingReconciliation))
        {
            return Err(unavailable("invalid release member evidence"));
        }
    }
    let expected_agents = match &display.scope {
        KillScope::Global => previous.state.guardrails.keys().cloned().collect(),
        KillScope::Agent { agent } => BTreeSet::from([agent.clone()]),
    };
    if agents != expected_agents {
        return Err(unavailable("release membership differs from scope"));
    }
    Ok(())
}

fn receipt(
    request: &ReleaseRequest,
    seq: u64,
    hash: String,
    at_ms: u64,
    state: &PersistedState,
) -> KillReleaseReceipt {
    KillReleaseReceipt {
        operation_id: request.display.operation_id.clone(),
        network: request.display.network,
        scope: request.display.scope.clone(),
        reviewed_stop_generation: request.display.stop_generation,
        policy_revision: seq,
        recorded_at_ms: at_ms,
        seq,
        hash,
        remaining_kill: state.kill.clone(),
    }
}
