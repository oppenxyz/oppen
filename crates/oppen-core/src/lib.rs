//! oppen core.
//!
//! The append-only, hash-chained event ledger (the single source for
//! `get_events`, the activity stream and the audit export), the guardrail
//! engine that runs immediately before signing, the kill switch and
//! dead-man's switch, condition alerts, the per-agent journal, and the
//! quant feature computations agents read through MCP.
//!
//! Guardrails live here and nowhere else. See `AGENTS.md` invariant 1.
//!
//! This crate holds no Tauri or webview dependency and must not acquire one:
//! `docs/decisions.md` R1 keeps it able to run headless with the console as a
//! client.

pub mod accounts;
pub mod alert;
pub mod book;
pub mod candles;
pub mod features;
pub mod feed;
pub mod guardrail;
pub mod journal;
pub mod keys;
pub mod ledger;
pub mod live_chart;
pub mod market;
pub mod operator;
pub mod reconcile;
pub mod state;
pub mod tca;

pub use oppen_hl::Network;

/// Where oppen keeps its per-network state. One database file and one hash
/// chain per network (`docs/decisions.md` R4): the ledger rowid is the agent's
/// `get_events` cursor, so a shared file would make testnet row 4,812 and
/// mainnet row 4,812 the same cursor position.
pub fn db_file_name(network: Network) -> &'static str {
    match network {
        Network::Testnet => "testnet.db",
        Network::Mainnet => "mainnet.db",
    }
}
