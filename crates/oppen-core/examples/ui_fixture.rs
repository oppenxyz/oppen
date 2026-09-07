//! Reproducible console QA data (spec #31/#32). No engine, keys or venue calls.
//! Pass a NEW directory, then launch the review app with OPPEN_DATA_DIR set to it.
use oppen_core::Network;
use oppen_core::guardrail::{AgentGuardrails, AgentId, PersistedState};
use oppen_core::ledger::{EventKind, Ledger, NewEvent};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args_os()
        .nth(1)
        .ok_or("pass a new fixture directory")?;
    // Fail if it already exists: never seed or overwrite a real gateway directory.
    std::fs::create_dir(&dir)?;
    let dir = std::path::Path::new(&dir);
    let ledger = Ledger::open(dir, Network::Testnet)?;
    // Deliberately unsigned legacy inspection data, never runtime authority.
    let store = rusqlite::Connection::open(dir.join("guardrails-testnet.db"))?;
    store.execute_batch(
        "CREATE TABLE guardrail_config (agent TEXT PRIMARY KEY, config_json TEXT NOT NULL);
        CREATE TABLE guardrail_state (key TEXT PRIMARY KEY, state_json TEXT NOT NULL);",
    )?;
    let agent = AgentId::new("UI-FIXTURE-ALPHA");
    let policy = AgentGuardrails::default();
    store.execute(
        "INSERT INTO guardrail_config VALUES (?1, ?2)",
        [agent.as_str(), &serde_json::to_string(&policy)?],
    )?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let state = PersistedState::paused(u64::try_from(now)?);
    for (key, value) in [
        ("kill_switch", serde_json::to_value(&state.kill)?),
        (
            "account_limits",
            serde_json::to_value(state.account_limits)?,
        ),
    ] {
        store.execute(
            "INSERT INTO guardrail_state VALUES (?1, ?2)",
            [key, &serde_json::to_string(&value)?],
        )?;
    }
    for (kind, payload) in [
        (
            EventKind::AgentDecision,
            json!({"reason": "UI fixture: <b>agent-authored claim</b>; no order sent.", "symbol": "BTC"}),
        ),
        (
            EventKind::Refusal,
            json!({"reason": "UI fixture: inspect a proposed $30 order; no order sent.", "refusal": "UI fixture: order notional $30 exceeds the $25 limit.", "observed_usd": "30", "limit_usd": "25"}),
        ),
    ] {
        ledger.append(&NewEvent {
            kind,
            ts_ms: now,
            agent_id: Some("UI-FIXTURE-ALPHA"),
            payload: &payload,
            snapshot: None,
        })?;
    }
    println!("UI fixture only: {}", dir.display());
    Ok(())
}
