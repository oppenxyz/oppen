//! Read-only console projection of the gateway's existing files (spec #31/#32).
//! It neither starts an engine nor infers live pairing/approval state from disk.

use std::path::Path;

use serde::Serialize;

use crate::Network;
use crate::guardrail::{PersistedState, SqliteGuardrailStore};
use crate::ledger::{EventPage, Ledger, LedgerError, PolicyJournal};

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum SourceRead<T> {
    Ready { value: T },
    Unavailable { detail: String },
}

impl<T> SourceRead<T> {
    fn from_result<E: std::fmt::Display>(result: Result<T, E>) -> Self {
        match result {
            Ok(value) => Self::Ready { value },
            Err(error) => Self::Unavailable {
                detail: error.to_string(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OperatorRead {
    network: Network,
    ledger: SourceRead<EventPage>,
    policy: SourceRead<PolicyInspection>,
}

/// Neither variant asserts that a runtime has authenticated or activated it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyProvenance {
    UnverifiedLegacy,
    UnverifiedLedger,
}

#[derive(Debug, Serialize)]
pub struct PolicyInspection {
    pub provenance: PolicyProvenance,
    pub revision: Option<u64>,
    pub observed_at_ms: u64,
    pub state: PersistedState,
}

/// Read the latest 200 event rows and inspect stored policy without creation
/// or initialization. An unreadable ledger cannot authorize legacy fallback;
/// no stored halt is presented as proof that cancels completed.
pub fn read(dir: &Path, network: Network) -> OperatorRead {
    let opened = Ledger::open_readonly(dir, network);
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).map_err(|error| error.to_string()));
    let policy =
        observed_at_ms.and_then(|at_ms| inspect_policy(dir, network, opened.as_ref(), at_ms));
    let ledger = opened.and_then(|ledger| {
        let head = ledger.chain_head()?;
        ledger.get_events(head.seq.saturating_sub(200), 200)
    });
    OperatorRead {
        network,
        ledger: SourceRead::from_result(ledger),
        policy: SourceRead::from_result(policy),
    }
}

fn inspect_policy(
    dir: &Path,
    network: Network,
    ledger: Result<&Ledger, &LedgerError>,
    observed_at_ms: u64,
) -> Result<PolicyInspection, String> {
    match ledger {
        Err(LedgerError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
        Ok(ledger) => {
            // An unreadable latest policy is an error, not permission to substitute
            // unsigned legacy state. This inspection never reads an HMAC key.
            if let Some(version) =
                PolicyJournal::inspect(ledger).map_err(|error| error.to_string())?
            {
                return Ok(PolicyInspection {
                    provenance: PolicyProvenance::UnverifiedLedger,
                    revision: Some(version.revision),
                    observed_at_ms,
                    state: version.state,
                });
            }
        }
    }
    let policy_file = match network {
        Network::Testnet => "guardrails-testnet.db",
        Network::Mainnet => "guardrails-mainnet.db",
    };
    let state = SqliteGuardrailStore::read_snapshot(dir.join(policy_file), network, observed_at_ms)
        .map_err(|error| error.to_string())?;
    Ok(PolicyInspection {
        provenance: PolicyProvenance::UnverifiedLegacy,
        revision: None,
        observed_at_ms,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::{AgentGuardrails, AgentId};
    use crate::ledger::{EventKind, NewEvent};
    use serde_json::json;
    use tempfile::TempDir;

    fn legacy_fixture(dir: &Path) {
        let conn = rusqlite::Connection::open(dir.join("guardrails-testnet.db")).expect("fixture");
        conn.execute_batch(
            "CREATE TABLE guardrail_config (agent TEXT PRIMARY KEY, config_json TEXT NOT NULL);
            CREATE TABLE guardrail_state (key TEXT PRIMARY KEY, state_json TEXT NOT NULL);",
        )
        .expect("schema");
        let state = PersistedState::paused(1);
        conn.execute(
            "INSERT INTO guardrail_config VALUES ('alpha', ?1)",
            [serde_json::to_string(&AgentGuardrails::default()).unwrap()],
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
                [key, &serde_json::to_string(&value).unwrap()],
            )
            .unwrap();
        }
    }

    #[test]
    fn unreadable_ledger_never_falls_back_to_valid_legacy_policy() {
        for failure in ["corrupt", "newer_schema", "wrong_network"] {
            let dir = TempDir::new().unwrap();
            legacy_fixture(dir.path());
            match failure {
                "corrupt" => {
                    std::fs::write(dir.path().join("testnet.db"), b"not a SQLite database").unwrap()
                }
                "newer_schema" => {
                    drop(Ledger::open(dir.path(), Network::Testnet).unwrap());
                    let conn = rusqlite::Connection::open(dir.path().join("testnet.db")).unwrap();
                    conn.pragma_update(None, "user_version", 999).unwrap();
                }
                "wrong_network" => {
                    drop(Ledger::open(dir.path(), Network::Mainnet).unwrap());
                    std::fs::copy(dir.path().join("mainnet.db"), dir.path().join("testnet.db"))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let result = read(dir.path(), Network::Testnet);
            let SourceRead::Unavailable {
                detail: ledger_error,
            } = result.ledger
            else {
                panic!("{failure}: ledger unexpectedly readable")
            };
            let SourceRead::Unavailable {
                detail: policy_error,
            } = result.policy
            else {
                panic!("{failure}: substituted valid legacy policy")
            };
            assert_eq!(
                policy_error, ledger_error,
                "{failure}: preserve original error"
            );
        }
    }

    #[test]
    fn explicit_missing_ledger_allows_unverified_legacy_inspection() {
        let dir = TempDir::new().unwrap();
        legacy_fixture(dir.path());
        let result = read(dir.path(), Network::Testnet);
        assert!(matches!(result.ledger, SourceRead::Unavailable { .. }));
        let SourceRead::Ready { value } = result.policy else {
            panic!("legacy inspection unavailable")
        };
        assert!(matches!(
            value.provenance,
            PolicyProvenance::UnverifiedLegacy
        ));
        assert_eq!(value.revision, None);
        assert!(!dir.path().join("testnet.db").exists());
    }

    #[test]
    fn missing_legacy_kill_is_unknown_not_unpaused() {
        let dir = TempDir::new().unwrap();
        legacy_fixture(dir.path());
        let conn = rusqlite::Connection::open(dir.path().join("guardrails-testnet.db")).unwrap();
        conn.execute("DELETE FROM guardrail_state WHERE key = 'kill_switch'", [])
            .unwrap();
        let result = read(dir.path(), Network::Testnet);
        let SourceRead::Unavailable { detail } = result.policy else {
            panic!("missing kill became policy")
        };
        assert!(detail.contains("missing kill_switch"));
    }

    #[test]
    fn unverified_ledger_contract_has_revision_not_runtime_claim() {
        let value = PolicyInspection {
            provenance: PolicyProvenance::UnverifiedLedger,
            revision: Some(17),
            observed_at_ms: 42,
            state: PersistedState::paused(1),
        };
        let json = serde_json::to_value(SourceRead::Ready { value }).unwrap();
        assert_eq!(json["status"], "ready");
        assert_eq!(json["value"]["provenance"], "unverified_ledger");
        assert_eq!(json["value"]["revision"], 17);
        assert_eq!(json["value"]["observed_at_ms"], 42);
        assert!(json["value"].get("state").is_some());
    }

    #[test]
    fn persisted_policy_is_unverified_and_corruption_never_falls_back_to_legacy() {
        use crate::guardrail::LegacyPolicyReview;
        use crate::keys::HmacKey;
        use crate::ledger::RegistryJournal;
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        legacy_fixture(dir.path());
        let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
        let registry = Arc::new(
            RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([5; 32]))).unwrap(),
        );
        let journal = PolicyJournal::new(registry);
        let review = LegacyPolicyReview::open(
            dir.path().join("guardrails-testnet.db"),
            Network::Testnet,
            1,
        )
        .unwrap();
        let replacement = PersistedState::paused(2);
        let version = journal.initialize(&review, replacement.clone(), 2).unwrap();
        let inspected = read(dir.path(), Network::Testnet);
        let SourceRead::Ready { value } = inspected.policy else {
            panic!("policy unavailable")
        };
        assert!(matches!(
            value.provenance,
            PolicyProvenance::UnverifiedLedger
        ));
        assert_eq!(value.revision, Some(version.revision));
        assert_eq!(value.state, replacement);

        let conn = rusqlite::Connection::open(dir.path().join("testnet.db")).unwrap();
        conn.execute(
            "UPDATE events SET payload = '{}' WHERE seq = ?1",
            [version.revision],
        )
        .unwrap();
        assert!(matches!(
            read(dir.path(), Network::Testnet).policy,
            SourceRead::Unavailable { .. }
        ));
    }

    #[test]
    fn reads_existing_events_and_policy_without_changing_them() {
        let dir = TempDir::new().expect("directory");
        let payload = json!({"reason": "<script>agent claim</script>", "observed_usd": "30", "limit_usd": "25"});
        let event = NewEvent {
            kind: EventKind::AgentDecision,
            ts_ms: 1000,
            agent_id: Some("alpha"),
            payload: &payload,
            snapshot: None,
        };
        {
            let ledger = Ledger::open(dir.path(), Network::Testnet).expect("writer");
            ledger.append(&event).expect("event");
            legacy_fixture(dir.path());
        }
        let before = std::fs::read(dir.path().join("testnet.db")).expect("before");
        let result = read(dir.path(), Network::Testnet);
        let SourceRead::Ready { value: page } = result.ledger else {
            panic!("ledger unavailable")
        };
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].payload.as_ref(), Some(&payload));
        let SourceRead::Ready { value: policy } = result.policy else {
            panic!("policy unavailable")
        };
        assert!(policy.state.guardrails.contains_key(&AgentId::new("alpha")));
        assert_eq!(
            serde_json::to_value(&policy).expect("serialize")["state"]["guardrails"]["alpha"]["max_order_usd"],
            "25"
        );
        let encoded = serde_json::to_value(&policy).expect("inspection");
        assert_eq!(encoded["provenance"], "unverified_legacy");
        assert!(encoded["revision"].is_null());
        assert!(encoded["observed_at_ms"].is_u64());
        assert_eq!(encoded.as_object().expect("object").len(), 4);
        let reader = Ledger::open_readonly(dir.path(), Network::Testnet).expect("reader");
        assert!(
            reader.append(&event).is_err(),
            "SQLite must refuse writes through the reader"
        );
        assert_eq!(reader.get_events(0, 10).expect("page").head_seq, 1);
        drop(reader);
        assert_eq!(
            std::fs::read(dir.path().join("testnet.db")).expect("after"),
            before
        );
    }

    #[test]
    fn missing_sources_stay_missing_and_policy_failure_does_not_hide_events() {
        let dir = TempDir::new().expect("directory");
        let absent = read(dir.path(), Network::Testnet);
        assert!(matches!(absent.ledger, SourceRead::Unavailable { .. }));
        assert!(matches!(absent.policy, SourceRead::Unavailable { .. }));
        assert_eq!(std::fs::read_dir(dir.path()).expect("contents").count(), 0);
        drop(Ledger::open(dir.path(), Network::Testnet).expect("writer"));
        let partial = read(dir.path(), Network::Testnet);
        assert!(matches!(partial.ledger, SourceRead::Ready { .. }));
        assert!(matches!(partial.policy, SourceRead::Unavailable { .. }));
    }

    #[test]
    fn renamed_testnet_file_is_not_accepted_as_mainnet() {
        let dir = TempDir::new().expect("directory");
        drop(Ledger::open(dir.path(), Network::Testnet).expect("writer"));
        std::fs::copy(dir.path().join("testnet.db"), dir.path().join("mainnet.db")).expect("copy");
        let result = read(dir.path(), Network::Mainnet);
        let SourceRead::Unavailable { detail } = result.ledger else {
            panic!("wrong network accepted")
        };
        assert!(detail.contains("belongs to testnet"));
    }
}
