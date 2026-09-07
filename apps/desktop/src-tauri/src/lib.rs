//! Tauri commands for the operator console.
//!
//! Thin by construction: every command assembles nothing itself and calls into
//! `oppen-core`, which holds no Tauri dependency (`docs/decisions.md` R1) so
//! the core can later run headless with this console as one of its clients.
//! The MCP `get_state` tool reads the same [`oppen_core::state::assemble`], so
//! the operator and the agent cannot be shown different accounts (A3).

mod feed;

use std::sync::Mutex;

use feed::ConsoleFeed;
use oppen_core::candles::Interval;
use oppen_core::keys::{KeyStore, KeychainKeyStore};
use oppen_core::market::{ChartSeries, MarketRow, MarketSnapshot, chart, rows, snapshot};
use oppen_core::state::{AccountState, VenueReadings, assemble};
use oppen_hl::{Address, InfoClient, Network};
use tauri::{Manager, State};

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
    /// A local operator data source failed independently of the venue feed.
    LocalState(String),
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsoleError::NotConfigured(detail)
            | ConsoleError::Venue(detail)
            | ConsoleError::LocalState(detail) => {
                write!(f, "{detail}")
            }
        }
    }
}

/// The gateway's existing ledger and persisted policy, never a second store.
#[tauri::command]
async fn operator_state(
    network: String,
) -> Result<oppen_core::operator::OperatorRead, ConsoleError> {
    let dir = std::env::var_os("OPPEN_DATA_DIR").ok_or_else(|| ConsoleError::NotConfigured(
        "Gateway data is not configured. Launch the console with OPPEN_DATA_DIR pointing to the gateway's data directory.".into()
    ))?;
    let network = network_of(&network);
    tauri::async_runtime::spawn_blocking(move || {
        oppen_core::operator::read(std::path::Path::new(&dir), network)
    })
    .await
    .map_err(|error| ConsoleError::LocalState(error.to_string()))
}

/// The configured account.
///
/// Read from the environment until the onboarding flow persists it. Failing
/// with a named variable is deliberate: an unconfigured console must say so,
/// not show a zeroed account that looks like a real empty one.
fn configured_account(network: Network) -> Result<Address, ConsoleError> {
    let variable = match network {
        Network::Testnet => "OPPEN_TESTNET_USER",
        Network::Mainnet => "OPPEN_MAINNET_USER",
    };
    let raw = std::env::var(variable).map_err(|_| {
        ConsoleError::NotConfigured(
            format!("No account configured. Set {variable} when launching the desktop app. Account provisioning is not available in the console yet."),
        )
    })?;
    raw.parse()
        .map_err(|e| ConsoleError::NotConfigured(format!("{variable} is not an address: {e}")))
}

/// Whether this machine's keychain answers.
///
/// The setup tracker's first real check. On a fresh machine the store is
/// usually reachable and empty, which is `reachable: true` — the question is
/// not whether anything is stored but whether the store can be *asked*. A
/// locked login keychain, a headless box with no keyring daemon, a denied
/// prompt: each of those makes every later step fail for a reason that has
/// nothing to do with what the operator was doing, and the console currently
/// shows none of them.
///
/// **Read-only, and no secret crosses this boundary.** `KeyStore::reachable`
/// probes one entry and discards what it read; this returns a boolean and, on
/// failure, the store's own message. There is deliberately no command here
/// that reads a key: the console never needs one, and a command that could is
/// a command that can be called.
#[tauri::command]
async fn keychain_status(network: String) -> KeychainStatus {
    let store = KeychainKeyStore::new(network_of(&network));
    match store.reachable() {
        Ok(()) => KeychainStatus {
            reachable: true,
            detail: None,
        },
        // The error is shown to the operator, so it is the store's own words:
        // "the keychain is locked" is actionable where "unreachable" is not.
        Err(error) => KeychainStatus {
            reachable: false,
            detail: Some(error.to_string()),
        },
    }
}

/// What [`keychain_status`] answers.
#[derive(Debug, serde::Serialize)]
struct KeychainStatus {
    reachable: bool,
    /// Absent when reachable. Present, and the store's own message, when not.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// Testnet unless the caller says otherwise (`AGENTS.md` invariant 5).
fn network_of(network: &str) -> Network {
    match network {
        "mainnet" => Network::Mainnet,
        _ => Network::Testnet,
    }
}

/// Every listed perp, busiest first (`docs/spec.md` item 30).
///
/// One read serves the rail *and* the strip above the chart, so selecting a
/// symbol costs nothing extra for the numbers both already show.
#[tauri::command]
async fn markets(network: String) -> Result<Vec<MarketRow>, ConsoleError> {
    let info =
        InfoClient::new(network_of(&network)).map_err(|e| ConsoleError::Venue(e.to_string()))?;
    let contexts = info
        .meta_and_asset_ctxs()
        .await
        .map_err(|e| ConsoleError::Venue(format!("asset contexts: {e}")))?;
    Ok(rows(&contexts))
}

/// One symbol in depth: book, and spec F's book, funding and vol packs.
///
/// Three reads, taken on selection rather than on the account tick. That is
/// why [`MarketSnapshot::as_of_ms`] exists — this panel ages on its own clock
/// and has to be able to say so.
#[tauri::command]
async fn market_snapshot(network: String, coin: String) -> Result<MarketSnapshot, ConsoleError> {
    let network = network_of(&network);
    let info = InfoClient::new(network).map_err(|e| ConsoleError::Venue(e.to_string()))?;
    let now_ms = now_ms();

    let contexts = info
        .meta_and_asset_ctxs()
        .await
        .map_err(|e| ConsoleError::Venue(format!("asset contexts: {e}")))?;
    let ctx = contexts
        .iter()
        .find(|(asset, _)| asset.name == coin)
        .map(|(_, ctx)| ctx.clone())
        .ok_or_else(|| ConsoleError::Venue(format!("{coin} is not a listed perp")))?;

    let book = info
        .l2_book(&coin, None)
        .await
        .map_err(|e| ConsoleError::Venue(format!("book: {e}")))?;
    // A day of hourly bars, for the vol pack. Failing to get candles costs
    // the panel its σ, not its book: the two answer different questions and
    // one being unavailable is no reason to withhold the other.
    let day_ms = 24 * 60 * 60 * 1_000;
    let hours = info
        .candles(&coin, "1h", now_ms.saturating_sub(day_ms), now_ms)
        .await
        .unwrap_or_default();

    Ok(snapshot(&coin, &book, &ctx, None, &hours, now_ms))
}

/// How many buckets the chart asks the venue for.
///
/// The renderer draws the last `cols` of whatever it is handed and clamps its
/// own grid at 500 columns, so this is that ceiling with room to spare rather
/// than a number the layout can outrun. Asking for more would be paying for
/// bars no grid can show.
const CHART_BARS: u64 = 600;

/// Bars for the chart panel, split into closed buckets and the forming one.
///
/// The interval is parsed rather than passed through, so `120s` reaches the
/// venue as `2m` and the axis is labelled with what was actually drawn. A
/// parse failure is the operator's typo and says so; a partition the venue
/// broke costs the chart and says that instead.
#[tauri::command]
async fn chart_series(
    network: String,
    coin: String,
    interval: String,
) -> Result<ChartSeries, ConsoleError> {
    let parsed = Interval::parse(&interval)
        .map_err(|e| ConsoleError::Venue(format!("{interval} is not an interval: {e}")))?;
    let info =
        InfoClient::new(network_of(&network)).map_err(|e| ConsoleError::Venue(e.to_string()))?;

    // The universe, for `max_price_decimals`. The axis is labelled to the
    // asset's own precision rather than to whatever the prices happen to
    // carry, so a quiet market does not relabel itself.
    let meta = info
        .meta()
        .await
        .map_err(|e| ConsoleError::Venue(format!("universe: {e}")))?;
    let universe = oppen_hl::Universe::from_meta(&meta)
        .map_err(|e| ConsoleError::Venue(format!("universe: {e}")))?;
    let asset = universe
        .get(&coin)
        .map_err(|e| ConsoleError::Venue(e.to_string()))?;

    let now_ms = now_ms();
    let width_ms = parsed.millis().unsigned_abs();
    let from_ms = now_ms.saturating_sub(CHART_BARS.saturating_mul(width_ms));
    let candles = info
        .candles(&coin, &parsed.to_string(), from_ms, now_ms)
        .await
        .map_err(|e| ConsoleError::Venue(format!("candles: {e}")))?;

    chart(&coin, parsed, &candles, asset.max_price_decimals(), now_ms)
        .map_err(|e| ConsoleError::Venue(format!("the venue's bars are not a partition: {e}")))
}

/// The account as it stands now: equity, margin, positions, resting orders.
///
/// Four venue reads rather than one, because Hyperliquid publishes no single
/// endpoint that answers it and spot is load-bearing under unified margin.
#[tauri::command]
async fn account_state(
    feeds: State<'_, Feeds>,
    network: String,
) -> Result<AccountState, ConsoleError> {
    let network = network_of(&network);
    let account = configured_account(network)?;
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
            // The live socket's own clock (item 34). Still `None` before the
            // first `watch_market` builds the feed, which is the honest answer
            // then: nothing has connected, so there is no last-good value
            // behind the overlay.
            last_tick_ms: feeds
                .0
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().filter(|f| f.serves(network))?.last_tick_ms()),
        },
    ))
}

/// The console's live socket, once something has asked for one.
///
/// Lazily built and rebuilt on a network switch: the pool is bound to one
/// network's endpoint, and invariant 5 makes that switch explicit rather than
/// something a socket can straddle. Dropping the old feed closes its sockets.
#[derive(Default)]
struct Feeds(Mutex<Option<ConsoleFeed>>);

/// Point the live feed at the symbol the operator selected (items 31, 34).
///
/// Called on selection and on an interval change, and it is what makes the
/// console real-time: everything the panels draw for the selected symbol
/// arrives on the socket from here on, and the REST reads beside it are only
/// the seed and the history a socket cannot supply.
#[tauri::command]
async fn watch_market(
    app: tauri::AppHandle,
    feeds: State<'_, Feeds>,
    network: String,
    coin: String,
    interval: String,
) -> Result<(), ConsoleError> {
    // Parsed and re-rendered, exactly as `chart_series` does. The venue takes
    // the interval as a string and answers an unknown one by refusing the
    // subscription, which arrives as a feed that silently never delivers — the
    // chart would then sit still with every other channel healthy, which is
    // the failure this whole change is about. Canonicalising here also means
    // `120s` reaches the socket as `2m`, so the streamed bucket width is the
    // one the chart's REST seed was drawn at.
    let interval = Interval::parse(&interval)
        .map_err(|e| ConsoleError::Venue(format!("{interval} is not an interval: {e}")))?
        .to_string();
    let network = network_of(&network);
    let mut slot = feeds
        .0
        .lock()
        .map_err(|_| ConsoleError::Venue("the feed lock is poisoned".into()))?;
    if !slot.as_ref().is_some_and(|feed| feed.serves(network)) {
        // The account is optional: market data needs none, and a console with
        // no account configured still has a chart to draw.
        let account = configured_account(network).ok().map(|a| a.to_string());
        *slot = Some(ConsoleFeed::start(&app, network, account).map_err(ConsoleError::Venue)?);
    }
    slot.as_ref()
        .expect("the feed was just built")
        .watch(&coin, &interval)
        .map_err(ConsoleError::Venue)
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
        .setup(|app| {
            app.manage(Feeds::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            account_state,
            operator_state,
            keychain_status,
            markets,
            market_snapshot,
            chart_series,
            watch_market
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
