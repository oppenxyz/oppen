//! Complete policy authority in the existing anchored event chain (ES18).

use std::sync::Arc;

use oppen_hl::Network;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EventKind, LedgerError, NewEvent, RegistryJournal};
use crate::guardrail::{LegacyPolicyEvidence, LegacyPolicyReview, PersistedState};

type Result<T> = std::result::Result<T, PolicyError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PolicyVersion {
    pub revision: u64,
    pub state: PersistedState,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("policy authority requires explicit paused initialization")]
    MigrationRequired,
    #[error("stale policy revision: expected {expected}, current {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("reviewed policy setup route changed")]
    RouteChanged,
    #[error("policy authority unavailable: {detail}")]
    Unavailable { detail: String },
}

impl From<rusqlite::Error> for PolicyError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Ledger(error.into())
    }
}

fn unavailable(detail: impl Into<String>) -> PolicyError {
    PolicyError::Unavailable {
        detail: detail.into(),
    }
}

#[derive(Debug)]
pub struct PolicyJournal {
    registry: Arc<RegistryJournal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyLink {
    seq: u64,
    hash: String,
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
    previous_policy: Option<PolicyLink>,
    operation: Operation,
    state: PersistedState,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    PolicyInitialized {
        review: LegacyPolicyEvidence,
        rechecked_at_ms: u64,
    },
    PolicyReplaced,
}

impl Operation {
    fn kind(&self) -> EventKind {
        match self {
            Self::PolicyInitialized { .. } => EventKind::PolicyInitialized,
            Self::PolicyReplaced => EventKind::PolicyReplaced,
        }
    }
}

struct Entry {
    version: PolicyVersion,
    hash: String,
    previous_policy: Option<PolicyLink>,
    operation: Operation,
}

impl Entry {
    fn link(&self) -> PolicyLink {
        PolicyLink {
            seq: self.version.revision,
            hash: self.hash.clone(),
        }
    }
}

fn timestamp(at_ms: u64) -> Result<i64> {
    i64::try_from(at_ms).map_err(|_| unavailable("policy timestamp out of range"))
}

fn value<T: Serialize>(data: &T) -> Result<Value> {
    serde_json::to_value(data).map_err(|_| unavailable("policy serialization failed"))
}

fn message(envelope: &Envelope) -> Result<String> {
    Ok(super::hash::canonical_json(&serde_json::json!({
        "domain": "oppen.policy-authority.v1", "envelope": envelope,
    }))?)
}

fn validate_state(state: &PersistedState, at_ms: u64) -> Result<()> {
    timestamp(at_ms)?;
    state
        .validate()
        .map_err(|error| unavailable(error.to_string()))?;
    let kill = value(&state.kill)?;
    let mut engagements = Vec::new();
    if !kill["global"].is_null() {
        engagements.push(&kill["global"]);
    }
    let agents = kill["agents"]
        .as_object()
        .ok_or_else(|| unavailable("invalid policy kill map"))?;
    engagements.extend(agents.values());
    for engagement in engagements {
        let engaged_at = engagement["engaged_at_ms"]
            .as_u64()
            .ok_or_else(|| unavailable("invalid policy engagement timestamp"))?;
        if engaged_at > at_ms {
            return Err(unavailable(
                "policy kill engagement is after its snapshot timestamp",
            ));
        }
    }
    Ok(())
}

fn require_paused(state: &PersistedState) -> Result<()> {
    if state.kill.global().is_none() {
        return Err(unavailable(
            "policy initialization requires an engaged global pause",
        ));
    }
    Ok(())
}

fn validate_review(
    review: &LegacyPolicyEvidence,
    network: Network,
    rechecked_at_ms: u64,
    at_ms: u64,
) -> Result<()> {
    timestamp(review.observed_at_ms)?;
    if review.network != network
        || review.observed_at_ms > rechecked_at_ms
        || rechecked_at_ms != at_ms
        || !std::path::Path::new(&review.source).is_absolute()
        || review.fingerprint.len() != 64
        || !review
            .fingerprint
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || (!review.file_present && (!review.schema.is_empty() || !review.tables.is_empty()))
    {
        return Err(unavailable("invalid policy initialization provenance"));
    }
    Ok(())
}

impl PolicyJournal {
    /// Does not load policy, read keys, initialize, or manufacture defaults.
    /// Restricted cleanup can be assembled even when policy is unavailable.
    pub fn new(registry: Arc<RegistryJournal>) -> Self {
        Self { registry }
    }

    pub(crate) fn network(&self) -> Network {
        self.registry.ledger().network
    }

    /// Structural, chain-checked inspection only, never authenticated authority.
    /// Does not load an HMAC key, publish a head, or initialize policy. Callers
    /// must label this snapshot unverified and must not use it for signing.
    pub(crate) fn inspect(ledger: &super::Ledger) -> Result<Option<PolicyVersion>> {
        let mut guard = ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(Self::replay_in(ledger, &tx, None)?.map(|entry| entry.version))
    }

    pub fn current(&self) -> Result<PolicyVersion> {
        let mut guard = self.registry.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
        self.current_in(&tx)
    }

    pub(super) fn current_in(&self, connection: &Connection) -> Result<PolicyVersion> {
        self.replay(connection)?
            .map(|entry| entry.version)
            .ok_or(PolicyError::MigrationRequired)
    }

    pub(super) fn registry(&self) -> &RegistryJournal {
        &self.registry
    }

    pub(crate) fn submissions(&self, pilot_required: bool) -> super::SubmissionJournal {
        super::SubmissionJournal::authenticated(self.registry.clone(), pilot_required)
    }

    /// Explicit adoption of a supplied complete policy, initially globally
    /// paused. Old writers must be stopped: retaining a fresh read-only legacy
    /// transaction gives consistent evidence, not cross-database atomicity.
    /// A post-commit publication error can leave durable initialized authority.
    pub fn initialize(
        &self,
        review: &LegacyPolicyReview,
        next: PersistedState,
        at_ms: u64,
    ) -> Result<PolicyVersion> {
        self.initialize_checked(review, next, at_ms, None)
    }

    /// Setup review and commit must refer to the same live registry route.
    pub fn initialize_for_route(
        &self,
        review: &LegacyPolicyReview,
        next: PersistedState,
        at_ms: u64,
        route: &super::AuthorizedRoute,
    ) -> Result<PolicyVersion> {
        self.initialize_checked(review, next, at_ms, Some(route))
    }

    fn initialize_checked(
        &self,
        review: &LegacyPolicyReview,
        next: PersistedState,
        at_ms: u64,
        route: Option<&super::AuthorizedRoute>,
    ) -> Result<PolicyVersion> {
        validate_state(&next, at_ms)?;
        require_paused(&next)?;
        validate_review(
            review.evidence(),
            self.registry.ledger().network,
            at_ms,
            at_ms,
        )?;
        let mut guard = self.registry.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.check_setup_route(&tx, route)?;
        if let Some(current) = self.replay(&tx)? {
            if matches!(&current.operation, Operation::PolicyInitialized { review: recorded, .. } if recorded == review.evidence())
                && current.version.state == next
            {
                self.publish_verified_head(tx)?;
                return Ok(current.version);
            }
            return Err(PolicyError::StaleRevision {
                expected: 0,
                actual: current.version.revision,
            });
        }
        let fresh = review
            .recheck(at_ms)
            .map_err(|error| unavailable(error.to_string()))?;
        let result = self.append(
            tx,
            None,
            Operation::PolicyInitialized {
                review: review.evidence().clone(),
                rechecked_at_ms: fresh.evidence().observed_at_ms,
            },
            next,
            at_ms,
        );
        // Explicitly retain the legacy SQL snapshot through commit/publication.
        drop(fresh);
        result
    }

    /// Compare-and-replace the complete policy. A post-commit anchor error has
    /// an uncertain durable outcome; it is not a promise that policy stayed
    /// unchanged. Exact retries publish the verified head before success.
    pub fn replace(
        &self,
        expected_revision: u64,
        next: PersistedState,
        at_ms: u64,
    ) -> Result<PolicyVersion> {
        self.replace_checked(expected_revision, next, at_ms, None)
    }

    pub fn replace_for_route(
        &self,
        expected_revision: u64,
        next: PersistedState,
        at_ms: u64,
        route: &super::AuthorizedRoute,
    ) -> Result<PolicyVersion> {
        self.replace_checked(expected_revision, next, at_ms, Some(route))
    }

    fn replace_checked(
        &self,
        expected_revision: u64,
        next: PersistedState,
        at_ms: u64,
        route: Option<&super::AuthorizedRoute>,
    ) -> Result<PolicyVersion> {
        validate_state(&next, at_ms)?;
        let mut guard = self.registry.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.check_setup_route(&tx, route)?;
        let current = self.replay(&tx)?.ok_or(PolicyError::MigrationRequired)?;
        let exact_retry = matches!(current.operation, Operation::PolicyReplaced)
            && current
                .previous_policy
                .as_ref()
                .is_some_and(|link| link.seq == expected_revision)
            && current.version.state == next;
        if current.version.revision != expected_revision && !exact_retry {
            return Err(PolicyError::StaleRevision {
                expected: expected_revision,
                actual: current.version.revision,
            });
        }
        if exact_retry || current.version.state == next {
            self.publish_verified_head(tx)?;
            return Ok(current.version);
        }
        self.append(
            tx,
            Some(current.link()),
            Operation::PolicyReplaced,
            next,
            at_ms,
        )
    }

    fn check_setup_route(
        &self,
        connection: &Connection,
        expected: Option<&super::AuthorizedRoute>,
    ) -> Result<()> {
        if let Some(expected) = expected {
            let current = self
                .registry
                .optional_route_in(connection, &expected.binding.agent)
                .map_err(|error| unavailable(error.to_string()))?;
            if current.as_ref() != Some(expected) {
                return Err(PolicyError::RouteChanged);
            }
        }
        Ok(())
    }

    fn append(
        &self,
        tx: Transaction<'_>,
        previous_policy: Option<PolicyLink>,
        operation: Operation,
        state: PersistedState,
        at_ms: u64,
    ) -> Result<PolicyVersion> {
        let (head_seq, prev_hash) = super::head(&tx)?;
        let seq = head_seq.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
        let kind = operation.kind();
        let idem_key = match &previous_policy {
            Some(link) => format!("policy_after:{}", link.seq),
            None => "policy_initialized".to_owned(),
        };
        let envelope = Envelope {
            version: 1,
            network: self.registry.ledger().network,
            seq,
            prev_hash,
            at_ms,
            previous_policy,
            operation,
            state: state.clone(),
        };
        let mac = self
            .registry
            .authority_key()
            .sign(message(&envelope)?.as_bytes())
            .map_err(|_| unavailable("policy MAC computation failed"))?;
        let payload = value(&Signed {
            envelope,
            mac: hex::encode(mac),
        })?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind,
                ts_ms: timestamp(at_ms)?,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
            &idem_key,
        )?
        .ok_or_else(|| unavailable("policy key without authenticated transition"))?;
        tx.commit()?;
        self.registry.ledger().note_head(&appended)?;
        Ok(PolicyVersion {
            revision: seq,
            state,
        })
    }

    fn publish_verified_head(&self, tx: Transaction<'_>) -> Result<()> {
        let (seq, hash) = super::head(&tx)?;
        tx.commit()?;
        self.registry
            .ledger()
            .note_head(&super::Appended { seq, hash })?;
        Ok(())
    }

    fn replay(&self, connection: &Connection) -> Result<Option<Entry>> {
        Self::replay_in(
            self.registry.ledger(),
            connection,
            Some(self.registry.authority_key()),
        )
    }

    fn replay_in(
        ledger: &super::Ledger,
        connection: &Connection,
        key: Option<&crate::keys::HmacKey>,
    ) -> Result<Option<Entry>> {
        let anchor = ledger
            .anchor
            .as_ref()
            .ok_or_else(|| unavailable("policy authority requires an anchored ledger"))?
            .load()?
            .ok_or_else(|| unavailable("policy anchor missing"))?;
        let report = super::verify::walk(connection, &ledger.genesis, Some(&anchor))?;
        if let Some(broken) = report.first_break {
            return Err(unavailable(format!(
                "policy chain broken at {}: {}",
                broken.seq, broken.reason
            )));
        }
        let mut latest: Option<Entry> = None;
        let mut statement = connection.prepare(&format!(
            "SELECT {}, idem_key FROM events WHERE kind IN ('policy_initialized', 'policy_replaced') ORDER BY seq",
            super::SELECT_EVENT_COLUMNS
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let event = super::event_from_row(row)?;
            let raw: Option<String> = row.get(4)?;
            let raw = raw.ok_or_else(|| unavailable("required policy snapshot redacted"))?;
            let payload = event
                .payload
                .ok_or_else(|| unavailable("required policy snapshot missing"))?;
            // Stored writers emit canonical JSON. Comparing raw bytes also
            // catches duplicate object/map keys before Value can hide them.
            if super::hash::canonical_json(&payload)? != raw {
                return Err(unavailable(
                    "policy snapshot has duplicate keys or noncanonical JSON",
                ));
            }
            let signed: Signed = serde_json::from_value(payload.clone())
                .map_err(|_| unavailable("malformed policy snapshot"))?;
            if value(&signed)? != payload {
                return Err(unavailable(
                    "missing, unknown, or noncanonical policy snapshot fields",
                ));
            }
            if signed.mac.len() != 64
                || !signed
                    .mac
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(unavailable("policy MAC is not canonical 32-byte hex"));
            }
            let mac = hex::decode(&signed.mac).map_err(|_| unavailable("invalid policy MAC"))?;
            let envelope = signed.envelope;
            if let Some(key) = key
                && !key.verify(message(&envelope)?.as_bytes(), &mac)
            {
                return Err(unavailable("policy authority MAC mismatch"));
            }
            if envelope.version != 1
                || envelope.network != ledger.network
                || envelope.seq != event.seq
                || envelope.prev_hash != event.prev_hash
                || timestamp(envelope.at_ms)? != event.ts_ms
                || envelope.operation.kind() != event.kind
                || event.agent_id.is_some()
                || event.snapshot_id.is_some()
                || event.snapshot_hash.is_some()
            {
                return Err(unavailable("policy envelope does not match its chain row"));
            }
            if envelope.previous_policy != latest.as_ref().map(Entry::link) {
                return Err(unavailable(
                    "policy predecessor is not the latest authenticated snapshot",
                ));
            }
            let idem_key: Option<String> = row.get(12)?;
            let expected_key = match (&envelope.operation, latest.as_ref()) {
                (
                    Operation::PolicyInitialized {
                        review,
                        rechecked_at_ms,
                    },
                    None,
                ) => {
                    validate_review(review, ledger.network, *rechecked_at_ms, envelope.at_ms)?;
                    require_paused(&envelope.state)?;
                    "policy_initialized".to_owned()
                }
                (Operation::PolicyReplaced, Some(previous)) => {
                    format!("policy_after:{}", previous.version.revision)
                }
                _ => return Err(unavailable("policy initialization is missing or repeated")),
            };
            if idem_key.as_deref() != Some(expected_key.as_str()) {
                return Err(unavailable("policy idempotence key mismatch"));
            }
            validate_state(&envelope.state, envelope.at_ms)?;
            latest = Some(Entry {
                version: PolicyVersion {
                    revision: event.seq,
                    state: envelope.state,
                },
                hash: event.hash,
                previous_policy: envelope.previous_policy,
                operation: envelope.operation,
            });
        }
        Ok(latest)
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
