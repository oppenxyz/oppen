//! ES39 initial consent. Existing signed rows retain their original meaning.
use super::*;
use crate::feed::{FeedSession, FeedStamp};
use crate::guardrail::{ActivationEvidence, AgentGuardrails, Refusal};
use crate::ledger::{PolicyJournal, PolicyVersion};
use crate::state::{AccountState, VenueReadings};
use oppen_hl::types::{ExtraAgent, UserRole};

pub use crate::reconcile::FillWalkReceipt as PilotConsentCoverage;

/// Operator statements, not proof of lifetime account history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PilotConsentAttestation {
    pub dedicated_exclusive_account: bool,
    pub never_used_for_trading: bool,
}

#[derive(Debug)]
pub struct PilotConsentEvidence {
    pub account: ActivationEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PilotConsentCorrelation {
    pub route: AuthorizedRoute,
    pub baseline: Anchor,
    pub baseline_at_ms: u64,
}

#[derive(Debug)]
pub struct PilotConsentObservation {
    owner: Arc<RegistryJournal>,
    feed: Arc<FeedSession>,
    stamp: FeedStamp,
    correlation: PilotConsentCorrelation,
    policy: PolicyVersion,
    coverage: PilotConsentCoverage,
}
impl PilotConsentObservation {
    pub fn route(&self) -> &AuthorizedRoute {
        &self.correlation.route
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PilotConsentDisplay {
    pub network: Network,
    pub correlation: PilotConsentCorrelation,
    pub policy_revision: u64,
    pub policy: AgentGuardrails,
    pub persisted_kill: crate::guardrail::KillSwitch,
    pub account: AccountState,
    pub wallet_approval: ExtraAgent,
    pub coverage: PilotConsentCoverage,
    pub required_attestations: PilotConsentAttestation,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub order_limit_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub gross_exposure_limit_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub executed_limit_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_loss_limit_usd: Decimal,
    pub max_leverage: u32,
}

#[derive(Debug)]
pub struct PilotConsentReview {
    observation: PilotConsentObservation,
    material: serde_json::Value,
    display: PilotConsentDisplay,
}
impl PilotConsentReview {
    pub fn display(&self) -> &PilotConsentDisplay {
        &self.display
    }
}

/// Correlated consent, not durable proof that an older row used the ES39 UI.
#[derive(Debug, Clone, Serialize)]
pub struct PilotConsentReceipt {
    pub correlation: PilotConsentCorrelation,
    pub seq: u64,
    pub hash: String,
}

#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PilotConsentError {
    #[error("pilot consent refused: {detail}")]
    Refused { detail: String },
    #[error("pilot consent outcome uncertain: {detail}")]
    Uncertain {
        correlation: Box<PilotConsentCorrelation>,
        detail: String,
    },
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PilotConsentOutcome {
    Committed {
        receipt: Box<PilotConsentReceipt>,
        current_pilot: PilotState,
    },
    /// Native retained-work terminality is required before claiming NotCommitted.
    Absent,
    Unknown {
        detail: String,
    },
}

fn refused(e: impl ToString) -> PilotConsentError {
    PilotConsentError::Refused {
        detail: e.to_string(),
    }
}

impl PilotJournal {
    /// Capture before account reads; synchronous existing-authority I/O only.
    pub fn begin_authorization_review(
        &self,
        agent: &AgentId,
        account: Address,
        feed: Arc<FeedSession>,
        clock: &dyn Fn() -> u64,
    ) -> Result<PilotConsentObservation> {
        let mut held = self.0.ledger().lock()?;
        let tx = held.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (route, policy, baseline) = observe(&self.0, &tx, agent, account)?;
        let stamp = feed.stamp();
        let admission = feed
            .admit(Some(&stamp), Network::Testnet, account)
            .map_err(|e| unavailable(e.to_string()))?;
        let now = clock();
        at(now)?;
        let coverage = admission.initial_fill_walk().cloned().ok_or_else(|| {
            unavailable("same monitored owner has no completed initial fill walk")
        })?;
        if !admission.uses_ledger(self.0.ledger())
            || !Arc::ptr_eq(&coverage.ledger_identity, self.0.ledger().identity())
        {
            return Err(unavailable(
                "consent feed and fill walk belong to another ledger owner",
            ));
        }
        drop(admission);
        Ok(PilotConsentObservation {
            owner: self.0.clone(),
            feed,
            stamp,
            correlation: PilotConsentCorrelation {
                route,
                baseline,
                baseline_at_ms: now,
            },
            policy,
            coverage,
        })
    }

    pub fn review_authorization(
        &self,
        observation: PilotConsentObservation,
        evidence: PilotConsentEvidence,
        clock: &dyn Fn() -> u64,
    ) -> Result<PilotConsentReview> {
        check_owner(self, &observation)?;
        let mut held = self.0.ledger().lock()?;
        let tx = held.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check(&self.0, &tx, &observation)?;
        let guard = observation
            .feed
            .admit(
                Some(&observation.stamp),
                Network::Testnet,
                observation.route().binding.container,
            )
            .map_err(|e| unavailable(e.to_string()))?;
        let now = clock();
        let (account, wallet_approval) =
            validate(&observation, &evidence, now, guard.last_tick_ms())?;
        let policy =
            observation.policy.state.guardrails[&observation.route().binding.agent].clone();
        let display = PilotConsentDisplay {
            network: Network::Testnet,
            correlation: observation.correlation.clone(),
            policy_revision: observation.policy.revision,
            persisted_kill: observation.policy.state.kill.clone(),
            gross_exposure_limit_usd: policy.risk.max_open_exposure_usd.unwrap_or(Decimal::ZERO),
            max_leverage: policy.risk.max_leverage,
            policy,
            account,
            wallet_approval,
            coverage: observation.coverage.clone(),
            required_attestations: PilotConsentAttestation {
                dedicated_exclusive_account: true,
                never_used_for_trading: true,
            },
            observed_at_ms: now,
            expires_at_ms: observation
                .correlation
                .baseline_at_ms
                .checked_add(60_000)
                .ok_or_else(|| unavailable("consent review deadline overflow"))?,
            order_limit_usd: Decimal::from(15),
            executed_limit_usd: Decimal::from(150),
            realized_loss_limit_usd: Decimal::from(5),
        };
        let material = material(&evidence)?;
        drop(guard);
        Ok(PilotConsentReview {
            observation,
            material,
            display,
        })
    }

    /// The caller retains this synchronous work through observer loss.
    pub fn confirm_authorization(
        &self,
        review: PilotConsentReview,
        fresh: PilotConsentEvidence,
        attestation: PilotConsentAttestation,
        clock: &dyn Fn() -> u64,
        final_authorize: &dyn Fn() -> std::result::Result<(), Refusal>,
    ) -> std::result::Result<PilotConsentReceipt, PilotConsentError> {
        if attestation != review.display.required_attestations {
            return Err(refused(
                "dedicated, exclusive and never-used account confirmation required",
            ));
        }
        let observation = &review.observation;
        check_owner(self, observation).map_err(refused)?;
        let mut held = self.0.ledger().lock().map_err(refused)?;
        let tx = held
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(refused)?;
        check(&self.0, &tx, observation).map_err(refused)?;
        let guard = observation
            .feed
            .admit(
                Some(&observation.stamp),
                Network::Testnet,
                observation.route().binding.container,
            )
            .map_err(refused)?;
        if material(&fresh).map_err(refused)? != review.material {
            return Err(refused("material consent account evidence changed"));
        }
        final_authorize().map_err(refused)?;
        validate(observation, &fresh, clock(), guard.last_tick_ms()).map_err(refused)?;
        let data = Authorized {
            version: 1,
            network: Network::Testnet,
            agent: observation.route().binding.agent.clone(),
            account: observation.route().binding.container,
            baseline_at_ms: observation.correlation.baseline_at_ms,
            baseline: observation.correlation.baseline.clone(),
            order_limit_usd: Decimal::from(15),
            executed_limit_usd: Decimal::from(150),
            realized_loss_limit_usd: Decimal::from(5),
        };
        let appended = append_in_tx(
            &self.0,
            &tx,
            observation.route().clone(),
            Operation::PilotAuthorized {
                authorization: data,
            },
            observation.correlation.baseline_at_ms,
            &format!("pilot_authorized:{}", observation.route().binding.container),
        )
        .map_err(refused)?;
        let uncertain = |e: String| PilotConsentError::Uncertain {
            correlation: Box::new(observation.correlation.clone()),
            detail: e,
        };
        tx.commit().map_err(|e| uncertain(e.to_string()))?;
        drop(guard);
        self.0
            .ledger()
            .note_head(&appended)
            .map_err(|e| uncertain(e.to_string()))?;
        // Revalidate a fresh write snapshot after publication; never retain a
        // pre-publication read snapshot across an uncoordinated SQLite writer.
        let tx = held
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| uncertain(e.to_string()))?;
        let history = history(self.0.ledger(), &tx, true).map_err(|e| uncertain(e.to_string()))?;
        verify(&self.0, &tx, &history).map_err(|e| uncertain(e.to_string()))?;
        let (seq, hash) = crate::ledger::head(&tx).map_err(|e| uncertain(e.to_string()))?;
        if seq != appended.seq || hash != appended.hash {
            return Err(uncertain("consent head changed during publication".into()));
        }
        let guard = observation
            .feed
            .admit(
                Some(&observation.stamp),
                Network::Testnet,
                observation.route().binding.container,
            )
            .map_err(|e| uncertain(e.to_string()))?;
        final_authorize().map_err(|e| uncertain(e.to_string()))?;
        validate(observation, &fresh, clock(), guard.last_tick_ms())
            .map_err(|e| uncertain(e.to_string()))?;
        Ok(PilotConsentReceipt {
            correlation: observation.correlation.clone(),
            seq,
            hash,
        })
    }

    /// Read-only exact row correlation, never anchor repair or authorization retry.
    pub fn authorization_outcome(
        &self,
        correlation: &PilotConsentCorrelation,
    ) -> Result<PilotConsentOutcome> {
        let mut held = self.0.ledger().lock()?;
        let tx = held.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let history = history(self.0.ledger(), &tx, true)?;
        verify(&self.0, &tx, &history)?;
        let (seq, hash) = crate::ledger::head(&tx)?;
        if self
            .0
            .ledger()
            .anchor
            .as_ref()
            .ok_or_else(|| unavailable("consent anchor missing"))?
            .load()?
            != Some(Anchor { seq, hash })
        {
            return Ok(PilotConsentOutcome::Unknown {
                detail: "consent head publication unconfirmed".into(),
            });
        }
        let Some(authority) = history.authorities.first() else {
            return Ok(PilotConsentOutcome::Absent);
        };
        if !is_signed(&authority.event) {
            return Ok(PilotConsentOutcome::Unknown {
                detail: "existing legacy consent is not this operation".into(),
            });
        }
        let signed = read(self.0.ledger(), &tx, &authority.event)?;
        if signed.envelope.route != correlation.route
            || authority.data.baseline != correlation.baseline
            || authority.data.baseline_at_ms != correlation.baseline_at_ms
        {
            return Ok(PilotConsentOutcome::Unknown {
                detail: "existing consent differs from operation".into(),
            });
        }
        Ok(PilotConsentOutcome::Committed {
            receipt: Box::new(PilotConsentReceipt {
                correlation: correlation.clone(),
                seq: authority.seq,
                hash: authority.hash.clone(),
            }),
            current_pilot: project(&history, authority)?,
        })
    }
}

fn check_owner(journal: &PilotJournal, observation: &PilotConsentObservation) -> Result<()> {
    if !Arc::ptr_eq(&journal.0, &observation.owner) {
        return Err(unavailable("consent review belongs to another owner"));
    }
    Ok(())
}

fn observe(
    registry: &Arc<RegistryJournal>,
    connection: &Connection,
    agent: &AgentId,
    account: Address,
) -> Result<(AuthorizedRoute, PolicyVersion, Anchor)> {
    let history = history(registry.ledger(), connection, true)?;
    verify(registry, connection, &history)?;
    if !history.authorities.is_empty() {
        return Err(unavailable(
            "existing consent is inspect-only; no fresh baseline",
        ));
    }
    let route = current_route(registry, connection, agent, account)?;
    strict_baseline(&history, agent, account)?;
    strict_order_evidence(connection, &history, agent, account)?;
    let policy = PolicyJournal::new(registry.clone())
        .current_in(connection)
        .map_err(|e| unavailable(e.to_string()))?;
    let config = policy
        .state
        .guardrails
        .get(agent)
        .ok_or_else(|| unavailable("consent policy agent missing"))?;
    if policy.state.kill.blocking(agent).is_none()
        || config.max_order_usd > Decimal::from(15)
        || config.risk.max_leverage > 1
        || config
            .risk
            .max_open_exposure_usd
            .is_none_or(|v| v > Decimal::from(25))
    {
        return Err(unavailable(
            "consent requires paused policy with supervised hard caps",
        ));
    }
    let (seq, hash) = crate::ledger::head(connection)?;
    Ok((route, policy, Anchor { seq, hash }))
}

fn check(
    registry: &Arc<RegistryJournal>,
    connection: &Connection,
    observation: &PilotConsentObservation,
) -> Result<()> {
    let (route, policy, head) = observe(
        registry,
        connection,
        &observation.route().binding.agent,
        observation.route().binding.container,
    )?;
    if route != observation.correlation.route
        || policy != observation.policy
        || head != observation.correlation.baseline
    {
        return Err(unavailable(
            "reviewed consent route, policy or checkpoint changed",
        ));
    }
    Ok(())
}

fn strict_baseline(history: &History, agent: &AgentId, account: Address) -> Result<()> {
    for submission in &history.submissions {
        if submission.account != account && submission.agent != agent.as_str() {
            continue;
        }
        if !matches!(
            submission.resolution,
            Some(SubmissionResolution::NotSent { .. } | SubmissionResolution::Rejected { .. })
        ) {
            return Err(unavailable(
                "prior execution or unresolved submission requires preservation",
            ));
        }
    }
    for event in &history.fills {
        let payload = event
            .payload
            .as_ref()
            .ok_or_else(|| unavailable("redacted fill prevents zero-history confirmation"))?;
        let fill_account: Address = serde_json::from_value(payload["account"].clone())
            .map_err(|_| unavailable("unscoped fill prevents zero-history confirmation"))?;
        if fill_account == Address::ZERO
            || fill_account == account
            || event.agent_id.as_deref() == Some(agent.as_str())
        {
            return Err(unavailable(
                "known account execution cannot receive a fresh baseline",
            ));
        }
    }
    Ok(())
}

fn strict_order_evidence(
    connection: &Connection,
    history: &History,
    agent: &AgentId,
    account: Address,
) -> Result<()> {
    let mut statement = connection.prepare(&format!(
        "SELECT {} FROM events WHERE kind IN ('order_intent', 'order_state_change') ORDER BY seq",
        crate::ledger::SELECT_EVENT_COLUMNS
    ))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let event = crate::ledger::event_from_row(row)?;
        let payload = event.payload.as_ref().ok_or_else(|| {
            unavailable("redacted order evidence prevents unused-account confirmation")
        })?;
        let account_value = payload
            .get("account")
            .or_else(|| payload.pointer("/route/binding/container"));
        let recorded_account =
            account_value.and_then(|v| serde_json::from_value::<Address>(v.clone()).ok());
        if recorded_account.is_some_and(|a| a != Address::ZERO && a != account)
            && event.agent_id.as_deref() != Some(agent.as_str())
        {
            continue;
        }
        // A verified linked submission with a definite not-sent/rejected
        // resolution was already checked above; orphan intents are ambiguous.
        if event.kind == EventKind::OrderIntent
            && history.submissions.iter().any(|s| &s.intent == payload)
        {
            continue;
        }
        return Err(unavailable(
            "prior or unscoped order evidence requires preservation",
        ));
    }
    Ok(())
}

fn material(evidence: &PilotConsentEvidence) -> Result<serde_json::Value> {
    let positions: Vec<_> = evidence.account.perps.asset_positions.iter().map(|held| {
        let p = &held.position;
        serde_json::json!({ "coin":p.coin, "size":p.szi, "entry":p.entry_px,
            "leverage": {"kind":p.leverage.kind,"value":p.leverage.value,"raw_usd":p.leverage.raw_usd},
            "margin_used":p.margin_used })
    }).collect();
    let spot: Vec<_> = evidence
        .account
        .spot
        .balances
        .iter()
        .map(|b| serde_json::json!({"coin":b.coin,"token":b.token,"total":b.total,"hold":b.hold}))
        .collect();
    Ok(
        serde_json::json!({ "account_role":evidence.account.account_role,
        "signer_role":evidence.account.signer_role, "approvals":evidence.account.extra_agents,
        "positions":positions, "spot":spot,
        "perps_raw_usd":evidence.account.perps.margin_summary.total_raw_usd }),
    )
}

fn validate(
    observation: &PilotConsentObservation,
    evidence: &PilotConsentEvidence,
    now: u64,
    tick: Option<u64>,
) -> Result<(AccountState, ExtraAgent)> {
    let route = observation.route();
    let config = &observation.policy.state.guardrails[&route.binding.agent];
    let e = &evidence.account;
    let baseline = observation.correlation.baseline_at_ms;
    if now < baseline
        || baseline
            .checked_add(60_000)
            .is_none_or(|expiry| now >= expiry)
        || e.read_started_at_ms < baseline
        || e.read_started_at_ms > e.read_completed_at_ms
        || e.read_completed_at_ms > now
        || now.saturating_sub(e.read_started_at_ms) >= config.freshness.max_account_age_ms
        || e.perps.time > now
        || now.saturating_sub(e.perps.time) > config.freshness.max_account_age_ms
        || !tick.is_some_and(|t| t <= now && now - t <= config.freshness.max_account_age_ms)
        || observation.coverage.local_read_started_at_ms < 0
        || observation.coverage.local_read_completed_at_ms
            < observation.coverage.local_read_started_at_ms
        || observation.coverage.local_read_completed_at_ms as u64 > now
    {
        return Err(unavailable(
            "consent evidence, coverage or feed is stale or clock inconsistent",
        ));
    }
    if now < route.binding.wallet.approved_at_ms || now >= route.binding.wallet.valid_until_ms {
        return Err(unavailable("consent wallet approval window invalid"));
    }
    let user = match (&e.account_role, route.binding.vault_address) {
        (UserRole::User, None) => route.binding.container,
        (UserRole::SubAccount { master }, Some(vault))
            if vault == route.binding.container && *master != Address::ZERO && *master != vault =>
        {
            *master
        }
        _ => return Err(unavailable("account role does not prove consent route")),
    };
    if e.signer_role != (UserRole::Agent { user }) {
        return Err(unavailable("signer role does not prove consent route"));
    }
    let mut addresses = HashSet::new();
    if e.extra_agents
        .iter()
        .any(|a| !addresses.insert(*a.address.as_bytes()))
    {
        return Err(unavailable("duplicate agent approvals"));
    }
    let approval = e
        .extra_agents
        .iter()
        .find(|a| a.address == route.binding.wallet.address && a.valid_until > now)
        .ok_or_else(|| unavailable("signer approval missing or expired"))?;
    if !e.orders.is_empty()
        || e.perps.asset_positions.iter().any(|p| {
            !p.position.szi.is_zero()
                || !p.position.margin_used.is_zero()
                || !p.position.position_value.is_zero()
        })
    {
        return Err(unavailable(
            "initial consent requires flat positions and no resting orders",
        ));
    }
    let mut symbols = HashSet::new();
    let mode = match config.risk.margin_mode {
        crate::guardrail::MarginMode::Cross => "cross",
        crate::guardrail::MarginMode::Isolated => "isolated",
    };
    if e.perps.asset_positions.iter().any(|held| {
        let p = &held.position;
        !symbols.insert(&p.coin)
            || p.leverage.value != 1
            || p.leverage.kind != mode
            || !p.unrealized_pnl.is_zero()
            || !p.cum_funding.all_time.is_zero()
            || !p.cum_funding.since_open.is_zero()
            || !p.cum_funding.since_change.is_zero()
    }) {
        return Err(unavailable(
            "position metadata contradicts unused 1x account evidence",
        ));
    }
    let mut coins = HashSet::new();
    if e.spot
        .balances
        .iter()
        .any(|b| !coins.insert(&b.coin) || b.total < Decimal::ZERO || !b.hold.is_zero())
    {
        return Err(unavailable("invalid or reserved spot balances"));
    }
    let account = crate::state::assemble(
        Network::Testnet,
        route.binding.container,
        e.read_started_at_ms,
        &VenueReadings {
            perps: &e.perps,
            spot: &e.spot,
            orders: &e.orders,
            mids: &e.reference_prices,
            last_tick_ms: tick,
        },
    );
    if account.balances.equity_usd <= Decimal::ZERO
        || !account.balances.total_margin_used_usd.is_zero()
    {
        return Err(unavailable(
            "initial consent requires funded, unused margin",
        ));
    }
    Ok((account, approval.clone()))
}
