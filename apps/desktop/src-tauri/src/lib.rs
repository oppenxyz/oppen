//! Tauri commands for the operator console.
//!
//! Thin by construction: every command assembles nothing itself and calls into
//! `oppen-core`, which holds no Tauri dependency (`docs/decisions.md` R1) so
//! the core can later run headless with this console as one of its clients.
//! The MCP `get_state` tool reads the same [`oppen_core::state::assemble`], so
//! the operator and the agent cannot be shown different accounts (A3).

use oppen_core::state::{AccountState, VenueReadings, assemble};
use oppen_hl::{Address, InfoClient, Network};

/// Why the console has nothing to show.
///
/// Rendered to the operator, so each variant names what to do about it rather
/// than what went wrong internally.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
enum ConsoleError {
    /// No account is configured yet. Onboarding has not run.
    NotConfigured(String),
    /// The venue did not answer.
    Venue(String),
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsoleError::NotConfigured(detail) | ConsoleError::Venue(detail) => {
                write!(f, "{detail}")
            }
        }
    }
}

/// The configured account.
///
/// Read from the environment until the onboarding flow persists it. Failing
/// with a named variable is deliberate: an unconfigured console must say so,
/// not show a zeroed account that looks like a real empty one.
fn configured_account() -> Result<Address, ConsoleError> {
    let raw = std::env::var("OPPEN_TESTNET_USER").map_err(|_| {
        ConsoleError::NotConfigured(
            "No account configured. Set OPPEN_TESTNET_USER, or run onboarding.".into(),
        )
    })?;
    raw.parse().map_err(|e| {
        ConsoleError::NotConfigured(format!("OPPEN_TESTNET_USER is not an address: {e}"))
    })
}

/// The account as it stands now: equity, margin, positions, resting orders.
///
/// Four venue reads rather than one, because Hyperliquid publishes no single
/// endpoint that answers it and spot is load-bearing under unified margin.
#[tauri::command]
async fn account_state(network: String) -> Result<AccountState, ConsoleError> {
    let network = match network.as_str() {
        "mainnet" => Network::Mainnet,
        _ => Network::Testnet,
    };
    let account = configured_account()?;
    let info = InfoClient::new(network).map_err(|e| ConsoleError::Venue(e.to_string()))?;

    let perps = info
        .clearinghouse_state(account)
        .await
        .map_err(|e| ConsoleError::Venue(format!("clearinghouse: {e}")))?;
    let spot = info
        .spot_clearinghouse_state(account)
        .await
        .map_err(|e| ConsoleError::Venue(format!("spot balances: {e}")))?;
    let orders = info
        .frontend_open_orders(account)
        .await
        .map_err(|e| ConsoleError::Venue(format!("open orders: {e}")))?;
    // Not `allMids`: it answers for an asset the venue has stopped quoting
    // with a frozen last print, and a liquidation distance computed from one
    // is the fabrication `VenueReadings::mids` exists to refuse.
    let mids = info
        .meta_and_asset_ctxs()
        .await
        .map_err(|e| ConsoleError::Venue(format!("marks: {e}")))?
        .reference_pxs();

    Ok(assemble(
        network,
        account,
        now_ms(),
        &VenueReadings {
            perps: &perps,
            spot: &spot,
            orders: &orders,
            mids: &mids,
            // No socket in the console yet. Reported as never-connected rather
            // than live, so the staleness overlay tells the truth.
            last_tick_ms: None,
        },
    ))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![account_state])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
