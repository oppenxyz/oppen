//! Read-only console projection of the gateway's existing files (spec #31/#32).
//! It neither starts an engine nor infers live pairing/approval state from disk.

use std::path::Path;

use serde::Serialize;

use crate::Network;
use crate::guardrail::{PersistedState, SqliteGuardrailStore};
use crate::ledger::{EventPage, Ledger};

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
    policy: SourceRead<PersistedState>,
}

/// Read the latest 200 event rows and the stored policy independently. Failure
/// of one source must not hide the other. No database or policy defaults are
/// created, and no stored halt is presented as proof that cancels completed.
pub fn read(dir: &Path, network: Network) -> OperatorRead {
    let ledger = Ledger::open_readonly(dir, network).and_then(|ledger| {
        let head = ledger.chain_head()?;
        ledger.get_events(head.seq.saturating_sub(200), 200)
    });
    let policy_file = match network {
        Network::Testnet => "guardrails-testnet.db",
        Network::Mainnet => "guardrails-mainnet.db",
    };
    OperatorRead {
        network,
        ledger: SourceRead::from_result(ledger),
        policy: SourceRead::from_result(SqliteGuardrailStore::read_snapshot(dir.join(policy_file))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::{AgentGuardrails, AgentId, GuardrailStore};
    use crate::ledger::{EventKind, NewEvent};
    use serde_json::json;
    use tempfile::TempDir;

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
            let policy = SqliteGuardrailStore::open(dir.path().join("guardrails-testnet.db"))
                .expect("policy");
            policy
                .save_guardrails(&AgentId::new("alpha"), &AgentGuardrails::default())
                .expect("save policy");
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
        assert!(policy.guardrails.contains_key(&AgentId::new("alpha")));
        assert_eq!(
            serde_json::to_value(&policy).expect("serialize")["guardrails"]["alpha"]["max_order_usd"],
            "25"
        );
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
