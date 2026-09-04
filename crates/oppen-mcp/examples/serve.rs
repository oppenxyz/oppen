//! Run the gateway and print a pairing token, so the loop can be driven by
//! hand: `cargo run -p oppen-mcp --example serve`.
//!
//! Testnet only. The token is printed because this is the pairing ceremony
//! standing in for the approve dialog of `docs/spec.md` item 15, which lands
//! with the console.

use std::sync::{Arc, RwLock};

use oppen_mcp::Network;
use oppen_mcp::auth::TokenStore;
use oppen_mcp::server::{MCP_PATH, serve};
use oppen_mcp::tools::Gateway;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let port: u16 = std::env::var("OPPEN_MCP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7433);

    let mut store = TokenStore::new();
    let issued = store.issue()?;
    println!("pairing  {}", issued.id);
    println!("token    {}", issued.reveal());
    println!("url      http://127.0.0.1:{port}{MCP_PATH}");
    println!();
    println!("claude mcp add --transport http oppen http://127.0.0.1:{port}{MCP_PATH} \\");
    println!("  --header \"Authorization: Bearer {}\"", issued.reveal());
    println!();

    let gateway = Gateway::new(Network::Testnet)?;
    serve(
        port,
        gateway,
        Arc::new(RwLock::new(store)),
        tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    Ok(())
}
