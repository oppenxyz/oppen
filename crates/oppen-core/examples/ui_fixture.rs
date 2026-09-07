//! Reproducible console QA data (spec #31/#32). No engine, keys or venue calls.
//! Pass a NEW directory, then launch the review app with OPPEN_DATA_DIR set to it.
use oppen_core::Network;
use oppen_core::guardrail::{AgentGuardrails, AgentId, GuardrailStore, SqliteGuardrailStore};
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
    let store = SqliteGuardrailStore::open(dir.join("guardrails-testnet.db"))?;
    let agent = AgentId::new("UI-FIXTURE-ALPHA");
    let policy = AgentGuardrails::default();
    store.save_guardrails(&agent, &policy)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
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
