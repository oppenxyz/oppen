//! Operator-only review capability. No authority is deserialized from the UI.

use super::*;
use crate::feed::FeedStamp;
use crate::ledger::PilotState;
use crate::ledger::activation::{ActivationAuthority, refused};
use crate::state::{AccountState, VenueReadings};
use oppen_hl::types::{
    ClearinghouseState, ExtraAgent, OpenOrder, ReferencePrices, SpotClearinghouseState, UserRole,
};

/// Raw responses gathered by the retained native owner after `begin_activation_review`.
/// These fields are not an IPC input schema or a claim that reconciliation succeeded.
#[derive(Debug)]
pub struct ActivationEvidence {
    pub read_started_at_ms: u64,
    pub read_completed_at_ms: u64,
    pub perps: ClearinghouseState,
    pub spot: SpotClearinghouseState,
    pub orders: Vec<OpenOrder>,
    pub reference_prices: ReferencePrices,
    pub account_role: UserRole,
    pub signer_role: UserRole,
    pub extra_agents: Vec<ExtraAgent>,
}

#[derive(Debug)]
pub struct ActivationObservation {
    owner: Arc<()>,
    authority: ActivationAuthority,
    generation: u64,
    feed_stamp: FeedStamp,
}

impl ActivationObservation {
    pub fn route(&self) -> &AuthorizedRoute {
        &self.authority.route
    }
    pub fn feed_stamp(&self) -> &FeedStamp {
        &self.feed_stamp
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivationDisplay {
    pub route: AuthorizedRoute,
    pub policy_revision: u64,
    pub stop_generation: u64,
    pub policy: AgentGuardrails,
    pub pilot: PilotState,
    pub account: AccountState,
    pub wallet_approval: ExtraAgent,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub gross_exposure_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub remaining_committed_usd: Decimal,
}

/// One engine-specific review; consuming it cannot acknowledge a second time.
#[derive(Debug)]
pub struct ActivationReview {
    observation: ActivationObservation,
    evidence: ActivationEvidence,
    display: ActivationDisplay,
}

impl ActivationReview {
    pub fn display(&self) -> &ActivationDisplay {
        &self.display
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivationReceipt {
    pub route: AuthorizedRoute,
    pub policy_revision: u64,
    pub stop_generation: u64,
    pub acknowledged_at_ms: u64,
    pub audit_seq: u64,
    pub audit_hash: String,
}

// Declared before the ledger permit so unwinding releases all guards before
// clearing admission. A committed request is not an acknowledgment receipt.
struct InhibitOnFailure<'a> {
    engine: &'a GuardrailEngine,
    complete: bool,
}
impl Drop for InhibitOnFailure<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.engine.state().inhibit();
        }
    }
}

impl GuardrailEngine {
    fn activation_authority(&self) -> Result<&PolicyJournal, Refusal> {
        if !self.supervised_alpha || self.network != Network::Testnet {
            return Err(refused(
                "activation requires an existing supervised testnet engine",
            ));
        }
        self.activation_authority
            .as_deref()
            .ok_or_else(|| refused("activation has no authenticated authority"))
    }

    /// Synchronous ledger I/O. Capture this capability before any account reads.
    /// Beginning another review inhibits admission and invalidates the previous one.
    pub fn begin_activation_review(
        &self,
        agent: &AgentId,
        account: Address,
    ) -> Result<ActivationObservation, Refusal> {
        let _mutation = self.mutation_lock().map_err(refused)?;
        self.state().inhibit();
        let authority = self.activation_authority()?;
        let permit = authority.activation_permit()?;
        let observed = permit.observe(agent, account)?;
        let mut state = self.state();
        state.publish(observed.policy.clone()).map_err(refused)?;
        check_local(&state, &observed, state.stop_generation)?;
        let feed_stamp = self.feed.stamp();
        drop(self.feed.admit(Some(&feed_stamp), self.network, account)?);
        Ok(ActivationObservation {
            owner: self.submission_owner.clone(),
            authority: observed,
            generation: state.stop_generation,
            feed_stamp,
        })
    }

    /// Synchronous verification only; the caller gathered the raw responses without locks.
    pub fn review_activation(
        &self,
        observation: ActivationObservation,
        evidence: ActivationEvidence,
        clock: &dyn Fn() -> u64,
    ) -> Result<ActivationReview, Refusal> {
        let _failure = InhibitOnFailure {
            engine: self,
            complete: false,
        };
        self.check_activation_owner(&observation)?;
        let permit = self.activation_authority()?.activation_permit()?;
        let current = permit.observe(
            &observation.route().binding.agent,
            observation.route().binding.container,
        )?;
        check_history(&observation.authority, &current)?;
        let state = self.state();
        check_local(&state, &current, observation.generation)?;
        let feed = self.feed.admit(
            Some(&observation.feed_stamp),
            self.network,
            current.route.binding.container,
        )?;
        let now_ms = clock();
        let mut display = validate_evidence(&observation, &evidence, now_ms, feed.last_tick_ms())?;
        display.expires_at_ms = now_ms
            .checked_add(60_000)
            .ok_or_else(|| refused("human review deadline overflow"))?
            .min(observation.route().binding.wallet.valid_until_ms)
            .min(display.wallet_approval.valid_until);
        drop(feed);
        drop(state);
        // A successful review leaves admission inhibited without changing its generation.
        let mut failure = _failure;
        failure.complete = true;
        Ok(ActivationReview {
            observation,
            evidence,
            display,
        })
    }

    /// Retain this synchronous operation independently of its IPC observer. The
    /// callback checks pinned pairing/runtime authority, without I/O or ledger locks.
    pub fn confirm_activation(
        &self,
        review: ActivationReview,
        evidence: ActivationEvidence,
        clock: &dyn Fn() -> u64,
        final_authorize: &dyn Fn() -> Result<(), Refusal>,
    ) -> Result<ActivationReceipt, Refusal> {
        let mut failure = InhibitOnFailure {
            engine: self,
            complete: false,
        };
        let _mutation = self.mutation_lock().map_err(refused)?;
        let observation = &review.observation;
        self.check_activation_owner(observation)?;
        check_material(&review.evidence, &evidence)?;
        let mut permit = self.activation_authority()?.activation_permit()?;
        let current = permit.observe(
            &observation.route().binding.agent,
            observation.route().binding.container,
        )?;
        check_history(&observation.authority, &current)?;
        let at_ms = {
            let state = self.state();
            check_local(&state, &current, observation.generation)?;
            let feed = self.feed.admit(
                Some(&observation.feed_stamp),
                self.network,
                current.route.binding.container,
            )?;
            final_authorize()?;
            let now_ms = clock();
            check_review_deadline(&review, &evidence, now_ms)?;
            validate_evidence(observation, &evidence, now_ms, feed.last_tick_ms())?;
            now_ms
        };
        let audit = permit
            .record_request(&current, observation.generation, at_ms)
            .map_err(|error| Unevaluable::AuditWriteFailed {
                detail: error.to_string(),
            })?;
        // No other coordinated ledger writer can pass during audit publication.
        // Ingress and a local emergency stop CAN change, so both are checked again.
        let mut state = self.state();
        check_local(&state, &current, observation.generation)?;
        let feed = self.feed.admit(
            Some(&observation.feed_stamp),
            self.network,
            current.route.binding.container,
        )?;
        final_authorize()?;
        let now_ms = clock();
        check_review_deadline(&review, &evidence, now_ms)?;
        validate_evidence(observation, &evidence, now_ms, feed.last_tick_ms())?;
        state.activation_scope = Some(current.route.clone());
        state.acknowledged = Some(PolicyAcknowledgment {
            revision: current.policy.revision,
            stop_generation: observation.generation,
        });
        failure.complete = true;
        drop(feed);
        drop(state);
        Ok(ActivationReceipt {
            route: current.route,
            policy_revision: current.policy.revision,
            stop_generation: observation.generation,
            acknowledged_at_ms: now_ms,
            audit_seq: audit.seq,
            audit_hash: audit.hash,
        })
    }

    fn check_activation_owner(&self, observation: &ActivationObservation) -> Result<(), Refusal> {
        if !Arc::ptr_eq(&self.submission_owner, &observation.owner) {
            return Err(refused("review belongs to another engine"));
        }
        Ok(())
    }
}

fn check_review_deadline(
    review: &ActivationReview,
    evidence: &ActivationEvidence,
    now_ms: u64,
) -> Result<(), Refusal> {
    if now_ms < review.display.observed_at_ms
        || now_ms >= review.display.expires_at_ms
        || evidence.read_started_at_ms < review.display.observed_at_ms
    {
        return Err(refused(
            "human review expired or confirmation evidence predates review",
        ));
    }
    Ok(())
}

fn check_material(initial: &ActivationEvidence, fresh: &ActivationEvidence) -> Result<(), Refusal> {
    let positions = |e: &ActivationEvidence| {
        e.perps
            .asset_positions
            .iter()
            .map(|held| {
                let p = &held.position;
                (
                    p.coin.clone(),
                    (p.szi, p.entry_px, p.leverage.clone(), held.kind.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let orders = |e: &ActivationEvidence| {
        e.orders
            .iter()
            .map(|order| (order.oid, order.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let approvals = |e: &ActivationEvidence| {
        e.extra_agents
            .iter()
            .map(|agent| (*agent.address.as_bytes(), agent.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let balances = |e: &ActivationEvidence| {
        e.spot
            .balances
            .iter()
            .map(|balance| (balance.token, balance.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let collateral = |e: &ActivationEvidence| -> Result<Decimal, Refusal> {
        let pnl = e
            .perps
            .asset_positions
            .iter()
            .try_fold(Decimal::ZERO, |sum, held| {
                sum.checked_add(held.position.unrealized_pnl)
                    .ok_or_else(|| refused("PnL overflow"))
            })?;
        e.perps
            .margin_summary
            .account_value
            .checked_sub(pnl)
            .ok_or_else(|| refused("collateral overflow"))
    };
    if positions(initial) != positions(fresh)
        || orders(initial) != orders(fresh)
        || balances(initial) != balances(fresh)
        || collateral(initial)? != collateral(fresh)?
        || initial.account_role != fresh.account_role
        || initial.signer_role != fresh.signer_role
        || approvals(initial) != approvals(fresh)
    {
        return Err(refused(
            "material account or signer approval changed; review again",
        ));
    }
    Ok(())
}

fn check_history(
    reviewed: &ActivationAuthority,
    current: &ActivationAuthority,
) -> Result<(), Refusal> {
    if reviewed.head != current.head
        || reviewed.route != current.route
        || reviewed.policy != current.policy
        || reviewed.pilot != current.pilot
    {
        return Err(refused("reviewed authority or accounting history changed"));
    }
    Ok(())
}

fn check_local(
    state: &EngineState,
    current: &ActivationAuthority,
    generation: u64,
) -> Result<(), Refusal> {
    if generation == u64::MAX
        || state.stop_generation != generation
        || state.policy_revision != current.policy.revision
        || state.acknowledged.is_some()
    {
        return Err(refused(
            "policy, local stop or admission changed since review",
        ));
    }
    if let Some((scope, engagement)) = state
        .effective_kill()
        .blocking(&current.route.binding.agent)
    {
        return Err(Refusal::TradingPaused {
            scope,
            since_ms: engagement.engaged_at_ms,
            reason: engagement.reason.clone(),
        });
    }
    Ok(())
}

fn validate_evidence(
    observation: &ActivationObservation,
    evidence: &ActivationEvidence,
    now_ms: u64,
    last_tick_ms: Option<u64>,
) -> Result<ActivationDisplay, Refusal> {
    let authority = &observation.authority;
    let route = &authority.route;
    let config = authority
        .policy
        .state
        .guardrails
        .get(&route.binding.agent)
        .ok_or_else(|| refused("policy agent missing"))?;
    config
        .validate()
        .map_err(|(field, detail)| refused(format!("{field}: {detail}")))?;
    // Activation never changes policy to make it acceptable. These durable caps
    // remain enforced on every subsequent order, including after this review expires.
    if config.max_order_usd > Decimal::from(15)
        || config.risk.max_leverage > 1
        || config
            .risk
            .max_open_exposure_usd
            .is_none_or(|cap| cap > Decimal::from(25))
    {
        return Err(refused("policy exceeds supervised alpha hard caps"));
    }
    let max_age = config
        .freshness
        .max_account_age_ms
        .min(config.freshness.max_market_age_ms);
    if !last_tick_ms
        .is_some_and(|tick| tick <= now_ms && now_ms - tick <= config.freshness.max_account_age_ms)
    {
        return Err(refused(
            "account feed tick is missing, stale or in the future",
        ));
    }
    let expires_at_ms = evidence
        .read_started_at_ms
        .checked_add(max_age)
        .ok_or_else(|| refused("review timestamp overflow"))?
        .min(route.binding.wallet.valid_until_ms);
    if evidence.read_started_at_ms > evidence.read_completed_at_ms
        || evidence.read_completed_at_ms > now_ms
        || now_ms >= expires_at_ms
        || evidence.perps.time > now_ms
        || now_ms.saturating_sub(evidence.perps.time) > config.freshness.max_account_age_ms
    {
        return Err(refused(
            "venue evidence is stale or its clock is inconsistent",
        ));
    }
    check_order_approval_window(route, now_ms)?;
    let approval_user = match (&evidence.account_role, route.binding.vault_address) {
        (UserRole::User, None) => route.binding.container,
        (UserRole::SubAccount { master }, Some(vault))
            if vault == route.binding.container && *master != Address::ZERO && *master != vault =>
        {
            *master
        }
        _ => return Err(refused("venue account role does not prove this route")),
    };
    if evidence.signer_role
        != (UserRole::Agent {
            user: approval_user,
        })
    {
        return Err(refused("venue signer role does not authorize this account"));
    }
    let mut addresses = BTreeSet::new();
    if evidence
        .extra_agents
        .iter()
        .any(|agent| !addresses.insert(*agent.address.as_bytes()))
    {
        return Err(refused("duplicate venue agent approval"));
    }
    let wallet_approval = evidence
        .extra_agents
        .iter()
        .find(|agent| agent.address == route.binding.wallet.address)
        .filter(|agent| agent.valid_until > now_ms)
        .ok_or_else(|| refused("current venue wallet approval missing or expired"))?;
    let mut symbols = BTreeSet::new();
    let mut marked_positions = Decimal::ZERO;
    for held in &evidence.perps.asset_positions {
        let p = &held.position;
        if !symbols.insert(p.coin.clone()) {
            return Err(refused("duplicate venue position"));
        }
        let expected_mode = match config.risk.margin_mode {
            crate::guardrail::MarginMode::Cross => "cross",
            crate::guardrail::MarginMode::Isolated => "isolated",
        };
        if p.leverage.value != 1
            || p.leverage.kind != expected_mode
            || p.margin_used < Decimal::ZERO
        {
            return Err(refused(
                "actual venue leverage or margin mode is unsuitable",
            ));
        }
        if !p.szi.is_zero() {
            let mark = evidence
                .reference_prices
                .get(&p.coin)
                .filter(|px| *px > Decimal::ZERO)
                .ok_or_else(|| refused("position has no valid reference price"))?;
            let notional = p
                .szi
                .abs()
                .checked_mul(mark)
                .ok_or_else(|| refused("position valuation overflow"))?;
            marked_positions = marked_positions
                .checked_add(notional.max(p.position_value.abs()))
                .ok_or_else(|| refused("position valuation overflow"))?;
        }
    }
    let mut oids = BTreeSet::new();
    for order in &evidence.orders {
        if !oids.insert(order.oid) || order.sz < Decimal::ZERO {
            return Err(refused("invalid or duplicate venue order"));
        }
        if !order.reduce_only && !order.sz.is_zero() && !symbols.contains(&order.coin) {
            return Err(refused(
                "resting opening order lacks actual leverage observation",
            ));
        }
    }
    let mut coins = BTreeSet::new();
    if evidence.spot.balances.iter().any(|balance| {
        !coins.insert(&balance.coin)
            || balance.total < Decimal::ZERO
            || balance.hold < Decimal::ZERO
            || balance.hold > balance.total
    }) {
        return Err(refused("invalid or duplicate spot balance"));
    }
    let account = crate::state::assemble(
        route.network,
        route.binding.container,
        evidence.read_started_at_ms,
        &VenueReadings {
            perps: &evidence.perps,
            spot: &evidence.spot,
            orders: &evidence.orders,
            mids: &evidence.reference_prices,
            last_tick_ms,
        },
    );
    // Only use the shared projection for exposure valuation here. Daily PnL is
    // not cumulative pilot accounting and is never substituted for it.
    let exposure = crate::state::exposure_from(
        &account,
        Decimal::ZERO,
        None,
        true,
        now_ms / 86_400_000 * 86_400_000,
    );
    let resting = exposure
        .agent
        .resting
        .ok_or_else(|| refused("resting exposure is unpriced"))?;
    let gross = marked_positions
        .checked_add(resting.notional_usd)
        .ok_or_else(|| refused("gross valuation overflow"))?;
    let cap = config.risk.max_open_exposure_usd.unwrap_or(Decimal::ZERO);
    if gross > cap
        || account.balances.equity_usd <= Decimal::ZERO
        || gross > account.balances.equity_usd
        || account.balances.total_margin_used_usd < Decimal::ZERO
        || account.balances.total_margin_used_usd > account.balances.equity_usd
    {
        return Err(refused(
            "account exposure or margin exceeds activation limits",
        ));
    }
    let remaining = Decimal::from(150)
        .checked_sub(authority.pilot.executed_usd)
        .and_then(|v| v.checked_sub(authority.pilot.reserved_usd))
        .filter(|v| *v > Decimal::ZERO)
        .ok_or_else(|| refused("pilot has no committed capacity"))?;
    Ok(ActivationDisplay {
        route: route.clone(),
        policy_revision: authority.policy.revision,
        stop_generation: observation.generation,
        policy: config.clone(),
        pilot: authority.pilot.clone(),
        account,
        wallet_approval: wallet_approval.clone(),
        observed_at_ms: now_ms,
        expires_at_ms: expires_at_ms.min(wallet_approval.valid_until),
        gross_exposure_usd: gross,
        remaining_committed_usd: remaining,
    })
}

#[cfg(test)]
#[path = "activation_tests.rs"]
mod tests;
