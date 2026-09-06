//! Run the gateway and print a pairing token, so the loop can be driven by
//! hand: `cargo run -p oppen-mcp --example serve`.
//!
//! Testnet only. The token is printed because this is the pairing ceremony
//! standing in for the approve dialog of `docs/spec.md` item 15, which lands
//! with the console.

use std::sync::{Arc, RwLock};

use oppen_core::alert::AlertStore;
use oppen_core::feed::FeedSession;
use oppen_core::feed::pump::FeedPump;
use oppen_core::guardrail::{AgentId, GuardrailEngine, SqliteGuardrailStore};
use oppen_core::journal::Journal;
use oppen_core::keys::KeychainKeyStore;
use oppen_core::ledger::{EventViews, Ledger, LedgerAuditSink};
use oppen_core::reconcile::VenueSource;
use oppen_hl::ws::{Subscription, WsPool, WsPoolConfig};
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
    // Item 21's scratchpad, per network for the same reason the ledger is.
    let journal = Arc::new(Journal::open(dir.join("journal-testnet.db"))?);

    // The socket, and the thing that folds it into the session (item 9). The
    // pool must outlive the pump: dropping it closes every connection, which
    // ends the event stream and returns `FeedPump::run`.
    let feed = Arc::new(FeedSession::new());
    // Item 22's alerts, per network for the reason everything is (R4).
    let alerts = Arc::new(AlertStore::open(dir.join("alerts-testnet.db"))?);
    let (pool, mut events) = WsPool::new(WsPoolConfig {
        network: Network::Testnet,
        ..WsPoolConfig::default()
    })?;
    // The two account channels the reconciler can close. Market data is
    // subscribed per request by the tools that need it.
    pool.subscribe(Subscription::UserFills { user: account })?;
    pool.subscribe(Subscription::OrderUpdates { user: account })?;
    let pump_ledger = Arc::clone(&ledger);
    let pump_feed = Arc::clone(&feed);
    let pump_alerts = Arc::clone(&alerts);
    // The pump subscribes an alert's own market feed through the pool, so the
    // two are handed to it together.
    let pump_pool = Arc::new(pool);
    let feeds = Arc::clone(&pump_pool);
    tokio::spawn(async move {
        let source = match VenueSource::new(Network::Testnet) {
            Ok(source) => source,
            Err(error) => return tracing::error!(%error, "no venue source; feed not pumped"),
        };
        match FeedPump::new(
            &pump_feed,
            &pump_ledger,
            account,
            source,
            &pump_alerts,
            feeds.as_ref(),
        ) {
            Ok(pump) => pump.run(&mut events).await,
            Err(error) => tracing::error!(%error, "no feed pump"),
        }
    });
    let gateway = Gateway::new(
        Network::Testnet,
        engine,
        EventViews::new(ledger),
        journal,
        feed,
        alerts,
    )?;
    // Held to the end of `main`: the pool's `Drop` stops every connection.
    let _pool = pump_pool;
    serve(
        port,
        gateway,
        Arc::new(RwLock::new(store)),
        tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    Ok(())
}
