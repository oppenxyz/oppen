//! Operator-only pairing authority, authenticated independently of the hash chain.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::{File, TryLockError};
use std::sync::Arc;

use oppen_hl::{Address, Network};
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EventKind, Ledger, LedgerError, NewEvent};
use crate::guardrail::AgentId;
use crate::keys::{HmacKey, checked_agent_id};

type Result<T> = std::result::Result<T, PairingError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingId {
    pub network: Network,
    pub issued_seq: u64,
}

impl fmt::Display for PairingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let network = match self.network {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        };
        write!(f, "pairing-{network}-{}", self.issued_seq)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingBinding {
    pub agent: AgentId,
    pub account: Address,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingRecord {
    pub id: PairingId,
    pub binding: PairingBinding,
    pub digest: [u8; 32],
    pub issued_at_ms: u64,
    pub revoked_at_ms: Option<u64>,
}

impl fmt::Debug for PairingRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingRecord")
            .field("id", &self.id)
            .field("binding", &self.binding)
            .field("digest", &"<redacted>")
            .field("issued_at_ms", &self.issued_at_ms)
            .field("revoked_at_ms", &self.revoked_at_ms)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("pairing authority unavailable: {detail}")]
    Unavailable { detail: String },
    #[error("pairing authority already has a live owner")]
    AlreadyOwned,
}

impl From<rusqlite::Error> for PairingError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Ledger(error.into())
    }
}

fn unavailable(detail: impl Into<String>) -> PairingError {
    PairingError::Unavailable {
        detail: detail.into(),
    }
}

/// A single runtime owns authentication sessions and their revocation signals.
/// Keep this capability alive for that runtime's entire lifetime; never expose
/// it through an agent view. The lease file must never be unlinked.
#[derive(Debug)]
pub struct PairingJournal {
    ledger: Arc<Ledger>,
    key: Arc<HmacKey>,
    _owner: File,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    envelope: Envelope,
    mac: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u64,
    network: Network,
    seq: u64,
    prev_hash: String,
    at_ms: u64,
    authority: Authority,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Authority {
    PairingIssued {
        id: PairingId,
        binding: PairingBinding,
        digest: String,
    },
    PairingRevoked {
        id: PairingId,
        issued_hash: String,
    },
}

impl Authority {
    fn kind(&self) -> EventKind {
        match self {
            Self::PairingIssued { .. } => EventKind::PairingIssued,
            Self::PairingRevoked { .. } => EventKind::PairingRevoked,
        }
    }
}

fn message(envelope: &Envelope) -> Result<String> {
    let value = serde_json::json!({
        "domain": "oppen.pairing-authority.v1",
        "envelope": envelope,
    });
    Ok(super::hash::canonical_json(&value)?)
}

fn decode_hex(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(unavailable(
            "pairing digest or MAC is not canonical 32-byte hex",
        ));
    }
    let mut bytes = [0; 32];
    hex::decode_to_slice(value, &mut bytes)
        .map_err(|_| unavailable("invalid pairing digest or MAC"))?;
    Ok(bytes)
}

fn validate_binding(binding: &PairingBinding) -> Result<()> {
    checked_agent_id(&binding.agent)
        .map_err(|_| unavailable("invalid pairing agent identifier"))?;
    if binding.account == Address::ZERO {
        return Err(unavailable("pairing account cannot be zero"));
    }
    Ok(())
}

fn timestamp(at_ms: u64) -> Result<i64> {
    i64::try_from(at_ms).map_err(|_| unavailable("pairing timestamp out of range"))
}

impl PairingJournal {
    /// Verifies all existing records with this key before returning. An empty
    /// history has no MAC witness with which to distinguish HMAC keys.
    pub fn open(ledger: Arc<Ledger>, key: Arc<HmacKey>) -> Result<Self> {
        if ledger.anchor.is_none() {
            return Err(unavailable("pairing authority requires an anchored ledger"));
        }
        let mut path = ledger.coordination_path.clone().into_os_string();
        path.push(".pairings-owner");
        let owner = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(std::path::PathBuf::from(path))
            .map_err(LedgerError::Io)?;
        match owner.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(PairingError::AlreadyOwned),
            Err(TryLockError::Error(error)) => return Err(LedgerError::Io(error).into()),
        }
        let journal = Self {
            ledger,
            key,
            _owner: owner,
        };
        journal.records()?;
        Ok(journal)
    }

    pub fn network(&self) -> Network {
        self.ledger.network
    }

    pub fn records(&self) -> Result<Vec<PairingRecord>> {
        let mut guard = self.ledger.lock()?;
        // Chain verification and authority replay must share one SQLite
        // snapshot even if a writer does not honor the coordination file.
        let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(self
            .replay(&tx)?
            .into_values()
            .map(|(record, _)| record)
            .collect())
    }

    pub fn issue(
        &self,
        binding: PairingBinding,
        digest: [u8; 32],
        at_ms: u64,
    ) -> Result<PairingRecord> {
        validate_binding(&binding)?;
        let ts_ms = timestamp(at_ms)?;
        let mut guard = self.ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let records = self.replay(&tx)?;
        if records.values().any(|(record, _)| record.digest == digest) {
            return Err(unavailable("pairing digest has already been issued"));
        }
        let (head_seq, prev_hash) = super::head(&tx)?;
        let seq = head_seq.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
        let id = PairingId {
            network: self.network(),
            issued_seq: seq,
        };
        let digest_hex = hex::encode(digest);
        let envelope = Envelope {
            version: 1,
            network: self.network(),
            seq,
            prev_hash,
            at_ms,
            authority: Authority::PairingIssued {
                id,
                binding: binding.clone(),
                digest: digest_hex.clone(),
            },
        };
        let payload = self.signed(envelope)?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::PairingIssued,
                ts_ms,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
            &format!("pairing_digest:{digest_hex}"),
        )?
        .ok_or_else(|| unavailable("pairing digest key without verified issuance"))?;
        tx.commit()?;
        self.ledger.note_head(&appended)?;
        Ok(PairingRecord {
            id,
            binding,
            digest,
            issued_at_ms: at_ms,
            revoked_at_ms: None,
        })
    }

    pub fn revoke(&self, id: PairingId, at_ms: u64) -> Result<bool> {
        if id.network != self.network() {
            return Err(unavailable("pairing ID belongs to another network"));
        }
        let ts_ms = timestamp(at_ms)?;
        let mut guard = self.ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let records = self.replay(&tx)?;
        let Some((record, issued_hash)) = records.get(&id.issued_seq) else {
            return Ok(false);
        };
        if record.revoked_at_ms.is_some() {
            return Ok(false);
        }
        if at_ms < record.issued_at_ms {
            return Err(unavailable("pairing revocation precedes issuance"));
        }
        let (head_seq, prev_hash) = super::head(&tx)?;
        let seq = head_seq.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
        let payload = self.signed(Envelope {
            version: 1,
            network: self.network(),
            seq,
            prev_hash,
            at_ms,
            authority: Authority::PairingRevoked {
                id,
                issued_hash: issued_hash.clone(),
            },
        })?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::PairingRevoked,
                ts_ms,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
            &format!("pairing_revoked:{}", id.issued_seq),
        )?
        .ok_or_else(|| unavailable("pairing revocation key without verified revocation"))?;
        tx.commit()?;
        self.ledger.note_head(&appended)?;
        Ok(true)
    }

    fn signed(&self, envelope: Envelope) -> Result<Value> {
        let mac = self
            .key
            .sign(message(&envelope)?.as_bytes())
            .map_err(|_| unavailable("pairing MAC computation failed"))?;
        serde_json::to_value(Signed {
            envelope,
            mac: hex::encode(mac),
        })
        .map_err(|_| unavailable("pairing payload serialization failed"))
    }

    fn replay(&self, connection: &Connection) -> Result<BTreeMap<u64, (PairingRecord, String)>> {
        let anchor = self
            .ledger
            .anchor
            .as_ref()
            .ok_or_else(|| unavailable("pairing authority requires an anchor"))?
            .load()?
            .ok_or_else(|| unavailable("pairing anchor missing"))?;
        let report = super::verify::walk(connection, &self.ledger.genesis, Some(&anchor))?;
        if let Some(broken) = report.first_break {
            return Err(unavailable(format!(
                "pairing chain broken at {}: {}",
                broken.seq, broken.reason
            )));
        }
        let mut records: BTreeMap<u64, (PairingRecord, String)> = BTreeMap::new();
        let mut digests = HashSet::new();
        let mut statement = connection.prepare(&format!(
            "SELECT {}, idem_key FROM events WHERE kind IN ('pairing_issued', 'pairing_revoked') ORDER BY seq",
            super::SELECT_EVENT_COLUMNS
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let event = super::event_from_row(row)?;
            let key: Option<String> = row.get(12)?;
            let value = event
                .payload
                .ok_or_else(|| unavailable("pairing authority history redacted"))?;
            let signed: Signed = serde_json::from_value(value)
                .map_err(|_| unavailable("malformed pairing authority payload"))?;
            let envelope = signed.envelope;
            let mac = decode_hex(&signed.mac)?;
            if !self.key.verify(message(&envelope)?.as_bytes(), &mac) {
                return Err(unavailable("pairing authority MAC mismatch"));
            }
            if envelope.version != 1
                || envelope.network != self.network()
                || envelope.seq != event.seq
                || envelope.prev_hash != event.prev_hash
                || timestamp(envelope.at_ms)? != event.ts_ms
                || envelope.authority.kind() != event.kind
                || event.agent_id.is_some()
                || event.snapshot_id.is_some()
                || event.snapshot_hash.is_some()
            {
                return Err(unavailable(
                    "pairing authority envelope does not match its chain row",
                ));
            }
            match envelope.authority {
                Authority::PairingIssued {
                    id,
                    binding,
                    digest,
                } => {
                    validate_binding(&binding)?;
                    let bytes = decode_hex(&digest)?;
                    if id.network != self.network()
                        || id.issued_seq != event.seq
                        || key.as_deref() != Some(format!("pairing_digest:{digest}").as_str())
                        || !digests.insert(bytes)
                        || records.contains_key(&id.issued_seq)
                    {
                        return Err(unavailable("duplicate or invalid pairing issuance"));
                    }
                    records.insert(
                        id.issued_seq,
                        (
                            PairingRecord {
                                id,
                                binding,
                                digest: bytes,
                                issued_at_ms: envelope.at_ms,
                                revoked_at_ms: None,
                            },
                            event.hash,
                        ),
                    );
                }
                Authority::PairingRevoked { id, issued_hash } => {
                    let (record, hash) = records
                        .get_mut(&id.issued_seq)
                        .ok_or_else(|| unavailable("pairing revocation has no earlier issuance"))?;
                    if id.network != self.network()
                        || *hash != issued_hash
                        || record.revoked_at_ms.is_some()
                        || envelope.at_ms < record.issued_at_ms
                        || key.as_deref()
                            != Some(format!("pairing_revoked:{}", id.issued_seq).as_str())
                    {
                        return Err(unavailable("invalid pairing revocation linkage"));
                    }
                    record.revoked_at_ms = Some(envelope.at_ms);
                }
            }
        }
        Ok(records)
    }
}

#[cfg(test)]
#[path = "pairing_tests.rs"]
mod tests;
