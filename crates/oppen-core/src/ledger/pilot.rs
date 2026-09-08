//! One immutable operator-authorized pilot, accounted for by the event chain.
//! Executed notional and outstanding allocation are deliberately distinct.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oppen_hl::{Address, Network, wire::Cloid};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::submission::{PilotSubmission, replay_for_pilot};
use super::{
    Anchor, Appended, Event, EventKind, Ledger, LedgerError, NewEvent, NewFill,
    SubmissionResolution,
};
use crate::guardrail::{AgentId, Clearance, ClearedKind, PilotMetric};

#[path = "pilot/authority.rs"]
mod authority;
pub use authority::LegacyPilotReview;

type Result<T> = std::result::Result<T, PilotError>;

#[derive(Debug, thiserror::Error)]
pub enum PilotError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("pilot accounting unavailable: {detail}")]
    Unavailable { detail: String },
    #[error("pilot {metric:?} budget exhausted: {observed_usd} USD, limit {limit_usd} USD")]
    Exhausted {
        metric: PilotMetric,
        observed_usd: Decimal,
        limit_usd: Decimal,
    },
}

impl From<rusqlite::Error> for PilotError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Ledger(error.into())
    }
}
impl From<serde_json::Error> for PilotError {
    fn from(error: serde_json::Error) -> Self {
        unavailable(error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum PilotStop {
    AwaitingReconciliation,
    Exhausted {
        metric: PilotMetric,
        #[serde(with = "rust_decimal::serde::str")]
        observed_usd: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        limit_usd: Decimal,
    },
    Unavailable {
        detail: String,
    },
}

impl PilotStop {
    fn error(&self) -> PilotError {
        match self {
            Self::AwaitingReconciliation => {
                unavailable("account fills await durable order reconciliation")
            }
            Self::Exhausted {
                metric,
                observed_usd,
                limit_usd,
            } => PilotError::Exhausted {
                metric: *metric,
                observed_usd: *observed_usd,
                limit_usd: *limit_usd,
            },
            Self::Unavailable { detail } => unavailable(detail.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PilotState {
    pub agent: AgentId,
    pub account: Address,
    pub authorized_at_ms: u64,
    pub baseline: Anchor,
    #[serde(with = "rust_decimal::serde::str")]
    pub executed_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub reserved_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub net_realized_pnl_usd: Decimal,
    pub halt: Option<PilotStop>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PilotStatus {
    pub agent: AgentId,
    pub account: Address,
    pub halt: Option<PilotStop>,
    pub authentication: PilotAuthentication,
    #[serde(flatten)]
    pub accounting: PilotAccounting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PilotAuthentication {
    Unverified,
    LegacyReviewRequired,
    Verified,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "accounting", rename_all = "snake_case")]
pub enum PilotAccounting {
    Known {
        #[serde(with = "rust_decimal::serde::str")]
        executed_usd: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        reserved_usd: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        net_realized_pnl_usd: Decimal,
    },
    Unavailable {
        detail: String,
    },
}

/// Operator capability. Never expose this constructor through an agent view.
#[derive(Clone, Debug)]
pub struct PilotJournal(Arc<super::RegistryJournal>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authorized {
    version: u64,
    network: Network,
    agent: AgentId,
    account: Address,
    baseline_at_ms: u64,
    baseline: Anchor,
    #[serde(with = "rust_decimal::serde::str")]
    order_limit_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    executed_limit_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    realized_loss_limit_usd: Decimal,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Halted {
    version: u64,
    account: Address,
    authorization_seq: u64,
    authorization_hash: String,
    trigger_seq: u64,
    trigger_hash: Option<String>,
    stop: PilotStop,
}

struct Authority {
    seq: u64,
    hash: String,
    data: Authorized,
    halt: Option<PilotStop>,
    event: Event,
}

struct History {
    authorities: Vec<Authority>,
    submissions: Vec<PilotSubmission>,
    fills: Vec<Event>,
    fill_keys: HashMap<u64, Option<String>>,
    adoption: Option<Event>,
}

fn unavailable(detail: impl Into<String>) -> PilotError {
    PilotError::Unavailable {
        detail: detail.into(),
    }
}
fn checked(value: Option<Decimal>) -> Result<Decimal> {
    value.ok_or_else(|| unavailable("pilot money arithmetic overflow"))
}
fn exhausted(metric: PilotMetric, observed_usd: Decimal, limit: Decimal) -> PilotStop {
    PilotStop::Exhausted {
        metric,
        observed_usd,
        limit_usd: limit,
    }
}

impl PilotJournal {
    pub fn new(registry: Arc<super::RegistryJournal>) -> Self {
        Self(registry)
    }

    /// Requires an operator-confirmed exclusive, flat, fully reconciled testnet
    /// account. This records that authority, not evidence of venue readiness.
    /// There is deliberately no renewal, replacement, or reset operation.
    /// A post-commit anchor error has an uncertain durable outcome. Retry the
    /// same identity and baseline timestamp to verify and publish that outcome.
    pub fn authorize(&self, agent: AgentId, account: Address, at_ms: u64) -> Result<PilotState> {
        authority::authorize(&self.0, agent, account, at_ms)
    }

    pub fn state(&self, account: Address) -> Result<Option<PilotState>> {
        let ledger = self.0.ledger();
        let mut guard = ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let history = history(ledger, &tx, true)?;
        authority::verify(&self.0, &tx, &history)?;
        history
            .authorities
            .iter()
            .find(|a| a.data.account == account)
            .map(|authority| project(&history, authority))
            .transpose()
    }

    pub fn status(&self, account: Address) -> Result<Option<PilotStatus>> {
        let ledger = self.0.ledger();
        let mut guard = ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let history = history(ledger, &tx, true)?;
        authority::verify(&self.0, &tx, &history)?;
        status_from_history(&history, account, PilotAuthentication::Verified)
    }

    pub fn review_legacy(&self, account: Address) -> Result<LegacyPilotReview> {
        authority::review(&self.0, account)
    }

    /// Authenticate exactly the reviewed legacy history without changing its
    /// baseline, usage, reservations or stops. A newer retry timestamp with the
    /// identical review is an idempotent publication retry, not fresh consent:
    /// the existing signed adoption and its original timestamp are retained.
    /// Post-commit anchor errors may leave that adoption durable; retry must
    /// verify and publish the current head before reporting success.
    pub fn adopt_legacy(&self, review: &LegacyPilotReview, at_ms: u64) -> Result<PilotState> {
        authority::adopt(&self.0, review, at_ms)
    }
}

pub(super) fn activation_state_in(
    registry: &super::RegistryJournal,
    connection: &Connection,
    route: &super::AuthorizedRoute,
) -> Result<PilotState> {
    let history = history(registry.ledger(), connection, true)?;
    authority::verify(registry, connection, &history)?;
    authority::verify_authorized_route(registry, connection, &history, route)?;
    let authority = history
        .authorities
        .iter()
        .find(|authority| {
            authority.data.agent == route.binding.agent
                && authority.data.account == route.binding.container
        })
        .ok_or_else(|| unavailable("matching authenticated pilot consent required"))?;
    let state = project(&history, authority)?;
    permitted(&state, Decimal::ZERO, &authority.data)?;
    if state.halt.is_some() {
        return Err(unavailable(
            "pilot reconciliation or stop blocks activation",
        ));
    }
    Ok(state)
}

pub(super) fn status(ledger: &Ledger, account: Address) -> Result<Option<PilotStatus>> {
    let mut guard = ledger.lock()?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let history = history(ledger, &tx, true)?;
    let authentication = if history
        .authorities
        .first()
        .is_some_and(|a| authority::is_signed(&a.event))
        || history.adoption.is_some()
    {
        PilotAuthentication::Unverified
    } else {
        PilotAuthentication::LegacyReviewRequired
    };
    status_from_history(&history, account, authentication)
}

fn status_from_history(
    history: &History,
    account: Address,
    authentication: PilotAuthentication,
) -> Result<Option<PilotStatus>> {
    let Some(authority) = history
        .authorities
        .iter()
        .find(|a| a.data.account == account)
    else {
        return Ok(None);
    };
    if history
        .fills
        .iter()
        .any(|fill| fill.seq > authority.seq && fill.payload.is_none())
    {
        return Err(unavailable("required pilot fill history redacted"));
    }
    let (halt, accounting) = match project(history, authority) {
        Ok(state) => (
            state.halt,
            PilotAccounting::Known {
                executed_usd: state.executed_usd,
                reserved_usd: state.reserved_usd,
                net_realized_pnl_usd: state.net_realized_pnl_usd,
            },
        ),
        Err(error) => (
            authority.halt.clone(),
            PilotAccounting::Unavailable {
                detail: error.to_string(),
            },
        ),
    };
    Ok(Some(PilotStatus {
        agent: authority.data.agent.clone(),
        account: authority.data.account,
        halt,
        authentication,
        accounting,
    }))
}

pub(super) fn check_before_sign(
    ledger: &Ledger,
    connection: &Connection,
    clearance: &Clearance,
) -> Result<()> {
    if !matches!(clearance.kind, ClearedKind::Order { .. }) {
        return Ok(());
    }
    let history = history(ledger, connection, true)?;
    let Some(authority) = history
        .authorities
        .iter()
        .find(|a| a.data.agent == clearance.agent)
    else {
        return Ok(());
    };
    identity(authority, authority.data.account, clearance)?;
    let state = project(&history, authority)?;
    permitted(&state, Decimal::ZERO, &authority.data)?;
    let value = serde_json::to_value(clearance)?;
    let matching = history
        .submissions
        .iter()
        .filter(|s| {
            s.seq > authority.seq
                && s.account == authority.data.account
                && s.agent == clearance.agent.as_str()
                && s.resolution.is_none()
        })
        .any(|s| without_reason(s.intent.clone()) == value);
    if !matching {
        return Err(unavailable(
            "active pilot requires a matching pending durable reservation",
        ));
    }
    order_limit(&value, &authority.data)?;
    Ok(())
}

pub(super) fn check_admission(
    ledger: &Ledger,
    connection: &Connection,
    account: Address,
    clearance: &Clearance,
) -> Result<()> {
    let history = history(ledger, connection, true)?;
    let Some(authority) = history
        .authorities
        .iter()
        .find(|a| a.data.account == account || a.data.agent == clearance.agent)
    else {
        return Ok(());
    };
    identity(authority, account, clearance)?;
    let state = project(&history, authority)?;
    let amount = order_limit(&serde_json::to_value(clearance)?, &authority.data)?;
    permitted(&state, amount, &authority.data)
}

fn authenticate_order(
    registry: &super::RegistryJournal,
    connection: &Connection,
    account: Address,
    clearance: &Clearance,
    required: bool,
) -> Result<()> {
    let history = history(registry.ledger(), connection, true)?;
    authority::verify(registry, connection, &history)?;
    let applicable = history
        .authorities
        .iter()
        .any(|a| a.data.agent == clearance.agent || a.data.account == account);
    if !applicable {
        return if required {
            Err(unavailable("authenticated pilot consent required"))
        } else {
            Ok(())
        };
    }
    authority::verify_route(registry, connection, &history, clearance)
}

pub(super) fn check_authenticated_admission(
    registry: &super::RegistryJournal,
    connection: &Connection,
    account: Address,
    clearance: &Clearance,
    required: bool,
) -> Result<()> {
    authenticate_order(registry, connection, account, clearance, required)?;
    check_admission(registry.ledger(), connection, account, clearance)
}

pub(super) fn check_authenticated_before_sign(
    registry: &super::RegistryJournal,
    connection: &Connection,
    clearance: &Clearance,
    required: bool,
) -> Result<()> {
    if !matches!(clearance.kind, ClearedKind::Order { .. }) {
        return Ok(());
    }
    authenticate_order(
        registry,
        connection,
        clearance.route.binding.container,
        clearance,
        required,
    )?;
    check_before_sign(registry.ledger(), connection, clearance)
}

/// Caller verifies the anchored chain before appending the fill. Its write
/// transaction now contains that fill, so do not compare the extended head
/// against the old anchor here. Publish the returned LAST head after commit.
pub(super) fn latch_in_tx(
    ledger: &Ledger,
    tx: &Transaction<'_>,
    incoming: &NewFill<'_>,
    at_ms: i64,
) -> Result<Option<Appended>> {
    if !has_authority(tx)? {
        return Ok(None);
    }
    let history = match history(ledger, tx, false) {
        Ok(history) => history,
        // The problematic row remains durable and every pilot read refuses it.
        Err(
            PilotError::Unavailable { .. } | PilotError::Ledger(LedgerError::PayloadNotJson { .. }),
        ) => return Ok(None),
        Err(error) => return Err(error),
    };
    let (trigger_seq, trigger_hash): (u64, String) = tx.query_row(
        "SELECT seq, hash FROM events WHERE idem_key = ?1",
        params![super::fill_idem_key(incoming.account, incoming.tid)],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut last = None;
    for authority in &history.authorities {
        if authority.halt.is_some() {
            continue;
        }
        let stop =
            match duplicate_conflict(tx, &history, authority, incoming).and_then(|conflict| {
                match conflict {
                    Some(stop) => Ok(Some(stop)),
                    None => project(&history, authority).map(|s| s.halt),
                }
            }) {
                Ok(stop) => stop,
                Err(PilotError::Unavailable { detail }) => Some(PilotStop::Unavailable { detail }),
                Err(error) => return Err(error),
            };
        let Some(stop) = stop else {
            continue;
        };
        if matches!(stop, PilotStop::AwaitingReconciliation) {
            continue;
        }
        let payload = serde_json::to_value(Halted {
            version: 1,
            account: authority.data.account,
            authorization_seq: authority.seq,
            authorization_hash: authority.hash.clone(),
            trigger_seq,
            trigger_hash: Some(trigger_hash.clone()),
            stop,
        })?;
        last = Some(
            super::append_keyed_in_tx(
                tx,
                &NewEvent {
                    kind: EventKind::PilotHalted,
                    ts_ms: at_ms,
                    agent_id: Some(authority.data.agent.as_str()),
                    payload: &payload,
                    snapshot: None,
                },
                &format!("pilot_halted:{}", authority.seq),
            )?
            .ok_or_else(|| unavailable("pilot halt key without halt record"))?,
        );
    }
    Ok(last)
}

fn has_authority(connection: &Connection) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE kind IN ('pilot_authorized', 'pilot_halted'))",
        [],
        |row| row.get(0),
    )?)
}

fn history(ledger: &Ledger, connection: &Connection, verify_anchor: bool) -> Result<History> {
    let submissions = replay_for_pilot(ledger, connection, verify_anchor)
        .map_err(|error| unavailable(format!("submission history: {error}")))?;
    let mut out = History {
        authorities: Vec::new(),
        submissions,
        fills: Vec::new(),
        fill_keys: HashMap::new(),
        adoption: None,
    };
    let mut statement = connection.prepare(&format!(
        "SELECT {}, idem_key FROM events WHERE kind IN ('pilot_authorized', 'pilot_adopted', 'pilot_halted', 'fill') ORDER BY seq",
        super::SELECT_EVENT_COLUMNS))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let event = super::event_from_row(row)?;
        let key: Option<String> = row.get(12)?;
        if event.kind == EventKind::Fill {
            if let Some(stop) = event.payload.as_ref().and_then(|p| p.get("pilot_stop")) {
                let halted = parse_halt(stop)?;
                validate_halt(&mut out, &event, None, halted, true)?;
            }
            out.fill_keys.insert(event.seq, key);
            out.fills.push(event);
            continue;
        }
        let payload = event
            .payload
            .clone()
            .ok_or_else(|| unavailable("pilot authority or halt redacted"))?;
        match event.kind {
            EventKind::PilotAuthorized => {
                let payload =
                    authority::authorization_payload(ledger, row.get(4)?, &event, payload)?;
                for field in [
                    "order_limit_usd",
                    "executed_limit_usd",
                    "realized_loss_limit_usd",
                ] {
                    money(&payload[field])?;
                }
                let data: Authorized = serde_json::from_value(payload)?;
                if data.version != 1
                    || !out.authorities.is_empty()
                    || data.order_limit_usd != Decimal::from(15)
                    || data.executed_limit_usd != Decimal::from(150)
                    || data.realized_loss_limit_usd != Decimal::from(5)
                    || data.network != Network::Testnet
                    || data.network != ledger.network
                    || event.agent_id.as_deref() != Some(data.agent.as_str())
                    || crate::keys::checked_agent_id(&data.agent).is_err()
                    || data.account == Address::ZERO
                    || i64::try_from(data.baseline_at_ms).ok() != Some(event.ts_ms)
                    || data.baseline.seq.checked_add(1) != Some(event.seq)
                    || key.as_deref() != Some(format!("pilot_authorized:{}", data.account).as_str())
                {
                    return Err(unavailable(
                        "invalid, duplicate, or wrong-network pilot authorization",
                    ));
                }
                let baseline_hash = if data.baseline.seq == 0 {
                    ledger.genesis.clone()
                } else {
                    connection.query_row(
                        "SELECT hash FROM events WHERE seq = ?1",
                        params![data.baseline.seq],
                        |row| row.get::<_, String>(0),
                    )?
                };
                if data.baseline.hash != baseline_hash {
                    return Err(unavailable("pilot baseline head mismatch"));
                }
                out.authorities.push(Authority {
                    seq: event.seq,
                    hash: event.hash.clone(),
                    data,
                    halt: None,
                    event,
                });
            }
            EventKind::PilotAdopted => {
                if out.adoption.is_some() {
                    return Err(unavailable("duplicate pilot adoption"));
                }
                authority::validate_adoption(ledger, row.get(4)?, &event, &out, key.as_deref())?;
                out.adoption = Some(event);
            }
            EventKind::PilotHalted => {
                let halted = parse_halt(&payload)?;
                validate_halt(&mut out, &event, key.as_deref(), halted, false)?;
            }
            _ => return Err(unavailable("unexpected pilot lifecycle kind")),
        }
    }
    Ok(out)
}

fn parse_halt(payload: &Value) -> Result<Halted> {
    if payload.get("trigger_hash").is_none() {
        return Err(unavailable("missing pilot trigger hash field"));
    }
    if payload["stop"]["reason"] == "exhausted" {
        money(&payload["stop"]["observed_usd"])?;
        money(&payload["stop"]["limit_usd"])?;
    }
    Ok(serde_json::from_value(payload.clone())?)
}

fn validate_halt(
    out: &mut History,
    event: &Event,
    key: Option<&str>,
    halted: Halted,
    embedded: bool,
) -> Result<()> {
    let authority = out
        .authorities
        .iter_mut()
        .find(|a| a.seq == halted.authorization_seq)
        .ok_or_else(|| unavailable("pilot halt without earlier authorization"))?;
    if halted.version != 1
        || halted.account != authority.data.account
        || halted.authorization_hash != authority.hash
        || authority.halt.is_some()
        || (!embedded
            && (event.agent_id.as_deref() != Some(authority.data.agent.as_str())
                || key != Some(format!("pilot_halted:{}", authority.seq).as_str())))
    {
        return Err(unavailable("pilot halt linkage mismatch"));
    }
    let trigger = if embedded {
        event
    } else {
        out.fills
            .iter()
            .find(|fill| fill.seq == halted.trigger_seq)
            .ok_or_else(|| unavailable("pilot halt has no earlier trigger fill"))?
    };
    if (embedded && (halted.trigger_hash.is_some() || halted.trigger_seq != event.seq))
        || (!embedded && halted.trigger_hash.as_deref() != Some(trigger.hash.as_str()))
        || trigger.seq <= authority.seq
    {
        return Err(unavailable("pilot halt trigger hash mismatch"));
    }
    if let Some(account) = trigger.payload.as_ref().and_then(|p| p.get("account")) {
        if serde_json::from_value::<Address>(account.clone())? != halted.account {
            return Err(unavailable("pilot halt trigger account mismatch"));
        }
    } else if matches!(halted.stop, PilotStop::Exhausted { .. }) {
        return Err(unavailable("pilot threshold trigger account missing"));
    }
    match &halted.stop {
        PilotStop::AwaitingReconciliation => {
            return Err(unavailable(
                "transient reconciliation state cannot be durably halted",
            ));
        }
        PilotStop::Exhausted {
            metric,
            observed_usd,
            limit_usd,
        } => {
            let limit = match metric {
                PilotMetric::ExecutedNotional => authority.data.executed_limit_usd,
                PilotMetric::RealizedLoss => authority.data.realized_loss_limit_usd,
                _ => return Err(unavailable("non-latching pilot metric")),
            };
            if *limit_usd != limit || *observed_usd < limit {
                return Err(unavailable("invalid pilot halt threshold"));
            }
        }
        PilotStop::Unavailable { detail } if detail.is_empty() => {
            return Err(unavailable("empty pilot halt reason"));
        }
        PilotStop::Unavailable { .. } => {}
    }
    authority.halt = Some(halted.stop);
    Ok(())
}

fn initial(data: &Authorized) -> PilotState {
    PilotState {
        agent: data.agent.clone(),
        account: data.account,
        authorized_at_ms: data.baseline_at_ms,
        baseline: data.baseline.clone(),
        executed_usd: Decimal::ZERO,
        reserved_usd: Decimal::ZERO,
        net_realized_pnl_usd: Decimal::ZERO,
        halt: None,
    }
}

fn check_baseline(history: &History, account: Address, agent: &AgentId) -> Result<()> {
    for submission in &history.submissions {
        if submission.account != account && submission.agent != agent.as_str() {
            continue;
        }
        match submission.resolution {
            None => return Err(unavailable("pilot baseline has a pending submission")),
            Some(SubmissionResolution::NotSent { .. } | SubmissionResolution::Rejected { .. }) => {
                continue;
            }
            Some(SubmissionResolution::Observed { oid, .. }) => {
                let terms = terms(&submission.intent)?;
                let mut quantity = Decimal::ZERO;
                let mut tids = HashSet::new();
                for event in &history.fills {
                    let value = event
                        .payload
                        .as_ref()
                        .ok_or_else(|| unavailable("baseline fill redacted"))?;
                    let fill_account: Address = serde_json::from_value(value["account"].clone())?;
                    if fill_account != submission.account {
                        continue;
                    }
                    if value["cloid"].as_str() != Some(submission.cloid.as_str())
                        && value["oid"].as_u64() != Some(oid)
                    {
                        continue;
                    }
                    let fill = fill(event, value)?;
                    if fill.oid != oid
                        || fill.symbol != terms.symbol
                        || fill.is_buy != terms.is_buy
                        || fill.cloid.as_ref().is_some_and(|c| c != &submission.cloid)
                        || fill.seq <= submission.seq
                    {
                        return Err(unavailable("contradictory baseline fill"));
                    }
                    if tids.insert(fill.tid) {
                        quantity = checked(quantity.checked_add(fill.sz))?;
                    }
                }
                if quantity != terms.sz {
                    return Err(unavailable(
                        "prior observed order retains unknown fill liability",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn duplicate_conflict(
    connection: &Connection,
    history: &History,
    authority: &Authority,
    incoming: &NewFill<'_>,
) -> Result<Option<PilotStop>> {
    let account = Address::parse(incoming.account).ok();
    let payload_account =
        serde_json::from_value::<Address>(incoming.payload["account"].clone()).ok();
    if account != Some(authority.data.account) && payload_account != Some(authority.data.account) {
        return Ok(None);
    }
    let mut statement = connection.prepare(&format!(
        "SELECT {} FROM events WHERE idem_key = ?1",
        super::SELECT_EVENT_COLUMNS
    ))?;
    let mut rows = statement.query(params![super::fill_idem_key(
        incoming.account,
        incoming.tid
    )])?;
    let row = rows
        .next()?
        .ok_or_else(|| unavailable("fill idempotence row missing after insertion"))?;
    let stored = super::event_from_row(row)?;
    let old_payload = stored
        .payload
        .as_ref()
        .ok_or_else(|| unavailable("cannot compare redacted duplicate fill"))?;
    let mut offered = stored.clone();
    offered.ts_ms = incoming.ts_ms;
    let old = fill(&stored, old_payload);
    let mut new = fill(&offered, incoming.payload);
    let conflicting_link = new.as_ref().is_ok_and(|fill| history.submissions.iter().any(|s| {
        s.account == authority.data.account
            && matches!(s.resolution, Some(SubmissionResolution::Observed { oid, .. }) if oid == fill.oid)
            && fill.cloid.as_ref().is_some_and(|cloid| cloid != &s.cloid)
    }));
    if let (Ok(old), Ok(new)) = (&old, &mut new) {
        let verified_enrichment = history.submissions.iter().any(|submission| {
            submission.account == authority.data.account
                && submission.agent == authority.data.agent.as_str()
                && submission.seq > authority.seq
                && submission.seq < stored.seq
                && matches!(submission.resolution,
                    Some(SubmissionResolution::Observed { oid, .. }) if oid == new.oid)
                && new.cloid.as_ref() == Some(&submission.cloid)
        });
        // An absent field may not erase existing identity. New identity may
        // be ignored as duplicate metadata only after its OID binding is proven.
        if new.cloid.is_none() || (old.cloid.is_none() && verified_enrichment) {
            new.cloid = old.cloid.clone();
        }
    }
    let identical = account == payload_account
        && !conflicting_link
        && old_payload["account"] == incoming.payload["account"]
        && matches!((&old, &new), (Ok(old), Ok(new)) if old == new && new.tid == incoming.tid);
    if identical {
        return Ok(None);
    }
    Ok(Some(PilotStop::Unavailable {
        detail: format!(
            "conflicting or malformed account fill tid {}: {}",
            incoming.tid, incoming.payload
        ),
    }))
}

fn identity(authority: &Authority, account: Address, clearance: &Clearance) -> Result<()> {
    if clearance.network != Network::Testnet
        || clearance.agent != authority.data.agent
        || account != authority.data.account
        || clearance.vault_address.is_some_and(|a| a != account)
    {
        return Err(unavailable(
            "pilot network, account, or agent does not match authorization",
        ));
    }
    Ok(())
}

fn without_reason(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("reason");
    }
    value
}

fn money(value: &Value) -> Result<Decimal> {
    let text = value
        .as_str()
        .ok_or_else(|| unavailable("pilot money must be a decimal string"))?;
    if text.is_empty()
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
    {
        return Err(unavailable("malformed pilot decimal"));
    }
    Decimal::from_str_exact(text)
        .map_err(|_| unavailable("malformed or over-precise pilot decimal"))
}

struct Terms {
    sz: Decimal,
    valuation: Decimal,
    symbol: String,
    is_buy: bool,
}
fn terms(intent: &Value) -> Result<Terms> {
    let kind = &intent["kind"];
    if kind["cleared"] != "order" {
        return Err(unavailable("pilot reservation has no order intent"));
    }
    let px = money(&kind["px"])?;
    let reference = money(&kind["reference_px"])?;
    let sz = money(&kind["sz"])?;
    if px <= Decimal::ZERO || reference <= Decimal::ZERO || sz <= Decimal::ZERO {
        return Err(unavailable("pilot order values must be positive"));
    }
    Ok(Terms {
        sz,
        valuation: px.max(reference),
        symbol: kind["symbol"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| unavailable("missing pilot symbol"))?
            .into(),
        is_buy: kind["is_buy"]
            .as_bool()
            .ok_or_else(|| unavailable("missing pilot order side"))?,
    })
}
fn order_limit(value: &Value, authority: &Authorized) -> Result<Decimal> {
    let terms = terms(value)?;
    let amount = checked(terms.sz.checked_mul(terms.valuation))?;
    if amount > authority.order_limit_usd {
        return Err(exhausted(
            PilotMetric::OrderNotional,
            amount,
            authority.order_limit_usd,
        )
        .error());
    }
    Ok(amount)
}
fn permitted(state: &PilotState, added: Decimal, authority: &Authorized) -> Result<()> {
    if let Some(stop) = &state.halt {
        return Err(stop.error());
    }
    let committed =
        checked(checked(state.executed_usd.checked_add(state.reserved_usd))?.checked_add(added))?;
    if committed > authority.executed_limit_usd {
        return Err(exhausted(
            PilotMetric::CommittedNotional,
            committed,
            authority.executed_limit_usd,
        )
        .error());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fill {
    seq: u64,
    ts_ms: u64,
    tid: u64,
    oid: u64,
    cloid: Option<Cloid>,
    px: Decimal,
    sz: Decimal,
    net: Decimal,
    closed_pnl: Decimal,
    fee: Decimal,
    symbol: String,
    is_buy: bool,
}
fn fill(event: &Event, value: &Value) -> Result<Fill> {
    let ts_ms = value["ts_ms"]
        .as_u64()
        .ok_or_else(|| unavailable("missing fill timestamp"))?;
    if i64::try_from(ts_ms).ok() != Some(event.ts_ms) {
        return Err(unavailable("fill timestamp mismatch"));
    }
    if value["fee_token"] != "USDC" {
        return Err(unavailable("pilot fees require USDC"));
    }
    let px = money(&value["px"])?;
    let sz = money(&value["sz"])?;
    if px <= Decimal::ZERO || sz <= Decimal::ZERO {
        return Err(unavailable("fill price and quantity must be positive"));
    }
    let cloid = match value.get("cloid") {
        Some(Value::Null) => None,
        Some(value) => Some(serde_json::from_value(value.clone())?),
        None => return Err(unavailable("missing fill cloid field")),
    };
    let closed_pnl = money(&value["closed_pnl"])?;
    let fee = money(&value["fee"])?;
    Ok(Fill {
        seq: event.seq,
        ts_ms,
        tid: value["tid"]
            .as_u64()
            .ok_or_else(|| unavailable("missing fill trade id"))?,
        oid: value["oid"]
            .as_u64()
            .ok_or_else(|| unavailable("missing fill order id"))?,
        cloid,
        px,
        sz,
        net: checked(closed_pnl.checked_sub(fee))?,
        closed_pnl,
        fee,
        symbol: value["coin"]
            .as_str()
            .ok_or_else(|| unavailable("missing fill symbol"))?
            .into(),
        is_buy: match value["side"].as_str() {
            Some("buy") => true,
            Some("sell") => false,
            _ => return Err(unavailable("invalid fill side")),
        },
    })
}

fn project(history: &History, authority: &Authority) -> Result<PilotState> {
    let mut state = initial(&authority.data);
    state.halt = authority.halt.clone();
    let mut allocations = Vec::new();
    for submission in &history.submissions {
        if submission.seq <= authority.seq {
            continue;
        }
        if submission.account != state.account && submission.agent != state.agent.as_str() {
            continue;
        }
        if submission.account != state.account || submission.agent != state.agent.as_str() {
            return Err(unavailable("pilot submission changed account or agent"));
        }
        let terms = terms(&submission.intent)?;
        allocations.push((submission, terms, Decimal::ZERO));
    }
    let mut fills = Vec::new();
    let mut seen: HashMap<u64, Fill> = HashMap::new();
    for event in &history.fills {
        if event.seq <= authority.seq {
            continue;
        }
        let value = event
            .payload
            .as_ref()
            .ok_or_else(|| unavailable("post-baseline fill payload redacted"))?;
        let account: Address = serde_json::from_value(
            value
                .get("account")
                .cloned()
                .ok_or_else(|| unavailable("fill account unknown"))?,
        )?;
        if account != state.account {
            continue;
        }
        let parsed = fill(event, value)?;
        if history
            .fill_keys
            .get(&event.seq)
            .and_then(|key| key.as_deref())
            != Some(super::fill_idem_key(&account.to_string(), parsed.tid).as_str())
        {
            return Err(unavailable("pilot fill idempotence key mismatch"));
        }
        if parsed.ts_ms < state.authorized_at_ms {
            return Err(unavailable("late pre-baseline account fill"));
        }
        if let Some(previous) = seen.get(&parsed.tid) {
            let mut duplicate = parsed.clone();
            duplicate.seq = previous.seq;
            if *previous != duplicate {
                return Err(unavailable("conflicting duplicate account fill"));
            }
            continue;
        }
        seen.insert(parsed.tid, parsed.clone());
        fills.push(parsed);
    }
    fills.sort_by_key(|f| (f.ts_ms, f.tid));
    let mut unmatched = false;
    for fill in fills {
        state.executed_usd = checked(
            state
                .executed_usd
                .checked_add(checked(fill.px.checked_mul(fill.sz))?),
        )?;
        state.net_realized_pnl_usd = checked(state.net_realized_pnl_usd.checked_add(fill.net))?;
        if state.halt.is_none() {
            if state.executed_usd >= authority.data.executed_limit_usd {
                state.halt = Some(exhausted(
                    PilotMetric::ExecutedNotional,
                    state.executed_usd,
                    authority.data.executed_limit_usd,
                ));
            } else if state.net_realized_pnl_usd <= -authority.data.realized_loss_limit_usd {
                state.halt = Some(exhausted(
                    PilotMetric::RealizedLoss,
                    -state.net_realized_pnl_usd,
                    authority.data.realized_loss_limit_usd,
                ));
            }
        }
        let candidates: Vec<_> = allocations
            .iter()
            .enumerate()
            .filter(|(_, (s, _, _))| {
                fill.cloid.as_ref() == Some(&s.cloid)
                    || matches!(s.resolution,
                Some(SubmissionResolution::Observed { oid, .. }) if oid == fill.oid)
            })
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            unmatched = true;
            continue;
        }
        if candidates.len() != 1 {
            return Err(unavailable("ambiguous fill submission linkage"));
        }
        let (submission, terms, quantity) = &mut allocations[candidates[0]];
        if fill.seq <= submission.seq
            || fill.cloid.as_ref().is_some_and(|c| c != &submission.cloid)
            || fill.symbol != terms.symbol
            || fill.is_buy != terms.is_buy
            || matches!(
                submission.resolution,
                Some(SubmissionResolution::NotSent { .. } | SubmissionResolution::Rejected { .. })
            )
            || matches!(submission.resolution, Some(SubmissionResolution::Observed { oid, .. }) if oid != fill.oid)
        {
            return Err(unavailable("fill contradicts its durable reservation"));
        }
        *quantity = checked(quantity.checked_add(fill.sz))?;
        if *quantity > terms.sz {
            return Err(unavailable("fills exceed reserved quantity"));
        }
    }
    for (submission, terms, quantity) in allocations {
        if matches!(
            submission.resolution,
            Some(SubmissionResolution::NotSent { .. } | SubmissionResolution::Rejected { .. })
        ) {
            continue;
        }
        let remaining = checked(terms.sz.checked_sub(quantity))?;
        state.reserved_usd = checked(
            state
                .reserved_usd
                .checked_add(checked(remaining.checked_mul(terms.valuation))?),
        )?;
    }
    if unmatched && state.halt.is_none() {
        state.halt = Some(PilotStop::AwaitingReconciliation);
    }
    Ok(state)
}

#[cfg(test)]
#[path = "pilot_tests.rs"]
mod tests;
