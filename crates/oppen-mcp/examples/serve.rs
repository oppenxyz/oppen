//! Run the gateway and print a pairing token, so the loop can be driven by
//! hand: `cargo run -p oppen-mcp --example serve`.
//!
//! Testnet only. The token is printed because this is the pairing ceremony
//! standing in for the approve dialog of `docs/spec.md` item 15, which lands
//! with the console.

use std::sync::{Arc, RwLock};

use oppen_core::guardrail::{AgentId, GuardrailEngine, SqliteGuardrailStore};
use oppen_core::keys::KeychainKeyStore;
use oppen_core::ledger::{EventViews, Ledger, LedgerAuditSink};
use oppen_mcp::Network;
use oppen_mcp::auth::{Binding, TokenStore};
use oppen_mcp::server::{MCP_PATH, serve};
use oppen_mcp::tools::Gateway;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let port: u16 = std::env::var("OPPEN_MCP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7433);

    let account: oppen_hl::Address = std::env::var("OPPEN_TESTNET_USER")
        .map_err(|_| "set OPPEN_TESTNET_USER (source ~/.oppen/testnet.env)")?
        .parse()?;
    let agent = AgentId::new("agent-alpha");

    // Item 15's approve dialog, as a one-liner: name the agent, bind it to its
    // container, then mint the token. There is no unbound token to mint.
    let mut store = TokenStore::new();
    let issued = store.issue(Binding {
        agent: agent.clone(),
        account,
    })?;
    println!("pairing  {} -> {agent}", issued.id);
    println!("token    {}", issued.reveal());
    println!("url      http://127.0.0.1:{port}{MCP_PATH}");
    println!();
    println!("claude mcp add --transport http oppen http://127.0.0.1:{port}{MCP_PATH} \\");
    println!("  --header \"Authorization: Bearer {}\"", issued.reveal());
    println!();

    // Per-network database (R4): the ledger rowid is an agent's get_events
    // cursor, so a shared file would make two networks share cursor positions.
    let dir = std::path::PathBuf::from(
        std::env::var("OPPEN_DATA_DIR").unwrap_or_else(|_| "/tmp/oppen-dev".into()),
    );
    std::fs::create_dir_all(&dir)?;
    let ledger = Arc::new(Ledger::open_at(
        &dir.join(oppen_core::db_file_name(Network::Testnet)),
        Network::Testnet,
    )?);
    let engine = Arc::new(GuardrailEngine::new(
        Arc::new(SqliteGuardrailStore::open(
            dir.join("guardrails-testnet.db"),
        )?),
        Arc::new(LedgerAuditSink::new(ledger.clone())),
        Arc::new(KeychainKeyStore::new(Network::Testnet)),
        Network::Testnet,
    )?);

    // D-c near-zero defaults: empty allowlist, $25 orders, $100 positions.
    // The first order is refused with the limit to raise named in the reason.
    let guardrails = engine.register_agent(&agent, None, now_ms())?;
    println!("agent    {agent} registered");
    println!("limits   {guardrails:?}");
    println!();

    // One gateway serves every paired agent; each request resolves its own
    // identity from the token (item 15).
    let gateway = Gateway::new(Network::Testnet, engine, EventViews::new(ledger))?;
    serve(
        port,
        gateway,
        Arc::new(RwLock::new(store)),
        tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    Ok(())
}
