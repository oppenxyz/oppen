//! Durable account reservations (spec items 7, 19 and D6).
//!
//! Dropping a receipt never releases a reservation. Only definite submission
//! evidence does; an unknown transport outcome must remain pending.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use oppen_hl::{Address, wire::Cloid};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EventKind, Ledger, LedgerError, NewEvent, SELECT_EVENT_COLUMNS};
use crate::guardrail::{Clearance, ClearedKind};

type Result<T> = std::result::Result<T, SubmissionError>;

#[derive(Debug, thiserror::Error)]
pub enum SubmissionError {
    #[error(transparent)]
    Pilot(#[from] super::pilot::PilotError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("account has pending submission {cloid}")]
    Busy { cloid: String },
    #[error("this account has already used this cloid")]
    DuplicateCloid,
    #[error("account submission revision has changed")]
    StaleRevision,
    #[error("no matching durable order intent")]
    MissingIntent,
    #[error("submission belongs to another network")]
    WrongNetwork,
    #[error("invalid submission record: {detail}")]
    InvalidRecord { detail: String },
}

impl From<rusqlite::Error> for SubmissionError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Ledger(error.into())
    }
}

impl From<serde_json::Error> for SubmissionError {
    fn from(error: serde_json::Error) -> Self {
        Self::Ledger(error.into())
    }
}

#[derive(Clone, Debug)]
pub struct SubmissionJournal(Arc<Ledger>);

#[derive(Clone, Debug, Default)]
pub struct SubmissionState {
    /// Latest lifecycle event sequence for this account, or zero.
    pub revision: u64,
    pub pending: Option<SubmissionReceipt>,
}

/// A reservation in one chain. Same-network identical copies of that chain
/// cannot be distinguished, just as with the ledger's intent receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmissionReceipt {
    chain: String,
    seq: u64,
    hash: String,
    agent: String,
    start: Started,
}

impl SubmissionReceipt {
    pub fn cloid(&self) -> &Cloid {
        &self.start.cloid
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubmissionResolution {
    Rejected { message: String },
    NotSent { detail: String },
    Observed { oid: u64, status: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Started {
    version: u64,
    account: Address,
    cloid: Cloid,
    intent_seq: u64,
    intent_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolved {
    version: u64,
    account: Address,
    start_seq: u64,
    start_hash: String,
    outcome: SubmissionResolution,
}

#[derive(Default)]
struct Replay {
    accounts: HashMap<Address, SubmissionState>,
    starts: HashMap<u64, SubmissionReceipt>,
    resolved: HashSet<u64>,
    used: HashSet<(Address, Cloid)>,
    pilot: BTreeMap<u64, PilotSubmission>,
}

pub(super) struct PilotSubmission {
    pub seq: u64,
    pub account: Address,
    pub agent: String,
    pub cloid: Cloid,
    pub intent: Value,
    pub resolution: Option<SubmissionResolution>,
}

pub(super) fn replay_for_pilot(
    ledger: &Ledger,
    connection: &Connection,
    verify_anchor: bool,
) -> Result<Vec<PilotSubmission>> {
    Ok(replay(ledger, connection, verify_anchor)?
        .pilot
        .into_values()
        .collect())
}

fn invalid(detail: impl Into<String>) -> SubmissionError {
    SubmissionError::InvalidRecord {
        detail: detail.into(),
    }
}

fn start_key(start: &Started) -> String {
    format!("submission:{}:{}", start.account, start.cloid.as_str())
}

fn resolve_key(seq: u64) -> String {
    format!("submission_resolved:{seq}")
}

impl SubmissionJournal {
    pub(super) fn new(ledger: Arc<Ledger>) -> Self {
        Self(ledger)
    }

    pub fn state(&self, account: Address) -> Result<SubmissionState> {
        let mut guard = self.0.lock()?;
        // A write reservation also stabilizes the snapshot against independent
        // handles while the anchor and chain are checked.
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut replay = self.replay(&tx)?;
        Ok(replay.accounts.remove(&account).unwrap_or_default())
    }

    pub fn begin(
        &self,
        account: Address,
        clearance: &Clearance,
        expected_revision: u64,
        at_ms: u64,
    ) -> Result<SubmissionReceipt> {
        if clearance.network != self.0.network {
            return Err(SubmissionError::WrongNetwork);
        }
        let ClearedKind::Order {
            cloid: Some(cloid), ..
        } = &clearance.kind
        else {
            return Err(SubmissionError::MissingIntent);
        };
        if clearance
            .vault_address
            .is_some_and(|vault| vault != account)
        {
            return Err(invalid("clearance vault does not match submission account"));
        }
        let mut guard = self.0.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let replay = self.replay(&tx)?;
        let state = replay.accounts.get(&account).cloned().unwrap_or_default();
        if let Some(pending) = state.pending {
            return Err(SubmissionError::Busy {
                cloid: pending.cloid().as_str().into(),
            });
        }
        if state.revision != expected_revision {
            return Err(SubmissionError::StaleRevision);
        }
        if replay.used.contains(&(account, cloid.clone())) {
            return Err(SubmissionError::DuplicateCloid);
        }
        let wanted = serde_json::to_value(clearance)?;
        let mut statement = tx.prepare(&format!(
            "SELECT {SELECT_EVENT_COLUMNS} FROM events WHERE kind = 'order_intent' \
             AND agent_id = ?1 ORDER BY seq DESC"
        ))?;
        let mut rows = statement.query(params![clearance.agent.as_str()])?;
        let mut intent = None;
        while let Some(row) = rows.next()? {
            let event = super::event_from_row(row)?;
            if let Some(mut payload) = event.payload {
                if let Some(object) = payload.as_object_mut() {
                    object.remove("reason");
                }
                if payload == wanted {
                    intent = Some((event.seq, event.hash));
                    break;
                }
            }
        }
        drop(rows);
        drop(statement);
        let (intent_seq, intent_hash) = intent.ok_or(SubmissionError::MissingIntent)?;
        super::pilot::check_admission(&self.0, &tx, account, clearance)?;
        let start = Started {
            version: 1,
            account,
            cloid: cloid.clone(),
            intent_seq,
            intent_hash,
        };
        let payload = serde_json::to_value(&start)?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::SubmissionStarted,
                ts_ms: timestamp(at_ms)?,
                agent_id: Some(clearance.agent.as_str()),
                payload: &payload,
                snapshot: None,
            },
            &start_key(&start),
        )?
        .ok_or(SubmissionError::DuplicateCloid)?;
        tx.commit()?;
        self.0.note_head(&appended)?;
        Ok(SubmissionReceipt {
            chain: self.0.genesis.clone(),
            seq: appended.seq,
            hash: appended.hash,
            agent: clearance.agent.as_str().into(),
            start,
        })
    }

    pub fn resolve(
        &self,
        receipt: &SubmissionReceipt,
        resolution: SubmissionResolution,
        at_ms: u64,
    ) -> Result<()> {
        if receipt.chain != self.0.genesis {
            return Err(SubmissionError::WrongNetwork);
        }
        let mut guard = self.0.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let replay = self.replay(&tx)?;
        if replay.starts.get(&receipt.seq) != Some(receipt) {
            return Err(invalid("receipt does not match durable start"));
        }
        // A replay of an old resolution is a no-op even if another start is
        // now pending. It must never release the account's newer reservation.
        if replay.resolved.contains(&receipt.seq) {
            return Ok(());
        }
        let payload = serde_json::to_value(Resolved {
            version: 1,
            account: receipt.start.account,
            start_seq: receipt.seq,
            start_hash: receipt.hash.clone(),
            outcome: resolution,
        })?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::SubmissionResolved,
                ts_ms: timestamp(at_ms)?,
                agent_id: Some(&receipt.agent),
                payload: &payload,
                snapshot: None,
            },
            &resolve_key(receipt.seq),
        )?
        .ok_or_else(|| invalid("resolution key without resolution"))?;
        tx.commit()?;
        self.0.note_head(&appended)?;
        Ok(())
    }

    fn replay(&self, connection: &Connection) -> Result<Replay> {
        replay(&self.0, connection, true)
    }
}

fn replay(ledger: &Ledger, connection: &Connection, verify_anchor: bool) -> Result<Replay> {
    // Do not call Ledger::verify here: it takes the mutex we already hold.
    let anchor = match (&ledger.anchor, verify_anchor) {
        (Some(anchor), true) => Some(anchor.load()?.ok_or_else(|| invalid("anchor missing"))?),
        _ => None,
    };
    let report = super::verify::walk(connection, &ledger.genesis, anchor.as_ref())?;
    if let Some(broken) = report.first_break {
        return Err(invalid(format!(
            "chain broken at {}: {}",
            broken.seq, broken.reason
        )));
    }
    let mut replay = Replay::default();
    // Never filter by account before parsing: a null or malformed payload
    // can conceal a pending reservation for any account.
    let mut statement = connection.prepare(&format!(
        "SELECT {SELECT_EVENT_COLUMNS}, idem_key FROM events \
             WHERE kind IN ('submission_started', 'submission_resolved') ORDER BY seq"
    ))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let event = super::event_from_row(row)?;
        let key: Option<String> = row.get(12)?;
        let agent = event
            .agent_id
            .ok_or_else(|| invalid("lifecycle row has no agent"))?;
        let payload = event
            .payload
            .ok_or_else(|| invalid("lifecycle payload redacted"))?;
        match event.kind {
            EventKind::SubmissionStarted => {
                let start: Started =
                    serde_json::from_value(payload).map_err(|error| invalid(error.to_string()))?;
                if start.version != 1 || start.intent_seq >= event.seq || start.intent_seq == 0 {
                    return Err(invalid("unsupported start version or invalid intent order"));
                }
                if key.as_deref() != Some(start_key(&start).as_str()) {
                    return Err(invalid("start idempotence key mismatch"));
                }
                let intent = validate_intent(ledger, connection, &start, &agent)?;
                replay.pilot.insert(
                    event.seq,
                    PilotSubmission {
                        seq: event.seq,
                        account: start.account,
                        agent: agent.clone(),
                        cloid: start.cloid.clone(),
                        intent,
                        resolution: None,
                    },
                );
                if !replay.used.insert((start.account, start.cloid.clone())) {
                    return Err(invalid("cloid reused in submission history"));
                }
                let state = replay.accounts.entry(start.account).or_default();
                if state.pending.is_some() {
                    return Err(invalid("overlapping account submissions"));
                }
                let receipt = SubmissionReceipt {
                    chain: ledger.genesis.clone(),
                    seq: event.seq,
                    hash: event.hash,
                    agent,
                    start,
                };
                state.revision = event.seq;
                state.pending = Some(receipt.clone());
                replay.starts.insert(event.seq, receipt);
            }
            EventKind::SubmissionResolved => {
                let resolved: Resolved =
                    serde_json::from_value(payload).map_err(|error| invalid(error.to_string()))?;
                let receipt = replay
                    .starts
                    .get(&resolved.start_seq)
                    .ok_or_else(|| invalid("resolution precedes or lacks start"))?;
                if resolved.version != 1
                    || resolved.account != receipt.start.account
                    || resolved.start_hash != receipt.hash
                    || agent != receipt.agent
                    || key.as_deref() != Some(resolve_key(receipt.seq).as_str())
                    || !replay.resolved.insert(receipt.seq)
                {
                    return Err(invalid("invalid or duplicate resolution linkage"));
                }
                let state = replay.accounts.entry(resolved.account).or_default();
                if state.pending.as_ref() != Some(receipt) {
                    return Err(invalid(
                        "resolution does not name current pending submission",
                    ));
                }
                state.pending = None;
                state.revision = event.seq;
                replay
                    .pilot
                    .get_mut(&receipt.seq)
                    .ok_or_else(|| invalid("resolution lacks validated pilot start"))?
                    .resolution = Some(resolved.outcome);
            }
            _ => return Err(invalid("unexpected lifecycle kind")),
        }
    }
    Ok(replay)
}

fn validate_intent(
    ledger: &Ledger,
    connection: &Connection,
    start: &Started,
    agent: &str,
) -> Result<Value> {
    let mut statement = connection.prepare(&format!(
        "SELECT {SELECT_EVENT_COLUMNS} FROM events WHERE seq = ?1"
    ))?;
    let mut rows = statement.query(params![
        i64::try_from(start.intent_seq).map_err(|_| invalid("intent sequence out of range"))?
    ])?;
    let event = super::event_from_row(
        rows.next()?
            .ok_or_else(|| invalid("missing linked intent"))?,
    )?;
    let payload = event
        .payload
        .ok_or_else(|| invalid("linked intent redacted"))?;
    if event.kind != EventKind::OrderIntent
        || event.hash != start.intent_hash
        || event.agent_id.as_deref() != Some(agent)
        || payload.get("agent").and_then(Value::as_str) != Some(agent)
        || payload.get("network") != Some(&serde_json::to_value(ledger.network)?)
        || payload["kind"]["cleared"] != "order"
        || payload["kind"]["cloid"].as_str() != Some(start.cloid.as_str())
        || (!payload["vault_address"].is_null()
            && payload["vault_address"] != serde_json::to_value(start.account)?)
    {
        return Err(invalid("linked intent does not match submission"));
    }
    Ok(payload)
}

fn timestamp(at_ms: u64) -> Result<i64> {
    i64::try_from(at_ms).map_err(|_| invalid("timestamp out of range"))
}

#[cfg(test)]
#[path = "submission_tests.rs"]
mod tests;
