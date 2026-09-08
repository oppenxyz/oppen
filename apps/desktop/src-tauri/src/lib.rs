//! Tauri commands for the operator console.
//!
//! Thin by construction: every command assembles nothing itself and calls into
//! `oppen-core`, which holds no Tauri dependency (`docs/decisions.md` R1) so
//! the core can later run headless with this console as one of its clients.
//! The MCP `get_state` tool reads the same [`oppen_core::state::assemble`], so
//! the operator and the agent cannot be shown different accounts (A3).

mod account_evidence;
mod feed;
mod local_reads;
mod mcp_runtime;
mod operator_activation;
mod operator_approvals;
mod operator_halt;
mod operator_release;
mod pilot_consent;
mod policy_setup;
mod runtime;
mod updates;

use std::sync::Arc;

use local_reads::ReadKind;
use oppen_core::candles::Interval;
use oppen_core::keys::{KeyStore, KeychainKeyStore};
use oppen_core::ledger::{EventViews, Ledger, PilotStatus};
use oppen_core::market::{ChartSeries, MarketRow, MarketSnapshot, chart, rows, snapshot};
use oppen_core::state::{AccountState, VenueReadings, assemble};
use oppen_hl::{Address, InfoClient, Network};
use runtime::{FeedBinding, Runtime, RuntimeError, RuntimeStatus};
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
    /// Local pilot evidence could not be verified. Independent of venue health.
    LocalStatus(String),
    /// The halt was refused before any operator mutation was admitted.
    HaltNotAdmitted(String),
    Prerequisite(String),
    Conflict(String),
    Validation(String),
    Uncertain(String),
    Unavailable(String),
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsoleError::NotConfigured(detail)
            | ConsoleError::Venue(detail)
            | ConsoleError::LocalStatus(detail)
            | ConsoleError::HaltNotAdmitted(detail)
            | ConsoleError::Prerequisite(detail)
            | ConsoleError::Conflict(detail)
            | ConsoleError::Validation(detail)
            | ConsoleError::Uncertain(detail)
            | ConsoleError::Unavailable(detail) => {
                write!(f, "{detail}")
            }
        }
    }
}

impl From<policy_setup::SetupError> for ConsoleError {
    fn from(error: policy_setup::SetupError) -> Self {
        use policy_setup::ErrorKind;
        match error.kind {
            ErrorKind::Prerequisite => Self::Prerequisite(error.detail),
            ErrorKind::Conflict => Self::Conflict(error.detail),
            ErrorKind::Validation => Self::Validation(error.detail),
            ErrorKind::Uncertain => Self::Uncertain(error.detail),
            ErrorKind::Unavailable => Self::Unavailable(error.detail),
        }
    }
}

#[tauri::command]
fn policy_setup_status(runtime: State<'_, Runtime>) -> policy_setup::Status {
    runtime.policy_setup_status()
}

#[tauri::command]
fn pilot_consent_status(runtime: State<'_, Runtime>) -> Option<pilot_consent::Status> {
    runtime.pilot_consent_status()
}
#[tauri::command]
fn review_pilot_consent(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<pilot_consent::Status, pilot_consent::Error> {
    runtime.review_pilot_consent(agent, account)
}
#[tauri::command]
fn confirm_pilot_consent(
    runtime: State<'_, Runtime>,
    owner_id: String,
    review_id: String,
    attestations: pilot_consent::Attestations,
) -> Result<pilot_consent::Status, pilot_consent::Error> {
    runtime.confirm_pilot_consent(owner_id, review_id, attestations)
}
#[tauri::command]
fn discard_pilot_consent(
    runtime: State<'_, Runtime>,
    owner_id: String,
    review_id: String,
) -> Result<pilot_consent::Status, pilot_consent::Error> {
    runtime.discard_pilot_consent(owner_id, review_id)
}
#[tauri::command]
fn reconcile_pilot_consent(
    runtime: State<'_, Runtime>,
    owner_id: String,
    review_id: String,
) -> Result<pilot_consent::Status, pilot_consent::Error> {
    runtime.reconcile_pilot_consent(owner_id, review_id)
}

#[tauri::command]
fn review_policy_setup(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    edits: policy_setup::PolicyEdits,
    empty_source_confirmed: bool,
    writers_stopped: bool,
) -> Result<policy_setup::Status, ConsoleError> {
    runtime
        .review_policy_setup(
            agent,
            account,
            edits,
            empty_source_confirmed,
            writers_stopped,
        )
        .map_err(Into::into)
}

#[tauri::command]
fn persist_policy_setup(
    runtime: State<'_, Runtime>,
    review_id: u64,
) -> Result<policy_setup::Status, ConsoleError> {
    runtime.persist_policy_setup(review_id).map_err(Into::into)
}

#[tauri::command]
fn discard_policy_setup(
    runtime: State<'_, Runtime>,
    review_id: u64,
) -> Result<policy_setup::Status, ConsoleError> {
    runtime.discard_policy_setup(review_id).map_err(Into::into)
}

impl From<RuntimeError> for ConsoleError {
    fn from(error: RuntimeError) -> Self {
        Self::LocalStatus(error.to_string())
    }
}

/// The gateway's existing ledger and persisted policy, never a second store.
#[tauri::command]
async fn operator_state(
    runtime: State<'_, Runtime>,
    network: String,
) -> Result<oppen_core::operator::OperatorRead, ConsoleError> {
    let dir = runtime.data_dir().to_owned();
    let network = network_of(&network);
    runtime
        .local_read(network, ReadKind::Operator, move || {
            oppen_core::operator::read(&dir, network)
        })?
        .await
        .map_err(|error| ConsoleError::LocalStatus(error.to_string()))
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

#[tauri::command]
async fn pilot_status(
    runtime: State<'_, Runtime>,
    network: String,
) -> Result<Option<PilotStatus>, ConsoleError> {
    let network = network_of(&network);
    let account = configured_account(network)?;
    let dir = runtime.data_dir().to_owned();
    runtime
        .local_read(network, ReadKind::Pilot, move || {
            read_pilot_status(&dir, network, account)
        })?
        .await
        .map_err(|e| ConsoleError::LocalStatus(format!("local status task: {e}")))?
}

fn read_pilot_status(
    dir: &std::path::Path,
    network: Network,
    account: Address,
) -> Result<Option<PilotStatus>, ConsoleError> {
    let path = dir.join(oppen_core::db_file_name(network));
    if !path
        .try_exists()
        .map_err(|e| ConsoleError::LocalStatus(e.to_string()))?
    {
        return Err(ConsoleError::LocalStatus(
            "No local ledger is available for this network.".into(),
        ));
    }
    let ledger = Ledger::open_at(&path, network)
        .map_err(|e| ConsoleError::LocalStatus(format!("local ledger: {e}")))?;
    EventViews::new(Arc::new(ledger))
        .pilot_status(account)
        .map_err(|e| ConsoleError::LocalStatus(format!("local pilot status: {e}")))
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
async fn keychain_status(
    runtime: State<'_, Runtime>,
    network: String,
) -> Result<KeychainStatus, ConsoleError> {
    let network = network_of(&network);
    let result = match runtime.local_read(network, ReadKind::Keychain, move || {
        KeychainKeyStore::new(network)
            .reachable()
            .map_err(|error| error.to_string())
    }) {
        Ok(task) => task
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result),
        Err(error) => Err(error.to_string()),
    };
    Ok(KeychainStatus {
        reachable: result.is_ok(),
        detail: result.err(),
    })
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
async fn markets(
    runtime: State<'_, Runtime>,
    network: String,
) -> Result<Vec<MarketRow>, ConsoleError> {
    let _read = runtime.read_lease(network_of(&network))?;
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
async fn market_snapshot(
    runtime: State<'_, Runtime>,
    network: String,
    coin: String,
) -> Result<MarketSnapshot, ConsoleError> {
    let network = network_of(&network);
    let _read = runtime.read_lease(network)?;
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
    runtime: State<'_, Runtime>,
    network: String,
    coin: String,
    interval: String,
) -> Result<ChartSeries, ConsoleError> {
    let _read = runtime.read_lease(network_of(&network))?;
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
    runtime: State<'_, Runtime>,
    network: String,
) -> Result<AccountState, ConsoleError> {
    let network = network_of(&network);
    let _read = runtime.read_lease(network)?;
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
            last_tick_ms: runtime.last_tick_ms(network),
        },
    ))
}

#[tauri::command]
async fn watch_market(
    app: tauri::AppHandle,
    runtime: State<'_, Runtime>,
    network: String,
    coin: String,
    interval: String,
) -> Result<FeedBinding, ConsoleError> {
    let interval = Interval::parse(&interval)
        .map_err(|error| ConsoleError::Venue(format!("{interval} is not an interval: {error}")))?
        .to_string();
    let network = network_of(&network);
    let account = configured_account(network)
        .ok()
        .map(|account| account.to_string());
    runtime
        .watch(app, network, account, coin, interval)?
        .await
        .map_err(|error| ConsoleError::LocalStatus(format!("feed reply: {error}")))?
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn runtime_status(runtime: State<'_, Runtime>) -> RuntimeStatus {
    runtime.status()
}

#[tauri::command]
fn mcp_status(runtime: State<'_, Runtime>) -> mcp_runtime::McpStatus {
    runtime.mcp_status()
}

#[tauri::command]
async fn start_mcp(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<mcp_runtime::McpStatus, ConsoleError> {
    let requested: Address = account.parse().map_err(|error| {
        ConsoleError::NotConfigured(format!("invalid testnet account: {error}"))
    })?;
    if configured_account(Network::Testnet)? != requested {
        return Err(ConsoleError::NotConfigured(
            "Requested supervisor account differs from the console's configured testnet account"
                .into(),
        ));
    }
    runtime
        .start_mcp(agent, requested.to_string())?
        .await
        .map_err(|error| ConsoleError::LocalStatus(format!("MCP startup reply: {error}")))?
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn halt_mcp(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<mcp_runtime::McpStatus, ConsoleError> {
    runtime
        .halt_mcp(agent, account)
        .map_err(|error| ConsoleError::HaltNotAdmitted(error.to_string()))
}

#[tauri::command]
async fn stop_mcp(runtime: State<'_, Runtime>) -> Result<RuntimeStatus, ConsoleError> {
    runtime.shutdown().await?;
    Ok(runtime.status())
}

#[tauri::command]
fn kill_release_status(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<operator_release::ReleaseStatus, ConsoleError> {
    runtime
        .kill_release_status(agent, account)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn review_kill_release(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    scope: oppen_core::guardrail::KillScope,
) -> Result<operator_release::ReleaseStatus, ConsoleError> {
    runtime
        .review_kill_release(agent, account, scope)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn confirm_kill_release(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    review_id: String,
) -> Result<operator_release::ReleaseStatus, ConsoleError> {
    runtime
        .confirm_kill_release(agent, account, owner_id, review_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn discard_kill_release(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    review_id: String,
) -> Result<operator_release::ReleaseStatus, ConsoleError> {
    runtime
        .discard_kill_release(agent, account, owner_id, review_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn reconcile_kill_release(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    operation_id: String,
) -> Result<operator_release::ReleaseStatus, ConsoleError> {
    runtime
        .reconcile_kill_release(agent, account, owner_id, operation_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn activation_status(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<operator_activation::ActivationStatus, ConsoleError> {
    runtime
        .activation_status(agent, account)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn review_activation(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<operator_activation::ActivationStatus, ConsoleError> {
    runtime
        .review_activation(agent, account)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn confirm_activation(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    review_id: String,
) -> Result<operator_activation::ActivationStatus, ConsoleError> {
    runtime
        .confirm_activation(agent, account, owner_id, review_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn discard_activation(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    review_id: String,
) -> Result<operator_activation::ActivationStatus, ConsoleError> {
    runtime
        .discard_activation(agent, account, owner_id, review_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn approval_queue_status(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<operator_approvals::ApprovalQueueStatus, ConsoleError> {
    runtime
        .approval_queue_status(agent, account)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn refresh_approval_queue(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
) -> Result<operator_approvals::ApprovalQueueStatus, ConsoleError> {
    runtime
        .refresh_approval_queue(agent, account)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn reject_approval_proposal(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    proposal_id: String,
) -> Result<operator_approvals::ApprovalQueueStatus, ConsoleError> {
    runtime
        .reject_approval_proposal(agent, account, owner_id, proposal_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn prepare_approval_review(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    proposal_id: String,
) -> Result<operator_approvals::ApprovalQueueStatus, ConsoleError> {
    runtime
        .prepare_approval_review(agent, account, owner_id, proposal_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn confirm_approval_review(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    review_id: String,
) -> Result<operator_approvals::ApprovalQueueStatus, ConsoleError> {
    runtime
        .confirm_approval_review(agent, account, owner_id, review_id)
        .map_err(ConsoleError::from)
}

#[tauri::command]
fn discard_approval_review(
    runtime: State<'_, Runtime>,
    agent: String,
    account: String,
    owner_id: String,
    review_id: String,
) -> Result<operator_approvals::ApprovalQueueStatus, ConsoleError> {
    runtime
        .discard_approval_review(agent, account, owner_id, review_id)
        .map_err(ConsoleError::from)
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let dir = feed::data_dir(app.handle()).map_err(std::io::Error::other)?;
            app.manage(Runtime::new(dir));
            app.manage(updates::Updates::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            policy_setup_status,
            pilot_consent_status,
            review_pilot_consent,
            confirm_pilot_consent,
            discard_pilot_consent,
            reconcile_pilot_consent,
            review_policy_setup,
            persist_policy_setup,
            discard_policy_setup,
            account_state,
            operator_state,
            pilot_status,
            keychain_status,
            markets,
            market_snapshot,
            chart_series,
            watch_market,
            runtime_status,
            mcp_status,
            start_mcp,
            stop_mcp,
            halt_mcp,
            approval_queue_status,
            activation_status,
            kill_release_status,
            review_kill_release,
            confirm_kill_release,
            discard_kill_release,
            reconcile_kill_release,
            review_activation,
            confirm_activation,
            discard_activation,
            refresh_approval_queue,
            reject_approval_proposal,
            prepare_approval_review,
            confirm_approval_review,
            discard_approval_review,
            updates::check_update,
            updates::download_update,
            updates::install_update
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::WindowEvent {
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } = &event
            {
                let runtime = app.state::<Runtime>();
                if !runtime.exit_allowed() {
                    api.prevent_close();
                    runtime.request_exit(app.clone(), 0);
                }
            }
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                let runtime = app.state::<Runtime>();
                if !runtime.exit_allowed() {
                    // Restart cannot be prevented here; the updater must drain first.
                    api.prevent_exit();
                    runtime.request_exit(app.clone(), code.unwrap_or(0));
                }
            }
        });
}

#[cfg(test)]
mod halt_command_tests {
    use super::*;

    #[tokio::test]
    async fn halt_admission_errors_are_typed_and_never_request_a_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(dir.path().to_owned());
        let app = tauri::test::mock_app();
        app.manage(runtime.clone());
        for account in ["invalid", "0x1111111111111111111111111111111111111111"] {
            let error = halt_mcp(app.state(), "fixture-agent".into(), account.into()).unwrap_err();
            assert!(matches!(error, ConsoleError::HaltNotAdmitted(_)));
            let detail = error.to_string();
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                serde_json::json!({
                    "kind": "halt_not_admitted", "detail": detail,
                })
            );
            let halt = runtime.mcp_status().halt;
            assert_eq!(halt.phase, operator_halt::HaltPhase::Idle);
            assert_eq!(halt.requested_at_ms, None);
        }
        runtime.shutdown().await.unwrap();
        let error = halt_mcp(
            app.state(),
            "fixture-agent".into(),
            "0x1111111111111111111111111111111111111111".into(),
        )
        .unwrap_err();
        assert!(matches!(error, ConsoleError::HaltNotAdmitted(_)));
        assert_eq!(runtime.mcp_status().halt.requested_at_ms, None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(matches!(
            ConsoleError::from(RuntimeError::Busy),
            ConsoleError::LocalStatus(_)
        ));
    }
}

#[cfg(test)]
mod pilot_status_tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "oppen-desktop-pilot-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir(&path).expect("isolated test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn account() -> Address {
        "0x1111111111111111111111111111111111111111"
            .parse()
            .expect("public address")
    }

    #[test]
    fn missing_ledger_is_a_local_error_and_is_not_created() {
        let dir = TestDir::new();
        let error =
            read_pilot_status(&dir.0, Network::Testnet, account()).expect_err("missing ledger");
        assert_eq!(
            serde_json::to_value(error).expect("error JSON")["kind"],
            "local_status"
        );
        assert!(
            !dir.0
                .join(oppen_core::db_file_name(Network::Testnet))
                .exists()
        );
    }

    #[test]
    fn existing_ledger_without_authority_returns_no_pilot() {
        let dir = TestDir::new();
        drop(Ledger::open(&dir.0, Network::Testnet).expect("fixture ledger"));
        assert!(
            read_pilot_status(&dir.0, Network::Testnet, account())
                .expect("local read")
                .is_none()
        );
        assert!(matches!(
            read_pilot_status(&dir.0, Network::Mainnet, account()),
            Err(ConsoleError::LocalStatus(_))
        ));
        assert!(
            !dir.0
                .join(oppen_core::db_file_name(Network::Mainnet))
                .exists()
        );
    }

    #[test]
    fn corrupt_local_ledger_is_not_a_venue_error() {
        let dir = TestDir::new();
        std::fs::write(
            dir.0.join(oppen_core::db_file_name(Network::Testnet)),
            b"not a SQLite ledger",
        )
        .expect("corrupt fixture");
        let error =
            read_pilot_status(&dir.0, Network::Testnet, account()).expect_err("corrupt ledger");
        assert_eq!(
            serde_json::to_value(error).expect("error JSON")["kind"],
            "local_status"
        );
    }
}
