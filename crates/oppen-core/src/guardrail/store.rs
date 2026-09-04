//! Persistence for the state that must survive a restart.
//!
//! Spec item 26 is explicit: kill-switch state persists across restart. A
//! switch that forgets on relaunch is worse than no switch, because the
//! operator believes it is engaged. Guardrail configuration persists for the
//! same reason — a crash must not silently restore an agent to D-c defaults
//! or, worse, to no limits at all.
//!
//! The tables live in the per-network database file (R4: one file and one
//! hash chain per network) and are namespaced `guardrail_*` so they do not
//! collide with the ledger's.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use oppen_hl::Address;
use rusqlite::Connection;

use super::AgentId;
use super::config::{AgentGuardrails, LossLimits};
use super::kill::KillSwitch;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored guardrail state is not valid json: {0}")]
    Json(#[from] serde_json::Error),
    /// A `Mutex` whose holder panicked. Recovered rather than propagated
    /// everywhere else in this module; surfaced here because a poisoned
    /// connection has an unknown transaction state.
    #[error("the store lock was poisoned")]
    Poisoned,
    /// A stored row that does not parse back into the type it was written
    /// from. Reachable only from a hand-edited or corrupted database.
    #[error("stored row in {table} is invalid: {detail}")]
    InvalidRow { table: &'static str, detail: String },
}

/// Everything the engine loads at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedState {
    pub guardrails: BTreeMap<AgentId, AgentGuardrails>,
    pub kill: KillSwitch,
    pub account_limits: LossLimits,
    /// The sub-account each agent trades (D1: the roster is 1:1 with
    /// sub-accounts). Persisted with the guardrails because a clearance is
    /// bound to it: an engine that forgot the binding on restart would sign
    /// against the master account instead of the agent's sub-account, which
    /// is a silent capital-segregation failure rather than an error.
    pub vaults: BTreeMap<AgentId, Address>,
}

impl Default for PersistedState {
    /// `LossLimits::UNSET`, **not** `LossLimits::default()`. The default is
    /// D-c's per-agent budget; applying it account-wide would silently
    /// impose a $25 fleet limit nobody configured — and, because a
    /// configured account-wide limit demands a fleet snapshot, would refuse
    /// every order on a fresh install with
    /// [`super::Unevaluable::MissingFleetState`].
    fn default() -> Self {
        PersistedState {
            guardrails: BTreeMap::new(),
            kill: KillSwitch::new(),
            account_limits: LossLimits::UNSET,
            vaults: BTreeMap::new(),
        }
    }
}

/// Where guardrail configuration and kill-switch state are kept.
///
/// A trait so the engine can be exercised without a filesystem, and so a
/// write failure can be injected — [`super::Unevaluable::StateWriteFailed`]
/// is a fail-closed path that has to be tested, and the only honest way to
/// test it is to make a write actually fail.
pub trait GuardrailStore: Send + Sync {
    fn load(&self) -> Result<PersistedState, StoreError>;
    fn save_guardrails(&self, agent: &AgentId, config: &AgentGuardrails) -> Result<(), StoreError>;
    fn save_kill_switch(&self, kill: &KillSwitch) -> Result<(), StoreError>;
    fn save_account_limits(&self, limits: &LossLimits) -> Result<(), StoreError>;
    /// Binds an agent to the sub-account it trades (D1).
    fn save_vault(&self, agent: &AgentId, vault: &Address) -> Result<(), StoreError>;
}

/// An in-memory store, for tests and for a headless run with no database.
#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<PersistedState>,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore::default()
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut PersistedState) -> T) -> T {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut state)
    }
}

impl GuardrailStore for MemoryStore {
    fn load(&self) -> Result<PersistedState, StoreError> {
        Ok(self.with_state(|s| s.clone()))
    }

    fn save_guardrails(&self, agent: &AgentId, config: &AgentGuardrails) -> Result<(), StoreError> {
        self.with_state(|s| s.guardrails.insert(agent.clone(), config.clone()));
        Ok(())
    }

    fn save_kill_switch(&self, kill: &KillSwitch) -> Result<(), StoreError> {
        self.with_state(|s| s.kill = kill.clone());
        Ok(())
    }

    fn save_account_limits(&self, limits: &LossLimits) -> Result<(), StoreError> {
        self.with_state(|s| s.account_limits = *limits);
        Ok(())
    }

    fn save_vault(&self, agent: &AgentId, vault: &Address) -> Result<(), StoreError> {
        self.with_state(|s| s.vaults.insert(agent.clone(), *vault));
        Ok(())
    }
}

/// Key of the kill-switch row in `guardrail_state`.
const KILL_SWITCH_KEY: &str = "kill_switch";
/// Key of the account-wide loss limits row in `guardrail_state`.
const ACCOUNT_LIMITS_KEY: &str = "account_limits";

/// The real store: two small tables in the per-network database file.
///
/// Configuration is stored as JSON rather than as columns because
/// [`AgentGuardrails`] is a product surface that will gain fields, and a
/// migration per guardrail is a tax on adding one. The cost is that the rows
/// are not queryable by limit, which nothing needs.
///
/// Not authenticated yet. Spec item 3 wants the guardrail config HMAC-checked
/// with a key from the OS keychain, so that editing the database file cannot
/// silently raise a limit; that needs the keychain dependency, which this
/// crate does not have. Until then the threat model is the one
/// `threat-model.md` already concedes for a same-user process.
#[derive(Debug)]
pub struct SqliteGuardrailStore {
    conn: Mutex<Connection>,
}

impl SqliteGuardrailStore {
    /// Opens or creates the store at `path`, which should be the per-network
    /// database file named by [`crate::db_file_name`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS guardrail_config (
                 agent TEXT PRIMARY KEY,
                 config_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS guardrail_state (
                 key TEXT PRIMARY KEY,
                 state_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS guardrail_vault (
                 agent TEXT PRIMARY KEY,
                 vault_address TEXT NOT NULL
             );",
        )?;
        Ok(SqliteGuardrailStore {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn.lock().map_err(|_| StoreError::Poisoned)
    }

    fn put_state(&self, key: &str, json: String) -> Result<(), StoreError> {
        self.conn()?.execute(
            "INSERT INTO guardrail_state (key, state_json) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET state_json = excluded.state_json",
            (key, json),
        )?;
        Ok(())
    }

    fn get_state(&self, key: &str) -> Result<Option<String>, StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT state_json FROM guardrail_state WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }
}

impl GuardrailStore for SqliteGuardrailStore {
    fn load(&self) -> Result<PersistedState, StoreError> {
        let guardrails = {
            let conn = self.conn()?;
            let mut stmt = conn.prepare("SELECT agent, config_json FROM guardrail_config")?;
            let mut rows = stmt.query([])?;
            let mut out = BTreeMap::new();
            while let Some(row) = rows.next()? {
                let agent: String = row.get(0)?;
                let json: String = row.get(1)?;
                out.insert(AgentId::new(agent), serde_json::from_str(&json)?);
            }
            out
        };
        let kill = match self.get_state(KILL_SWITCH_KEY)? {
            Some(json) => serde_json::from_str(&json)?,
            None => KillSwitch::new(),
        };
        let account_limits = match self.get_state(ACCOUNT_LIMITS_KEY)? {
            Some(json) => serde_json::from_str(&json)?,
            None => LossLimits::UNSET,
        };
        let vaults = {
            let conn = self.conn()?;
            let mut stmt = conn.prepare("SELECT agent, vault_address FROM guardrail_vault")?;
            let mut rows = stmt.query([])?;
            let mut out = BTreeMap::new();
            while let Some(row) = rows.next()? {
                let agent: String = row.get(0)?;
                let vault: String = row.get(1)?;
                let vault = Address::parse(&vault).map_err(|e| StoreError::InvalidRow {
                    table: "guardrail_vault",
                    detail: e.to_string(),
                })?;
                out.insert(AgentId::new(agent), vault);
            }
            out
        };
        Ok(PersistedState {
            guardrails,
            kill,
            account_limits,
            vaults,
        })
    }

    fn save_guardrails(&self, agent: &AgentId, config: &AgentGuardrails) -> Result<(), StoreError> {
        let json = serde_json::to_string(config)?;
        self.conn()?.execute(
            "INSERT INTO guardrail_config (agent, config_json) VALUES (?1, ?2)
             ON CONFLICT(agent) DO UPDATE SET config_json = excluded.config_json",
            (agent.as_str(), json),
        )?;
        Ok(())
    }

    fn save_kill_switch(&self, kill: &KillSwitch) -> Result<(), StoreError> {
        self.put_state(KILL_SWITCH_KEY, serde_json::to_string(kill)?)
    }

    fn save_account_limits(&self, limits: &LossLimits) -> Result<(), StoreError> {
        self.put_state(ACCOUNT_LIMITS_KEY, serde_json::to_string(limits)?)
    }

    fn save_vault(&self, agent: &AgentId, vault: &Address) -> Result<(), StoreError> {
        self.conn()?.execute(
            "INSERT INTO guardrail_vault (agent, vault_address) VALUES (?1, ?2)
             ON CONFLICT(agent) DO UPDATE SET vault_address = excluded.vault_address",
            (agent.as_str(), vault.to_string()),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::kill::{Engagement, KillReason, KillScope};

    /// A vault binding that does not survive a restart would silently sign a
    /// sub-account's order against the master account.
    #[test]
    fn a_round_trip_through_sqlite_preserves_the_vault_binding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("testnet.db");
        let agent = AgentId::new("alpha");
        let vault = Address::parse("0x0d1d9635d0640821d15e323ac8adadfa9c111414").expect("address");
        {
            let store = SqliteGuardrailStore::open(&path).expect("open");
            store.save_vault(&agent, &vault).expect("save vault");
        }
        let store = SqliteGuardrailStore::open(&path).expect("reopen");
        assert_eq!(store.load().expect("load").vaults.get(&agent), Some(&vault));
    }

    #[test]
    fn a_round_trip_through_sqlite_preserves_config_and_kill_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("testnet.db");
        let agent = AgentId::new("alpha");
        let mut config = AgentGuardrails::default();
        config.symbols.insert("BTC".to_owned());
        config.max_order_usd = rust_decimal::Decimal::from(500);

        {
            let store = SqliteGuardrailStore::open(&path).expect("open");
            store.save_guardrails(&agent, &config).expect("save config");
            let mut kill = KillSwitch::new();
            kill.engage(
                KillScope::agent(agent.clone()),
                Engagement {
                    engaged_at_ms: 42,
                    reason: KillReason::Operator,
                },
            );
            store.save_kill_switch(&kill).expect("save kill");
        }

        let store = SqliteGuardrailStore::open(&path).expect("reopen");
        let loaded = store.load().expect("load");
        assert_eq!(loaded.guardrails.get(&agent), Some(&config));
        assert_eq!(
            loaded.kill.blocking(&agent).map(|(s, e)| (s, e.clone())),
            Some((
                KillScope::agent(agent.clone()),
                Engagement {
                    engaged_at_ms: 42,
                    reason: KillReason::Operator,
                }
            ))
        );
    }

    #[test]
    fn an_empty_store_loads_defaults_not_an_error() {
        let store = SqliteGuardrailStore::in_memory().expect("open");
        let loaded = store.load().expect("load");
        assert!(loaded.guardrails.is_empty());
        assert!(loaded.kill.global().is_none());
        assert!(loaded.vaults.is_empty());
        assert_eq!(loaded.account_limits, LossLimits::UNSET);
    }
}
