//! Activation uses the signing authority's coordination lease, not a second store.

use super::*;
use crate::guardrail::{AgentId, PolicyVersion, Refusal, Unevaluable};
use oppen_hl::Address;

pub(crate) fn refused(detail: impl ToString) -> Refusal {
    Unevaluable::ActivationAuthority {
        detail: detail.to_string(),
    }
    .into()
}

#[derive(Debug)]
pub(crate) struct ActivationAuthority {
    pub head: Anchor,
    pub route: AuthorizedRoute,
    pub policy: PolicyVersion,
    pub pilot: PilotState,
}

pub(crate) struct ActivationPermit<'a> {
    held: LedgerSigningPermit<'a>,
    policy: &'a PolicyJournal,
}

impl PolicyJournal {
    pub(crate) fn activation_permit(&self) -> std::result::Result<ActivationPermit<'_>, Refusal> {
        let guard = self.registry().ledger().lock().map_err(refused)?;
        Ok(ActivationPermit {
            held: LedgerSigningPermit::new(guard).map_err(refused)?,
            policy: self,
        })
    }
}

impl ActivationPermit<'_> {
    pub(crate) fn observe(
        &self,
        agent: &AgentId,
        account: Address,
    ) -> std::result::Result<ActivationAuthority, Refusal> {
        Self::observe_in(self.policy, &self.held, agent, account)
    }

    fn observe_in(
        authority: &PolicyJournal,
        connection: &Connection,
        agent: &AgentId,
        account: Address,
    ) -> std::result::Result<ActivationAuthority, Refusal> {
        let registry = authority.registry();
        // route_in walks the anchored chain before replaying authenticated grants.
        let route = registry.route_in(connection, agent).map_err(refused)?;
        if route.network != oppen_hl::Network::Testnet || route.binding.container != account {
            return Err(refused("activation requires the exact testnet route"));
        }
        let policy = authority.current_in(connection).map_err(refused)?;
        if !policy.state.guardrails.contains_key(agent) {
            return Err(refused("activation policy does not include this agent"));
        }
        if let Some((scope, engagement)) = policy.state.kill.blocking(agent) {
            return Err(Refusal::TradingPaused {
                scope,
                since_ms: engagement.engaged_at_ms,
                reason: engagement.reason.clone(),
            });
        }
        let pilot = pilot::activation_state_in(registry, connection, &route).map_err(refused)?;
        let submissions = authority
            .submissions(true)
            .state_in(connection, account)
            .map_err(refused)?;
        if submissions.pending.is_some() {
            return Err(refused("unresolved submission liability blocks activation"));
        }
        let (seq, hash) = head(connection).map_err(refused)?;
        Ok(ActivationAuthority {
            head: Anchor { seq, hash },
            route,
            policy,
            pilot,
        })
    }

    /// Retain coordination through COMMIT and anchor publication. Callers must
    /// release engine/feed guards before invoking this disk-I/O operation.
    pub(crate) fn record_request(
        &mut self,
        observed: &ActivationAuthority,
        stop_generation: u64,
        at_ms: u64,
    ) -> std::result::Result<Appended, Refusal> {
        let ts_ms = i64::try_from(at_ms).map_err(refused)?;
        self.held.guard.execute_batch("ROLLBACK").map_err(refused)?;
        self.held.transaction_active = false;
        let tx = self
            .held
            .guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(refused)?;
        let current = Self::observe_in(
            self.policy,
            &tx,
            &observed.route.binding.agent,
            observed.route.binding.container,
        )?;
        check_checkpoint(observed, &current, &observed.head)?;
        let payload = serde_json::json!({
            "action": "activation_acknowledgment_requested",
            "route": observed.route,
            "policy_revision": observed.policy.revision,
            "stop_generation": stop_generation,
            "reviewed_head": observed.head,
            "pilot_baseline": observed.pilot.baseline,
            "reason": "operator confirmation request, not continuing venue eligibility"
        });
        let appended = append_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::OperatorAction,
                ts_ms,
                agent_id: Some(observed.route.binding.agent.as_str()),
                payload: &payload,
                snapshot: None,
            },
        )
        .map_err(refused)?;
        tx.commit().map_err(refused)?;
        self.policy
            .registry()
            .ledger()
            .note_head(&appended)
            .map_err(refused)?;
        // The coordinated lease alone cannot exclude a raw WAL writer. Reserve
        // SQLite's writer slot again, verify the post-publication snapshot, and
        // retain it through acknowledgment. Drop rolls it back on every outcome.
        self.held
            .guard
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(refused)?;
        self.held.transaction_active = true;
        let current = self.observe(
            &observed.route.binding.agent,
            observed.route.binding.container,
        )?;
        check_checkpoint(
            observed,
            &current,
            &Anchor {
                seq: appended.seq,
                hash: appended.hash.clone(),
            },
        )?;
        Ok(appended)
    }
}

fn check_checkpoint(
    observed: &ActivationAuthority,
    current: &ActivationAuthority,
    expected_head: &Anchor,
) -> std::result::Result<(), Refusal> {
    if &current.head != expected_head
        || current.route != observed.route
        || current.policy != observed.policy
        || current.pilot != observed.pilot
    {
        return Err(refused(
            "activation authority or checkpoint changed across transaction boundary",
        ));
    }
    Ok(())
}
