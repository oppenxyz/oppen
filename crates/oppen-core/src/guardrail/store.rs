//! ES18 authenticated persistence and keyless legacy review.
//! Legacy rows are evidence only, never an execution-policy fallback.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};

use super::AgentId;
use super::config::{AgentGuardrails, LossLimits};
use super::kill::{Engagement, KillReason, KillScope, KillSwitch};
use crate::Network;
pub use crate::ledger::PolicyVersion;
use crate::ledger::{PolicyError, PolicyJournal};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("policy json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("policy source: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error("the store lock was poisoned")]
    Poisoned,
    #[error("policy revision changed: expected {expected}, found {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("invalid legacy policy: {0}")]
    InvalidLegacy(String),
    #[error("invalid policy state: {0}")]
    InvalidState(String),
    #[error("legacy policy changed since operator review")]
    LegacyChanged,
}

/// Complete replacement state. Constructing this value grants no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedState {
    pub guardrails: BTreeMap<AgentId, AgentGuardrails>,
    pub kill: KillSwitch,
    pub account_limits: LossLimits,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            guardrails: BTreeMap::new(),
            kill: KillSwitch::new(),
            account_limits: LossLimits::UNSET,
        }
    }
}

impl PersistedState {
    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        validate_policy_state(self)
    }

    /// Explicit starting value for reviewed initialization. Callers supply their
    /// complete agent policies; the journal still requires review evidence.
    pub fn paused(at_ms: u64) -> Self {
        let mut state = Self::default();
        state.kill.engage(
            KillScope::Global,
            Engagement {
                engaged_at_ms: at_ms,
                reason: KillReason::Operator,
            },
        );
        state
    }
}

pub trait GuardrailStore: Send + Sync {
    fn load(&self) -> Result<PolicyVersion, StoreError>;
    fn compare_exchange(
        &self,
        expected_revision: u64,
        next: &PersistedState,
        at_ms: u64,
    ) -> Result<PolicyVersion, StoreError>;
}

/// The execution adapter owns authenticated authority, not a legacy connection.
/// Construction deliberately does not replay policy so cleanup can be built.
#[derive(Debug)]
pub struct SqliteGuardrailStore {
    journal: Arc<PolicyJournal>,
}

impl SqliteGuardrailStore {
    pub fn new(journal: Arc<PolicyJournal>) -> Self {
        Self { journal }
    }

    /// Structurally complete, but UNVERIFIED legacy inspection. Missing fields
    /// are errors, never an unengaged kill or an unlimited account.
    pub(crate) fn read_snapshot(
        path: impl AsRef<Path>,
        network: Network,
        observed_at_ms: u64,
    ) -> Result<PersistedState, StoreError> {
        let review = LegacyPolicyReview::open(path, network, observed_at_ms)?;
        review.inspect()
    }
}

impl GuardrailStore for SqliteGuardrailStore {
    fn load(&self) -> Result<PolicyVersion, StoreError> {
        Ok(self.journal.current()?)
    }

    fn compare_exchange(
        &self,
        expected_revision: u64,
        next: &PersistedState,
        at_ms: u64,
    ) -> Result<PolicyVersion, StoreError> {
        Ok(self
            .journal
            .replace(expected_revision, next.clone(), at_ms)?)
    }
}

/// Raw SQL values preserve malformed JSON, duplicate keys and non-text bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum LegacyCell {
    Null,
    Integer(String),
    RealBits(String),
    Text(String),
    TextBytes(String),
    Blob(String),
}

impl LegacyCell {
    fn from_sql(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(value) => Self::Integer(value.to_string()),
            ValueRef::Real(value) => Self::RealBits(format!("{:016x}", value.to_bits())),
            ValueRef::Text(value) => match std::str::from_utf8(value) {
                Ok(text) => Self::Text(text.to_owned()),
                Err(_) => Self::TextBytes(hex::encode(value)),
            },
            ValueRef::Blob(value) => Self::Blob(hex::encode(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyTable {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<LegacyCell>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacySchema {
    pub kind: String,
    pub name: String,
    pub table: String,
    pub sql: Option<String>,
}

/// Network is the operator-selected context, not an authenticated assertion
/// about an unsigned file. Missing tables differ from present empty tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyPolicyEvidence {
    pub source: String,
    pub network: Network,
    pub observed_at_ms: u64,
    pub fingerprint: String,
    pub file_present: bool,
    pub schema: Vec<LegacySchema>,
    pub tables: Vec<LegacyTable>,
}

/// A retained source snapshot, not a lock against subsequent WAL writes.
#[derive(Debug)]
pub struct LegacyPolicyReview {
    evidence: LegacyPolicyEvidence,
    _connection: Option<Connection>,
}

impl LegacyPolicyReview {
    pub fn open(
        path: impl AsRef<Path>,
        network: Network,
        observed_at_ms: u64,
    ) -> Result<Self, StoreError> {
        let path = canonical_source(path.as_ref())?;
        let source = path
            .to_str()
            .ok_or_else(|| invalid("source is not a UTF-8 path"))?
            .to_owned();
        let present = path.try_exists()?;
        let connection = if present {
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            conn.execute_batch("BEGIN DEFERRED")?;
            Some(conn)
        } else {
            None
        };
        let mut evidence = LegacyPolicyEvidence {
            source,
            network,
            observed_at_ms,
            fingerprint: String::new(),
            file_present: present,
            schema: Vec::new(),
            tables: Vec::new(),
        };
        if let Some(conn) = &connection {
            let mut statement = conn.prepare(
                "SELECT type, name, tbl_name, sql FROM sqlite_schema
                 WHERE name NOT GLOB 'sqlite_*' ORDER BY type, name, tbl_name",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                evidence.schema.push(LegacySchema {
                    kind: row.get(0)?,
                    name: row.get(1)?,
                    table: row.get(2)?,
                    sql: row.get(3)?,
                });
            }
            for entry in &evidence.schema {
                if entry.kind != "table" {
                    continue;
                }
                let quoted = entry.name.replace('"', "\"\"");
                let mut statement = conn.prepare(&format!("SELECT * FROM \"{quoted}\""))?;
                let columns = statement
                    .column_names()
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>();
                let mut rows = statement.query([])?;
                let mut ordered = Vec::new();
                while let Some(row) = rows.next()? {
                    let values = (0..columns.len())
                        .map(|index| row.get_ref(index).map(LegacyCell::from_sql))
                        .collect::<Result<Vec<_>, _>>()?;
                    ordered.push((serde_json::to_string(&values)?, values));
                }
                ordered.sort_by(|a, b| a.0.cmp(&b.0));
                evidence.tables.push(LegacyTable {
                    name: entry.name.clone(),
                    columns,
                    rows: ordered.into_iter().map(|(_, values)| values).collect(),
                });
            }
        }
        // Time records review provenance; it must not make an unchanged source
        // appear changed when reopened immediately before initialization.
        let preimage = serde_json::to_vec(&serde_json::json!({
            "domain": "oppen.legacy-policy-review.v1",
            "source": evidence.source, "network": evidence.network,
            "file_present": evidence.file_present,
            "schema": evidence.schema, "tables": evidence.tables,
        }))?;
        evidence.fingerprint = hex::encode(Sha3_256::digest(preimage));
        Ok(Self {
            evidence,
            _connection: connection,
        })
    }

    pub fn evidence(&self) -> &LegacyPolicyEvidence {
        &self.evidence
    }

    /// Open and compare a FRESH source snapshot; retain the result through the
    /// ledger commit. The earlier review transaction is not adoption evidence.
    pub fn recheck(&self, observed_at_ms: u64) -> Result<Self, StoreError> {
        let fresh = Self::open(&self.evidence.source, self.evidence.network, observed_at_ms)?;
        if fresh.evidence.fingerprint != self.evidence.fingerprint {
            return Err(StoreError::LegacyChanged);
        }
        Ok(fresh)
    }

    fn inspect(&self) -> Result<PersistedState, StoreError> {
        let table = |name: &str, columns: &[&str]| -> Result<&LegacyTable, StoreError> {
            let table = self
                .evidence
                .tables
                .iter()
                .find(|table| table.name == name)
                .ok_or_else(|| invalid(format!("missing {name} table")))?;
            if table.columns.iter().map(String::as_str).collect::<Vec<_>>() != columns {
                return Err(invalid(format!("unexpected {name} columns")));
            }
            Ok(table)
        };
        let configs = table("guardrail_config", &["agent", "config_json"])?;
        let states = table("guardrail_state", &["key", "state_json"])?;
        let mut guardrails = serde_json::Map::new();
        for row in &configs.rows {
            let agent = text_cell(&row[0])?;
            let value = parse_legacy_json(text_cell(&row[1])?)?;
            if guardrails.insert(agent.to_owned(), value).is_some() {
                return Err(invalid("duplicate agent policy"));
            }
        }
        let mut fields = BTreeMap::new();
        for row in &states.rows {
            let key = text_cell(&row[0])?;
            if !matches!(key, "kill_switch" | "account_limits") {
                return Err(invalid(format!("unknown policy state field {key}")));
            }
            if fields
                .insert(key, parse_legacy_json(text_cell(&row[1])?)?)
                .is_some()
            {
                return Err(invalid("duplicate policy state field"));
            }
        }
        let raw = serde_json::json!({
            "guardrails": guardrails,
            "kill": fields.get("kill_switch").ok_or_else(|| invalid("missing kill_switch"))?,
            "account_limits": fields.get("account_limits").ok_or_else(|| invalid("missing account_limits"))?,
        });
        let state: PersistedState = serde_json::from_value(raw.clone())?;
        // Existing nested types have serde defaults/unknown-field tolerance.
        // Inspection must not quietly complete or discard those source fields.
        if serde_json::to_value(&state)? != raw {
            return Err(invalid("missing, unknown or noncanonical policy fields"));
        }
        validate_policy_state(&state)?;
        Ok(state)
    }
}

/// Shared semantic validation for complete typed snapshots. Strict raw field
/// membership/duplicate detection remains the journal decoder's responsibility.
pub(crate) fn validate_policy_state(state: &PersistedState) -> Result<(), StoreError> {
    let bad = |detail: String| StoreError::InvalidState(detail);
    for (agent, config) in &state.guardrails {
        crate::keys::checked_agent_id(agent).map_err(|error| bad(error.to_string()))?;
        config
            .validate()
            .map_err(|(field, detail)| bad(format!("{field}: {detail}")))?;
    }
    for limit in [
        state.account_limits.max_daily_loss_usd,
        state.account_limits.max_drawdown_usd,
    ]
    .into_iter()
    .flatten()
    {
        if limit.is_sign_negative() {
            return Err(bad("account loss limits must not be negative".into()));
        }
    }
    // KillSwitch keeps its maps private. Deserialize its public wire shape
    // rather than weakening that mutation boundary for validation.
    #[derive(Deserialize)]
    struct KillFields {
        global: Option<Engagement>,
        agents: BTreeMap<AgentId, Engagement>,
    }
    let kill: KillFields = serde_json::from_value(serde_json::to_value(&state.kill)?)?;
    for agent in kill.agents.keys() {
        crate::keys::checked_agent_id(agent).map_err(|error| bad(error.to_string()))?;
    }
    for engagement in kill.global.iter().chain(kill.agents.values()) {
        if i64::try_from(engagement.engaged_at_ms).is_err() {
            return Err(bad("kill timestamp is out of range".into()));
        }
        if let KillReason::LossLimit {
            observed_usd,
            limit_usd,
            ..
        } = &engagement.reason
            && (observed_usd.is_sign_negative() || limit_usd.is_sign_negative())
        {
            return Err(bad("kill loss evidence must not be negative".into()));
        }
    }
    Ok(())
}

fn invalid(detail: impl Into<String>) -> StoreError {
    StoreError::InvalidLegacy(detail.into())
}

/// Validate object membership before Value can discard repeated keys. Unlike
/// journal JSON, legacy JSON need not have canonical whitespace or key order.
fn parse_legacy_json(raw: &str) -> Result<serde_json::Value, StoreError> {
    struct UniqueKeys;
    impl<'de> Deserialize<'de> for UniqueKeys {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = UniqueKeys;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("JSON without duplicate object keys")
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<UniqueKeys, E> {
                    Ok(UniqueKeys)
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<UniqueKeys, A::Error> {
                    while seq.next_element::<UniqueKeys>()?.is_some() {}
                    Ok(UniqueKeys)
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<UniqueKeys, A::Error> {
                    let mut keys = std::collections::BTreeSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !keys.insert(key) {
                            return Err(serde::de::Error::custom("duplicate legacy JSON key"));
                        }
                        map.next_value::<UniqueKeys>()?;
                    }
                    Ok(UniqueKeys)
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    let mut parser = serde_json::Deserializer::from_str(raw);
    UniqueKeys::deserialize(&mut parser)?;
    parser.end()?;
    Ok(serde_json::from_str(raw)?)
}

fn text_cell(cell: &LegacyCell) -> Result<&str, StoreError> {
    match cell {
        LegacyCell::Text(value) => Ok(value),
        _ => Err(invalid("policy field is not SQL text")),
    }
}

fn canonical_source(path: &Path) -> Result<PathBuf, StoreError> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let absolute = if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?.join(path)
            };
            let parent = absolute
                .parent()
                .ok_or_else(|| invalid("source has no parent"))?;
            let name = absolute
                .file_name()
                .ok_or_else(|| invalid("source has no filename"))?;
            Ok(canonical_source(parent)?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct MemoryStore {
    state: Mutex<PolicyVersion>,
}

#[cfg(test)]
impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(PolicyVersion {
                revision: 1,
                state: PersistedState::default(),
            }),
        }
    }
}

#[cfg(test)]
impl MemoryStore {
    pub(super) fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
impl GuardrailStore for MemoryStore {
    fn load(&self) -> Result<PolicyVersion, StoreError> {
        Ok(self.state.lock().map_err(|_| StoreError::Poisoned)?.clone())
    }
    fn compare_exchange(
        &self,
        expected_revision: u64,
        next: &PersistedState,
        _at_ms: u64,
    ) -> Result<PolicyVersion, StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        if state.revision != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
                actual: state.revision,
            });
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Io(std::io::Error::other("policy revision exhausted")))?;
        *state = PolicyVersion {
            revision,
            state: next.clone(),
        };
        Ok(state.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE guardrail_config (agent TEXT PRIMARY KEY, config_json TEXT NOT NULL);
            CREATE TABLE guardrail_state (key TEXT PRIMARY KEY, state_json TEXT NOT NULL);
            INSERT INTO guardrail_config VALUES ('alpha', '{\"unknown\":1,\"unknown\":2}');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn missing_source_and_fields_never_become_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let missing = LegacyPolicyReview::open(&path, Network::Testnet, 1).unwrap();
        assert!(!missing.evidence().file_present);
        assert!(!path.exists());
        let _writer = fixture(&path);
        assert!(matches!(missing.recheck(2), Err(StoreError::LegacyChanged)));
        let review = LegacyPolicyReview::open(&path, Network::Testnet, 2).unwrap();
        assert!(review.inspect().is_err());
        assert_eq!(
            review
                .evidence()
                .tables
                .iter()
                .find(|table| table.name == "guardrail_config")
                .unwrap()
                .rows[0][1],
            LegacyCell::Text("{\"unknown\":1,\"unknown\":2}".into())
        );
        assert!(
            review
                .evidence()
                .tables
                .iter()
                .find(|table| table.name == "guardrail_state")
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn legacy_duplicate_global_and_nested_limits_are_rejected_without_losing_raw_evidence() {
        for duplicate_global in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("legacy.db");
            let conn = fixture(&path);
            let state = PersistedState::paused(1);
            let config = AgentGuardrails::default();
            let config_json = serde_json::to_string_pretty(&config).unwrap();
            conn.execute(
                "UPDATE guardrail_config SET config_json = ?1",
                [&config_json],
            )
            .unwrap();
            for (key, value) in [
                ("kill_switch", serde_json::to_value(&state.kill).unwrap()),
                (
                    "account_limits",
                    serde_json::to_value(state.account_limits).unwrap(),
                ),
            ] {
                conn.execute(
                    "INSERT INTO guardrail_state VALUES (?1, ?2)",
                    [key, &serde_json::to_string_pretty(&value).unwrap()],
                )
                .unwrap();
            }
            let before = LegacyPolicyReview::open(&path, Network::Testnet, 1).unwrap();
            assert!(
                before.inspect().is_ok(),
                "ordinary legacy formatting remains valid"
            );
            let duplicate = if duplicate_global {
                let engagement = serde_json::to_string(state.kill.global().unwrap()).unwrap();
                let raw = format!("{{\"global\":{engagement},\"global\":null,\"agents\":{{}}}}");
                conn.execute(
                    "UPDATE guardrail_state SET state_json = ?1 WHERE key = 'kill_switch'",
                    [&raw],
                )
                .unwrap();
                raw
            } else {
                let raw = serde_json::to_string(&config).unwrap().replace(
                    &serde_json::to_string(&config.loss).unwrap(),
                    r#"{"max_daily_loss_usd":"25","max_daily_loss_usd":null,"max_drawdown_usd":null}"#,
                );
                conn.execute("UPDATE guardrail_config SET config_json = ?1", [&raw])
                    .unwrap();
                raw
            };
            let after = LegacyPolicyReview::open(&path, Network::Testnet, 2).unwrap();
            let error = after.inspect().unwrap_err();
            assert!(
                error.to_string().contains("duplicate legacy JSON key"),
                "{error}"
            );
            assert_ne!(before.evidence().fingerprint, after.evidence().fingerprint);
            assert!(
                after
                    .evidence()
                    .tables
                    .iter()
                    .flat_map(|table| &table.rows)
                    .flatten()
                    .any(|cell| cell == &LegacyCell::Text(duplicate.clone()))
            );
        }
    }

    #[test]
    fn legacy_duplicate_detection_descends_into_arrays_and_decodes_key_escapes() {
        assert!(parse_legacy_json(r#"[{"outer":{"limit":1,"\u006cimit":2}}]"#).is_err());
        assert!(
            parse_legacy_json(" { \"z\": [null, true, 1, -2, 1.5, \"text\"], \"a\": {} } ").is_ok()
        );
    }

    #[test]
    fn fresh_review_detects_wal_changes_and_is_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let writer = fixture(&path);
        let review = LegacyPolicyReview::open(&path, Network::Testnet, 1).unwrap();
        assert_eq!(
            review.recheck(2).unwrap().evidence().fingerprint,
            review.evidence().fingerprint
        );
        writer.execute("DELETE FROM guardrail_config", []).unwrap();
        assert!(matches!(review.recheck(3), Err(StoreError::LegacyChanged)));
        assert_eq!(
            review
                .evidence()
                .tables
                .iter()
                .find(|table| table.name == "guardrail_config")
                .unwrap()
                .rows
                .len(),
            1
        );
        assert!(
            review
                ._connection
                .as_ref()
                .unwrap()
                .execute("DELETE FROM guardrail_config", [])
                .is_err()
        );
    }

    #[test]
    fn fingerprint_binds_source_network_schema_and_presence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let writer = fixture(&path);
        let review = LegacyPolicyReview::open(&path, Network::Testnet, 1).unwrap();
        let other = LegacyPolicyReview::open(&path, Network::Mainnet, 1).unwrap();
        assert_ne!(review.evidence().fingerprint, other.evidence().fingerprint);
        let other_path = dir.path().join("other.db");
        let _other_writer = fixture(&other_path);
        assert_ne!(
            review.evidence().fingerprint,
            LegacyPolicyReview::open(&other_path, Network::Testnet, 1)
                .unwrap()
                .evidence()
                .fingerprint
        );
        writer.execute_batch("DROP TABLE guardrail_state;").unwrap();
        assert!(matches!(review.recheck(2), Err(StoreError::LegacyChanged)));
    }

    #[test]
    fn paused_replacement_is_explicit_and_memory_cas_rejects_lost_updates() {
        let next = PersistedState::paused(42);
        assert_eq!(next.kill.global().unwrap().engaged_at_ms, 42);
        assert!(next.guardrails.is_empty());
        let store = MemoryStore::new();
        let initial = store.load().unwrap();
        let committed = store.compare_exchange(initial.revision, &next, 1).unwrap();
        assert!(committed.revision > initial.revision);
        assert!(matches!(
            store.compare_exchange(initial.revision, &initial.state, 2),
            Err(StoreError::RevisionConflict { .. })
        ));
        assert_eq!(store.load().unwrap(), committed);
    }
}
