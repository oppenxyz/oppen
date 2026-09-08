//! Authenticated operator grants for routing and signer identity.
//! Legacy account metadata is not an authorization source.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use oppen_hl::{Address, Network};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EventKind, Ledger, LedgerError, NewEvent, Owner, OwnerType, SubAccount};
use crate::guardrail::AgentId;
use crate::keys::{AgentWallet, HmacKey, MAX_GENERATION, checked_agent_id};

type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryBinding {
    pub agent: AgentId,
    pub container: Address,
    #[serde(deserialize_with = "required_vault")]
    pub vault_address: Option<Address>,
    pub wallet: AgentWallet,
}

fn required_vault<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Address>, D::Error> {
    Option::<Address>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedRoute {
    pub network: Network,
    pub binding_seq: u64,
    pub binding: RegistryBinding,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("route authority unavailable: {detail}")]
    Unavailable { detail: String },
}

impl From<rusqlite::Error> for RegistryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Ledger(error.into())
    }
}

fn unavailable(detail: impl Into<String>) -> RegistryError {
    RegistryError::Unavailable {
        detail: detail.into(),
    }
}

/// Operator capability. No cached authority, implicit adoption, or owner lease.
#[derive(Debug)]
pub struct RegistryJournal {
    ledger: Arc<Ledger>,
    key: Arc<HmacKey>,
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
    RegistryGranted {
        binding: RegistryBinding,
    },
    RegistryRetired {
        binding_seq: u64,
        binding_hash: String,
    },
}

impl Authority {
    fn kind(&self) -> EventKind {
        match self {
            Self::RegistryGranted { .. } => EventKind::RegistryGranted,
            Self::RegistryRetired { .. } => EventKind::RegistryRetired,
        }
    }
}

struct Grant {
    route: AuthorizedRoute,
    hash: String,
    at_ms: u64,
    retired: bool,
}

fn timestamp(at_ms: u64) -> Result<i64> {
    i64::try_from(at_ms).map_err(|_| unavailable("registry timestamp out of range"))
}

fn validate_binding(binding: &RegistryBinding) -> Result<()> {
    checked_agent_id(&binding.agent)
        .map_err(|_| unavailable("invalid registry agent identifier"))?;
    if binding.container == Address::ZERO
        || binding.wallet.address == Address::ZERO
        || binding.wallet.address == binding.container
        || binding
            .vault_address
            .is_some_and(|address| address != binding.container)
        || binding.wallet.generation > MAX_GENERATION
        || binding.wallet.approved_at_ms >= binding.wallet.valid_until_ms
    {
        return Err(unavailable("invalid registry container, vault, or wallet"));
    }
    timestamp(binding.wallet.approved_at_ms)?;
    timestamp(binding.wallet.valid_until_ms)?;
    Ok(())
}

fn valid_at(binding: &RegistryBinding, at_ms: u64) -> Result<()> {
    timestamp(at_ms)?;
    if at_ms < binding.wallet.approved_at_ms || at_ms >= binding.wallet.valid_until_ms {
        return Err(unavailable("authorized wallet is not valid at this time"));
    }
    Ok(())
}

fn message(envelope: &Envelope) -> Result<String> {
    Ok(super::hash::canonical_json(&serde_json::json!({
        "domain": "oppen.registry-authority.v1",
        "envelope": envelope,
    }))?)
}

fn signed(key: &HmacKey, envelope: Envelope) -> Result<Value> {
    let mac = key
        .sign(message(&envelope)?.as_bytes())
        .map_err(|_| unavailable("registry MAC computation failed"))?;
    serde_json::to_value(Signed {
        envelope,
        mac: hex::encode(mac),
    })
    .map_err(|_| unavailable("registry payload serialization failed"))
}

impl RegistryJournal {
    /// Empty history has no MAC witness to distinguish keys; existing history
    /// must verify before this capability is returned.
    pub fn open(ledger: Arc<Ledger>, key: Arc<HmacKey>) -> Result<Self> {
        {
            let mut guard = ledger.lock()?;
            let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
            replay(&ledger, &tx, &key)?;
        }
        Ok(Self { ledger, key })
    }

    /// Explicit adoption of an operator-supplied route, never inferred from
    /// mutable legacy metadata. An identical live grant is idempotent.
    ///
    /// An error after commit can leave durable live authority: anchor
    /// publication follows the atomic event/projection commit. This journal
    /// has no failure cache or automatic revocation. Callers must verify the
    /// durable outcome before activation or retry, not infer absence from Err.
    /// An idempotent retry reports success only after publishing the verified
    /// current head; a continuing anchor failure still returns an error.
    pub fn grant(&self, binding: RegistryBinding, at_ms: u64) -> Result<AuthorizedRoute> {
        validate_binding(&binding)?;
        valid_at(&binding, at_ms)?;
        let mut guard = self.ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let grants = replay(&self.ledger, &tx, &self.key)?;
        if let Some(existing) = grants
            .values()
            .find(|grant| !grant.retired && grant.route.binding == binding)
        {
            let route = existing.route.clone();
            self.publish_verified_head(tx)?;
            return Ok(route);
        }
        if grants.values().any(|grant| {
            grant.route.binding.container == binding.container
                || grant.route.binding.wallet.address == binding.wallet.address
                || (!grant.retired && grant.route.binding.agent == binding.agent)
        }) {
            return Err(unavailable(
                "registry agent is live or container/signer was previously granted",
            ));
        }
        let owner = registry_owner(&binding);
        let mut accounts = projection_accounts(&tx)?;
        if accounts.iter().any(|(address, account)| {
            account.active
                && account.owner.as_ref() == Some(&owner)
                && *address != binding.container
        }) {
            return Err(unavailable(
                "legacy active agent metadata names another container",
            ));
        }
        let mut projection = match accounts.remove(&binding.container) {
            Some(account) => {
                if !account.active
                    || account
                        .owner
                        .as_ref()
                        .is_some_and(|existing| existing != &owner)
                {
                    return Err(unavailable(
                        "legacy container is retired or owned by another identity",
                    ));
                }
                account
            }
            None => SubAccount {
                address: binding.container.to_string(),
                name: binding.agent.as_str().to_owned(),
                owner: None,
                recorded: true,
                provisioned_by_oppen: false,
                active: true,
                created_ts_ms: timestamp(at_ms)?,
            },
        };
        projection.owner = Some(owner);
        projection.recorded = true;
        super::write_sub_account_on(&tx, &projection)?;
        let (head_seq, prev_hash) = super::head(&tx)?;
        let seq = head_seq.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
        let payload = signed(
            &self.key,
            Envelope {
                version: 1,
                network: self.ledger.network,
                seq,
                prev_hash,
                at_ms,
                authority: Authority::RegistryGranted {
                    binding: binding.clone(),
                },
            },
        )?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::RegistryGranted,
                ts_ms: timestamp(at_ms)?,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
            &format!("registry_container:{}", binding.container),
        )?
        .ok_or_else(|| unavailable("registry grant key without authenticated grant"))?;
        tx.commit()?;
        self.ledger.note_head(&appended)?;
        Ok(AuthorizedRoute {
            network: self.ledger.network,
            binding_seq: seq,
            binding,
        })
    }

    /// Retire the exact grant permanently. As with grant, an error publishing
    /// the anchor can follow a committed retirement. Verify the durable outcome
    /// before retrying or reporting whether the route remains live.
    /// An already-retired result also publishes the verified current head.
    pub fn retire(&self, route: &AuthorizedRoute, at_ms: u64) -> Result<bool> {
        if route.network != self.ledger.network {
            return Err(unavailable("registry route belongs to another network"));
        }
        let ts_ms = timestamp(at_ms)?;
        let mut guard = self.ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let grants = replay(&self.ledger, &tx, &self.key)?;
        let grant = grants
            .get(&route.binding_seq)
            .filter(|grant| grant.route == *route)
            .ok_or_else(|| {
                unavailable("registry retirement does not identify an authenticated grant")
            })?;
        if grant.retired {
            self.publish_verified_head(tx)?;
            return Ok(false);
        }
        if at_ms < grant.at_ms {
            return Err(unavailable("registry retirement precedes its grant"));
        }
        let mut projection = projection_accounts(&tx)?
            .remove(&route.binding.container)
            .ok_or_else(|| unavailable("registry projection missing"))?;
        projection.active = false;
        super::write_sub_account_on(&tx, &projection)?;
        let (head_seq, prev_hash) = super::head(&tx)?;
        let seq = head_seq.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
        let payload = signed(
            &self.key,
            Envelope {
                version: 1,
                network: self.ledger.network,
                seq,
                prev_hash,
                at_ms,
                authority: Authority::RegistryRetired {
                    binding_seq: route.binding_seq,
                    binding_hash: grant.hash.clone(),
                },
            },
        )?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind: EventKind::RegistryRetired,
                ts_ms,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
            &format!("registry_retired:{}", route.binding_seq),
        )?
        .ok_or_else(|| unavailable("registry retirement key without authenticated retirement"))?;
        tx.commit()?;
        self.ledger.note_head(&appended)?;
        Ok(true)
    }

    pub fn route_for_agent(&self, agent: &AgentId) -> Result<AuthorizedRoute> {
        let mut guard = self.ledger.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
        self.route_in(&tx, agent)
    }

    // Idempotent success must repair a previous commit-to-anchor failure,
    // including when a later verified row is now the current head.
    fn publish_verified_head(&self, tx: Transaction<'_>) -> Result<()> {
        let (seq, hash) = super::head(&tx)?;
        tx.commit()?;
        self.ledger.note_head(&super::Appended { seq, hash })?;
        Ok(())
    }

    pub(super) fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub(super) fn authority_key(&self) -> &HmacKey {
        &self.key
    }

    /// Caller retains the ledger coordination guard and a stable SQL snapshot.
    pub(super) fn route_in(
        &self,
        connection: &Connection,
        agent: &AgentId,
    ) -> Result<AuthorizedRoute> {
        let grants = replay(&self.ledger, connection, &self.key)?;
        grants
            .into_values()
            .find(|grant| !grant.retired && &grant.route.binding.agent == agent)
            .map(|grant| grant.route)
            .ok_or_else(|| unavailable("agent has no live authenticated registry route"))
    }

    /// Caller keeps the same ledger guard through the cryptographic signing step.
    pub(super) fn verify_route_in(
        &self,
        connection: &Connection,
        expected: &AuthorizedRoute,
        signer: Address,
    ) -> Result<()> {
        let current = self.route_in(connection, &expected.binding.agent)?;
        if current != *expected || signer != current.binding.wallet.address {
            return Err(unavailable(
                "signer route does not match live registry authority",
            ));
        }
        Ok(())
    }
}

fn replay(ledger: &Ledger, connection: &Connection, key: &HmacKey) -> Result<BTreeMap<u64, Grant>> {
    let anchor = ledger
        .anchor
        .as_ref()
        .ok_or_else(|| unavailable("registry authority requires an anchored ledger"))?
        .load()?
        .ok_or_else(|| unavailable("registry anchor missing"))?;
    let report = super::verify::walk(connection, &ledger.genesis, Some(&anchor))?;
    if let Some(broken) = report.first_break {
        return Err(unavailable(format!(
            "registry chain broken at {}: {}",
            broken.seq, broken.reason
        )));
    }
    let mut grants: BTreeMap<u64, Grant> = BTreeMap::new();
    let mut containers = HashSet::new();
    let mut signers = HashSet::new();
    let mut live_agents = HashSet::new();
    let mut statement = connection.prepare(&format!(
        "SELECT {}, idem_key FROM events WHERE kind IN ('registry_granted', 'registry_retired') ORDER BY seq",
        super::SELECT_EVENT_COLUMNS
    ))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let event = super::event_from_row(row)?;
        let idem_key: Option<String> = row.get(12)?;
        let payload = event
            .payload
            .ok_or_else(|| unavailable("required registry authority history redacted"))?;
        let record: Signed = serde_json::from_value(payload.clone())
            .map_err(|_| unavailable("malformed registry authority payload"))?;
        if serde_json::to_value(&record).map_err(|_| unavailable("invalid registry payload"))?
            != payload
        {
            return Err(unavailable(
                "noncanonical or unknown registry authority fields",
            ));
        }
        if record.mac.len() != 64
            || !record
                .mac
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(unavailable("registry MAC is not canonical 32-byte hex"));
        }
        let mac = hex::decode(&record.mac).map_err(|_| unavailable("invalid registry MAC"))?;
        let envelope = record.envelope;
        if !key.verify(message(&envelope)?.as_bytes(), &mac) {
            return Err(unavailable("registry authority MAC mismatch"));
        }
        if envelope.version != 1
            || envelope.network != ledger.network
            || envelope.seq != event.seq
            || envelope.prev_hash != event.prev_hash
            || timestamp(envelope.at_ms)? != event.ts_ms
            || envelope.authority.kind() != event.kind
            || event.agent_id.is_some()
            || event.snapshot_id.is_some()
            || event.snapshot_hash.is_some()
        {
            return Err(unavailable(
                "registry authority envelope does not match its chain row",
            ));
        }
        match envelope.authority {
            Authority::RegistryGranted { binding } => {
                validate_binding(&binding)?;
                valid_at(&binding, envelope.at_ms)?;
                if !containers.insert(binding.container)
                    || !signers.insert(binding.wallet.address)
                    || !live_agents.insert(binding.agent.clone())
                    || idem_key.as_deref()
                        != Some(format!("registry_container:{}", binding.container).as_str())
                {
                    return Err(unavailable(
                        "duplicate registry agent, container, signer, or invalid grant key",
                    ));
                }
                grants.insert(
                    event.seq,
                    Grant {
                        route: AuthorizedRoute {
                            network: ledger.network,
                            binding_seq: event.seq,
                            binding,
                        },
                        hash: event.hash,
                        at_ms: envelope.at_ms,
                        retired: false,
                    },
                );
            }
            Authority::RegistryRetired {
                binding_seq,
                binding_hash,
            } => {
                let grant = grants
                    .get_mut(&binding_seq)
                    .ok_or_else(|| unavailable("registry retirement without earlier grant"))?;
                if grant.retired
                    || grant.hash != binding_hash
                    || envelope.at_ms < grant.at_ms
                    || idem_key.as_deref()
                        != Some(format!("registry_retired:{binding_seq}").as_str())
                {
                    return Err(unavailable("invalid registry retirement linkage"));
                }
                grant.retired = true;
                live_agents.remove(&grant.route.binding.agent);
            }
        }
    }
    validate_projection(connection, &grants)?;
    Ok(grants)
}

fn registry_owner(binding: &RegistryBinding) -> Owner {
    Owner {
        owner_type: OwnerType::Agent,
        owner_id: binding.agent.as_str().to_owned(),
    }
}

fn validate_projection(connection: &Connection, grants: &BTreeMap<u64, Grant>) -> Result<()> {
    let accounts = projection_accounts(connection)?;
    for grant in grants.values() {
        let expected = registry_owner(&grant.route.binding);
        let address = grant.route.binding.container;
        let projection = accounts
            .get(&address)
            .ok_or_else(|| unavailable("authenticated registry projection missing"))?;
        if projection.owner.as_ref() != Some(&expected)
            || projection.active == grant.retired
            || !projection.recorded
        {
            return Err(unavailable(
                "registry projection owner, active, or recording flag contradicts authenticated history",
            ));
        }
        if !grant.retired
            && accounts.iter().any(|(other_address, account)| {
                account.active
                    && account.owner.as_ref() == Some(&expected)
                    && *other_address != address
            })
        {
            return Err(unavailable(
                "ambiguous active agent metadata contradicts registry authority",
            ));
        }
    }
    Ok(())
}

fn projection_accounts(connection: &Connection) -> Result<HashMap<Address, SubAccount>> {
    let mut accounts = HashMap::new();
    for account in super::sub_accounts_on(connection)? {
        let address = Address::parse(&account.address)
            .map_err(|_| unavailable("invalid registry metadata address"))?;
        if accounts.insert(address, account).is_some() {
            return Err(unavailable("duplicate registry metadata address aliases"));
        }
    }
    Ok(accounts)
}

/// Keyless protection of generic metadata mutations, never an authorization
/// lookup. Parse all required rows before returning even a negative answer.
pub(super) fn managed_on(connection: &Connection, address: &str) -> super::Result<bool> {
    let inspect = || -> Result<bool> {
        let requested = Address::parse(address)
            .map_err(|_| unavailable("invalid registry container address"))?;
        let mut grants: BTreeMap<u64, (Address, String, bool)> = BTreeMap::new();
        let mut statement = connection.prepare(&format!(
            "SELECT {} FROM events WHERE kind IN ('registry_granted', 'registry_retired') ORDER BY seq",
            super::SELECT_EVENT_COLUMNS
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let event = super::event_from_row(row)?;
            let payload = event
                .payload
                .ok_or_else(|| unavailable("registry lifecycle redacted"))?;
            let signed: Signed = serde_json::from_value(payload.clone())
                .map_err(|_| unavailable("malformed registry lifecycle"))?;
            if serde_json::to_value(&signed)
                .map_err(|_| unavailable("invalid registry lifecycle"))?
                != payload
                || signed.mac.len() != 64
                || !signed
                    .mac
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(unavailable("malformed registry signed envelope"));
            }
            let envelope = signed.envelope;
            if envelope.version != 1
                || envelope.seq != event.seq
                || envelope.prev_hash != event.prev_hash
                || timestamp(envelope.at_ms)? != event.ts_ms
                || envelope.authority.kind() != event.kind
                || event.agent_id.is_some()
                || event.snapshot_id.is_some()
                || event.snapshot_hash.is_some()
            {
                return Err(unavailable("registry envelope does not match row"));
            }
            match envelope.authority {
                Authority::RegistryGranted { binding } => {
                    validate_binding(&binding)?;
                    valid_at(&binding, envelope.at_ms)?;
                    grants.insert(event.seq, (binding.container, event.hash, false));
                }
                Authority::RegistryRetired {
                    binding_seq,
                    binding_hash,
                } => {
                    let grant = grants
                        .get_mut(&binding_seq)
                        .ok_or_else(|| unavailable("registry retirement has no grant"))?;
                    if grant.1 != binding_hash || grant.2 {
                        return Err(unavailable("invalid registry retirement"));
                    }
                    grant.2 = true;
                }
            }
        }
        Ok(grants
            .values()
            .any(|(container, _, _)| *container == requested))
    };
    inspect().map_err(|error| match error {
        RegistryError::Ledger(error) => error,
        RegistryError::Unavailable { .. } => LedgerError::UseRegistryJournal,
    })
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
