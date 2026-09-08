//! Authenticated consent; fill accounting remains keyless and deny-only.

use super::*;
use crate::ledger::{AuthorizedRoute, RegistryJournal};

#[derive(Clone, Debug, Serialize)]
pub struct LegacyPilotReview {
    authorization_seq: u64,
    authorization_hash: String,
    head: Anchor,
    state: PilotState,
    route: AuthorizedRoute,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    envelope: Envelope,
    mac: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u64,
    network: Network,
    seq: u64,
    prev_hash: String,
    at_ms: u64,
    route: AuthorizedRoute,
    route_hash: String,
    operation: Operation,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    PilotAuthorized {
        authorization: Authorized,
    },
    PilotAdopted {
        authorization_seq: u64,
        authorization_hash: String,
        reviewed_head: Anchor,
        reviewed_state: PilotState,
    },
}

impl Operation {
    fn kind(&self) -> EventKind {
        match self {
            Self::PilotAuthorized { .. } => EventKind::PilotAuthorized,
            Self::PilotAdopted { .. } => EventKind::PilotAdopted,
        }
    }
}

fn message(envelope: &Envelope) -> Result<String> {
    Ok(super::super::hash::canonical_json(&serde_json::json!({
        "domain": "oppen.pilot-consent.v1", "envelope": envelope,
    }))?)
}

fn valid_identity(agent: &AgentId, account: Address) -> Result<()> {
    crate::keys::checked_agent_id(agent).map_err(|error| unavailable(error.to_string()))?;
    if account == Address::ZERO {
        return Err(unavailable("pilot account must be nonzero"));
    }
    Ok(())
}

fn at(at_ms: u64) -> Result<i64> {
    i64::try_from(at_ms).map_err(|_| unavailable("pilot timestamp out of range"))
}

pub(super) fn is_signed(event: &Event) -> bool {
    event
        .payload
        .as_ref()
        .is_some_and(|p| p.get("envelope").is_some())
}

fn parse(ledger: &Ledger, raw: Option<String>, event: &Event) -> Result<Signed> {
    let payload = event
        .payload
        .as_ref()
        .ok_or_else(|| unavailable("pilot consent redacted"))?;
    if raw.as_deref() != Some(super::super::hash::canonical_json(payload)?.as_str()) {
        return Err(unavailable(
            "pilot consent duplicate keys or noncanonical JSON",
        ));
    }
    let signed: Signed = serde_json::from_value(payload.clone())?;
    if serde_json::to_value(&signed)? != *payload {
        return Err(unavailable(
            "pilot consent missing, unknown, or noncanonical fields",
        ));
    }
    let e = &signed.envelope;
    if e.version != 2
        || e.network != Network::Testnet
        || e.network != ledger.network
        || e.seq != event.seq
        || e.prev_hash != event.prev_hash
        || at(e.at_ms)? != event.ts_ms
        || e.operation.kind() != event.kind
        || e.route.network != e.network
        || event.agent_id.as_deref() != Some(e.route.binding.agent.as_str())
        || event.snapshot_id.is_some()
        || event.snapshot_hash.is_some()
        || signed.mac.len() != 64
        || !signed
            .mac
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(unavailable("pilot consent envelope does not match its row"));
    }
    valid_identity(&e.route.binding.agent, e.route.binding.container)?;
    if e.route
        .binding
        .vault_address
        .is_some_and(|vault| vault != e.route.binding.container)
    {
        return Err(unavailable("pilot consent vault differs from container"));
    }
    Ok(signed)
}

fn read(ledger: &Ledger, connection: &Connection, event: &Event) -> Result<Signed> {
    let raw = connection.query_row(
        "SELECT payload FROM events WHERE seq = ?1",
        params![event.seq],
        |row| row.get(0),
    )?;
    let signed = parse(ledger, raw, event)?;
    let route_hash: String = connection.query_row(
        "SELECT hash FROM events WHERE seq = ?1 AND kind = 'registry_granted'",
        params![signed.envelope.route.binding_seq],
        |row| row.get(0),
    )?;
    if route_hash != signed.envelope.route_hash || signed.envelope.route.binding_seq >= event.seq {
        return Err(unavailable("pilot registry linkage mismatch"));
    }
    Ok(signed)
}

pub(super) fn authorization_payload(
    ledger: &Ledger,
    raw: Option<String>,
    event: &Event,
    payload: Value,
) -> Result<Value> {
    if !is_signed(event) {
        if raw.as_deref() != Some(super::super::hash::canonical_json(&payload)?.as_str()) {
            return Err(unavailable(
                "legacy pilot duplicate keys or noncanonical JSON",
            ));
        }
        let data: Authorized = serde_json::from_value(payload.clone())?;
        if serde_json::to_value(data)? != payload {
            return Err(unavailable("legacy pilot fields missing or unknown"));
        }
        return Ok(payload);
    }
    let signed = parse(ledger, raw, event)?;
    match signed.envelope.operation {
        Operation::PilotAuthorized { authorization } => {
            if signed.envelope.route.binding.agent != authorization.agent
                || signed.envelope.route.binding.container != authorization.account
                || signed.envelope.at_ms != authorization.baseline_at_ms
            {
                return Err(unavailable("pilot consent identity or baseline mismatch"));
            }
            Ok(serde_json::to_value(authorization)?)
        }
        _ => Err(unavailable("pilot authorization operation mismatch")),
    }
}

pub(super) fn validate_adoption(
    ledger: &Ledger,
    raw: Option<String>,
    event: &Event,
    history: &History,
    key: Option<&str>,
) -> Result<()> {
    let authority = history
        .authorities
        .first()
        .ok_or_else(|| unavailable("pilot adoption without legacy authorization"))?;
    if is_signed(&authority.event) {
        return Err(unavailable("signed pilot cannot be adopted again"));
    }
    let signed = parse(ledger, raw, event)?;
    let Operation::PilotAdopted {
        authorization_seq,
        authorization_hash,
        reviewed_head,
        reviewed_state,
    } = &signed.envelope.operation
    else {
        return Err(unavailable("pilot adoption operation mismatch"));
    };
    if *authorization_seq != authority.seq
        || *authorization_hash != authority.hash
        || reviewed_head.seq.checked_add(1) != Some(event.seq)
        || reviewed_head.hash != event.prev_hash
        || reviewed_state.agent != authority.data.agent
        || reviewed_state.account != authority.data.account
        || reviewed_state.baseline != authority.data.baseline
        || reviewed_state.authorized_at_ms != authority.data.baseline_at_ms
        || signed.envelope.route.binding.agent != authority.data.agent
        || signed.envelope.route.binding.container != authority.data.account
        || key != Some(format!("pilot_adopted:{}", authority.seq).as_str())
    {
        return Err(unavailable(
            "pilot adoption does not bind original authority and reviewed history",
        ));
    }
    Ok(())
}

pub(super) fn verify(
    registry: &RegistryJournal,
    connection: &Connection,
    history: &History,
) -> Result<()> {
    let Some(authority) = history.authorities.first() else {
        return Ok(());
    };
    let event = if is_signed(&authority.event) {
        &authority.event
    } else {
        history.adoption.as_ref().ok_or_else(|| {
            unavailable("legacy pilot consent requires explicit review and adoption")
        })?
    };
    let signed = read(registry.ledger(), connection, event)?;
    let mac = hex::decode(&signed.mac).map_err(|_| unavailable("invalid pilot consent MAC"))?;
    if !registry
        .authority_key()
        .verify(message(&signed.envelope)?.as_bytes(), &mac)
    {
        return Err(unavailable("pilot consent MAC mismatch"));
    }
    Ok(())
}

pub(super) fn verify_route(
    registry: &RegistryJournal,
    connection: &Connection,
    history: &History,
    clearance: &Clearance,
) -> Result<()> {
    let Some(authority) = history.authorities.first() else {
        return Ok(());
    };
    let event = history.adoption.as_ref().unwrap_or(&authority.event);
    let signed = read(registry.ledger(), connection, event)?;
    if signed.envelope.route != clearance.route {
        return Err(unavailable("pilot consent signing route changed"));
    }
    registry
        .verify_route_in(
            connection,
            &clearance.route,
            clearance.route.binding.wallet.address,
        )
        .map_err(|error| unavailable(error.to_string()))
}

fn current_route(
    registry: &RegistryJournal,
    connection: &Connection,
    agent: &AgentId,
    account: Address,
) -> Result<AuthorizedRoute> {
    valid_identity(agent, account)?;
    if registry.ledger().network != Network::Testnet {
        return Err(unavailable("pilot consent requires testnet"));
    }
    let route = registry
        .route_in(connection, agent)
        .map_err(|error| unavailable(error.to_string()))?;
    if route.binding.container != account {
        return Err(unavailable(
            "pilot account differs from authenticated registry route",
        ));
    }
    Ok(route)
}

fn append(
    registry: &RegistryJournal,
    tx: Transaction<'_>,
    route: AuthorizedRoute,
    operation: Operation,
    at_ms: u64,
    key: &str,
) -> Result<()> {
    let (seq, prev_hash) = super::super::head(&tx)?;
    let route_hash = tx.query_row(
        "SELECT hash FROM events WHERE seq = ?1",
        params![route.binding_seq],
        |row| row.get(0),
    )?;
    let kind = operation.kind();
    let envelope = Envelope {
        version: 2,
        network: registry.ledger().network,
        seq: seq.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?,
        prev_hash,
        at_ms,
        route,
        route_hash,
        operation,
    };
    let mac = registry
        .authority_key()
        .sign(message(&envelope)?.as_bytes())
        .map_err(|_| unavailable("pilot consent MAC computation failed"))?;
    let agent = envelope.route.binding.agent.to_string();
    let payload = serde_json::to_value(Signed {
        envelope,
        mac: hex::encode(mac),
    })?;
    let appended = super::super::append_keyed_in_tx(
        &tx,
        &NewEvent {
            kind,
            ts_ms: at(at_ms)?,
            agent_id: Some(&agent),
            payload: &payload,
            snapshot: None,
        },
        key,
    )?
    .ok_or_else(|| unavailable("pilot consent key without verified transition"))?;
    tx.commit()?;
    registry.ledger().note_head(&appended)?;
    Ok(())
}

fn publish(registry: &RegistryJournal, tx: Transaction<'_>) -> Result<()> {
    let (seq, hash) = super::super::head(&tx)?;
    tx.commit()?;
    registry.ledger().note_head(&Appended { seq, hash })?;
    Ok(())
}

pub(super) fn authorize(
    registry: &RegistryJournal,
    agent: AgentId,
    account: Address,
    at_ms: u64,
) -> Result<PilotState> {
    at(at_ms)?;
    let ledger = registry.ledger();
    let mut guard = ledger.lock()?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let history = history(ledger, &tx, true)?;
    let route = current_route(registry, &tx, &agent, account)?;
    if let Some(existing) = history.authorities.first() {
        verify(registry, &tx, &history)?;
        if !is_signed(&existing.event)
            || existing.data.agent != agent
            || existing.data.account != account
            || existing.data.baseline_at_ms != at_ms
        {
            return Err(unavailable(
                "pilot already authorized; renewal or reset forbidden",
            ));
        }
        if read(ledger, &tx, &existing.event)?.envelope.route != route {
            return Err(unavailable("pilot consent route changed"));
        }
        let state = project(&history, existing)?;
        publish(registry, tx)?;
        return Ok(state);
    }
    check_baseline(&history, account, &agent)?;
    let (seq, hash) = super::super::head(&tx)?;
    let data = Authorized {
        version: 1,
        network: Network::Testnet,
        agent,
        account,
        baseline_at_ms: at_ms,
        baseline: Anchor { seq, hash },
        order_limit_usd: Decimal::from(15),
        executed_limit_usd: Decimal::from(150),
        realized_loss_limit_usd: Decimal::from(5),
    };
    let state = initial(&data);
    append(
        registry,
        tx,
        route,
        Operation::PilotAuthorized {
            authorization: data,
        },
        at_ms,
        &format!("pilot_authorized:{account}"),
    )?;
    Ok(state)
}

pub(super) fn review(registry: &RegistryJournal, account: Address) -> Result<LegacyPilotReview> {
    let ledger = registry.ledger();
    let mut guard = ledger.lock()?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let history = history(ledger, &tx, true)?;
    let authority = history
        .authorities
        .first()
        .ok_or_else(|| unavailable("legacy pilot authorization missing"))?;
    if is_signed(&authority.event) || history.adoption.is_some() {
        return Err(unavailable("pilot consent already authenticated"));
    }
    let route = current_route(registry, &tx, &authority.data.agent, account)?;
    if authority.data.account != account {
        return Err(unavailable("legacy review account mismatch"));
    }
    let (seq, hash) = super::super::head(&tx)?;
    Ok(LegacyPilotReview {
        authorization_seq: authority.seq,
        authorization_hash: authority.hash.clone(),
        head: Anchor { seq, hash },
        state: project(&history, authority)?,
        route,
    })
}

pub(super) fn adopt(
    registry: &RegistryJournal,
    review: &LegacyPilotReview,
    at_ms: u64,
) -> Result<PilotState> {
    at(at_ms)?;
    let ledger = registry.ledger();
    let mut guard = ledger.lock()?;
    let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let history = history(ledger, &tx, true)?;
    let route = current_route(registry, &tx, &review.state.agent, review.state.account)?;
    if route != review.route {
        return Err(unavailable("reviewed pilot registry route changed"));
    }
    let authority = history
        .authorities
        .first()
        .ok_or_else(|| unavailable("reviewed pilot missing"))?;
    if authority.seq != review.authorization_seq
        || authority.hash != review.authorization_hash
        || is_signed(&authority.event)
    {
        return Err(unavailable("reviewed legacy pilot changed"));
    }
    let operation = Operation::PilotAdopted {
        authorization_seq: review.authorization_seq,
        authorization_hash: review.authorization_hash.clone(),
        reviewed_head: review.head.clone(),
        reviewed_state: review.state.clone(),
    };
    let state = project(&history, authority)?;
    if let Some(adoption) = &history.adoption {
        verify(registry, &tx, &history)?;
        if read(ledger, &tx, adoption)?.envelope.operation != operation {
            return Err(unavailable("pilot adopted from a different review"));
        }
        publish(registry, tx)?;
        return Ok(state);
    }
    let (seq, hash) = super::super::head(&tx)?;
    if review.head != (Anchor { seq, hash }) || state != review.state {
        return Err(unavailable(
            "pilot history changed; renewed review required",
        ));
    }
    append(
        registry,
        tx,
        route,
        operation,
        at_ms,
        &format!("pilot_adopted:{}", authority.seq),
    )?;
    Ok(state)
}
