//! The tools an agent may call (`docs/spec.md` section C).
//!
//! Two of them read (`get_meta`, `get_state`, `get_order_status`) and four
//! act (`place`, `cancel`, `cancel_all`, `close_position`). Every acting tool
//! reaches the signer through `oppen_core`'s single pre-sign path — `evaluate`
//! or `clear_cancel`, then `sign_cleared` — and nothing here offers a second
//! route (`AGENTS.md` invariant 1). There is no `sign_unchecked` in this file
//! and no branch around a clearance.
//!
//! What every tool answers with lives in [`crate::outcome`]: a typed status
//! on success, a typed code with its retryability on failure, never a
//! formatted sentence (invariant 8).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use oppen_core::alert::{AlertStore, Condition, Direction};
use oppen_core::book;
use oppen_core::features::quotes::{QuoteCache, SigmaCache, Volatility};
use oppen_core::features::{
    BPS_PER_UNIT, book_features, funding_features, margin_runway_h, position_risk, vol_features,
};
use oppen_core::feed::FeedSession;
use oppen_core::guardrail::{
    CancelContext, CancelIntent, CancelTarget, Cleared, FeedQuality, GuardrailEngine, MarketRef,
    OrderIntent, OriginalRequest, RequestedOrderKind,
};
use oppen_core::journal::Journal;
use oppen_core::ledger::{
    EventViews, PilotAccounting, PilotStatus, PilotStop, SubmissionError, SubmissionJournal,
    SubmissionReceipt, SubmissionResolution,
};
use oppen_core::state::{
    AccountState, VenueReadings, assemble, exposure_from, realized_pnl_since, utc_day_start_ms,
};
use oppen_core::tca::{ExecutionReport, ScoredFill};
use oppen_hl::info::OrderRef;
use oppen_hl::types::{Candle, OrderStatusResponse, ReferencePrices};
use oppen_hl::wire::{CancelWire, Cloid, Grouping, Tif, Tpsl};
use oppen_hl::{Address, InfoClient, Network, Universe, meta::MIN_NOTIONAL_USD};
use oppen_hl::{
    ExchangeClient, ExchangeResponse, ExchangeResponseKind, NonceAllocator, OrderKind, Status,
};

use crate::auth::Binding;
use crate::server::ExecutionTracker;

mod operator;

/// Basis points per unit. A bound 1% wide is 100 bps.
/// The execution report's default window. A day: long enough that a few
/// fills accumulate into a sample worth reading, short enough that it
/// describes how the agent is trading now rather than how it traded last
/// week under different guardrails.
const DEFAULT_REPORT_HOURS: u32 = 24;
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
use crate::outcome::{self, CancelFailure, Reply, ToolError};
use rmcp::RoleServer;
use rmcp::service::RequestContext;

#[cfg(test)]
#[path = "execution_fixture.rs"]
mod execution_fixture;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Shared, cheap to clone: `rmcp` builds one service per session.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<GatewayInner>,
}

struct GatewayInner {
    // Serialize account reads within this gateway; the journal below retains
    // unresolved submissions across gateways and process restarts.
    execution: Mutex<HashMap<Address, Arc<tokio::sync::Mutex<()>>>>,
    // A timed-out blocking replay may still be finishing. Never accumulate
    // more replay tasks behind its ledger lock on subsequent sweeps.
    pilot_reader: Arc<tokio::sync::Semaphore>,
    route_reader: Arc<tokio::sync::Semaphore>,
    decision_worker: Arc<tokio::sync::Semaphore>,
    submission_worker: Arc<tokio::sync::Semaphore>,
    submissions: SubmissionJournal,
    network: Network,
    info: InfoClient,
    /// The agent and account are **not** here. They come from the pairing
    /// token on every request (`docs/spec.md` item 15), so one gateway serves
    /// every paired agent and each acts as itself — under its own guardrails,
    /// on its own container (D1 as revised), seeing only its own events (C6).
    /// A fixed identity here would make all of that decorative.
    /// Guardrails live in `oppen-core` and nowhere else (`AGENTS.md`
    /// invariant 1). This gateway cannot evaluate a predicate itself, and
    /// there is no path from a tool to the signer that does not go through
    /// `evaluate` then `sign_cleared`.
    engine: Arc<GuardrailEngine>,
    exchange: ExchangeClient,
    nonces: NonceAllocator,
    /// The per-agent scratchpad (`docs/spec.md` item 21). Keyed by the agent
    /// the token names, like everything else here.
    journal: Arc<Journal>,
    /// What the socket knows: how fresh the feed is, and whether the account
    /// has been reconciled since the last outage. Both were constants until
    /// this existed, and the second one refused every order (`docs/spec.md`
    /// items 9 and 34).
    feed: Arc<FeedSession>,
    /// What agents asked to be woken for (`docs/spec.md` item 22). The feed
    /// pump evaluates it; this only arms.
    alerts: Arc<AlertStore>,
    /// The latest `bbo` per symbol, filled by the pump. `get_features` leases
    /// a symbol here and the pump subscribes what is leased — see
    /// [`QuoteCache`] for why a synchronous read needs a lease rather than the
    /// alert module's armed set.
    quotes: Arc<QuoteCache>,
    /// Daily σ per symbol, so `get_state`'s risk fields do not re-measure on
    /// every call. Read only by `get_state`; the guardrail path never touches
    /// it (`docs/decisions.md` K1).
    sigmas: SigmaCache,
    /// Hands out one agent's read-only slice of the one ledger
    /// (`docs/spec.md` D6), built per request for whoever the token names. An
    /// [`EventViews`] and never a `Ledger`: `redact`, `upsert_sub_account` and
    /// the gap surface are not spellable from here, so `AGENTS.md` invariant 3
    /// is a compile error rather than a review note.
    events: EventViews,
    #[cfg(test)]
    test_registry: Option<oppen_core::ledger::RegistryJournal>,
    #[cfg(test)]
    _test_dir: Option<tempfile::TempDir>,
}

/// The inputs `GuardrailEngine::evaluate` needs, gathered once per call.
struct EvaluationContext {
    universe: Universe,
    state: AccountState,
    exposure: oppen_core::guardrail::Exposure,
    market: MarketRef,
}

#[derive(Debug)]
struct ExecutionPermit {
    _queue: tokio::sync::OwnedMutexGuard<()>,
    account: Address,
    revision: u64,
}

enum SubmissionOwnership {
    Order(ExecutionPermit),
    Cancellation {
        account: Address,
        _queue: tokio::sync::OwnedMutexGuard<()>,
    },
}

impl SubmissionOwnership {
    fn account(&self) -> Address {
        match self {
            Self::Order(permit) => permit.account,
            Self::Cancellation { account, .. } => *account,
        }
    }

    fn order(&self) -> Option<&ExecutionPermit> {
        match self {
            Self::Order(permit) => Some(permit),
            Self::Cancellation { .. } => None,
        }
    }
}

struct CancelAllResult {
    reply: Reply,
    complete: bool,
}

fn pilot_cancellation_needed(
    bound: &Binding,
    status: Option<PilotStatus>,
) -> Result<bool, ToolError> {
    let Some(status) = status else {
        return Ok(false);
    };
    if status.agent != bound.agent || status.account != bound.account {
        return Err(ToolError::unavailable(
            "pilot cancellation identity",
            "pilot status does not match the bound agent and account",
        ));
    }
    Ok(matches!(
        status.halt,
        Some(PilotStop::Exhausted { .. } | PilotStop::Unavailable { .. })
    ) || matches!(status.accounting, PilotAccounting::Unavailable { .. }))
}

/// What `place` takes.
///
/// `reason` is required by spec item 19 and is an untrusted claim (item 30):
/// stored and rendered as inert plain text, never interpreted.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlaceParams {
    /// Symbol as `get_meta` reports it, e.g. "BTC".
    pub symbol: String,
    /// True to buy/long, false to sell/short.
    pub is_buy: bool,
    /// Size in the asset's own units, at `get_meta`'s `size_decimals`.
    ///
    /// A **decimal string**, e.g. "0.0002". Not a JSON number: a JSON number
    /// is an IEEE double, and 0.1 is not representable in one. The venue
    /// validates the exact digits, so a size that round-tripped through a
    /// float is a rejected order.
    pub size: String,
    /// What kind of order: `limit`, `market` or `stop_market`.
    #[serde(flatten)]
    pub order: PlaceKind,
    /// Why you are placing this order. Required.
    pub reason: String,
    #[serde(default)]
    pub reduce_only: bool,
    /// Optional. One is minted when absent, and either way it comes back on
    /// the result — item 19's reconcile-by-cloid needs one to exist.
    #[serde(default)]
    pub cloid: Option<String>,
}

/// The order types of `docs/spec.md` item 12, as one closed choice.
///
/// A tagged enum rather than a price plus a pile of optional fields: "a stop
/// with no trigger price" and "a market order with a limit price" are then not
/// spellable, which is a better answer than refusing them one by one.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "order_type", rename_all = "snake_case")]
pub enum PlaceKind {
    /// Rests on the book at `limit_px`.
    Limit {
        /// Limit price, at `get_meta`'s `price_decimals`. A decimal string,
        /// for the same reason as `size`.
        limit_px: String,
        /// `gtc` rests until cancelled, `ioc` cancels any unfilled remainder,
        /// `alo` is cancelled outright rather than taking the book.
        #[serde(default)]
        tif: PlaceTif,
    },

    /// Crosses now, priced at your configured max slippage from the mid.
    ///
    /// Hyperliquid has no market order type: this is an IOC priced through the
    /// book, and the bound is the operator's, not yours (item 24, D3). The
    /// price is rounded toward the mid, so pricing *at* that bound cannot be
    /// refused *for* it (`docs/decisions.md` C5).
    Market,

    /// Rests off-book until the mark reaches `trigger_px`, then takes the
    /// book at your configured max slippage.
    ///
    /// The guardrail engine measures slippage for a trigger order against the
    /// trigger price rather than today's mid, because a stop fills when it
    /// triggers and not now.
    StopMarket {
        /// The price that arms it. A decimal string.
        trigger_px: String,
        /// `sl` for a stop-loss, `tp` for a take-profit. They differ in which
        /// side of the trigger the venue arms on.
        #[serde(default)]
        tpsl: PlaceTpsl,
    },
}

/// Time in force, as the wire spells it in lower case.
#[derive(Debug, Default, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlaceTif {
    #[default]
    Gtc,
    Ioc,
    Alo,
}

impl From<PlaceTif> for Tif {
    fn from(tif: PlaceTif) -> Self {
        match tif {
            PlaceTif::Gtc => Tif::Gtc,
            PlaceTif::Ioc => Tif::Ioc,
            PlaceTif::Alo => Tif::Alo,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlaceTpsl {
    #[default]
    Sl,
    Tp,
}

impl From<PlaceTpsl> for Tpsl {
    fn from(tpsl: PlaceTpsl) -> Self {
        match tpsl {
            PlaceTpsl::Sl => Tpsl::Sl,
            PlaceTpsl::Tp => Tpsl::Tp,
        }
    }
}

/// What `cancel` takes: one order, named the way the agent knows it.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CancelParams {
    /// The venue's order id. Supply this or `cloid`.
    #[serde(default)]
    pub oid: Option<u64>,
    /// The client order id `place` returned. Supply this or `oid`.
    #[serde(default)]
    pub cloid: Option<String>,
    /// Required by item 19, and untrusted text (item 30).
    pub reason: String,
}

/// What `cancel_all` and `close_position` take.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SymbolActionParams {
    /// Restrict to one symbol. `cancel_all` without one cancels every resting
    /// order on the account.
    #[serde(default)]
    pub symbol: Option<String>,
    pub reason: String,
}

/// What `close_position` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClosePositionParams {
    /// The symbol to flatten. Required: closing "everything" is a different
    /// action with a different blast radius, and item 26 keeps flatten
    /// decoupled from anything that sweeps.
    pub symbol: String,
    pub reason: String,
}

/// What `preflight` takes: the order, without the reason.
///
/// No `reason`: item 19 requires one on every *action*, and item 20 is a
/// question. Requiring an agent to justify asking would train it to write a
/// placeholder, which is worse than not asking for one.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PreflightParams {
    pub symbol: String,
    pub is_buy: bool,
    pub size: String,
    pub limit_px: String,
    #[serde(default)]
    pub reduce_only: bool,
}

/// What `remember` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberParams {
    /// A label you will recall it by. Remembering the same key again replaces
    /// what is under it.
    pub key: String,
    /// The note. Plain text; oppen never interprets it.
    pub value: String,
}

/// What `set_alert` takes.
///
/// A flat shape rather than `alert::Condition`'s tagged one: an agent writes
/// this by hand from a schema, and `{"kind": "price_cross", "symbol": "BTC",
/// "direction": "above", "px": "70000"}` is easier to get right than a nested
/// tag. The mapping onto the core type is [`SetAlertParams::condition`], and
/// it is where a field belonging to another kind is refused.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetAlertParams {
    /// `price_cross`, `fill`, or `funding_rate`.
    pub kind: AlertKind,
    /// The symbol to watch. Required for `price_cross` and `funding_rate`;
    /// optional for `fill`, where absent means any symbol.
    #[serde(default)]
    pub symbol: Option<String>,
    /// `above` or `below`. Required for `price_cross` and `funding_rate`.
    #[serde(default)]
    pub direction: Option<AlertDirection>,
    /// The mark price to cross, as a decimal string. `price_cross` only.
    #[serde(default)]
    pub px: Option<String>,
    /// The hour-to-date funding rate in basis points, as a decimal string.
    /// `funding_rate` only — and it is the rate for one hour, never an
    /// annualised APR.
    #[serde(default)]
    pub hour_to_date_bps: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    PriceCross,
    Fill,
    FundingRate,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertDirection {
    Above,
    Below,
}

impl From<AlertDirection> for Direction {
    fn from(direction: AlertDirection) -> Self {
        match direction {
            AlertDirection::Above => Direction::Above,
            AlertDirection::Below => Direction::Below,
        }
    }
}

impl SetAlertParams {
    /// Map onto the stored condition, naming the missing field rather than
    /// defaulting it.
    ///
    /// A defaulted level is the worst outcome available here: the agent stops
    /// watching, believing it will be woken, and the alert either never fires
    /// or fires immediately on a level nobody chose.
    fn condition(self) -> Result<Condition, ToolError> {
        let missing = |field: &'static str| ToolError::InvalidParams {
            field,
            detail: "required for this alert kind".into(),
        };
        let symbol = |symbol: Option<String>| symbol.ok_or_else(|| missing("symbol"));
        // Decimal strings, never JSON numbers: `AGENTS.md` invariant 6, and a
        // level that lost precision to a float is a level nobody chose.
        let decimal = |value: Option<String>, field: &'static str| {
            value
                .ok_or_else(|| missing(field))?
                .parse::<Decimal>()
                .map_err(|e| ToolError::InvalidParams {
                    field,
                    detail: e.to_string(),
                })
        };
        Ok(match self.kind {
            AlertKind::PriceCross => Condition::PriceCross {
                symbol: symbol(self.symbol)?,
                direction: self.direction.ok_or_else(|| missing("direction"))?.into(),
                px: decimal(self.px, "px")?,
            },
            AlertKind::Fill => Condition::Fill {
                symbol: self.symbol,
            },
            AlertKind::FundingRate => Condition::FundingRate {
                symbol: symbol(self.symbol)?,
                direction: self.direction.ok_or_else(|| missing("direction"))?.into(),
                hour_to_date_bps: decimal(self.hour_to_date_bps, "hour_to_date_bps")?,
            },
        })
    }
}

/// What `get_features` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetFeaturesParams {
    /// The symbol to read.
    pub symbol: String,
}

/// What `cancel_alert` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CancelAlertParams {
    /// The `alert_id` `set_alert` returned, or one from `get_alerts`.
    pub alert_id: i64,
}

/// What `recall` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallParams {
    /// One key, or absent for everything you have remembered.
    #[serde(default)]
    pub key: Option<String>,
}

/// What `get_execution_report` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExecutionReportParams {
    /// How far back to look, in hours. Absent is 24.
    #[serde(default)]
    pub hours: Option<u32>,
}

/// What `get_events` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetEventsParams {
    /// The cursor from the previous call. Absent or 0 starts at the beginning
    /// of what the ledger still retains.
    #[serde(default)]
    pub since_cursor: u64,
    /// How many events at most. Absent takes the ledger's page cap, and a
    /// larger value is clamped to it rather than refused.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// What `get_order_status` takes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OrderStatusParams {
    #[serde(default)]
    pub oid: Option<u64>,
    #[serde(default)]
    pub cloid: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMetaParams {
    /// Restrict the response to these symbols. Omit for the whole universe.
    #[serde(default)]
    pub symbols: Option<Vec<String>>,
}

/// One symbol's trading rules, as item 17 specifies them.
///
/// Every value is pre-rounded and carries its unit in the field name, per the
/// deterministic-JSON principle in `docs/mcp-contract.md`. An agent that
/// computes a price from `price_decimals` and a size from `size_decimals` is
/// computing the same numbers the venue validates against.
#[derive(Debug, Serialize)]
pub struct SymbolMeta {
    pub symbol: String,
    /// The on-chain asset id. Positional and never compacted — see
    /// [`Universe`].
    pub asset_id: u32,
    pub size_decimals: u32,
    pub price_decimals: u32,
    pub max_leverage: u32,
    pub min_notional_usd: String,
    /// Funding accrues hourly on Hyperliquid.
    pub funding_interval_hours: u32,
    pub only_isolated: bool,
    pub is_delisted: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayInitError {
    #[error(transparent)]
    Transport(#[from] oppen_hl::Error),
    #[error(transparent)]
    Authority(#[from] oppen_core::guardrail::GuardrailError),
}

#[tool_router]
impl Gateway {
    pub fn new(
        network: Network,
        engine: Arc<GuardrailEngine>,
        events: EventViews,
        journal: Arc<Journal>,
        alerts: Arc<AlertStore>,
        quotes: Arc<QuoteCache>,
    ) -> Result<Self, GatewayInitError> {
        let feed = engine.feed();
        Ok(Self {
            inner: Arc::new(GatewayInner {
                execution: Mutex::new(HashMap::new()),
                pilot_reader: Arc::new(tokio::sync::Semaphore::new(1)),
                route_reader: Arc::new(tokio::sync::Semaphore::new(1)),
                decision_worker: Arc::new(tokio::sync::Semaphore::new(1)),
                submission_worker: Arc::new(tokio::sync::Semaphore::new(1)),
                submissions: engine.submissions()?,
                network,
                info: InfoClient::new(network)?,
                engine,
                exchange: ExchangeClient::new(network)?,
                nonces: NonceAllocator::new(),
                journal,
                feed,
                alerts,
                quotes,
                sigmas: SigmaCache::new(),
                events,
                #[cfg(test)]
                _test_dir: None,
                #[cfg(test)]
                test_registry: None,
            }),
        })
    }

    /// Redirect only transport to loopback for cross-crate lifecycle fixtures.
    /// Authority, guards and signing ownership remain the real gateway's.
    #[cfg(feature = "test-support")]
    pub fn with_loopback_fixture(mut self, port: u16) -> std::io::Result<Self> {
        let info = InfoClient::loopback_fixture(port).map_err(std::io::Error::other)?;
        let exchange = ExchangeClient::loopback_fixture(port).map_err(std::io::Error::other)?;
        let inner = Arc::get_mut(&mut self.inner)
            .ok_or_else(|| std::io::Error::other("fixture gateway must be uniquely owned"))?;
        inner.info = info;
        inner.exchange = exchange;
        Ok(self)
    }

    /// Who this request's token names (`docs/spec.md` item 15).
    ///
    /// `rmcp` republishes the request's `http::request::Parts` into the tool's
    /// context, and the door put its session authority there after authenticating.
    /// An absent authority is not a caller error to explain: the door refuses
    /// every unauthenticated request, so reaching a tool without one means the
    /// gateway was mounted without its guard. Fail closed and say so.
    fn bound(ctx: &RequestContext<RoleServer>) -> Result<Binding, ToolError> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<crate::auth::SessionAuthority>())
            .map(|authority| authority.binding().clone())
            .ok_or(ToolError::Unavailable {
                what: "pairing",
                detail: "this request carries no pairing authority; the gateway is \
                         mounted without its door"
                    .to_owned(),
            })
    }

    pub(crate) fn network(&self) -> Network {
        self.inner.network
    }

    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn execution_queue(&self, account: Address) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .execution
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(account)
            .or_default()
            .clone()
    }

    #[cfg(test)]
    async fn reserve_submission(&self, bound: &Binding) -> Result<ExecutionPermit, ToolError> {
        self.reserve_submission_tracked(bound, None).await
    }

    async fn reserve_submission_tracked(
        &self,
        bound: &Binding,
        tracker: Option<ExecutionTracker>,
    ) -> Result<ExecutionPermit, ToolError> {
        self.require_route_tracked(bound, tracker.clone()).await?;
        self.reserve_submission_account(bound, tracker).await
    }

    async fn reserve_submission_account(
        &self,
        bound: &Binding,
        tracker: Option<ExecutionTracker>,
    ) -> Result<ExecutionPermit, ToolError> {
        let held = self.execution_queue(bound.account).lock_owned().await;
        let slot = self
            .inner
            .submission_worker
            .clone()
            .try_acquire_owned()
            .map_err(|error| ToolError::unavailable("submission worker busy", error))?;
        let gateway = self.clone();
        let bound = bound.clone();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            runtime.block_on(reserve_account_held(
                held,
                &gateway.inner.submissions,
                bound.account,
                |cloid| async {
                    gateway
                        .require_route_tracked(&bound, tracker.clone())
                        .await?;
                    gateway
                        .inner
                        .info
                        .order_status(bound.account, OrderRef::Cloid(cloid))
                        .await
                        .map_err(|e| ToolError::unavailable("submission reconciliation", e))
                },
            ))
        })
        .await
        .map_err(|error| ToolError::worker_failed("submission reservation worker", error))?
    }

    async fn require_route(&self, bound: &Binding) -> Result<(), ToolError> {
        self.require_route_tracked(bound, None).await
    }

    async fn require_route_tracked(
        &self,
        bound: &Binding,
        tracker: Option<ExecutionTracker>,
    ) -> Result<(), ToolError> {
        let permit = self
            .inner
            .route_reader
            .clone()
            .try_acquire_owned()
            .map_err(|error| ToolError::unavailable("registry reader busy", error))?;
        let engine = self.inner.engine.clone();
        let agent = bound.agent.clone();
        let route = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _tracker = tracker;
                engine.route_for_agent(&agent)
            }),
        )
        .await
        .map_err(|error| ToolError::unavailable("registry read timeout", error))?
        .map_err(|error| ToolError::worker_failed("registry reader", error))?
        .map_err(|refusal| ToolError::GuardrailRefused { refusal })?;
        if route.binding.container != bound.account {
            return Err(ToolError::unavailable(
                "registry route",
                "pairing account differs from authorized container",
            ));
        }
        Ok(())
    }

    /// Runtime-only: retry from persisted pause and pilot state, never from the
    /// drainable kill-effect queue. This is not an MCP tool or an unpause path.
    pub(crate) async fn enforce_pauses(
        &self,
        bindings: &[Binding],
        tracker: ExecutionTracker,
    ) -> Result<(), ToolError> {
        // A separate operator can change policy while this gateway is idle.
        // Replay failure inhibits the engine but must not suppress cleanup.
        let refresh_failure = match self
            .decision(tracker.clone(), |engine| engine.policy_observation())
            .await
        {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => {
                tracing::warn!(%error, "policy unavailable; continuing registry-authenticated cleanup");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "policy refresh incomplete; attempting known-stop cleanup before retry");
                Some(error)
            }
        };
        let cleanup = self
            .enforce_pauses_with(bindings, |bound| {
                let tracker = tracker.clone();
                async move {
                    let result = self
                        .cancel_all_bound(
                            &bound,
                            &SymbolActionParams {
                                symbol: None,
                                reason:
                                    "cancel resting orders while trading is paused or pilot stopped"
                                        .to_owned(),
                            },
                            true,
                            tracker,
                        )
                        .await?;
                    if result.complete {
                        Ok(())
                    } else {
                        Err(ToolError::unavailable(
                            "pause cancellation",
                            "resting orders were not all confirmed canceled",
                        ))
                    }
                }
            })
            .await;
        cleanup?;
        match refresh_failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn decision<T: Send + 'static>(
        &self,
        tracker: ExecutionTracker,
        work: impl FnOnce(Arc<GuardrailEngine>) -> T + Send + 'static,
    ) -> Result<T, ToolError> {
        let permit = self
            .inner
            .decision_worker
            .clone()
            .try_acquire_owned()
            .map_err(|error| ToolError::unavailable("decision worker busy", error))?;
        let engine = self.inner.engine.clone();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                let _tracker = tracker;
                let _permit = permit;
                work(engine)
            }),
        )
        .await
        .map_err(|error| ToolError::unavailable("decision timeout", error))?
        .map_err(|error| ToolError::worker_failed("decision worker", error))
    }

    async fn runtime_cancellation_needed(&self, bound: &Binding) -> Result<bool, ToolError> {
        if self.inner.engine.cancellation_needed(&bound.agent) {
            return Ok(true);
        }
        let permit = self
            .inner
            .pilot_reader
            .clone()
            .try_acquire_owned()
            .map_err(|error| ToolError::unavailable("pilot cancellation reader busy", error))?;
        let events = self.inner.events.clone();
        let account = bound.account;
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                events.pilot_status(account)
            }),
        )
        .await
        .map_err(|error| ToolError::unavailable("pilot cancellation status timeout", error))?
        .map_err(|error| ToolError::worker_failed("pilot cancellation reader", error))?
        .map_err(|error| ToolError::unavailable("pilot cancellation status", error))?;
        pilot_cancellation_needed(bound, status)
    }

    async fn enforce_pauses_with<F, Fut>(
        &self,
        bindings: &[Binding],
        mut cancel: F,
    ) -> Result<(), ToolError>
    where
        F: FnMut(Binding) -> Fut,
        Fut: Future<Output = Result<(), ToolError>>,
    {
        let mut seen = HashSet::new();
        let mut failure = None;
        for bound in bindings {
            if !seen.insert((bound.agent.clone(), bound.account)) {
                continue;
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                if self.runtime_cancellation_needed(bound).await? {
                    cancel(bound.clone()).await
                } else {
                    Ok(())
                }
            })
            .await
            .unwrap_or_else(|error| {
                Err(ToolError::unavailable("pause cancellation timeout", error))
            });
            if let Err(error) = result {
                tracing::warn!(agent = %bound.agent, account = %bound.account, %error, "paused agent cancellation failed; will retry");
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// `get_meta` — per-symbol trading rules (`docs/spec.md` item 17).
    #[tool(
        description = "Per-symbol trading rules: asset id, size and price decimals, max leverage, \
                       minimum notional, funding interval. Read-only."
    )]
    async fn get_meta(
        &self,
        Parameters(GetMetaParams { symbols }): Parameters<GetMetaParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let meta = self
            .inner
            .info
            .meta()
            .await
            .map_err(|e| ToolError::unavailable("meta", e))?;
        let universe =
            Universe::from_meta(&meta).map_err(|e| ToolError::unavailable("universe", e))?;

        let mut out: Vec<SymbolMeta> = Vec::new();
        match symbols {
            Some(wanted) => {
                for symbol in wanted {
                    let asset = universe
                        .get(&symbol)
                        .map_err(|e| ToolError::invalid("symbols", e))?;
                    out.push(describe(asset));
                }
            }
            None => out.extend(universe.iter().map(describe)),
        }

        // Stable order so two calls with the same universe produce the same
        // bytes, which is what makes a cached response comparable.
        out.sort_by_key(|symbol| symbol.asset_id);

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&out).expect("SymbolMeta serializes"),
        )]))
    }

    /// `place` — the only path from an agent to a signature
    /// (`docs/spec.md` item 19, `AGENTS.md` invariant 1).
    ///
    /// Every refusal is returned as structured data rather than prose: the
    /// predicate that failed, what was observed and what the limit is, so the
    /// agent can act on it and the operator can see which limit to raise.
    #[tool(
        description = "Place an order. Requires a reason. The order is checked against this \
                       agent's guardrails immediately before signing and is refused with a \
                       structured reason naming the predicate, the observed value and the \
                       configured limit. A refusal is not an error to retry blindly."
    )]
    async fn place(
        &self,
        Parameters(params): Parameters<PlaceParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tracker = ExecutionTracker::from_context(&ctx)?;
        let bound = Self::bound(&ctx)?;
        let permit = self
            .reserve_submission_tracked(&bound, Some(tracker.clone()))
            .await?;
        let now_ms = now_ms();

        let context = self
            .evaluation_context(&bound, &params.symbol, now_ms)
            .await?;
        let asset = context
            .universe
            .get(&params.symbol)
            .map_err(|e| ToolError::invalid("symbol", e))?;

        let sz: Decimal = params
            .size
            .parse()
            .map_err(|e| ToolError::invalid("size", e))?;

        // A cloid is minted when the agent supplies none, and it is minted
        // *before* signing. Item 19 says the only safe move after
        // `timeout_unknown_outcome` is a query by cloid — an order sent
        // without one is unreconcilable by construction, so oppen never sends
        // one that way.
        let cloid = match &params.cloid {
            Some(supplied) => Cloid::parse(supplied).map_err(|e| ToolError::invalid("cloid", e))?,
            None => mint_cloid()?,
        };

        // The price and the order type together: a market order has no price
        // of its own, and a stop takes the book when it triggers, so both are
        // priced from the operator's slippage bound rather than the caller's.
        let (px, kind, original) =
            normalize_place_order(&params.order, params.is_buy, asset, &context.market, || {
                self.operator_slippage_bps(&bound)
            })?;

        let intent = OrderIntent {
            symbol: params.symbol.clone(),
            is_buy: params.is_buy,
            px,
            sz,
            kind,
            reduce_only: params.reduce_only,
            cloid: Some(cloid.clone()),
            grouping: Grouping::Na,
            builder: None,
            max_slippage_bps: None,
            reason: params.reason,
            original: Some(original),
        };

        // The gate. There is no branch around it.
        let agent = bound.agent.clone();
        let asset = asset.clone();
        let cleared = match self
            .decision(tracker.clone(), move |engine| {
                engine.evaluate(
                    &agent,
                    &intent,
                    &asset,
                    &context.market,
                    &context.exposure,
                    self::now_ms(),
                )
            })
            .await?
        {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let response = match self
            .submit(cleared, Some(&cloid), &bound, Some(permit), Some(tracker))
            .await
        {
            Ok(response) => response,
            Err(ToolError::GuardrailRefused { refusal }) => {
                return Ok(outcome::refused(refusal).into_result());
            }
            Err(error) => return Err(error.into()),
        };
        Ok(order_outcome(response, Some(cloid.as_str().to_owned()))?.into_result())
    }

    /// `cancel` — one resting order (`docs/spec.md` item 19).
    #[tool(
        description = "Cancel one proven own resting order by oid or by the cloid place returned. Requires \
                       a reason. Approval mode retains the exact target for operator review; \
                       runtime emergency cleanup remains independent."
    )]
    async fn cancel(
        &self,
        Parameters(params): Parameters<CancelParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let tracker = ExecutionTracker::from_context(&ctx)?;
        let bound = Self::bound(&ctx)?;
        let queue = self.execution_queue(bound.account);
        let execution = queue.lock_owned().await;
        self.require_route(&bound).await?;

        // Which resting order this names, and on which asset. The asset id is
        // part of both cancel wires, so an oid alone is not enough and the
        // open-order list is the only place it can come from.
        let orders = inner
            .info
            .frontend_open_orders(bound.account)
            .await
            .map_err(|e| ToolError::unavailable("orders", e))?;
        let observed_at_ms = now_ms();
        let universe = self.universe().await?;

        let target = match (&params.oid, &params.cloid) {
            (Some(oid), _) => orders.iter().find(|o| o.oid == *oid),
            (None, Some(cloid)) => orders
                .iter()
                .find(|o| o.cloid.as_ref().is_some_and(|c| c.as_str() == cloid)),
            (None, None) => {
                return Err(ToolError::invalid("oid", "supply either oid or cloid").into());
            }
        };
        // Not an error: an order that is already gone is the state the caller
        // wanted. Reported as a cancel that the venue did not take, with the
        // count that says so, rather than as a failure to act on.
        let Some(order) = target else {
            return Ok(outcome::canceled(
                1,
                vec![CancelFailure {
                    oid: params.oid,
                    cloid: params.cloid.clone(),
                    venue_message: "no such resting order".to_owned(),
                }],
            )
            .into_result());
        };
        let agent = bound.agent.clone();
        if orders
            .iter()
            .filter(|candidate| {
                candidate.oid == order.oid || params.oid.is_none() && candidate.cloid == order.cloid
            })
            .count()
            != 1
        {
            return Err(
                ToolError::unavailable("cancellation target", "ambiguous order identity").into(),
            );
        }
        let context = cancellation_context(
            &bound,
            std::slice::from_ref(order),
            &universe,
            observed_at_ms,
        )?;
        let intent = CancelIntent {
            targets: context.targets.clone(),
            reason: params.reason,
        };
        let (decision, execution) = self
            .decision(tracker.clone(), move |engine| {
                (
                    engine.evaluate_cancel(&agent, &intent, &context, self::now_ms()),
                    execution,
                )
            })
            .await?;
        let cleared = match decision {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let response = self
            .submit_cancellation(cleared, &bound, execution, None, tracker)
            .await?;
        Ok(cancel_outcome(
            response,
            vec![(
                Some(order.oid),
                order.cloid.as_ref().map(|c| c.as_str().to_owned()),
            )],
        )?
        .into_result())
    }

    /// `cancel_all` — every resting order, or every one on a symbol
    /// (`docs/spec.md` item 19).
    #[tool(
        description = "Cancel every resting order, or every one on a symbol, only when all targets \
                       have authenticated own-order evidence. Requires a reason. \
                       Partial success is normal — an order that filled a moment ago cannot be \
                       cancelled — so the result itemises what the venue would not take. \
                       Approval mode retains a frozen target set, never future orders."
    )]
    async fn cancel_all(
        &self,
        Parameters(params): Parameters<SymbolActionParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        let tracker = ExecutionTracker::from_context(&ctx)?;
        Ok(self
            .cancel_all_bound(&bound, &params, false, tracker)
            .await?
            .reply
            .into_result())
    }

    async fn cancel_all_bound(
        &self,
        bound: &Binding,
        params: &SymbolActionParams,
        paused_only: bool,
        tracker: ExecutionTracker,
    ) -> Result<CancelAllResult, ToolError> {
        self.cancel_all_with(
            bound,
            params,
            paused_only,
            tracker,
            async {
                let orders = self
                    .inner
                    .info
                    .frontend_open_orders(bound.account)
                    .await
                    .map_err(|e| ToolError::unavailable("orders", e))?;
                Ok((orders, self.universe().await?))
            },
            |cleared, execution, tracker| {
                self.submit_cancellation(cleared, bound, execution, None, tracker)
            },
        )
        .await
    }

    async fn cancel_all_with<Read, Submit, Posted>(
        &self,
        bound: &Binding,
        params: &SymbolActionParams,
        paused_only: bool,
        tracker: ExecutionTracker,
        read: Read,
        submit: Submit,
    ) -> Result<CancelAllResult, ToolError>
    where
        Read: Future<Output = Result<(Vec<oppen_hl::types::OpenOrder>, Universe), ToolError>>,
        Submit: FnOnce(Cleared, tokio::sync::OwnedMutexGuard<()>, ExecutionTracker) -> Posted,
        Posted: Future<Output = Result<ExchangeResponse, ToolError>>,
    {
        let queue = self.execution_queue(bound.account);
        let execution = queue.lock_owned().await;
        self.require_route(bound).await?;
        let cancellation_needed = || async {
            if paused_only {
                self.runtime_cancellation_needed(bound).await
            } else {
                Ok(true)
            }
        };
        let skipped = || CancelAllResult {
            reply: outcome::canceled(0, Vec::new()),
            complete: true,
        };
        // The runtime's sweep snapshot may predate an operator resume while
        // this account was waiting for an in-flight execution.
        if !cancellation_needed().await? {
            return Ok(skipped());
        }
        let now_ms = now_ms();
        let (orders, universe) = read.await?;

        let targets: Vec<_> = orders
            .into_iter()
            .filter(|o| params.symbol.as_ref().is_none_or(|s| *s == o.coin))
            .collect();
        // Nothing resting is the state the caller wanted, and the engine
        // refuses an empty cancel as an input mismatch — so this answers
        // without asking it.
        if targets.is_empty() {
            return Ok(CancelAllResult {
                reply: outcome::canceled(0, Vec::new()),
                complete: true,
            });
        }

        let discretionary = if paused_only {
            None
        } else {
            let context = cancellation_context(bound, &targets, &universe, now_ms)?;
            let intent = CancelIntent {
                targets: context.targets.clone(),
                reason: params.reason.clone(),
            };
            Some((intent, context))
        };

        let mut wires = Vec::with_capacity(targets.len());
        let mut named = Vec::with_capacity(targets.len());
        for order in &targets {
            let asset = universe
                .get(&order.coin)
                .map_err(|e| ToolError::unavailable("universe", e))?;
            wires.push(CancelWire {
                a: asset.index,
                o: order.oid,
            });
            named.push((
                Some(order.oid),
                order.cloid.as_ref().map(|c| c.as_str().to_owned()),
            ));
        }

        let agent = bound.agent.clone();
        let reason = params.reason.clone();
        let (decision, execution) = self
            .decision(tracker.clone(), move |engine| {
                let decision = match discretionary {
                    Some((intent, context)) => {
                        engine.evaluate_cancel(&agent, &intent, &context, self::now_ms())
                    }
                    None => engine.clear_cancel(&agent, wires, &reason, now_ms),
                };
                (decision, execution)
            })
            .await?;
        let cleared = match decision {
            Ok(cleared) => cleared,
            Err(refusal) => {
                return Ok(CancelAllResult {
                    reply: outcome::refused(refusal),
                    complete: false,
                });
            }
        };

        // Reads above yield; a resume during them also withdraws the runtime's
        // reason to cancel. Agent-requested cancels are independent of pause.
        if !cancellation_needed().await? {
            return Ok(skipped());
        }
        let response = submit(cleared, execution, tracker).await?;
        let complete = response.statuses.len() == named.len()
            && response
                .statuses
                .iter()
                .all(|status| matches!(status, Status::Success));
        Ok(CancelAllResult {
            reply: cancel_outcome(response, named)?,
            complete,
        })
    }

    /// `close_position` — flatten one symbol (`docs/spec.md` item 19).
    #[tool(
        description = "Flatten the open position on one symbol with a reduce-only IOC order, \
                       priced at this agent's configured max slippage. Requires a reason. It \
                       cannot open or flip a position: the size is the position's own and the \
                       order is reduce-only, so a fill that would cross through flat is refused \
                       by the venue rather than reversed."
    )]
    async fn close_position(
        &self,
        Parameters(params): Parameters<ClosePositionParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tracker = ExecutionTracker::from_context(&ctx)?;
        let bound = Self::bound(&ctx)?;
        let permit = self
            .reserve_submission_tracked(&bound, Some(tracker.clone()))
            .await?;
        let now_ms = now_ms();

        let context = self
            .evaluation_context(&bound, &params.symbol, now_ms)
            .await?;
        let asset = context
            .universe
            .get(&params.symbol)
            .map_err(|e| ToolError::invalid("symbol", e))?;

        // Nothing open is not a failure: the caller wanted the symbol flat and
        // it is flat. Answered as a cancel-shaped no-op rather than as an
        // error, so an agent unwinding a book can call this per symbol without
        // branching on which ones it still holds.
        let Some(position) = context
            .state
            .positions
            .iter()
            .find(|p| p.symbol == params.symbol)
            .filter(|p| !p.size.is_zero())
        else {
            return Ok(outcome::canceled(0, Vec::new()).into_result());
        };

        // Without a mid there is no price to send. This is the gateway
        // failing to build an order, not a guardrail verdict, so it is an
        // `unavailable` rather than a fabricated zero-price intent handed to
        // the engine to refuse — that would record an order nobody asked for
        // in the ledger and would trip the venue's price rule rather than the
        // engine's missing-reference one, naming the wrong cause.
        let Some(reference_px) = context.market.reference_px else {
            return Err(ToolError::Unavailable {
                what: "reference price",
                detail: format!("no mid for {}; refusing to price a close", params.symbol),
            }
            .into());
        };

        let intent = close_intent(
            &params.symbol,
            &params.reason,
            position.size,
            reference_px,
            context.market.as_of_ms,
            // The operator's bound, shared with `place`'s market order.
            self.operator_slippage_bps(&bound)?,
            asset,
            mint_cloid()?,
        );

        // The same gate as `place`. A close is not privileged: it is refused
        // when the account is unreconciled or the feed is stale, exactly as an
        // opening order is.
        let cloid = intent.cloid.clone();
        let agent = bound.agent.clone();
        let asset = asset.clone();
        let cleared = match self
            .decision(tracker.clone(), move |engine| {
                engine.evaluate(
                    &agent,
                    &intent,
                    &asset,
                    &context.market,
                    &context.exposure,
                    self::now_ms(),
                )
            })
            .await?
        {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let response = match self
            .submit(cleared, cloid.as_ref(), &bound, Some(permit), Some(tracker))
            .await
        {
            Ok(response) => response,
            Err(ToolError::GuardrailRefused { refusal }) => {
                return Ok(outcome::refused(refusal).into_result());
            }
            Err(error) => return Err(error.into()),
        };
        Ok(order_outcome(response, cloid.map(|c| c.as_str().to_owned()))?.into_result())
    }

    /// `preflight` — what would happen, without it happening
    /// (`docs/spec.md` item 20).
    #[tool(
        description = "What this order would do, without sending it: the guardrail verdict with \
                       the predicate that would refuse it, a live book walk giving the average \
                       fill and slippage, how much size fits within 5/10/25 bps, margin, and \
                       exposure after the fill. Costs no order-rate token. A clear verdict is \
                       not a promise — the book moves, and the token it did not spend may be \
                       gone by the time you send."
    )]
    async fn preflight(
        &self,
        Parameters(params): Parameters<PreflightParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let tracker = ExecutionTracker::from_context(&ctx)?;
        let bound = Self::bound(&ctx)?;
        let now_ms = now_ms();

        let context = self
            .evaluation_context(&bound, &params.symbol, now_ms)
            .await?;
        let asset = context
            .universe
            .get(&params.symbol)
            .map_err(|e| ToolError::invalid("symbol", e))?;

        let px: Decimal = params
            .limit_px
            .parse()
            .map_err(|e| ToolError::invalid("limit_px", e))?;
        let sz: Decimal = params
            .size
            .parse()
            .map_err(|e| ToolError::invalid("size", e))?;

        // The verdict comes from the engine's own predicates, run in the same
        // order against the same state — never a restatement of them here
        // (`AGENTS.md` invariant 1). It spends nothing and returns no
        // authority to sign.
        let intent = OrderIntent {
            symbol: params.symbol.clone(),
            is_buy: params.is_buy,
            px,
            sz,
            kind: OrderKind::Limit { tif: Tif::Gtc },
            reduce_only: params.reduce_only,
            cloid: None,
            grouping: Grouping::Na,
            builder: None,
            max_slippage_bps: None,
            // Item 19 requires a reason on an action; this is a question. The
            // engine still checks one, so a fixed non-empty marker keeps the
            // predicate honest without inviting a placeholder from the agent.
            reason: "preflight".to_owned(),
            original: None,
        };
        let agent = bound.agent.clone();
        let decision_asset = asset.clone();
        let market = context.market.clone();
        let exposure = context.exposure.clone();
        let verdict = self
            .decision(tracker, move |engine| {
                engine.preflight(&agent, &intent, &decision_asset, &market, &exposure, now_ms)
            })
            .await?;

        // The live book, walked for this exact size.
        let l2 = inner
            .info
            .l2_book(&params.symbol, None)
            .await
            .map_err(|e| ToolError::unavailable("book", e))?;
        let walk = book::walk(&l2, params.is_buy, sz);

        // Margin at the asset's own maximum leverage, which is the least
        // margin the venue could ask. The operator's leverage cap is lower or
        // equal, so this understates nothing and the guardrail verdict above
        // carries the binding limit.
        let notional_usd = asset.round_price(px) * asset.round_size(sz);
        let initial_margin_usd = notional_usd
            .checked_div(Decimal::from(asset.info.max_leverage))
            .unwrap_or(notional_usd);
        let balances = &context.state.balances;

        let body = serde_json::json!({
            "contract_version": 0,
            "symbol": params.symbol,
            "is_buy": params.is_buy,
            "notional_usd": notional_usd,
            "guardrail": verdict,
            "book": walk,
            "book_as_of_ms": l2.time,
            "margin": {
                "initial_margin_usd": initial_margin_usd,
                "withdrawable_usd": balances.withdrawable_usd,
                "sufficient": balances.withdrawable_usd >= initial_margin_usd,
            },
            "post_fill": {
                "equity_usd": balances.equity_usd,
                "margin_used_usd": balances.total_margin_used_usd + initial_margin_usd,
            },
            "feed": context.state.feed,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    /// `remember` — the scratchpad (`docs/spec.md` item 21).
    #[tool(
        description = "Keep a note for your future self. Agents are amnesiac across sessions; \
                       this is what survives. Remembering the same key again replaces what is \
                       under it — the journal holds what is true now, and get_events holds what \
                       happened. Your notes are yours: no other agent can read them."
    )]
    async fn remember(
        &self,
        Parameters(params): Parameters<RememberParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        self.inner
            .journal
            .remember(
                bound.agent.as_str(),
                &params.key,
                &params.value,
                now_ms() as i64,
            )
            // The bounds are the caller's to fix — a shorter note, or a key it
            // already keeps — so they are `invalid_params` rather than an
            // outage, and the message names the limit.
            .map_err(|e| ToolError::invalid("value", e))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({ "contract_version": 0, "remembered": params.key }).to_string(),
        )]))
    }

    /// `set_alert` — wakeups instead of polling (`docs/spec.md` item 22).
    #[tool(
        description = "Ask to be woken when a condition holds, instead of holding a session \
                       open and polling. Three kinds: price_cross (the venue's mark reaches a \
                       level), fill (an order on this account trades), funding_rate (the \
                       hour-to-date rate reaches a level in basis points). It fires once — \
                       re-arm it if you want it again — and arrives as an `alert` event in \
                       get_events, carrying what you asked for and what was observed. \
                       Liquidation distance and feature thresholds are not available yet."
    )]
    async fn set_alert(
        &self,
        Parameters(params): Parameters<SetAlertParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        let condition = params.condition()?;
        // A symbol that is not on this venue can never tick, so the alert
        // would wait forever. Checked here rather than in the store, which
        // holds no universe and should not learn one.
        if let Some(symbol) = condition.watched_symbol() {
            let universe = self.universe().await?;
            universe.get(symbol).map_err(|_| ToolError::InvalidParams {
                field: "symbol",
                detail: format!("{symbol} is not a symbol on this venue"),
            })?;
        }
        let armed = self
            .inner
            .alerts
            .arm(bound.agent.as_str(), &condition, now_ms() as i64)
            // The ceiling and the unusable conditions are the caller's to fix,
            // so they are `invalid_params` and the message names the limit.
            .map_err(|e| ToolError::invalid("condition", e))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({
                "contract_version": 0,
                "alert_id": armed.alert_id,
                "condition": armed.condition,
                "armed_at_ms": armed.armed_at_ms,
            })
            .to_string(),
        )]))
    }

    /// `get_features` — the numbers an agent decides from (`docs/spec.md`
    /// spec F).
    #[tool(
        description = "Deterministic market features for one symbol: book (spread_bps, \
                       book_imbalance, micro_tilt_bps, depth per band), funding (hour-to-date \
                       bps, APR, the venue's predicted APR, seconds to settlement, basis_bps) \
                       and realised vol (rv_1h_bps, rv_24h_bps, vol_ratio, with the bar counts \
                       behind each). READ THE covers_band FLAG on each depth band: false means \
                       the venue's ladder stopped short of that band, so the figure is the \
                       whole of that side's book and a floor on the real depth, not the band's \
                       contents. micro_tilt_bps may be null on the first call for a symbol \
                       while its quote feed warms up; call again."
    )]
    async fn get_features(
        &self,
        Parameters(params): Parameters<GetFeaturesParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::bound(&ctx)?;
        let inner = &self.inner;
        let now = now_ms();
        let universe = self.universe().await?;
        let asset = universe
            .get(&params.symbol)
            .map_err(|_| ToolError::InvalidParams {
                field: "symbol",
                detail: format!("{} is not a symbol on this venue", params.symbol),
            })?;
        let symbol = asset.name().to_owned();

        // The quote feed is leased before anything is fetched, so the socket
        // has the whole of the rest of this call to deliver its first frame.
        let leased = inner.quotes.lease(&symbol, now);

        let book = inner
            .info
            .l2_book(&symbol, None)
            .await
            .map_err(|e| ToolError::unavailable("book", e))?;
        let contexts = inner
            .info
            .meta_and_asset_ctxs()
            .await
            .map_err(|e| ToolError::unavailable("asset contexts", e))?;
        let asset_ctx = contexts
            .iter()
            .find(|(info, _)| info.name == symbol)
            .map(|(_, ctx)| ctx)
            .ok_or_else(|| ToolError::Unavailable {
                what: "asset context",
                detail: format!("the venue did not return a context for {symbol}"),
            })?;

        // A failed prediction is an absent field, not a failed call: it is one
        // of five funding numbers and the other four are still true.
        let predicted = match inner.info.predicted_fundings().await {
            Ok(rows) => rows
                .iter()
                .find(|row| row.coin() == symbol)
                .and_then(|row| row.hyperliquid())
                .map(|funding| funding.funding_rate),
            Err(error) => {
                tracing::warn!(%error, symbol, "no predicted funding");
                None
            }
        };

        let (minutes, hours) = self.vol_bars(&symbol, now).await;
        let quote = match leased {
            Some(quote) => Some(quote),
            // Nothing held, so this is a first look at the symbol. Wait a
            // bounded moment for the frame the lease just asked for.
            None => inner.quotes.warm(&symbol).await,
        };

        let body = serde_json::json!({
            "contract_version": 0,
            "symbol": symbol,
            "as_of_ms": now,
            "book": book_features(&book, quote.as_ref()),
            "funding": funding_features(asset_ctx, predicted, now),
            "vol": vol_features(&minutes, &hours),
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    /// The two bar series the vol pack reads.
    ///
    /// A failed fetch is an empty series rather than a failed call: the vol
    /// pack then reports `bars_1h: 0` and a null σ, which is the honest answer
    /// and leaves the book and funding packs — which did arrive — readable.
    async fn vol_bars(&self, symbol: &str, now_ms: u64) -> (Vec<Candle>, Vec<Candle>) {
        let minute_ms = 60_000;
        let fetch = |interval: &'static str, span_ms: u64| async move {
            self.inner
                .info
                .candles(symbol, interval, now_ms.saturating_sub(span_ms), now_ms)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, symbol, interval, "no candles");
                    Vec::new()
                })
        };
        (
            fetch("1m", 60 * minute_ms).await,
            fetch("1h", 24 * 60 * minute_ms).await,
        )
    }

    /// `get_alerts` — what this agent is watching for (`docs/spec.md` item 22).
    #[tool(
        description = "Everything you have armed or that has fired, newest first. An alert with \
                       no fired_at_ms is still watching. You are amnesiac across sessions, so \
                       this is how you find out what a previous you asked to be woken for — and \
                       where the alert_id to cancel one comes from. Only your own alerts."
    )]
    async fn get_alerts(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        let alerts = self
            .inner
            .alerts
            .for_agent(bound.agent.as_str())
            .map_err(|e| ToolError::unavailable("alerts", e))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({ "contract_version": 0, "alerts": alerts }).to_string(),
        )]))
    }

    /// `cancel_alert` — disarming one (`docs/spec.md` item 22).
    #[tool(
        description = "Stop watching for a condition you armed. Cancelling frees a slot and \
                       releases the market feed the alert was holding. An alert that has \
                       already fired cannot be cancelled — it is history — and answers the \
                       same way as an id you do not own: cancelled false, no error."
    )]
    async fn cancel_alert(
        &self,
        Parameters(params): Parameters<CancelAlertParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        // `false` rather than an error: the caller wanted the alert not to be
        // watching, and it is not watching. Reporting an id it does not own as
        // a distinct failure would let it probe for another agent's ids.
        let cancelled = self
            .inner
            .alerts
            .cancel(bound.agent.as_str(), params.alert_id)
            .map_err(|e| ToolError::unavailable("alerts", e))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({
                "contract_version": 0,
                "alert_id": params.alert_id,
                "cancelled": cancelled,
            })
            .to_string(),
        )]))
    }

    /// `recall` — reading it back (`docs/spec.md` item 21).
    #[tool(
        description = "Read back what you remembered: one key, or everything, newest first. \
                       A key you never wrote comes back empty rather than as an error."
    )]
    async fn recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        let agent = bound.agent.as_str();

        // One shape either way: a caller that asked for one key and a caller
        // that asked for all of them read the same field, and "not found" is
        // an empty list rather than a different envelope to branch on.
        let notes = match &params.key {
            Some(key) => self
                .inner
                .journal
                .recall(agent, key)
                .map_err(|e| ToolError::unavailable("journal", e))?
                .into_iter()
                .collect(),
            None => self
                .inner
                .journal
                .recall_all(agent)
                .map_err(|e| ToolError::unavailable("journal", e))?,
        };

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({ "contract_version": 0, "notes": notes }).to_string(),
        )]))
    }

    /// `get_events` — the durable record (`docs/spec.md` item 18, D6).
    #[tool(
        description = "Events since a cursor: your orders, fills, refusals and guardrail trips, \
                       plus account-wide ones like the kill switch and feed drops. Pass \
                       next_cursor back to continue. If resync_required is true your cursor is \
                       too old or from another chain — discard local state and re-read from \
                       get_state, never assume the gap was empty."
    )]
    async fn get_events(
        &self,
        Parameters(params): Parameters<GetEventsParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        // The ledger clamps to its own page cap, so a caller asking for more
        // gets the cap rather than a refusal — the cursor makes a short page
        // correct either way.
        let limit = params.limit.unwrap_or(oppen_core::ledger::MAX_PAGE);
        let bound = Self::bound(&ctx)?;
        Ok(self.events_page(&bound, limit, params.since_cursor)?)
    }

    /// The body of `get_events`, over an already-resolved binding.
    ///
    /// Split out because `rmcp::service::Peer` cannot be constructed outside
    /// that crate, so a `RequestContext` cannot be built in a unit test. The
    /// tool method above is then exactly one thing — resolving who is asking —
    /// and this is everything that depends on the answer.
    fn events_page(
        &self,
        bound: &Binding,
        limit: usize,
        since_cursor: u64,
    ) -> Result<CallToolResult, ToolError> {
        let page = self
            .inner
            .events
            .for_agent(bound.agent.as_str())
            .get_events(since_cursor, limit)
            .map_err(|e| ToolError::unavailable("ledger", e))?;

        // `EventPage` is already the wire shape, in declaration order. The
        // envelope adds the version and nothing else, so there is one
        // serialization of one page (`AGENTS.md` invariant 6).
        let body = serde_json::json!({
            "contract_version": 0,
            "events": page.events,
            "next_cursor": page.next_cursor,
            "resync_required": page.resync_required,
            "head_seq": page.head_seq,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    /// `get_order_status` — the reconcile after a timeout
    /// (`docs/spec.md` item 19).
    #[tool(
        description = "The venue's own status for one order, by oid or cloid. This is the only \
                       safe move after timeout_unknown_outcome: query by the cloid the failed \
                       call returned, never resend the order."
    )]
    async fn get_order_status(
        &self,
        Parameters(params): Parameters<OrderStatusParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        self.require_route(&bound).await?;
        let reference = match (params.oid, &params.cloid) {
            (Some(oid), _) => OrderRef::Oid(oid),
            (None, Some(cloid)) => {
                OrderRef::Cloid(Cloid::parse(cloid).map_err(|e| ToolError::invalid("cloid", e))?)
            }
            (None, None) => {
                return Err(ToolError::invalid("oid", "supply either oid or cloid").into());
            }
        };

        let status = self
            .inner
            .info
            .order_status(bound.account, reference)
            .await
            .map_err(|e| ToolError::unavailable("order status", e))?;

        // A not-yet-visible order is still ambiguous after a timeout. Never
        // turn a single unknown lookup into permission to resend.
        let body = match status {
            OrderStatusResponse::UnknownOid => serde_json::json!({
                "contract_version": 0,
                "known": false,
            }),
            OrderStatusResponse::Order { order } => serde_json::json!({
                "contract_version": 0,
                "known": true,
                "status": order.status,
                "status_timestamp_ms": order.status_timestamp,
                "oid": order.order.oid,
                "cloid": order.order.cloid.as_ref().map(|c| c.as_str()),
                "symbol": order.order.coin,
                "is_buy": order.order.side.is_buy(),
                "limit_px": order.order.limit_px,
                "sz_remaining": order.order.sz,
                "sz_original": order.order.orig_sz,
            }),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(
            body.to_string(),
        )]))
    }

    /// `get_state` — the agent's eyes (`docs/spec.md` item 16).
    #[tool(
        description = "The account right now: equity, margin, open positions with distance to \
                       liquidation, resting orders, and feed freshness. Read this before acting. \
                       If `feed` is not `live`, the data is stale and execution will fail closed. \
                       `policy_status` is a cached local observation, not fresh authority. \
                       An uninhibited observation does not mean ready: all other execution gates apply."
    )]
    async fn get_state(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        self.require_route(&bound).await?;
        let now = now_ms();
        let mut state = self.read_state(bound.account, now).await?;
        self.add_position_risk(&mut state, now).await;
        let before = self.inner.engine.policy_status();
        if !before.admission_inhibited {
            self.add_loss_budget(&mut state, &bound, now).await;
        }
        let policy_status = self.inner.engine.policy_status();
        if policy_status.admission_inhibited || before != policy_status {
            state.loss_budget.clear();
        }
        #[derive(serde::Serialize)]
        struct StateReply {
            #[serde(flatten)]
            account: AccountState,
            policy_status: oppen_core::guardrail::PolicyStatus,
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&StateReply {
                account: state,
                policy_status,
            })
            .expect("StateReply serializes"),
        )]))
    }

    /// `get_execution_report` — what your execution actually cost
    /// (`docs/spec.md` spec F).
    #[tool(
        description = "Transaction cost analysis of your own fills: slippage against the price \
                       each order was decided at, split by whether you crossed the spread or \
                       rested, plus realized PnL and fees. Every statistic carries the number of \
                       fills behind it. Read it before concluding a strategy works: a good mean \
                       over four fills is not evidence."
    )]
    async fn get_execution_report(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(params): Parameters<ExecutionReportParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        let to_ms = now_ms() as i64;
        let hours = i64::from(params.hours.unwrap_or(DEFAULT_REPORT_HOURS));
        let from_ms = to_ms.saturating_sub(hours.saturating_mul(3_600_000));

        let payloads = self
            .inner
            .events
            .for_agent(bound.agent.as_str())
            .fills_between(from_ms, to_ms)
            .map_err(|e| ToolError::unavailable("ledger", e))?;
        // A row that cannot be scored is counted, never dropped: a report
        // whose denominator quietly excluded half the account's trading would
        // be the most misleading artefact in the product.
        let scored: Vec<ScoredFill> = payloads
            .iter()
            .filter_map(ScoredFill::from_payload)
            .collect();
        let unscored = payloads.len() - scored.len();
        let report = ExecutionReport::of(&scored, from_ms, to_ms, unscored);

        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&report).expect("ExecutionReport serializes"),
        )]))
    }

    /// The one place a `Cleared` becomes bytes on the wire.
    ///
    /// Every acting tool goes through here, so the nonce, the signer and the
    /// unknown-outcome classification are written once. A transport failure is
    /// **always** `timeout_unknown_outcome`, never a retryable error: the
    /// request may have been applied, and the cost of being wrong in that
    /// direction is a duplicate order against the cost of one cheap
    /// `orderStatus` query in the other (`docs/spec.md` item 19).
    async fn submit(
        &self,
        cleared: Cleared,
        cloid: Option<&Cloid>,
        bound: &Binding,
        submission: Option<ExecutionPermit>,
        tracker: Option<ExecutionTracker>,
    ) -> Result<ExchangeResponse, ToolError> {
        self.submit_authorized(cleared, cloid, bound, submission, None, tracker)
            .await
    }

    async fn submit_cancellation(
        &self,
        cleared: Cleared,
        bound: &Binding,
        execution: tokio::sync::OwnedMutexGuard<()>,
        operator: Option<&crate::server::OperatorWork>,
        tracker: ExecutionTracker,
    ) -> Result<ExchangeResponse, ToolError> {
        if matches!(
            cleared.clearance().kind,
            oppen_core::guardrail::ClearedKind::DiscretionaryCancel { .. }
        ) {
            return self
                .submit_retained(
                    cleared,
                    None,
                    bound,
                    SubmissionOwnership::Cancellation {
                        account: bound.account,
                        _queue: execution,
                    },
                    operator,
                    Some(tracker),
                )
                .await;
        }
        // Runtime cleanup retains its separate admission and capacity.
        let _execution = execution;
        self.submit_authorized(cleared, None, bound, None, operator, Some(tracker))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_authorized(
        &self,
        cleared: Cleared,
        cloid: Option<&Cloid>,
        bound: &Binding,
        submission: Option<ExecutionPermit>,
        operator: Option<&crate::server::OperatorWork>,
        tracker: Option<ExecutionTracker>,
    ) -> Result<ExchangeResponse, ToolError> {
        if cleared.clearance().agent != bound.agent
            || cleared.clearance().route.binding.container != bound.account
        {
            return Err(ToolError::unavailable(
                "submission route",
                "clearance and pairing identity differ",
            ));
        }
        if matches!(
            cleared.clearance().kind,
            oppen_core::guardrail::ClearedKind::Order { .. }
        ) {
            let submission = submission.ok_or_else(|| {
                ToolError::unavailable("submission ledger", "order has no account reservation")
            })?;
            return self
                .submit_retained(
                    cleared,
                    cloid,
                    bound,
                    SubmissionOwnership::Order(submission),
                    operator,
                    tracker,
                )
                .await;
        }
        if matches!(
            cleared.clearance().kind,
            oppen_core::guardrail::ClearedKind::DiscretionaryCancel { .. }
        ) {
            return Err(ToolError::unavailable(
                "submission owner",
                "discretionary cancellation requires retained account authority",
            ));
        }
        self.submit_inline(cleared, cloid, bound, None, operator, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_retained(
        &self,
        cleared: Cleared,
        cloid: Option<&Cloid>,
        bound: &Binding,
        ownership: SubmissionOwnership,
        operator: Option<&crate::server::OperatorWork>,
        tracker: Option<ExecutionTracker>,
    ) -> Result<ExchangeResponse, ToolError> {
        if cleared.clearance().agent != bound.agent
            || cleared.clearance().route.binding.container != bound.account
        {
            return Err(ToolError::unavailable(
                "submission route",
                "clearance, execution owner and pairing identity differ",
            ));
        }
        if ownership.account() != bound.account {
            return Err(ToolError::unavailable(
                "submission ledger",
                "reservation and bound account differ",
            ));
        }
        let tracker = operator
            .map(|work| work.tracker.clone())
            .or(tracker)
            .ok_or_else(|| {
                ToolError::unavailable("submission owner", "submission has no execution tracker")
            })?;
        let slot = self
            .inner
            .submission_worker
            .clone()
            .try_acquire_owned()
            .map_err(|error| ToolError::unavailable("submission worker busy", error))?;
        let gateway = self.clone();
        let bound = bound.clone();
        let cloid = cloid.cloned();
        let operator = operator.cloned();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            runtime.block_on(gateway.submit_inline(
                cleared,
                cloid.as_ref(),
                &bound,
                ownership.order(),
                operator.as_ref(),
                Some(&tracker),
            ))
        })
        .await
        .map_err(|error| ToolError::worker_failed("submission worker", error))?
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_inline(
        &self,
        cleared: Cleared,
        cloid: Option<&Cloid>,
        bound: &Binding,
        submission: Option<&ExecutionPermit>,
        operator: Option<&crate::server::OperatorWork>,
        tracker: Option<&ExecutionTracker>,
    ) -> Result<ExchangeResponse, ToolError> {
        let inner = &self.inner;
        let receipt = if matches!(
            cleared.clearance().kind,
            oppen_core::guardrail::ClearedKind::Order { .. }
        ) {
            let permit = submission.ok_or_else(|| {
                ToolError::unavailable("submission ledger", "order has no account reservation")
            })?;
            if permit.account != bound.account {
                return Err(ToolError::unavailable(
                    "submission ledger",
                    "reservation and bound account differ",
                ));
            }
            Some(
                inner
                    .submissions
                    .begin(
                        bound.account,
                        cleared.clearance(),
                        permit.revision,
                        now_ms(),
                    )
                    .map_err(submission_error)?,
            )
        } else {
            None
        };
        let nonce = inner.nonces.next();
        let authorize = || {
            operator
                .map(|work| work.signing_admission())
                .or_else(|| tracker.map(|tracker| tracker.signing_admission(bound)))
                .transpose()
                .map_err(|error| {
                    oppen_core::guardrail::Refusal::from(
                        oppen_core::guardrail::Unevaluable::RouteAuthority {
                            detail: error.to_string(),
                        },
                    )
                })
        };
        if let Some(receipt) = &receipt {
            let signed = inner
                .engine
                .sign_submission_authorized(
                    cleared,
                    &inner.submissions,
                    receipt,
                    nonce,
                    None,
                    self::now_ms,
                    authorize,
                )
                .map_err(|error| {
                    signing_submission_error(&inner.submissions, Some(receipt), error)
                })?;
            let result = match inner
                .engine
                .post_submission_authorized(signed, &inner.exchange, self::now_ms, authorize)
                .await
            {
                Ok(response) => Ok(response),
                Err(oppen_core::guardrail::SubmissionPostError::Transport(error)) => Err(error),
                Err(oppen_core::guardrail::SubmissionPostError::NotSent(refusal)) => {
                    return Err(signing_submission_error(
                        &inner.submissions,
                        Some(receipt),
                        oppen_core::guardrail::SignClearedError::Refused(refusal),
                    ));
                }
                Err(oppen_core::guardrail::SubmissionPostError::JournalUncertain(error)) => {
                    return Err(ToolError::TimeoutUnknownOutcome {
                        cloid: Some(receipt.cloid().as_str().to_owned()),
                        detail: error.to_string(),
                    });
                }
            };
            return track_submission(
                cloid,
                Some((&inner.submissions, receipt)),
                std::future::ready(result),
            )
            .await;
        }
        let discretionary = matches!(
            cleared.clearance().kind,
            oppen_core::guardrail::ClearedKind::DiscretionaryCancel { .. }
        );
        if discretionary {
            let signed = inner
                .engine
                .sign_discretionary_cancel_authorized(cleared, nonce, None, self::now_ms, authorize)
                .map_err(|error| signing_submission_error(&inner.submissions, None, error))?;
            let response = match inner
                .engine
                .post_cancellation_authorized(signed, &inner.exchange, self::now_ms, authorize)
                .await
            {
                Ok(response) => Ok(response),
                Err(oppen_core::guardrail::SubmissionPostError::Transport(error)) => Err(error),
                Err(oppen_core::guardrail::SubmissionPostError::NotSent(refusal)) => {
                    return Err(ToolError::GuardrailRefused { refusal });
                }
                Err(oppen_core::guardrail::SubmissionPostError::JournalUncertain(error)) => {
                    return Err(ToolError::TimeoutUnknownOutcome {
                        cloid: cloid.map(|cloid| cloid.as_str().to_owned()),
                        detail: error.to_string(),
                    });
                }
            };
            return track_submission(cloid, None, std::future::ready(response)).await;
        }
        let signed =
            inner
                .engine
                .sign_cleared_authorized(cleared, nonce, None, self::now_ms, authorize);
        let (request, _clearance) = match signed {
            Ok(signed) => signed,
            Err(error) => {
                return Err(signing_submission_error(&inner.submissions, None, error));
            }
        };

        track_submission(cloid, None, inner.exchange.post(&request)).await
    }

    /// Everything [`GuardrailEngine::evaluate`] needs, assembled once.
    ///
    /// `place` and `close_position` both call it, and they must: two
    /// assemblies of the same inputs drift, and the one that drifts is the one
    /// that decides whether an order is refused.
    async fn evaluation_context(
        &self,
        bound: &Binding,
        symbol: &str,
        now_ms: u64,
    ) -> Result<EvaluationContext, ToolError> {
        self.evaluation_context_tracked(bound, symbol, now_ms, None)
            .await
    }

    async fn evaluation_context_tracked(
        &self,
        bound: &Binding,
        symbol: &str,
        now_ms: u64,
        tracker: Option<ExecutionTracker>,
    ) -> Result<EvaluationContext, ToolError> {
        self.require_route_tracked(bound, tracker).await?;
        let inner = &self.inner;
        let feed_stamp = inner.feed.stamp();
        let account = bound.account;
        let universe = self.universe().await?;
        let state = self.read_state(account, now_ms).await?;

        // Realised PnL is summed from the venue's own fills for the UTC day,
        // net of fees: a daily-loss limit that ignores fees is not a limit.
        let day_start_ms = utc_day_start_ms(now_ms);
        let fills = inner
            .info
            .user_fills_by_time(account, day_start_ms, Some(now_ms))
            .await
            .map_err(|e| ToolError::unavailable("fills", e))?;
        let portfolio = inner
            .info
            .portfolio(account)
            .await
            .map_err(|e| ToolError::unavailable("portfolio", e))?;

        let mut exposure = exposure_from(
            &state,
            realized_pnl_since(&fills, day_start_ms),
            portfolio.window("day").and_then(|w| w.peak_account_value()),
            // Whether a reconcile has actually returned since the last
            // outage. The engine decides what that means, not this gateway —
            // and it refuses while it is false, which is why nothing could
            // clear before the feed session existed.
            inner.feed.state().reconciled,
            day_start_ms,
        );
        exposure.feed_stamp = Some(feed_stamp);

        // A missing price is a refusal, never a fallback. This used to read
        // `allMids`, which answers for an unquoted asset with a frozen last
        // print — so the refusal this comment describes never happened and the
        // caps below were measured against a price that could be 9.9× off.
        let reference_px = self.reference_pxs().await?.get(symbol);
        // Spec F's vol-scaled cap needs a volatility, and this is the one
        // path that pays for it — but only for an agent that configured one.
        // K1 kept sigma off the signing path because no predicate read it;
        // now one does, so the cost is owed, and owed by exactly the agents
        // whose caps depend on it. The engine refuses rather than sizing
        // against a `None`, so a fetch that fails is a refusal and never a
        // silently unenforced cap.
        let volatility = match inner
            .engine
            .guardrails(&bound.agent)
            .and_then(|config| config.risk.max_risk_usd)
        {
            Some(_) => self.volatility(symbol, now_ms).await,
            None => None,
        };
        let market = MarketRef {
            symbol: symbol.to_owned(),
            reference_px,
            as_of_ms: now_ms,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
            sigma_day: volatility.map(|v| v.sigma_day),
            vol_ratio: volatility.and_then(|v| v.vol_ratio),
        };

        Ok(EvaluationContext {
            universe,
            state,
            exposure,
            market,
        })
    }

    /// The agent's configured max slippage, in basis points.
    ///
    /// Read from the engine rather than taken from the caller: item 24 and D3
    /// make slippage operator-set, and an agent that could widen it to cross
    /// the book could widen it to cross a worse one. In bps because that is
    /// what the guardrail config stores and what every refusal reports — the
    /// conversions to fractions live in the normalization helpers.
    fn operator_slippage_bps(&self, bound: &Binding) -> Result<Decimal, ToolError> {
        let guardrails =
            self.inner
                .engine
                .guardrails(&bound.agent)
                .ok_or_else(|| ToolError::Unavailable {
                    what: "guardrails",
                    detail: format!(
                        "no verified policy projection is available for {}",
                        bound.agent
                    ),
                })?;
        Ok(guardrails.max_slippage_bps)
    }

    /// Fill spec F's σ-unit risk fields on an assembled state.
    ///
    /// **Deliberately here and not in `read_state`.** `evaluation_context`
    /// calls that on the signing path, and these are numbers an agent reads
    /// rather than numbers a guardrail evaluates — so `place` never pays for a
    /// candle fetch it does not use (`docs/decisions.md` K1).
    ///
    /// Every input is optional and a missing one leaves its field absent
    /// rather than failing the call: a state with no σ is still a state worth
    /// answering with.
    async fn add_position_risk(&self, state: &mut AccountState, now_ms: u64) {
        if state.positions.is_empty() {
            return;
        }
        // One call covers funding for every symbol held, so the cost does not
        // grow with the position count.
        let contexts = match self.inner.info.meta_and_asset_ctxs().await {
            Ok(contexts) => contexts,
            Err(error) => {
                tracing::warn!(%error, "no asset contexts; position risk omitted");
                return;
            }
        };
        let funding: std::collections::HashMap<&str, Decimal> = contexts
            .iter()
            .map(|(info, ctx)| (info.name.as_str(), ctx.funding.hour_to_date_1h()))
            .collect();
        // The same read the order path sizes against, from the same response:
        // an asset the venue has stopped quoting has no price here either, so
        // its risk fields stay absent rather than being computed against a
        // frozen print (`docs/decisions.md` H1).
        let marks = contexts.reference_pxs();

        let mut total_carry = Decimal::ZERO;
        for position in &mut state.positions {
            // The *measured* daily sigma, not the hour-corrected one: this
            // answers "how many ordinary days of movement to liquidation",
            // and tightening it by a live regime would make the distance
            // shrink for a reason that has nothing to do with the position.
            // The correction belongs to the cap, which is a limit; this is a
            // description.
            let sigma = self
                .volatility(&position.symbol, now_ms)
                .await
                .map(|volatility| volatility.sigma_day);
            let mark = marks.get(&position.symbol);
            let risk = position_risk(
                position.size,
                mark,
                position.liquidation_px,
                sigma,
                funding.get(position.symbol.as_str()).copied(),
            );
            if let Some(carry) = risk.carry_usd_per_day {
                total_carry += carry;
            }
            position.liq_distance_sigma = risk.liq_distance_sigma;
            position.carry_usd_per_day = risk.carry_usd_per_day;
        }
        state.margin_runway_h = margin_runway_h(state.balances.withdrawable_usd, total_carry);
    }

    /// Fills the loss-budget gauge (`docs/spec.md` spec F).
    ///
    /// **Here rather than in `read_state`, for the K1 reason again**, but with
    /// a sharper edge: `evaluation_context` already assembles this exposure on
    /// the signing path, and the engine already runs the breaker over it
    /// there. Computing the gauge in `read_state` would make `place` pay to
    /// *display* a number it is about to enforce anyway.
    ///
    /// The two venue reads are the ones the exposure needs and no more: the
    /// day's fills for realised PnL and the portfolio for the equity
    /// high-water mark. Both are the same calls `evaluation_context` makes,
    /// so the gauge an agent reads and the budget its next order is measured
    /// against are built from the same two responses — a gauge assembled from
    /// a cheaper approximation would be a second, quieter definition of the
    /// number that decides whether the account keeps trading.
    ///
    /// A failed read leaves the gauge empty rather than failing `get_state`:
    /// an account state without a gauge is still worth answering with, and the
    /// breaker itself is unaffected — it runs on the order path from its own
    /// fetch, so nothing here can make a budget go unenforced.
    ///
    /// **Reported while the account is unreconciled, unlike the breaker.** The
    /// engine refuses an *order* on an unreconciled snapshot because its caps
    /// are measured against position sizes it may have mis-stated. The gauge
    /// is measured against equity and the day's fills, both read from the
    /// venue in this same call — and an agent that sized up on a stale gauge
    /// would still be refused when it tried, by the very predicate it was
    /// looking at. Withholding the number in the one state where an agent most
    /// wants to know how much room it has left would buy nothing.
    async fn add_loss_budget(&self, state: &mut AccountState, bound: &Binding, now_ms: u64) {
        let inner = &self.inner;
        let day_start_ms = utc_day_start_ms(now_ms);
        let fills = match inner
            .info
            .user_fills_by_time(bound.account, day_start_ms, Some(now_ms))
            .await
        {
            Ok(fills) => fills,
            Err(error) => {
                tracing::warn!(%error, "no fills; loss budget omitted");
                return;
            }
        };
        let portfolio = match inner.info.portfolio(bound.account).await {
            Ok(portfolio) => portfolio,
            Err(error) => {
                tracing::warn!(%error, "no portfolio; loss budget omitted");
                return;
            }
        };
        let exposure = exposure_from(
            state,
            realized_pnl_since(&fills, day_start_ms),
            portfolio.window("day").and_then(|w| w.peak_account_value()),
            inner.feed.state().reconciled,
            day_start_ms,
        );
        state.loss_budget = inner.engine.loss_budget(&bound.agent, &exposure, now_ms);
    }

    /// One symbol's daily σ as a fraction, measured at most once per TTL.
    async fn volatility(&self, symbol: &str, now_ms: u64) -> Option<Volatility> {
        if let Some(volatility) = self.inner.sigmas.get(symbol, now_ms) {
            return Some(volatility);
        }
        let hour_ms = 60 * 60 * 1_000;
        let day_ms = 24 * hour_ms;
        // Both legs, because `vol_features` derives the ratio between them and
        // deriving it here from two separately-fetched numbers would be the
        // same arithmetic in a place with no tests. The minute leg is the one
        // that notices a regime an hour old; the hourly leg is what the cap is
        // denominated in.
        let (hours, minutes) = tokio::join!(
            self.candles_or_empty(symbol, "1h", now_ms.saturating_sub(day_ms), now_ms),
            self.candles_or_empty(symbol, "1m", now_ms.saturating_sub(hour_ms), now_ms),
        );
        let features = vol_features(&minutes, &hours);
        // The daily sigma is the denominator and its absence is a refusal
        // upstream; the ratio only ever tightens, so it rides along as an
        // `Option` and a missing minute series costs the correction, not the
        // cap.
        let sigma_day = features
            .rv_24h_bps
            .and_then(|bps| bps.checked_div(BPS_PER_UNIT))
            .filter(|sigma| *sigma > Decimal::ZERO)?;
        let volatility = Volatility {
            sigma_day,
            vol_ratio: features.vol_ratio,
        };
        self.inner.sigmas.put(symbol, volatility, now_ms);
        Some(volatility)
    }

    /// Candles for a window, or none — a volatility measurement that cannot
    /// fetch its bars degrades to a missing reading, which every caller
    /// already handles, rather than to an error that would refuse an order for
    /// a reason the agent cannot act on.
    async fn candles_or_empty(
        &self,
        symbol: &str,
        interval: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Vec<Candle> {
        self.inner
            .info
            .candles(symbol, interval, from_ms, to_ms)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, symbol, interval, "no candles for sigma");
                Vec::new()
            })
    }

    /// The prices oppen will size against, from the venue's own contexts.
    ///
    /// One reader for both `get_state` and the guardrail path, so a liquidation
    /// distance and a notional cap can never disagree about what an asset is
    /// worth. `metaAndAssetCtxs` rather than `allMids`: see
    /// [`oppen_hl::types::ReferencePrices`] for what that map does to an asset
    /// the venue has stopped quoting.
    async fn reference_pxs(&self) -> Result<ReferencePrices, ToolError> {
        Ok(self
            .inner
            .info
            .meta_and_asset_ctxs()
            .await
            .map_err(|e| ToolError::unavailable("asset contexts", e))?
            .reference_pxs())
    }

    /// The validated universe, which every tool that names a symbol needs.
    async fn universe(&self) -> Result<Universe, ToolError> {
        let meta = self
            .inner
            .info
            .meta()
            .await
            .map_err(|e| ToolError::unavailable("meta", e))?;
        Universe::from_meta(&meta).map_err(|e| ToolError::unavailable("universe", e))
    }

    /// The four venue reads `get_state` and `place` share.
    ///
    /// Four and not one: Hyperliquid publishes no single endpoint that answers
    /// this, and spot is load-bearing because margin is unified.
    async fn read_state(&self, account: Address, now_ms: u64) -> Result<AccountState, ToolError> {
        let inner = &self.inner;

        let perps = inner
            .info
            .clearinghouse_state(account)
            .await
            .map_err(|e| ToolError::unavailable("clearinghouse", e))?;
        let spot = inner
            .info
            .spot_clearinghouse_state(account)
            .await
            .map_err(|e| ToolError::unavailable("spot", e))?;
        let orders = inner
            .info
            .frontend_open_orders(account)
            .await
            .map_err(|e| ToolError::unavailable("orders", e))?;
        let mids = self.reference_pxs().await?;

        Ok(assemble(
            inner.network,
            account,
            now_ms,
            &VenueReadings {
                perps: &perps,
                spot: &spot,
                orders: &orders,
                mids: &mids,
                // The socket's own answer. `None` still means never
                // connected, which item 34 keeps distinct from having gone
                // quiet — but it is now a fact rather than a placeholder.
                last_tick_ms: inner.feed.state().last_tick_ms,
            },
        ))
    }
}

fn cancellation_context(
    bound: &Binding,
    orders: &[oppen_hl::types::OpenOrder],
    universe: &Universe,
    observed_at_ms: u64,
) -> Result<CancelContext, ToolError> {
    let targets = orders
        .iter()
        .map(|order| {
            let asset = universe
                .get(&order.coin)
                .map_err(|error| ToolError::unavailable("cancellation asset", error))?;
            Ok(CancelTarget {
                symbol: order.coin.clone(),
                asset_index: asset.index,
                oid: order.oid,
                cloid: order.cloid.clone(),
                is_buy: order.side.is_buy(),
                limit_px: order.limit_px,
                sz: order.sz,
                orig_sz: order.orig_sz,
                timestamp: order.timestamp,
                order_type: order.order_type.clone(),
                tif: order.tif,
                reduce_only: order.reduce_only,
                is_trigger: order.is_trigger,
                trigger_px: order.trigger_px,
                trigger_condition: order.trigger_condition.clone(),
                is_position_tpsl: order.is_position_tpsl,
            })
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    Ok(CancelContext {
        account: bound.account,
        observed_at_ms,
        targets,
    })
}

fn signing_submission_error(
    journal: &SubmissionJournal,
    receipt: Option<&SubmissionReceipt>,
    error: oppen_core::guardrail::SignClearedError,
) -> ToolError {
    if let Some(receipt) = receipt
        && let Err(error) = journal.resolve(
            receipt,
            SubmissionResolution::NotSent {
                detail: error.to_string(),
            },
            now_ms(),
        )
    {
        return submission_error(error);
    }
    match error {
        oppen_core::guardrail::SignClearedError::Refused(refusal) => {
            ToolError::GuardrailRefused { refusal }
        }
        other => ToolError::unavailable("signer", other),
    }
}

async fn track_submission(
    cloid: Option<&Cloid>,
    durable: Option<(&SubmissionJournal, &SubmissionReceipt)>,
    post: impl Future<Output = Result<ExchangeResponse, oppen_hl::Error>>,
) -> Result<ExchangeResponse, ToolError> {
    let cloid = durable.map(|(_, receipt)| receipt.cloid()).or(cloid);
    let response = match post.await {
        Ok(response) => response,
        Err(oppen_hl::Error::ExchangeRejected { message }) => {
            if let Some((journal, receipt)) = durable {
                journal
                    .resolve(
                        receipt,
                        SubmissionResolution::Rejected {
                            message: message.clone(),
                        },
                        now_ms(),
                    )
                    .map_err(submission_error)?;
            }
            return Err(ToolError::venue(200, message));
        }
        Err(oppen_hl::Error::Venue { status, message }) => {
            return Err(ToolError::venue(status, message));
        }
        Err(transport) => {
            return Err(ToolError::TimeoutUnknownOutcome {
                cloid: cloid.map(|c| c.as_str().to_owned()),
                detail: transport.to_string(),
            });
        }
    };
    // Only a structured rejection proves that this request cannot fill.
    // Successful submissions remain reserved until a later status read,
    // followed by fresh account reads under the same container lock.
    if response.kind == ExchangeResponseKind::Order
        && let [Status::Error(message)] = response.statuses.as_slice()
        && let Some((journal, receipt)) = durable
    {
        journal
            .resolve(
                receipt,
                SubmissionResolution::Rejected {
                    message: message.clone(),
                },
                now_ms(),
            )
            .map_err(submission_error)?;
    }
    Ok(response)
}

// Return the account lock only after the previous submission is visible at the
// venue. Callers then refresh exposure and keep this lock through submission.
#[cfg(test)]
async fn reserve_account<F, Fut>(
    queue: Arc<tokio::sync::Mutex<()>>,
    journal: &SubmissionJournal,
    account: Address,
    lookup: F,
) -> Result<ExecutionPermit, ToolError>
where
    F: FnOnce(Cloid) -> Fut,
    Fut: Future<Output = Result<OrderStatusResponse, ToolError>>,
{
    let held = queue.lock_owned().await;
    reserve_account_held(held, journal, account, lookup).await
}

async fn reserve_account_held<F, Fut>(
    held: tokio::sync::OwnedMutexGuard<()>,
    journal: &SubmissionJournal,
    account: Address,
    lookup: F,
) -> Result<ExecutionPermit, ToolError>
where
    F: FnOnce(Cloid) -> Fut,
    Fut: Future<Output = Result<OrderStatusResponse, ToolError>>,
{
    let mut state = journal.state(account).map_err(submission_error)?;
    if let Some(receipt) = state.pending.as_ref() {
        match lookup(receipt.cloid().clone()).await? {
            OrderStatusResponse::Order { order } => {
                if order.order.cloid.as_ref().is_some_and(|cloid| cloid != receipt.cloid()) {
                    return Err(ToolError::TimeoutUnknownOutcome {
                        cloid: Some(receipt.cloid().as_str().to_owned()),
                        detail: "venue returned a different cloid; reservation retained".into(),
                    });
                }
                journal.resolve(receipt, SubmissionResolution::Observed { oid: order.order.oid, status: order.status }, now_ms()).map_err(submission_error)?;
                state = journal.state(account).map_err(submission_error)?;
            }
            OrderStatusResponse::UnknownOid => return Err(ToolError::TimeoutUnknownOutcome {
                cloid: Some(receipt.cloid().as_str().to_owned()),
                detail: "previous submission is not confirmed by the venue; this container remains reserved".into(),
            }),
        }
    }
    if let Some(pending) = state.pending {
        return Err(ToolError::TimeoutUnknownOutcome {
            cloid: Some(pending.cloid().as_str().to_owned()),
            detail: "another gateway reserved this account during reconciliation".into(),
        });
    }
    Ok(ExecutionPermit {
        _queue: held,
        account,
        revision: state.revision,
    })
}

fn submission_error(error: SubmissionError) -> ToolError {
    match error {
        SubmissionError::Pilot(error) => ToolError::GuardrailRefused {
            refusal: error.into_refusal(),
        },
        SubmissionError::Busy { cloid } => ToolError::TimeoutUnknownOutcome {
            cloid: Some(cloid),
            detail: "this account already has an unresolved durable submission".into(),
        },
        SubmissionError::DuplicateCloid => {
            ToolError::invalid("cloid", "already submitted; reconcile the original request")
        }
        other => ToolError::unavailable("submission ledger", other),
    }
}

/// Map an order response onto item 19's synchronous result.
///
/// A venue `error` status is a `venue_error` rather than a refusal: the nonce
/// was spent and the request was seen, which is a different fact about the
/// world than a guardrail saying no.
fn order_outcome(response: ExchangeResponse, cloid: Option<String>) -> Result<Reply, ToolError> {
    // A different envelope or extra status cannot be attributed to this order.
    let [status]: [Status; 1] =
        response
            .statuses
            .try_into()
            .map_err(|_| ToolError::TimeoutUnknownOutcome {
                cloid: cloid.clone(),
                detail: "order response did not contain exactly one status; reconcile by cloid"
                    .into(),
            })?;
    if response.kind != ExchangeResponseKind::Order {
        return Err(ToolError::TimeoutUnknownOutcome {
            cloid,
            detail: "exchange response was not an order response; reconcile by cloid".into(),
        });
    }
    match status {
        Status::Resting { oid } => Ok(outcome::resting(oid, cloid)),
        Status::Filled {
            oid,
            total_sz,
            avg_px,
        } => Ok(outcome::filled(oid, cloid, total_sz, avg_px)),
        Status::Error(message) => Err(ToolError::venue(200, message)),
        // A resting trigger order and a `success` on an order action are both
        // "the venue took it and there is no oid yet". Reported as an unknown
        // outcome rather than as a fill, because the agent must reconcile.
        Status::Success | Status::WaitingForTrigger | Status::WaitingForFill => {
            Err(ToolError::TimeoutUnknownOutcome {
                cloid,
                detail: "the venue accepted the order without an id; reconcile by cloid".to_owned(),
            })
        }
    }
}

/// Map a cancel response onto `status: canceled`, pairing each status with the
/// order it answers for.
///
/// The venue returns statuses positionally, aligned with the cancels sent.
fn cancel_outcome(
    response: ExchangeResponse,
    named: Vec<(Option<u64>, Option<String>)>,
) -> Result<Reply, ToolError> {
    let requested = named.len();
    if response.kind != ExchangeResponseKind::Cancel
        || response.statuses.len() != requested
        || response
            .statuses
            .iter()
            .any(|status| !matches!(status, Status::Success | Status::Error(_)))
    {
        return Err(ToolError::TimeoutUnknownOutcome {
            cloid: if requested == 1 {
                named[0].1.clone()
            } else {
                None
            },
            detail:
                "cancellation response did not acknowledge every target with a cancellation status"
                    .into(),
        });
    }
    let failed = response
        .statuses
        .into_iter()
        .zip(named)
        .filter_map(|(status, (oid, cloid))| match status {
            Status::Error(venue_message) => Some(CancelFailure {
                oid,
                cloid,
                venue_message,
            }),
            _ => None,
        })
        .collect();
    Ok(outcome::canceled(requested, failed))
}

/// Retain the requested semantics alongside the unchanged execution conversion.
/// The operator bound is read once, only for orders that require crossing prices.
fn normalize_place_order(
    order: &PlaceKind,
    is_buy: bool,
    asset: &oppen_hl::meta::Asset,
    market: &MarketRef,
    slippage: impl FnOnce() -> Result<Decimal, ToolError>,
) -> Result<(Decimal, OrderKind, OriginalRequest), ToolError> {
    let (px, kind, requested) = match order {
        PlaceKind::Limit { limit_px, tif } => {
            let limit_px = limit_px
                .parse::<Decimal>()
                .map_err(|error| ToolError::invalid("limit_px", error))?;
            let tif = (*tif).into();
            (
                limit_px,
                OrderKind::Limit { tif },
                RequestedOrderKind::Limit { limit_px, tif },
            )
        }
        PlaceKind::Market => {
            let mid = market.reference_px.ok_or_else(|| ToolError::Unavailable {
                what: "reference price",
                detail: format!(
                    "no mid for {}; refusing to price a market order",
                    market.symbol
                ),
            })?;
            let slippage_bps = slippage()?;
            (
                asset.slippage_price_bounded(mid, is_buy, slippage_bps / BPS),
                OrderKind::Limit { tif: Tif::Ioc },
                RequestedOrderKind::Market { slippage_bps },
            )
        }
        PlaceKind::StopMarket { trigger_px, tpsl } => {
            let trigger_px = trigger_px
                .parse::<Decimal>()
                .map_err(|error| ToolError::invalid("trigger_px", error))?;
            let tpsl = (*tpsl).into();
            let slippage_bps = slippage()?;
            // Stops cross from the trigger, not from the contemporaneous quote.
            (
                asset.slippage_price_bounded(trigger_px, is_buy, slippage_bps / BPS),
                OrderKind::Trigger {
                    is_market: true,
                    trigger_px,
                    tpsl,
                },
                RequestedOrderKind::StopMarket {
                    trigger_px,
                    tpsl,
                    slippage_bps,
                },
            )
        }
    };
    Ok((
        px,
        kind,
        OriginalRequest {
            kind: requested,
            reference_px: market.reference_px,
            reference_at_ms: market.as_of_ms,
        },
    ))
}

/// The order a close is, decided from the position and the operator's bound.
///
/// Free-standing and pure, because the two things that make a close dangerous
/// are decided here and are testable without a venue: **closing a long by
/// buying doubles it**, and a size that is not the position's own magnitude can
/// flip it. `reduce_only` is the venue-side backstop for both, but the venue
/// refusing a reversed order is a worse way to learn this is wrong than a test.
///
/// `position_size` is signed as the venue reports it: negative is short.
#[allow(clippy::too_many_arguments)]
fn close_intent(
    symbol: &str,
    reason: &str,
    position_size: Decimal,
    reference_px: Decimal,
    reference_at_ms: u64,
    max_slippage_bps: Decimal,
    asset: &oppen_hl::meta::Asset,
    cloid: Cloid,
) -> OrderIntent {
    // Closing a long is a sell and closing a short is a buy.
    let is_buy = position_size.is_sign_negative();
    let slippage = max_slippage_bps / BPS;
    OrderIntent {
        symbol: symbol.to_owned(),
        is_buy,
        // Rounded toward the mid, so pricing *at* the operator's limit cannot
        // be refused *for* that limit by a rounding step nobody chose.
        px: asset.slippage_price_bounded(reference_px, is_buy, slippage),
        // The position's own magnitude. This tool cannot be asked for a
        // different one, which is what stops it opening or flipping.
        sz: position_size.abs(),
        // A market order on Hyperliquid is an IOC priced through the book;
        // there is no market order type to send.
        kind: OrderKind::Limit { tif: Tif::Ioc },
        reduce_only: true,
        cloid: Some(cloid),
        grouping: Grouping::Na,
        builder: None,
        // The agent may not tighten below the operator's bound here: the price
        // is already computed from that bound, and a second, tighter limit
        // would refuse the order this function just priced.
        max_slippage_bps: None,
        reason: reason.to_owned(),
        original: Some(OriginalRequest {
            kind: RequestedOrderKind::ClosePosition {
                position_size,
                slippage_bps: max_slippage_bps,
            },
            reference_px: Some(reference_px),
            reference_at_ms,
        }),
    }
}

/// Mint a client order id from OS entropy.
///
/// Fails closed rather than falling back to a counter or a timestamp: a cloid
/// that another order might share makes item 19's reconcile-by-cloid answer
/// for the wrong order, which is worse than refusing to place one.
fn mint_cloid() -> Result<Cloid, ToolError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| ToolError::Unavailable {
        what: "OS entropy",
        detail: format!("refusing to place an order with no client order id: {e}"),
    })?;
    Ok(Cloid::from_bytes(bytes))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn describe(asset: &oppen_hl::meta::Asset) -> SymbolMeta {
    SymbolMeta {
        symbol: asset.name().to_owned(),
        asset_id: asset.index,
        size_decimals: asset.sz_decimals(),
        price_decimals: asset.max_price_decimals(),
        max_leverage: asset.info.max_leverage,
        min_notional_usd: MIN_NOTIONAL_USD.to_string(),
        funding_interval_hours: 1,
        only_isolated: asset.info.only_isolated,
        is_delisted: asset.info.is_delisted,
    }
}

#[tool_handler]
impl ServerHandler for Gateway {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]`, so it is built from the default
        // and adjusted rather than written as a struct expression.
        let mut info = rmcp::model::ServerInfo::default();
        // Without this the handshake advertises no capabilities and an agent
        // never learns the tools exist. `#[tool_handler]` generates the routes;
        // it does not announce them.
        info.capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .build();
        info.server_info = rmcp::model::Implementation::new("oppen", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "oppen — a local-first Hyperliquid terminal. You are one agent among several; \
             the human supervises. Every action you take is guardrail-checked before it is \
             signed and is recorded in an append-only ledger. Read AGENTS.md in the oppen \
             repository before trading."
                .into(),
        );
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    fn response(statuses: Vec<Status>) -> ExchangeResponse {
        ExchangeResponse {
            kind: ExchangeResponseKind::Order,
            statuses,
        }
    }

    fn cancel_response(statuses: Vec<Status>) -> ExchangeResponse {
        ExchangeResponse {
            kind: ExchangeResponseKind::Cancel,
            statuses,
        }
    }

    fn body(result: CallToolResult) -> String {
        match result.content.into_iter().next() {
            Some(ContentBlock::Text(text)) => text.text,
            other => panic!("expected one text block, got {other:?}"),
        }
    }

    #[test]
    fn a_resting_order_reports_its_oid_and_the_cloid_it_can_be_reconciled_with() {
        let reply = order_outcome(
            response(vec![Status::Resting { oid: 42 }]),
            Some("0xaa".into()),
        )
        .expect("resting is not an error");
        assert_eq!(
            body(reply.into_result()),
            r#"{"contract_version":0,"status":"resting","oid":42,"cloid":"0xaa"}"#
        );
    }

    #[test]
    fn a_filled_order_reports_the_venue_size_and_price() {
        let reply = order_outcome(
            response(vec![Status::Filled {
                total_sz: d("0.25"),
                avg_px: d("63999.5"),
                oid: 43,
            }]),
            None,
        )
        .expect("filled is not an error");
        assert_eq!(
            body(reply.into_result()),
            r#"{"contract_version":0,"status":"filled","oid":43,"cloid":null,"filled_sz":"0.25","avg_px":"63999.5"}"#
        );
    }

    /// The venue saw the order and said no. That is a different fact from a
    /// guardrail refusal — the nonce was spent — so it is a `venue_error` and
    /// the venue's words are carried verbatim.
    #[test]
    fn a_venue_error_status_is_a_venue_error_carrying_the_message_verbatim() {
        let err = order_outcome(
            response(vec![Status::Error(
                "Insufficient margin to place order.".into(),
            )]),
            None,
        )
        .expect_err("a venue error is not a success");
        assert_eq!(
            err,
            ToolError::VenueError {
                http_status: 200,
                venue_message: "Insufficient margin to place order.".to_owned(),
            }
        );
    }

    /// An `ok` envelope with no statuses in it is the venue not answering the
    /// question. Reporting it as success would invent an outcome.
    #[test]
    fn an_empty_status_list_is_never_reported_as_a_fill() {
        let err = order_outcome(response(vec![]), None).expect_err("no status is not a success");
        assert!(
            matches!(err, ToolError::TimeoutUnknownOutcome { .. }),
            "{err:?}"
        );
    }

    /// A trigger order rests off-book with no oid. The agent must reconcile
    /// rather than assume, so it gets the cloid and the unknown-outcome code.
    #[test]
    fn an_accepted_order_with_no_id_is_an_unknown_outcome_carrying_the_cloid() {
        let err = order_outcome(
            response(vec![Status::WaitingForTrigger]),
            Some("0xbb".into()),
        )
        .expect_err("no oid is not a success");
        assert!(
            matches!(&err, ToolError::TimeoutUnknownOutcome { cloid, .. } if cloid.as_deref() == Some("0xbb")),
            "{err:?}"
        );
    }

    /// The venue answers cancels positionally. Pairing them by position is
    /// what lets a failure name the order it belongs to; getting it wrong
    /// would attribute one order's refusal to another.
    #[test]
    fn cancel_failures_are_paired_with_the_order_each_one_answers_for() {
        let reply = cancel_outcome(
            cancel_response(vec![
                Status::Success,
                Status::Error("Order was never placed, already canceled, or filled.".into()),
                Status::Success,
            ]),
            vec![
                (Some(1), None),
                (Some(2), Some("0xcc".to_owned())),
                (Some(3), None),
            ],
        );
        let body = body(reply.unwrap().into_result());
        assert!(body.contains(r#""requested":3,"canceled":2"#), "{body}");
        assert!(body.contains(r#""oid":2,"cloid":"0xcc""#), "{body}");
        assert!(!body.contains(r#""oid":1"#), "{body}");
        assert!(!body.contains(r#""oid":3"#), "{body}");
    }

    #[test]
    fn a_cancel_the_venue_took_in_full_reports_no_failures() {
        let reply = cancel_outcome(
            cancel_response(vec![Status::Success, Status::Success]),
            vec![(Some(1), None), (Some(2), None)],
        );
        assert_eq!(
            body(reply.unwrap().into_result()),
            r#"{"contract_version":0,"status":"canceled","requested":2,"canceled":2,"failed":[]}"#
        );
    }

    #[test]
    fn incomplete_or_order_shaped_cancel_responses_are_unknown_not_success() {
        for statuses in [
            vec![],
            vec![Status::Success, Status::Success],
            vec![Status::Resting { oid: 7 }],
        ] {
            let error = cancel_outcome(
                cancel_response(statuses),
                vec![(Some(7), Some("0xcc".into()))],
            )
            .unwrap_err();
            assert!(
                matches!(&error, ToolError::TimeoutUnknownOutcome { cloid, .. }
                if cloid.as_deref() == Some("0xcc"))
            );
            let error: ErrorData = error.into();
            assert_eq!(error.data.unwrap()["retryable"], false);
        }
    }

    #[test]
    fn wrong_envelopes_and_extra_order_statuses_are_nonretryable_unknown() {
        for (kind, statuses) in [
            ("cancel", serde_json::json!([{"resting":{"oid":7}}])),
            (
                "cancel",
                serde_json::json!([{"error":"not an order rejection"}]),
            ),
            ("default", serde_json::json!([])),
            ("order", serde_json::json!([])),
            (
                "order",
                serde_json::json!([{"resting":{"oid":7}},{"error":"extra"}]),
            ),
            (
                "order",
                serde_json::json!([{"filled":{"oid":7,"totalSz":"1","avgPx":"100"}},"success"]),
            ),
            (
                "order",
                serde_json::json!([{"error":"first"},{"resting":{"oid":8}}]),
            ),
        ] {
            let parsed = ExchangeResponse::parse(
                &serde_json::json!({
                    "status":"ok", "response":{"type":kind,"data":{"statuses":statuses}}
                })
                .to_string(),
            )
            .unwrap();
            let error = order_outcome(parsed, Some("0xcc".into())).unwrap_err();
            assert!(
                matches!(&error, ToolError::TimeoutUnknownOutcome { cloid, .. }
                if cloid.as_deref() == Some("0xcc")),
                "{kind}: {error:?}"
            );
            let error: ErrorData = error.into();
            assert_eq!(error.data.unwrap()["retryable"], false);
        }
        for kind in ["order", "default"] {
            let parsed = ExchangeResponse::parse(
                &serde_json::json!({
                    "status":"ok", "response":{"type":kind,"data":{"statuses":["success"]}}
                })
                .to_string(),
            )
            .unwrap();
            let error = cancel_outcome(parsed, vec![(Some(7), Some("0xcc".into()))]).unwrap_err();
            assert!(
                matches!(&error, ToolError::TimeoutUnknownOutcome { cloid, .. }
                if cloid.as_deref() == Some("0xcc"))
            );
            let error: ErrorData = error.into();
            assert_eq!(error.data.unwrap()["retryable"], false);
        }
    }

    fn test_asset(sz_decimals: u32) -> oppen_hl::meta::Asset {
        oppen_hl::meta::Asset {
            index: 0,
            info: oppen_hl::types::AssetInfo {
                name: "TEST".into(),
                sz_decimals,
                max_leverage: 50,
                margin_table_id: 0,
                is_delisted: false,
                only_isolated: false,
            },
        }
    }

    fn a_cloid() -> Cloid {
        Cloid::from_bytes([7u8; 16])
    }

    fn normalization_market(as_of_ms: u64) -> MarketRef {
        MarketRef {
            symbol: "TEST".into(),
            reference_px: Some(d("100")),
            as_of_ms,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
            sigma_day: None,
            vol_ratio: None,
        }
    }

    #[test]
    fn market_and_explicit_ioc_keep_distinct_originals_with_identical_normalization() {
        let asset = test_asset(4);
        let market = normalization_market(1234);
        for is_buy in [false, true] {
            let reads = std::cell::Cell::new(0);
            let (px, kind, original) =
                normalize_place_order(&PlaceKind::Market, is_buy, &asset, &market, || {
                    reads.set(reads.get() + 1);
                    Ok(d("50"))
                })
                .unwrap();
            assert_eq!(
                reads.get(),
                1,
                "normalization and evidence share one bound read"
            );
            let (limit_px, limit_kind, limit_original) = normalize_place_order(
                &PlaceKind::Limit {
                    limit_px: px.to_string(),
                    tif: PlaceTif::Ioc,
                },
                is_buy,
                &asset,
                &market,
                || panic!("explicit limits must not read the operator crossing bound"),
            )
            .unwrap();
            assert_eq!((px, kind), (limit_px, limit_kind));
            assert_eq!(
                original,
                OriginalRequest {
                    kind: RequestedOrderKind::Market {
                        slippage_bps: d("50")
                    },
                    reference_px: Some(d("100")),
                    reference_at_ms: 1234,
                }
            );
            assert_eq!(
                limit_original,
                OriginalRequest {
                    kind: RequestedOrderKind::Limit {
                        limit_px: px,
                        tif: Tif::Ioc
                    },
                    reference_px: Some(d("100")),
                    reference_at_ms: 1234,
                }
            );
            assert_ne!(original, limit_original);
        }
    }

    #[test]
    fn stop_market_preserves_trigger_quote_time_and_the_single_normalization_bound() {
        let asset = test_asset(4);
        let market = normalization_market(4321);
        for (requested_tpsl, tpsl) in [(PlaceTpsl::Sl, Tpsl::Sl), (PlaceTpsl::Tp, Tpsl::Tp)] {
            for is_buy in [false, true] {
                let reads = std::cell::Cell::new(0);
                let (px, kind, original) = normalize_place_order(
                    &PlaceKind::StopMarket {
                        trigger_px: "90.1234".into(),
                        tpsl: requested_tpsl,
                    },
                    is_buy,
                    &asset,
                    &market,
                    || {
                        reads.set(reads.get() + 1);
                        Ok(d("0.6"))
                    },
                )
                .unwrap();
                assert_eq!(reads.get(), 1);
                assert_eq!(
                    px,
                    asset.slippage_price_bounded(d("90.1234"), is_buy, d("0.6") / BPS)
                );
                assert_eq!(
                    kind,
                    OrderKind::Trigger {
                        is_market: true,
                        trigger_px: d("90.1234"),
                        tpsl
                    }
                );
                assert_eq!(
                    original,
                    OriginalRequest {
                        kind: RequestedOrderKind::StopMarket {
                            trigger_px: d("90.1234"),
                            tpsl,
                            slippage_bps: d("0.6")
                        },
                        reference_px: Some(d("100")),
                        reference_at_ms: 4321,
                    }
                );
            }
        }
    }

    #[test]
    fn missing_market_reference_still_refuses_before_reading_slippage() {
        let mut market = normalization_market(1234);
        market.reference_px = None;
        let result =
            normalize_place_order(&PlaceKind::Market, true, &test_asset(4), &market, || {
                panic!("a missing quote must not price an order")
            });
        assert!(matches!(
            result,
            Err(ToolError::Unavailable {
                what: "reference price",
                ..
            })
        ));
    }

    #[test]
    fn close_original_preserves_signed_position_size_and_reference_evidence() {
        let asset = test_asset(4);
        for position_size in [d("1.2345"), d("-1.2345")] {
            let intent = close_intent(
                "TEST",
                "reduce risk",
                position_size,
                d("100"),
                9876,
                d("50"),
                &asset,
                a_cloid(),
            );
            assert_eq!(
                intent.original,
                Some(OriginalRequest {
                    kind: RequestedOrderKind::ClosePosition {
                        position_size,
                        slippage_bps: d("50")
                    },
                    reference_px: Some(d("100")),
                    reference_at_ms: 9876,
                })
            );
            assert_eq!(intent.sz, position_size.abs());
            assert_eq!(intent.is_buy, position_size.is_sign_negative());
            assert_eq!(
                intent.px,
                asset.slippage_price_bounded(d("100"), intent.is_buy, d("50") / BPS)
            );
            assert_eq!(intent.kind, OrderKind::Limit { tif: Tif::Ioc });
            assert!(intent.reduce_only);
            assert_eq!(intent.cloid, Some(a_cloid()));
            assert_eq!(intent.max_slippage_bps, None);
        }
    }

    /// The one that doubles a position if it is backwards.
    #[test]
    fn closing_a_long_sells_and_closing_a_short_buys() {
        let asset = test_asset(4);
        let long = close_intent(
            "BTC",
            "flat",
            d("1.5"),
            d("100"),
            0,
            d("50"),
            &asset,
            a_cloid(),
        );
        assert!(!long.is_buy, "closing a long must sell");
        assert_eq!(long.sz, d("1.5"));

        let short = close_intent(
            "BTC",
            "flat",
            d("-1.5"),
            d("100"),
            0,
            d("50"),
            &asset,
            a_cloid(),
        );
        assert!(short.is_buy, "closing a short must buy");
        assert_eq!(short.sz, d("1.5"), "size is the magnitude, never signed");
    }

    /// A close carries the venue's reduce-only flag and is an IOC. Without
    /// reduce-only a close that races a fill becomes an opening order on the
    /// other side; as a GTC it would rest instead of closing.
    #[test]
    fn a_close_is_a_reduce_only_ioc() {
        let asset = test_asset(4);
        let intent = close_intent(
            "BTC",
            "flat",
            d("2"),
            d("100"),
            0,
            d("50"),
            &asset,
            a_cloid(),
        );
        assert!(intent.reduce_only);
        assert_eq!(intent.kind, OrderKind::Limit { tif: Tif::Ioc });
        assert!(intent.cloid.is_some(), "a close must be reconcilable too");
    }

    /// The whole point of `slippage_price_bounded`. At the operator's bound
    /// the price must not come back *more* adverse than the bound, or the
    /// engine refuses the order this function just priced.
    #[test]
    fn a_close_priced_at_the_operator_bound_is_never_more_adverse_than_it() {
        let asset = test_asset(4);
        // 0.6 bp on a 100 mid is the case where rounding to nearest lands on
        // 100.01 — 1 bp — and would be refused for exceeding 0.6 bp.
        let buy = close_intent(
            "T",
            "flat",
            d("-1"),
            d("100"),
            0,
            d("0.6"),
            &asset,
            a_cloid(),
        );
        assert!(buy.is_buy);
        assert!(
            buy.px <= d("100.006"),
            "priced at {} against a 0.6 bp bound on a 100 mid",
            buy.px
        );

        let sell = close_intent(
            "T",
            "flat",
            d("1"),
            d("100"),
            0,
            d("0.6"),
            &asset,
            a_cloid(),
        );
        assert!(!sell.is_buy);
        assert!(
            sell.px >= d("99.994"),
            "priced at {} against a 0.6 bp bound on a 100 mid",
            sell.px
        );
    }

    /// D3 and item 24 make slippage operator-set. The intent carries no
    /// tightening of its own: the price already encodes the operator's bound,
    /// and a second limit on top would refuse the order at its own price.
    #[test]
    fn a_close_does_not_set_its_own_slippage_limit() {
        let asset = test_asset(4);
        let intent = close_intent(
            "BTC",
            "flat",
            d("1"),
            d("100"),
            0,
            d("50"),
            &asset,
            a_cloid(),
        );
        assert_eq!(intent.max_slippage_bps, None);
    }

    /// The reason is the agent's and is carried through untouched — item 19
    /// requires one and item 30 makes it a claim, not a fact.
    #[test]
    fn the_reason_is_carried_verbatim() {
        let asset = test_asset(4);
        let intent = close_intent(
            "BTC",
            "risk off",
            d("1"),
            d("100"),
            0,
            d("50"),
            &asset,
            a_cloid(),
        );
        assert_eq!(intent.reason, "risk off");
    }

    /// A gateway over a throwaway ledger, seeded with `events`.
    ///
    /// `get_events` reads only the ledger, so this touches no venue and signs
    /// nothing — which is what makes the real envelope testable here rather
    /// than only over a live socket.
    fn gateway_over(events: &[(Option<&str>, &str)]) -> Gateway {
        gateway_fixture(events, false)
    }

    fn activated_gateway() -> Gateway {
        gateway_fixture(&[], true)
    }

    fn gateway_fixture(events: &[(Option<&str>, &str)], initialize_policy: bool) -> Gateway {
        use oppen_core::guardrail::GuardrailEngine;
        use oppen_core::ledger::{EventKind, Ledger, NewEvent, PolicyJournal};

        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = std::sync::Arc::new(
            Ledger::open_at(&dir.path().join("testnet.db"), Network::Testnet).expect("ledger"),
        );
        for (n, (agent, reason)) in events.iter().enumerate() {
            ledger
                .append(&NewEvent {
                    // `append` refuses OrderIntent and Fill; those have their
                    // own recording paths.
                    kind: EventKind::AgentDecision,
                    ts_ms: 1_756_000_000_000 + n as i64,
                    agent_id: *agent,
                    payload: &serde_json::json!({ "reason": reason }),
                    snapshot: None,
                })
                .expect("append");
        }
        let policy = Arc::new(PolicyJournal::new(Arc::new(
            oppen_core::ledger::RegistryJournal::open(
                ledger.clone(),
                Arc::new(oppen_core::keys::HmacKey::from_bytes([42; 32])),
            )
            .unwrap(),
        )));
        let at = now_ms();
        if initialize_policy {
            let review = oppen_core::guardrail::LegacyPolicyReview::open(
                dir.path().join("guardrails.db"),
                Network::Testnet,
                at,
            )
            .unwrap();
            policy
                .initialize(
                    &review,
                    oppen_core::guardrail::PersistedState::paused(at),
                    at,
                )
                .unwrap();
        }
        let engine = std::sync::Arc::new(
            GuardrailEngine::new(
                policy,
                std::sync::Arc::new(NoKeys),
                Arc::new(FeedSession::new()),
            )
            .expect("engine"),
        );
        if initialize_policy {
            engine
                .operator_release_kill(&oppen_core::guardrail::KillScope::Global, at)
                .unwrap();
            acknowledge_test_policy(&engine);
        }
        let journal = std::sync::Arc::new(
            oppen_core::journal::Journal::open(dir.path().join("journal.db")).expect("journal"),
        );
        let registry = oppen_core::ledger::RegistryJournal::open(
            ledger.clone(),
            Arc::new(oppen_core::keys::HmacKey::from_bytes([42; 32])),
        )
        .unwrap();
        let mut gateway = Gateway::new(
            Network::Testnet,
            engine,
            EventViews::new(ledger),
            journal,
            std::sync::Arc::new(oppen_core::alert::AlertStore::open(":memory:").expect("alerts")),
            std::sync::Arc::new(oppen_core::features::quotes::QuoteCache::new()),
        )
        .expect("gateway");
        Arc::get_mut(&mut gateway.inner).unwrap()._test_dir = Some(dir);
        Arc::get_mut(&mut gateway.inner).unwrap().test_registry = Some(registry);
        gateway
    }

    fn pairing_store(path: &std::path::Path) -> crate::auth::TokenStore {
        let ledger = Arc::new(oppen_core::ledger::Ledger::open_at(path, Network::Testnet).unwrap());
        crate::auth::TokenStore::open(
            oppen_core::ledger::PairingJournal::open(
                ledger,
                Arc::new(oppen_core::keys::HmacKey::from_bytes([42; 32])),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn test_tracker(gateway: &Gateway) -> ExecutionTracker {
        let path = gateway
            .inner
            ._test_dir
            .as_ref()
            .unwrap()
            .path()
            .join("testnet.db");
        ExecutionTracker::supervision(
            &tokio::sync::watch::channel(()).0,
            Arc::new(std::sync::RwLock::new(pairing_store(&path))),
        )
    }

    #[tokio::test]
    async fn request_binding_requires_authority_and_ignores_an_untrusted_binding() {
        let gateway = activated_gateway();
        let (transport, _client) = tokio::io::duplex(4096);
        let service = rmcp::service::serve_directly(gateway.clone(), transport, None);
        let mut context = RequestContext::new(
            rmcp::model::NumberOrString::Number(1),
            service.peer().clone(),
        );
        let (mut parts, ()) = http::Request::new(()).into_parts();
        parts.extensions.insert(binding_for("not-authorized"));
        context.extensions.insert(parts.clone());
        assert!(matches!(
            Gateway::bound(&context),
            Err(ToolError::Unavailable { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut store = pairing_store(&dir.path().join("pairings.db"));
        let expected = binding_for("alpha");
        let token = store.issue(expected.clone()).unwrap();
        parts
            .extensions
            .insert(store.authenticate(token.reveal()).unwrap().authority());
        context.extensions.insert(parts);
        assert_eq!(Gateway::bound(&context).unwrap(), expected);
        let error = gateway
            .preflight(
                Parameters(PreflightParams {
                    symbol: "TEST".into(),
                    is_buy: true,
                    size: "0.12".into(),
                    limit_px: "100".into(),
                    reduce_only: false,
                }),
                context,
            )
            .await
            .unwrap_err();
        let data = error.data.unwrap();
        assert_eq!(data["code"], "unavailable");
        assert!(data["detail"].as_str().unwrap().contains("tracked method"));
        service.cancel().await.unwrap();
    }

    struct NoKeys;

    impl oppen_core::keys::KeyStore for NoKeys {
        fn network(&self) -> Network {
            Network::Testnet
        }
        fn read(
            &self,
            _: &oppen_core::keys::EntryName,
        ) -> Result<Option<oppen_core::keys::SecretText>, oppen_core::keys::KeyStoreError> {
            Ok(None)
        }
        fn write(
            &self,
            _: &oppen_core::keys::EntryName,
            _: &str,
        ) -> Result<(), oppen_core::keys::KeyStoreError> {
            panic!("submission tests must never write credentials")
        }
        fn remove(
            &self,
            _: &oppen_core::keys::EntryName,
        ) -> Result<(), oppen_core::keys::KeyStoreError> {
            panic!("submission tests must never delete credentials")
        }
    }

    /// The binding the door would have injected for `agent`.
    fn binding_for(agent: &str) -> Binding {
        Binding {
            agent: oppen_core::guardrail::AgentId::new(agent),
            account: "0xbf829199c1ae7f0caf21fb6fc45e10edff25b7d2"
                .parse()
                .expect("address"),
        }
    }

    fn grant_test_route(gateway: &Gateway, bound: &Binding) {
        let registry = gateway.inner.test_registry.as_ref().unwrap();
        if let Ok(route) = registry.route_for_agent(&bound.agent) {
            assert_eq!(route.binding.container, bound.account);
            return;
        }
        let mut wallet_address = *bound.account.as_bytes();
        wallet_address[0] ^= 0x80;
        let now = now_ms();
        registry
            .grant(
                oppen_core::ledger::RegistryBinding {
                    agent: bound.agent.clone(),
                    container: bound.account,
                    vault_address: None,
                    wallet: oppen_core::keys::AgentWallet {
                        generation: 0,
                        address: Address::from_bytes(wallet_address),
                        approved_at_ms: now,
                        valid_until_ms: now + 86_400_000,
                    },
                },
                now,
            )
            .unwrap();
    }

    fn cleared_test_order(gateway: &Gateway, bound: &Binding, cloid: Cloid) -> Cleared {
        use oppen_core::guardrail::{AccountSnapshot, Exposure, RestingExposure};
        grant_test_route(gateway, bound);
        reconcile_test_feed(gateway, bound.account);
        let at = now_ms();
        let mut config = gateway
            .inner
            .engine
            .register_agent(&bound.agent, at)
            .expect("register");
        config.symbols.insert("TEST".into());
        config.approval_required = false;
        gateway
            .inner
            .engine
            .operator_set_guardrails(&bound.agent, config, at)
            .expect("policy");
        acknowledge_test_policy(&gateway.inner.engine);
        let intent = OrderIntent {
            symbol: "TEST".into(),
            is_buy: true,
            px: d("100"),
            sz: d("0.12"),
            kind: OrderKind::Limit { tif: Tif::Gtc },
            reduce_only: false,
            cloid: Some(cloid),
            grouping: Grouping::Na,
            builder: None,
            max_slippage_bps: None,
            reason: "durable submission fixture".into(),
            original: None,
        };
        let market = MarketRef {
            symbol: "TEST".into(),
            reference_px: Some(d("100")),
            as_of_ms: at,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
            sigma_day: None,
            vol_ratio: None,
        };
        let exposure = Exposure {
            account: bound.account,
            feed_stamp: Some(gateway.inner.feed.stamp()),
            agent: AccountSnapshot {
                as_of_ms: at,
                reconciled: true,
                equity_usd: d("1000"),
                peak_equity_usd: d("1000"),
                realized_pnl_today_usd: Decimal::ZERO,
                unrealized_pnl_usd: Decimal::ZERO,
                day_start_ms: utc_day_start_ms(at),
                total_position_notional_usd: Decimal::ZERO,
                positions: Default::default(),
                resting: Some(RestingExposure::default()),
            },
            fleet: None,
        };
        gateway
            .inner
            .engine
            .evaluate(
                &bound.agent,
                &intent,
                &test_asset(2),
                &market,
                &exposure,
                at,
            )
            .expect("evaluated and durably audited")
    }

    fn reconcile_test_feed(gateway: &Gateway, account: Address) {
        use oppen_core::feed::pump::{FeedPump, FeedSubscriber};
        use oppen_core::reconcile::ReconcileSource;
        use oppen_hl::types::{Fill, OpenOrder};
        use oppen_hl::ws::{PoolError, Subscription};

        struct EmptyVenue;
        impl ReconcileSource for EmptyVenue {
            fn network(&self) -> Network {
                Network::Testnet
            }
            async fn user_fills_by_time(
                &self,
                _: Address,
                _: u64,
                _: Option<u64>,
            ) -> Result<Vec<Fill>, oppen_hl::Error> {
                Ok(Vec::new())
            }
            async fn frontend_open_orders(
                &self,
                _: Address,
            ) -> Result<Vec<OpenOrder>, oppen_hl::Error> {
                Ok(Vec::new())
            }
            async fn order_status(
                &self,
                _: Address,
                _: OrderRef,
            ) -> Result<OrderStatusResponse, oppen_hl::Error> {
                panic!("empty fixture has no orders to reconcile")
            }
        }
        impl FeedSubscriber for EmptyVenue {
            fn subscribe(&self, _: Subscription) -> Result<(), PoolError> {
                panic!("ledger fixture must not subscribe")
            }
            fn unsubscribe(&self, _: &Subscription) -> Result<(), PoolError> {
                panic!("ledger fixture has no subscriptions")
            }
        }

        if gateway.inner.feed.state().reconciled {
            return;
        }
        // These helpers also run inside Tokio tests; join a separate runtime
        // rather than nesting block_on or exposing a production readiness setter.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let ledger = oppen_core::ledger::Ledger::open_at(
                        &gateway
                            .inner
                            ._test_dir
                            .as_ref()
                            .unwrap()
                            .path()
                            .join("testnet.db"),
                        Network::Testnet,
                    )
                    .unwrap();
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            let pump = FeedPump::new(
                                &gateway.inner.feed,
                                &ledger,
                                account,
                                EmptyVenue,
                                &gateway.inner.alerts,
                                &gateway.inner.quotes,
                                &EmptyVenue,
                            )
                            .unwrap();
                            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                            drop(tx);
                            pump.run(&mut rx).await;
                        });
                })
                .join()
                .unwrap();
        });
        assert!(gateway.inner.feed.state().reconciled);
    }

    fn pending_fixture() -> (Gateway, Binding, SubmissionReceipt) {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        let journal = &gateway.inner.submissions;
        let revision = journal.state(bound.account).expect("state").revision;
        let receipt = journal
            .begin(bound.account, cleared.clearance(), revision, now_ms())
            .expect("durable start");
        (gateway, bound, receipt)
    }

    #[tokio::test]
    async fn a_fresh_gateway_recovers_the_previous_gateways_pending_submission() {
        let (mut gateway, bound, receipt) = pending_fixture();
        let mut fresh = Gateway::new(
            Network::Testnet,
            gateway.inner.engine.clone(),
            gateway.inner.events.clone(),
            gateway.inner.journal.clone(),
            gateway.inner.alerts.clone(),
            gateway.inner.quotes.clone(),
        )
        .expect("fresh gateway");
        Arc::get_mut(&mut fresh.inner).unwrap()._test_dir =
            Arc::get_mut(&mut gateway.inner).unwrap()._test_dir.take();
        assert!(!Arc::ptr_eq(
            &gateway.execution_queue(bound.account),
            &fresh.execution_queue(bound.account)
        ));
        drop(gateway);
        let error = reserve_account(
            fresh.execution_queue(bound.account),
            &fresh.inner.submissions,
            bound.account,
            |_| async { Ok(OrderStatusResponse::UnknownOid) },
        )
        .await
        .expect_err("restart must not clear an unknown submission");
        assert!(
            matches!(error, ToolError::TimeoutUnknownOutcome { cloid: Some(cloid), .. } if cloid == receipt.cloid().as_str())
        );
        assert_eq!(
            fresh
                .inner
                .submissions
                .state(bound.account)
                .expect("state")
                .pending,
            Some(receipt)
        );
    }

    #[tokio::test]
    async fn an_order_cannot_reach_the_signer_without_an_account_permit() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        let error = gateway
            .submit(cleared, Some(&a_cloid()), &bound, None, None)
            .await
            .expect_err("no permit");
        assert!(matches!(
            error,
            ToolError::Unavailable {
                what: "submission ledger",
                ..
            }
        ));
        assert_eq!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .expect("state")
                .revision,
            0
        );
    }

    #[tokio::test]
    async fn clearance_pairing_and_reservation_accounts_must_agree_before_submission() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        let other = Binding {
            account: Address::from_bytes([8; 20]),
            ..bound.clone()
        };
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        let error = gateway
            .submit(cleared, Some(&a_cloid()), &other, None, None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ToolError::Unavailable {
                what: "submission route",
                ..
            }
        ));
        let permit = reserve_account(
            gateway.execution_queue(other.account),
            &gateway.inner.submissions,
            other.account,
            |_| async { panic!("no pending order") },
        )
        .await
        .unwrap();
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        let error = gateway
            .submit(
                cleared,
                Some(&a_cloid()),
                &bound,
                Some(permit),
                Some(test_tracker(&gateway)),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ToolError::Unavailable {
                what: "submission ledger",
                ..
            }
        ));
        assert!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .unwrap()
                .pending
                .is_none()
        );
        assert!(
            gateway
                .inner
                .submissions
                .state(other.account)
                .unwrap()
                .pending
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_signer_failure_durably_records_not_sent_and_prevents_cloid_reuse() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        let permit = reserve_account(
            gateway.execution_queue(bound.account),
            &gateway.inner.submissions,
            bound.account,
            |_| async { panic!("no earlier submission") },
        )
        .await
        .expect("permit");
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        let error = gateway
            .submit(
                cleared,
                Some(&a_cloid()),
                &bound,
                Some(permit),
                Some(test_tracker(&gateway)),
            )
            .await
            .expect_err("test store has no key");
        assert!(matches!(
            error,
            ToolError::Unavailable { what: "signer", .. }
        ));
        let state = gateway
            .inner
            .submissions
            .state(bound.account)
            .expect("state");
        assert!(state.pending.is_none());
        assert!(state.revision > 0);
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        assert!(matches!(
            gateway.inner.submissions.begin(
                bound.account,
                cleared.clearance(),
                state.revision,
                now_ms()
            ),
            Err(SubmissionError::DuplicateCloid)
        ));
        let events = gateway
            .inner
            .events
            .for_agent(bound.agent.as_str())
            .get_events(0, 100)
            .expect("events");
        assert!(events.events.iter().any(|event| {
            event.kind == oppen_core::ledger::EventKind::SubmissionResolved
                && event
                    .payload
                    .as_ref()
                    .is_some_and(|payload| payload["outcome"]["resolution"] == "not_sent")
        }));
    }

    #[tokio::test]
    async fn a_stale_account_revision_refuses_before_signing() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        let permit = reserve_account(
            gateway.execution_queue(bound.account),
            &gateway.inner.submissions,
            bound.account,
            |_| async { panic!("no earlier submission") },
        )
        .await
        .expect("permit");
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        let receipt = gateway
            .inner
            .submissions
            .begin(
                bound.account,
                cleared.clearance(),
                permit.revision,
                now_ms(),
            )
            .expect("another process submitted");
        gateway
            .inner
            .submissions
            .resolve(
                &receipt,
                SubmissionResolution::Rejected {
                    message: "fixture".into(),
                },
                now_ms(),
            )
            .expect("other process resolved");
        let error = gateway
            .submit(
                cleared,
                Some(&a_cloid()),
                &bound,
                Some(permit),
                Some(test_tracker(&gateway)),
            )
            .await
            .expect_err("stale snapshot");
        assert!(
            matches!(error, ToolError::Unavailable { what: "submission ledger", detail } if detail.contains("revision"))
        );
    }

    #[tokio::test]
    async fn waiting_for_an_account_does_not_take_its_owners_submission_slot() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        grant_test_route(&gateway, &bound);
        let permit = gateway.reserve_submission(&bound).await.unwrap();
        let tracker = test_tracker(&gateway);
        let waiting = gateway.reserve_submission_account(&bound, Some(tracker.clone()));
        let mut waiting = std::pin::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(gateway.inner.submission_worker.available_permits(), 1);
        let cleared = cleared_test_order(&gateway, &bound, a_cloid());
        let error = gateway
            .submit(
                cleared,
                Some(&a_cloid()),
                &bound,
                Some(permit),
                Some(tracker),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::Unavailable { what: "signer", .. }),
            "{error:?}"
        );
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.account, bound.account);
        assert!(next.revision > 0);
    }

    #[tokio::test]
    async fn different_agents_on_one_address_share_the_execution_queue() {
        let gateway = activated_gateway();
        let alpha = binding_for("alpha");
        let beta = binding_for("beta");
        let other: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .expect("address");
        let alpha_queue = gateway.execution_queue(alpha.account);
        let beta_queue = gateway.clone().execution_queue(beta.account);
        let held = alpha_queue.lock().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), beta_queue.lock())
                .await
                .is_err()
        );
        let independent = gateway.execution_queue(other);
        assert!(
            independent.try_lock().is_ok(),
            "one account blocked another"
        );
        drop(held);
        assert!(
            beta_queue.try_lock().is_ok(),
            "account lock was not released"
        );
    }

    #[tokio::test]
    async fn a_dropped_submission_with_unknown_status_blocks_another_agent_order() {
        let (gateway, bound, receipt) = pending_fixture();
        let queue = gateway.execution_queue(bound.account);
        let submitted_queue = queue.clone();
        let journal = gateway.inner.submissions.clone();
        let (sent, received) = tokio::sync::oneshot::channel();
        let request = tokio::spawn(async move {
            let _held = submitted_queue.lock_owned().await;
            track_submission(Some(&a_cloid()), Some((&journal, &receipt)), async {
                sent.send(()).expect("observer");
                std::future::pending().await
            })
            .await
            .expect("submission");
        });
        received.await.expect("submission started");
        let next_queue = gateway.execution_queue(binding_for("beta").account);
        let waiting_queue = next_queue.clone();
        let recovered_journal = gateway.inner.submissions.clone();
        let mut next_request = tokio::spawn(async move {
            reserve_account(
                waiting_queue,
                &recovered_journal,
                bound.account,
                |cloid| async move {
                    assert_eq!(cloid, a_cloid());
                    Ok(OrderStatusResponse::UnknownOid)
                },
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut next_request)
                .await
                .is_err()
        );
        request.abort();
        assert!(request.await.expect_err("request dropped").is_cancelled());

        let error = tokio::time::timeout(std::time::Duration::from_secs(1), next_request)
            .await
            .expect("second request stayed blocked")
            .expect("second request")
            .expect_err("unknown outcome must block another order");
        let data = ErrorData::from(error).data.expect("typed refusal");
        assert_eq!(data["code"], "timeout_unknown_outcome");
        assert_eq!(data["cloid"], a_cloid().as_str());
        assert_eq!(data["retryable"], false);
        assert_eq!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .expect("durable state")
                .pending
                .expect("still reserved")
                .cloid(),
            &a_cloid()
        );

        // Dropping a reconciliation lookup cannot release the reservation either.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reserve_account(
                    next_queue.clone(),
                    &gateway.inner.submissions,
                    bound.account,
                    |_| std::future::pending()
                )
            )
            .await
            .is_err()
        );
        assert!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .expect("state")
                .pending
                .is_some()
        );
        let known = serde_json::from_value(serde_json::json!({
            "status": "order", "order": {
                "status": "filled", "statusTimestamp": 1,
                "order": { "coin": "BTC", "side": "B", "limitPx": "100", "sz": "0", "oid": 1,
                    "timestamp": 1, "origSz": "1", "cloid": a_cloid().as_str() }
            }
        }))
        .expect("known order");
        let next = reserve_account(
            next_queue,
            &gateway.inner.submissions,
            bound.account,
            |_| async { Ok(known) },
        )
        .await
        .expect("confirmed submission releases next order");
        assert!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .expect("state")
                .pending
                .is_none()
        );
        assert!(next.revision > 0);
    }

    #[tokio::test]
    async fn a_mismatched_venue_cloid_cannot_resolve_a_pending_submission() {
        let (gateway, bound, receipt) = pending_fixture();
        let other = "0x11111111111111111111111111111111";
        assert_ne!(other, receipt.cloid().as_str());
        let known = serde_json::from_value(serde_json::json!({
            "status": "order", "order": {
                "status": "filled", "statusTimestamp": 1,
                "order": { "coin": "TEST", "side": "B", "limitPx": "100", "sz": "0", "oid": 1,
                    "timestamp": 1, "origSz": "0.12", "cloid": other }
            }
        }))
        .expect("order response");
        let error = reserve_account(
            gateway.execution_queue(bound.account),
            &gateway.inner.submissions,
            bound.account,
            |_| async { Ok(known) },
        )
        .await
        .expect_err("different cloid");
        assert!(matches!(error, ToolError::TimeoutUnknownOutcome { .. }));
        assert_eq!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .expect("state")
                .pending,
            Some(receipt)
        );
    }

    #[tokio::test]
    async fn ambiguous_error_reports_the_durable_cloid_not_a_callers_other_id() {
        let (gateway, _, receipt) = pending_fixture();
        let other = Cloid::from_bytes([17u8; 16]);
        let error = track_submission(
            Some(&other),
            Some((&gateway.inner.submissions, &receipt)),
            async { Err(oppen_hl::Error::InvalidExchangeResponse("fixture".into())) },
        )
        .await
        .expect_err("ambiguous");
        assert!(
            matches!(error, ToolError::TimeoutUnknownOutcome { cloid: Some(cloid), .. } if cloid == receipt.cloid().as_str())
        );
    }

    #[tokio::test]
    async fn only_a_structured_order_rejection_clears_the_submission_reservation() {
        for (statuses, retained) in [
            (vec![Status::Error("rejected".into())], false),
            (vec![Status::Resting { oid: 1 }], true),
            (vec![Status::WaitingForTrigger], true),
            (vec![], true),
        ] {
            let (gateway, bound, receipt) = pending_fixture();
            track_submission(
                Some(&a_cloid()),
                Some((&gateway.inner.submissions, &receipt)),
                async { Ok(response(statuses)) },
            )
            .await
            .expect("structured response");
            assert_eq!(
                gateway
                    .inner
                    .submissions
                    .state(bound.account)
                    .expect("state")
                    .pending
                    .is_some(),
                retained
            );
        }
    }

    #[tokio::test]
    async fn a_cancel_envelope_error_cannot_release_an_order_reservation() {
        let (gateway, bound, receipt) = pending_fixture();
        let parsed = ExchangeResponse::parse(r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"error":"wrong request identity"}]}}}"#).unwrap();
        let response = track_submission(
            Some(receipt.cloid()),
            Some((&gateway.inner.submissions, &receipt)),
            async { Ok(parsed) },
        )
        .await
        .unwrap();
        let error = order_outcome(response, Some(receipt.cloid().as_str().to_owned())).unwrap_err();
        assert!(
            matches!(&error, ToolError::TimeoutUnknownOutcome { cloid, .. }
            if cloid.as_deref() == Some(receipt.cloid().as_str()))
        );
        let error: ErrorData = error.into();
        assert_eq!(error.data.unwrap()["retryable"], false);
        assert_eq!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .unwrap()
                .pending,
            Some(receipt)
        );
    }

    #[tokio::test]
    async fn a_top_level_exchange_rejection_releases_the_next_submission() {
        let (gateway, bound, receipt) = pending_fixture();
        let queue = gateway.execution_queue(bound.account);
        let error = track_submission(
            Some(&a_cloid()),
            Some((&gateway.inner.submissions, &receipt)),
            async {
                Err(oppen_hl::Error::ExchangeRejected {
                    message: "invalid nonce".into(),
                })
            },
        )
        .await
        .expect_err("authoritative rejection");
        assert!(
            matches!(error, ToolError::VenueError { http_status: 200, venue_message }
            if venue_message == "invalid nonce")
        );
        assert!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .expect("state")
                .pending
                .is_none()
        );
        let next = reserve_account(
            queue,
            &gateway.inner.submissions,
            bound.account,
            |_| async { panic!("a rejected request must not need order-status reconciliation") },
        )
        .await
        .expect("next submission can proceed");
        assert!(next.revision > 0);
    }

    #[tokio::test]
    async fn http_and_unparseable_responses_keep_the_submission_reserved() {
        for error in [
            oppen_hl::Error::Venue {
                status: 503,
                message: "upstream unavailable".into(),
            },
            oppen_hl::Error::Venue {
                status: 400,
                message: "bad request".into(),
            },
            oppen_hl::Error::InvalidExchangeResponse("truncated JSON".into()),
        ] {
            let (gateway, bound, receipt) = pending_fixture();
            let queue = gateway.execution_queue(bound.account);
            assert!(
                track_submission(
                    Some(&a_cloid()),
                    Some((&gateway.inner.submissions, &receipt)),
                    async { Err(error) }
                )
                .await
                .is_err()
            );
            assert!(
                gateway
                    .inner
                    .submissions
                    .state(bound.account)
                    .expect("state")
                    .pending
                    .is_some()
            );
            assert!(matches!(
                reserve_account(
                    queue,
                    &gateway.inner.submissions,
                    bound.account,
                    |_| async { Ok(OrderStatusResponse::UnknownOid) }
                )
                .await,
                Err(ToolError::TimeoutUnknownOutcome { .. })
            ));
        }
    }

    #[tokio::test]
    async fn parsed_exchange_bodies_release_only_authoritative_rejections() {
        for (body, rejected) in [
            (r#"{"status":"err","response":"invalid nonce"}"#, true),
            ("{", false),
            (
                r#"{"status":"ok","response":{"type":"order","data":{}}}"#,
                false,
            ),
        ] {
            let (gateway, bound, receipt) = pending_fixture();
            let error = track_submission(
                Some(&a_cloid()),
                Some((&gateway.inner.submissions, &receipt)),
                async { ExchangeResponse::parse(body) },
            )
            .await
            .expect_err("rejected or malformed response");
            if rejected {
                assert!(matches!(
                    error,
                    ToolError::VenueError {
                        http_status: 200,
                        ..
                    }
                ));
                assert!(
                    gateway
                        .inner
                        .submissions
                        .state(bound.account)
                        .expect("state")
                        .pending
                        .is_none()
                );
            } else {
                assert!(
                    matches!(error, ToolError::TimeoutUnknownOutcome { cloid: Some(cloid), .. }
                    if cloid == a_cloid().as_str())
                );
                assert!(
                    gateway
                        .inner
                        .submissions
                        .state(bound.account)
                        .expect("state")
                        .pending
                        .is_some()
                );
            }
        }
    }

    fn acknowledge_test_policy(engine: &GuardrailEngine) {
        engine
            .operator_acknowledge_policy(engine.policy_observation().unwrap(), now_ms())
            .unwrap();
    }

    fn resume(gateway: &Gateway, bound: &Binding) {
        gateway
            .inner
            .engine
            .operator_release_kill(
                &oppen_core::guardrail::KillScope::Agent {
                    agent: bound.agent.clone(),
                },
                now_ms(),
            )
            .expect("resume");
        acknowledge_test_policy(&gateway.inner.engine);
    }

    #[test]
    fn pilot_cancellation_requires_verified_identity_and_distinguishes_waiting() {
        let bound = binding_for("alpha");
        let status = PilotStatus {
            agent: bound.agent.clone(),
            account: bound.account,
            authentication: oppen_core::ledger::PilotAuthentication::Unverified,
            halt: Some(PilotStop::AwaitingReconciliation),
            accounting: PilotAccounting::Known {
                executed_usd: Decimal::ZERO,
                reserved_usd: Decimal::from(150),
                net_realized_pnl_usd: Decimal::ZERO,
            },
        };
        assert!(!pilot_cancellation_needed(&bound, None).unwrap());
        assert!(!pilot_cancellation_needed(&bound, Some(status.clone())).unwrap());
        let mut stopped = status.clone();
        stopped.halt = Some(PilotStop::Unavailable {
            detail: "contradictory fill".into(),
        });
        assert!(pilot_cancellation_needed(&bound, Some(stopped)).unwrap());
        let mut unavailable = status.clone();
        unavailable.halt = None;
        unavailable.accounting = PilotAccounting::Unavailable {
            detail: "invalid fee unit".into(),
        };
        assert!(pilot_cancellation_needed(&bound, Some(unavailable.clone())).unwrap());
        unavailable.agent = oppen_core::guardrail::AgentId::new("other");
        assert!(pilot_cancellation_needed(&bound, Some(unavailable)).is_err());
        let mut wrong_account = status;
        wrong_account.account = Address::from_bytes([7; 20]);
        assert!(pilot_cancellation_needed(&bound, Some(wrong_account)).is_err());
    }

    #[derive(Debug, Default)]
    struct BlockingPilotAnchor {
        head: Mutex<Option<oppen_core::ledger::Anchor>>,
        wait: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        entered: tokio::sync::Notify,
    }

    #[derive(Debug)]
    struct PilotAnchorHandle(Arc<BlockingPilotAnchor>);

    impl oppen_core::ledger::HeadAnchor for PilotAnchorHandle {
        fn load(
            &self,
        ) -> Result<Option<oppen_core::ledger::Anchor>, oppen_core::ledger::LedgerError> {
            if let Some(wait) = self.0.wait.lock().unwrap().take() {
                self.0.entered.notify_one();
                // Disconnection also releases the holder if the test panics.
                let _ = wait.recv_timeout(std::time::Duration::from_secs(15));
            }
            Ok(self.0.head.lock().unwrap().clone())
        }

        fn store(
            &self,
            anchor: &oppen_core::ledger::Anchor,
        ) -> Result<(), oppen_core::ledger::LedgerError> {
            *self.0.head.lock().unwrap() = Some(anchor.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn blocked_pilot_reads_are_single_flight_and_do_not_hold_server_shutdown() {
        for same_handle in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let anchor = Arc::new(BlockingPilotAnchor::default());
            let ledger = Arc::new(
                oppen_core::ledger::Ledger::open_anchored(
                    &dir.path().join("pilot.db"),
                    Network::Testnet,
                    Some(Box::new(PilotAnchorHandle(anchor.clone()))),
                )
                .unwrap(),
            );
            let mut gateway = activated_gateway();
            Arc::get_mut(&mut gateway.inner).unwrap().events = EventViews::new(ledger.clone());
            let alpha = binding_for("alpha");
            let beta = Binding {
                account: Address::from_bytes([7; 20]),
                ..binding_for("beta")
            };
            pause(&gateway, &beta);
            let (release, wait) = std::sync::mpsc::channel();
            let holder = if same_handle {
                *anchor.wait.lock().unwrap() = Some(wait);
                let ledger = ledger.clone();
                let holder = tokio::task::spawn_blocking(move || {
                    ledger.verify().unwrap();
                });
                anchor.entered.notified().await;
                holder
            } else {
                let lock = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(dir.path().join("pilot.db.lock"))
                    .unwrap();
                lock.lock().unwrap();
                tokio::task::spawn_blocking(move || {
                    let _ = wait.recv_timeout(std::time::Duration::from_secs(15));
                    drop(lock);
                })
            };
            // Separate authority journal: this test deliberately blocks the pilot ledger.
            let mut pairings = pairing_store(&dir.path().join("pairings.db"));
            pairings.issue(alpha.clone()).unwrap();
            let shutdown = tokio_util::sync::CancellationToken::new();
            let server = tokio::spawn(crate::server::serve(
                0,
                gateway.clone(),
                Arc::new(std::sync::RwLock::new(pairings)),
                shutdown.clone(),
            ));
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while gateway.inner.pilot_reader.available_permits() != 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("server started its status read");
            shutdown.cancel();
            tokio::time::timeout(std::time::Duration::from_millis(500), server)
                .await
                .expect("status read blocked server shutdown")
                .unwrap()
                .unwrap();
            for _ in 0..3 {
                let mut attempts = Vec::new();
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    gateway.enforce_pauses_with(&[alpha.clone(), beta.clone()], |bound| {
                        attempts.push(bound);
                        async { Ok(()) }
                    }),
                )
                .await
                .expect("blocked reader starved a paused account");
                assert!(matches!(
                    result,
                    Err(ToolError::Unavailable {
                        what: "pilot cancellation reader busy",
                        ..
                    })
                ));
                assert_eq!(attempts.as_slice(), std::slice::from_ref(&beta));
                assert_eq!(gateway.inner.pilot_reader.available_permits(), 0);
            }
            release.send(()).unwrap();
            holder.await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while gateway.inner.pilot_reader.available_permits() == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("read releases its permit after lock recovery");
            assert!(!gateway.runtime_cancellation_needed(&alpha).await.unwrap());
        }
    }

    #[tokio::test]
    async fn canceled_registry_read_holds_single_flight_until_blocking_work_drains() {
        use oppen_core::ledger::{Ledger, PolicyJournal, RegistryJournal};
        let dir = tempfile::tempdir().unwrap();
        let anchor = Arc::new(BlockingPilotAnchor::default());
        let ledger = Arc::new(
            Ledger::open_anchored(
                &dir.path().join("registry.db"),
                Network::Testnet,
                Some(Box::new(PilotAnchorHandle(anchor.clone()))),
            )
            .unwrap(),
        );
        let hmac = Arc::new(oppen_core::keys::HmacKey::from_bytes([42; 32]));
        let mut gateway = activated_gateway();
        let inner = Arc::get_mut(&mut gateway.inner).unwrap();
        inner.engine = Arc::new(
            GuardrailEngine::new(
                Arc::new(PolicyJournal::new(Arc::new(
                    RegistryJournal::open(ledger.clone(), hmac.clone()).unwrap(),
                ))),
                Arc::new(NoKeys),
                inner.feed.clone(),
            )
            .unwrap(),
        );
        inner.test_registry = Some(RegistryJournal::open(ledger, hmac).unwrap());
        let bound = binding_for("alpha");
        grant_test_route(&gateway, &bound);
        let (release, wait) = std::sync::mpsc::channel();
        *anchor.wait.lock().unwrap() = Some(wait);
        let task_gateway = gateway.clone();
        let task_bound = bound.clone();
        let request = tokio::spawn(async move { task_gateway.require_route(&task_bound).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), anchor.entered.notified())
            .await
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(gateway.inner.route_reader.available_permits(), 0);
        assert!(matches!(
            gateway.require_route(&bound).await,
            Err(ToolError::Unavailable {
                what: "registry reader busy",
                ..
            })
        ));
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while gateway.inner.route_reader.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gateway.require_route(&bound).await.unwrap();
    }

    #[tokio::test]
    async fn busy_pilot_reader_does_not_hide_cleanup_or_ownership_refusal() {
        let gateway = activated_gateway();
        let alpha = binding_for("alpha");
        grant_test_route(&gateway, &alpha);
        gateway
            .inner
            .engine
            .register_agent(&alpha.agent, now_ms())
            .unwrap();
        let mut policy = gateway.inner.engine.guardrails(&alpha.agent).unwrap();
        policy.approval_required = false;
        gateway
            .inner
            .engine
            .operator_set_guardrails(&alpha.agent, policy, now_ms())
            .unwrap();
        let beta = Binding {
            account: Address::from_bytes([7; 20]),
            ..binding_for("beta")
        };
        pause(&gateway, &beta);
        let _busy = gateway
            .inner
            .pilot_reader
            .clone()
            .try_acquire_owned()
            .unwrap();
        let mut attempts = Vec::new();
        let result = gateway
            .enforce_pauses_with(&[alpha.clone(), beta.clone()], |bound| {
                attempts.push(bound);
                async { Ok(()) }
            })
            .await;
        assert!(matches!(
            result,
            Err(ToolError::Unavailable {
                what: "pilot cancellation reader busy",
                ..
            })
        ));
        assert_eq!(attempts, [beta]);
        let result = gateway
            .cancel_all_with(
                &alpha,
                &SymbolActionParams {
                    symbol: None,
                    reason: "operator independent agent cancel".into(),
                },
                false,
                test_tracker(&gateway),
                async { Ok(cancel_fixture()) },
                |_, _, _| async { panic!("unowned target must not submit") },
            )
            .await
            .unwrap();
        assert!(!result.complete);
        assert_eq!(
            serde_json::to_value(&result.reply).unwrap()["refusal"]["unevaluable"],
            "submission_authority"
        );
    }

    fn cancel_fixture() -> (Vec<oppen_hl::types::OpenOrder>, Universe) {
        let orders = serde_json::from_value(serde_json::json!([{
            "coin": "BTC", "side": "B", "limitPx": "100", "sz": "1",
            "origSz": "1", "oid": 42, "timestamp": 1, "orderType": "Limit",
            "reduceOnly": false, "isTrigger": false, "isPositionTpsl": false,
            "tif": "Gtc", "triggerPx": "0", "triggerCondition": "N/A"
        }]))
        .expect("orders");
        let meta = serde_json::from_value(serde_json::json!({
            "universe": [{ "name": "BTC", "szDecimals": 2, "maxLeverage": 40 }]
        }))
        .expect("meta");
        (orders, Universe::from_meta(&meta).expect("universe"))
    }

    #[test]
    fn cancellation_context_preserves_protective_target_evidence_as_string_money() {
        let (mut orders, universe) = cancel_fixture();
        let order = &mut orders[0];
        order.reduce_only = true;
        order.is_trigger = true;
        order.is_position_tpsl = true;
        order.trigger_px = Some("95.25".parse().unwrap());
        order.trigger_condition = Some("Below <untrusted>".into());
        order.order_type = "Stop Market".into();
        order.cloid = Some(Cloid::from_bytes([19; 16]));
        let bound = binding_for("alpha");
        let context = cancellation_context(&bound, &orders, &universe, 1234).unwrap();
        assert_eq!(context.account, bound.account);
        assert_eq!(context.observed_at_ms, 1234);
        let target = serde_json::to_value(&context.targets[0]).unwrap();
        assert_eq!(target["oid"], 42);
        assert_eq!(target["cloid"], orders[0].cloid.as_ref().unwrap().as_str());
        assert_eq!(target["limit_px"], "100");
        assert_eq!(target["sz"], "1");
        assert_eq!(target["trigger_px"], "95.25");
        assert_eq!(target["trigger_condition"], "Below <untrusted>");
        assert_eq!(target["reduce_only"], true);
        assert_eq!(target["is_trigger"], true);
        assert_eq!(target["is_position_tpsl"], true);
    }

    #[tokio::test]
    async fn a_resume_while_waiting_for_execution_skips_the_stale_pause_cancel() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        pause(&gateway, &bound);
        let queue = gateway.execution_queue(bound.account);
        let held = queue.lock().await;
        let params = SymbolActionParams {
            symbol: None,
            reason: "pause sweep".into(),
        };
        let mut cancel = std::pin::pin!(gateway.cancel_all_with(
            &bound,
            &params,
            true,
            test_tracker(&gateway),
            async { panic!("resumed account must not even read cancellation targets") },
            |_, _, _| async { panic!("resumed account must not submit a cancel") },
        ));
        assert!(matches!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(cancel.as_mut().poll(cx))).await,
            std::task::Poll::Pending
        ));
        resume(&gateway, &bound);
        drop(held);
        let result = cancel.await.expect("skip stale sweep");
        assert!(result.complete);
        assert!(queue.try_lock().is_ok());
    }

    #[tokio::test]
    async fn a_resume_skips_cleanup_but_does_not_authorize_an_unowned_agent_target() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        pause(&gateway, &bound);
        let mut policy = gateway.inner.engine.guardrails(&bound.agent).unwrap();
        policy.approval_required = false;
        gateway
            .inner
            .engine
            .operator_set_guardrails(&bound.agent, policy, now_ms())
            .unwrap();
        let params = SymbolActionParams {
            symbol: None,
            reason: "cancel".into(),
        };
        let result = gateway
            .cancel_all_with(
                &bound,
                &params,
                true,
                test_tracker(&gateway),
                async {
                    resume(&gateway, &bound);
                    Ok(cancel_fixture())
                },
                |_, _, _| async { panic!("resume during reads must prevent cancellation") },
            )
            .await
            .expect("skip after resume");
        assert!(result.complete);
        let result = gateway
            .cancel_all_with(
                &bound,
                &params,
                false,
                test_tracker(&gateway),
                async { Ok(cancel_fixture()) },
                |_, _, _| async { panic!("unowned target must not submit") },
            )
            .await
            .expect("unowned target refusal while unpaused");
        assert!(!result.complete);
        assert_eq!(
            serde_json::to_value(&result.reply).unwrap()["refusal"]["unevaluable"],
            "submission_authority"
        );
        assert!(!gateway.inner.engine.paused_agents().contains(&bound.agent));
    }

    #[tokio::test]
    async fn a_still_paused_account_submits_cancellation_without_releasing_its_reservation() {
        let (gateway, bound, receipt) = pending_fixture();
        pause(&gateway, &bound);
        let queue = gateway.execution_queue(bound.account);
        let params = SymbolActionParams {
            symbol: None,
            reason: "pause sweep".into(),
        };
        let mut submitted = false;
        let result = gateway
            .cancel_all_with(
                &bound,
                &params,
                true,
                test_tracker(&gateway),
                async { Ok(cancel_fixture()) },
                |cleared, _execution, _tracker| {
                    assert!(
                        queue.try_lock().is_err(),
                        "cancel must keep the execution lock"
                    );
                    assert!(matches!(
                        cleared.clearance().kind,
                        oppen_core::guardrail::ClearedKind::Cancel { count: 1 }
                    ));
                    submitted = true;
                    async { Ok(cancel_response(vec![Status::Success])) }
                },
            )
            .await
            .expect("paused cancel");
        assert!(submitted && result.complete);
        assert_eq!(
            gateway
                .inner
                .submissions
                .state(bound.account)
                .expect("state")
                .pending,
            Some(receipt)
        );
        assert!(gateway.inner.engine.paused_agents().contains(&bound.agent));
    }

    fn pause(gateway: &Gateway, bound: &Binding) {
        use oppen_core::guardrail::{KillReason, KillScope};
        grant_test_route(gateway, bound);
        gateway
            .inner
            .engine
            .register_agent(&bound.agent, now_ms())
            .expect("register");
        gateway
            .inner
            .engine
            .operator_engage_kill(
                KillScope::Agent {
                    agent: bound.agent.clone(),
                },
                KillReason::Operator,
                now_ms(),
            )
            .expect("persist pause");
        acknowledge_test_policy(&gateway.inner.engine);
    }

    #[tokio::test]
    async fn busy_decision_worker_does_not_report_a_successful_fresh_pause_sweep() {
        let gateway = activated_gateway();
        let permit = gateway
            .inner
            .decision_worker
            .clone()
            .try_acquire_owned()
            .unwrap();
        assert!(matches!(
            gateway.enforce_pauses(&[], test_tracker(&gateway)).await,
            Err(ToolError::Unavailable {
                what: "decision worker busy",
                ..
            })
        ));
        drop(permit);
        gateway
            .enforce_pauses(&[], test_tracker(&gateway))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pause_enforcement_retries_revoked_bindings_and_continues_after_a_failure() {
        let gateway = activated_gateway();
        let alpha = binding_for("alpha");
        let mut beta = binding_for("beta");
        beta.account = "0x1111111111111111111111111111111111111111"
            .parse()
            .expect("address");
        pause(&gateway, &alpha);
        pause(&gateway, &beta);
        let dir = tempfile::tempdir().unwrap();
        let mut store = pairing_store(&dir.path().join("pairings.db"));
        let issued = store.issue(alpha.clone()).expect("token");
        store.revoke(issued.id).unwrap();
        store.issue(beta.clone()).expect("token");
        store.issue(beta.clone()).expect("duplicate pairing");
        store.issue(binding_for("unpaused")).expect("token");
        let mut bindings = store.bindings();
        bindings.sort_by(|a, b| a.agent.cmp(&b.agent));
        gateway.inner.engine.take_pending_kill_effects();

        let mut attempts = Vec::new();
        let failed = gateway
            .enforce_pauses_with(&bindings, |bound| {
                attempts.push(bound.agent.clone());
                let fail = bound.agent == alpha.agent;
                async move {
                    if fail {
                        Err(ToolError::unavailable("cancel", "connection reset"))
                    } else {
                        Ok(())
                    }
                }
            })
            .await;
        assert!(matches!(failed, Err(ToolError::Unavailable { .. })));
        assert_eq!(attempts, [alpha.agent.clone(), beta.agent.clone()]);

        attempts.clear();
        gateway
            .enforce_pauses_with(&bindings, |bound| {
                attempts.push(bound.agent);
                async { Ok(()) }
            })
            .await
            .expect("next sweep retries");
        assert_eq!(attempts, [alpha.agent.clone(), beta.agent.clone()]);
        assert!(gateway.inner.engine.paused_agents().contains(&alpha.agent));
    }

    #[tokio::test]
    async fn dropping_a_pause_sweep_keeps_the_persisted_pause_for_retry() {
        let gateway = activated_gateway();
        let bound = binding_for("alpha");
        pause(&gateway, &bound);
        let bindings = [bound.clone()];
        let mut started = false;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                gateway.enforce_pauses_with(&bindings, |_| {
                    started = true;
                    std::future::pending()
                })
            )
            .await
            .is_err()
        );
        assert!(started);
        gateway.inner.engine.take_pending_kill_effects();
        let mut retried = false;
        gateway
            .enforce_pauses_with(&bindings, |retry| {
                assert_eq!(retry, bound);
                retried = true;
                async { Ok(()) }
            })
            .await
            .expect("retry after dropped sweep");
        assert!(retried);
        assert!(gateway.inner.engine.paused_agents().contains(&bound.agent));
    }

    #[tokio::test]
    async fn a_hung_pause_cancel_does_not_starve_another_account() {
        let gateway = activated_gateway();
        let alpha = binding_for("alpha");
        let mut beta = binding_for("beta");
        beta.account = "0x1111111111111111111111111111111111111111"
            .parse()
            .expect("address");
        pause(&gateway, &alpha);
        pause(&gateway, &beta);
        let mut attempts = Vec::new();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            gateway.enforce_pauses_with(&[alpha.clone(), beta.clone()], |bound| {
                attempts.push(bound.agent.clone());
                let hangs = bound.agent == alpha.agent;
                async move {
                    if hangs {
                        std::future::pending::<()>().await;
                    }
                    Ok(())
                }
            }),
        )
        .await
        .expect("hung account starved the sweep");
        assert!(matches!(
            result,
            Err(ToolError::Unavailable {
                what: "pause cancellation timeout",
                ..
            })
        ));
        assert_eq!(attempts, [alpha.agent, beta.agent]);
    }

    fn events_of(gateway: &Gateway, agent: &str, params: GetEventsParams) -> serde_json::Value {
        let result = gateway
            .events_page(
                &binding_for(agent),
                params.limit.unwrap_or(oppen_core::ledger::MAX_PAGE),
                params.since_cursor,
            )
            .expect("get_events");
        serde_json::from_str(&body(result)).expect("json")
    }

    /// The envelope an agent actually receives. Invariant 6 is a claim about
    /// bytes, so the keys and their order are asserted rather than assumed.
    #[test]
    fn the_event_envelope_is_versioned_and_carries_the_cursor_contract() {
        let gateway = gateway_over(&[(Some("agent-alpha"), "mine")]);
        let page = events_of(
            &gateway,
            "agent-alpha",
            GetEventsParams {
                since_cursor: 0,
                limit: None,
            },
        );

        assert_eq!(page["contract_version"], 0);
        assert_eq!(page["resync_required"], false);
        assert_eq!(page["head_seq"], 1);
        assert_eq!(page["next_cursor"], 1);
        assert_eq!(page["events"].as_array().expect("array").len(), 1);
        assert_eq!(page["events"][0]["kind"], "agent_decision");
        assert_eq!(page["events"][0]["payload"]["reason"], "mine");
    }

    /// Item 15, and the thing that makes C6 more than a filter nobody reaches:
    /// **one gateway, two paired agents, each acting as itself.** Before this,
    /// the gateway held one hardcoded agent and every token got that identity,
    /// so per-agent scoping was decorative.
    #[test]
    fn two_pairings_on_one_gateway_read_as_two_different_agents() {
        let gateway = gateway_over(&[
            (Some("agent-alpha"), "alpha's reason"),
            (Some("agent-beta"), "beta's reason"),
            (None, "kill switch engaged"),
        ]);

        let reasons = |agent: &str| -> Vec<String> {
            events_of(
                &gateway,
                agent,
                GetEventsParams {
                    since_cursor: 0,
                    limit: None,
                },
            )["events"]
                .as_array()
                .expect("array")
                .iter()
                .map(|e| e["payload"]["reason"].as_str().expect("reason").to_owned())
                .collect()
        };

        assert_eq!(
            reasons("agent-alpha"),
            ["alpha's reason", "kill switch engaged"]
        );
        assert_eq!(
            reasons("agent-beta"),
            ["beta's reason", "kill switch engaged"]
        );
    }

    /// `docs/decisions.md` C6, at the surface an agent actually calls. The
    /// scope is enforced in `oppen-core`; this proves the gateway asks for the
    /// scoped read rather than the operator one.
    #[test]
    fn an_agent_does_not_receive_another_agents_events() {
        let gateway = gateway_over(&[
            (Some("agent-alpha"), "mine"),
            (Some("agent-beta"), "secret strategy"),
            (None, "kill switch engaged"),
        ]);
        let page = events_of(
            &gateway,
            "agent-alpha",
            GetEventsParams {
                since_cursor: 0,
                limit: None,
            },
        );

        let reasons: Vec<&str> = page["events"]
            .as_array()
            .expect("array")
            .iter()
            .map(|e| e["payload"]["reason"].as_str().expect("reason"))
            .collect();
        assert_eq!(reasons, vec!["mine", "kill switch engaged"]);
        assert!(
            !body_contains(&page, "secret strategy"),
            "another agent's reason string reached this agent"
        );
    }

    /// Item 18: a cursor the ledger cannot serve is explicit, never a page with
    /// a hole in it. A cursor past the head is what a mainnet cursor presented
    /// to a testnet file looks like (R4).
    #[test]
    fn a_cursor_past_the_head_asks_for_a_resync_rather_than_an_empty_page() {
        let gateway = gateway_over(&[(Some("agent-alpha"), "mine")]);
        let page = events_of(
            &gateway,
            "agent-alpha",
            GetEventsParams {
                since_cursor: 9_999,
                limit: None,
            },
        );

        assert_eq!(page["resync_required"], true);
        assert_eq!(page["events"].as_array().expect("array").len(), 0);
        assert_eq!(page["head_seq"], 1);
    }

    /// The caller's page size is honoured, and the cursor it returns resumes
    /// exactly where the page stopped.
    #[test]
    fn a_limit_bounds_the_page_and_the_cursor_resumes_from_it() {
        let gateway = gateway_over(&[
            (Some("agent-alpha"), "one"),
            (Some("agent-alpha"), "two"),
            (Some("agent-alpha"), "three"),
        ]);
        let first = events_of(
            &gateway,
            "agent-alpha",
            GetEventsParams {
                since_cursor: 0,
                limit: Some(2),
            },
        );
        assert_eq!(first["events"].as_array().expect("array").len(), 2);
        assert_eq!(first["next_cursor"], 2);

        let rest = events_of(
            &gateway,
            "agent-alpha",
            GetEventsParams {
                since_cursor: 2,
                limit: Some(10),
            },
        );
        let reasons: Vec<&str> = rest["events"]
            .as_array()
            .expect("array")
            .iter()
            .map(|e| e["payload"]["reason"].as_str().expect("reason"))
            .collect();
        assert_eq!(
            reasons,
            vec!["three"],
            "no event may be skipped or repeated"
        );
    }

    fn body_contains(page: &serde_json::Value, needle: &str) -> bool {
        page.to_string().contains(needle)
    }

    fn place_params(json: serde_json::Value) -> PlaceParams {
        serde_json::from_value(json).expect("params")
    }

    /// The journal is the one place an agent's own reasoning accumulates, so
    /// the scoping that matters for events (C6) matters at least as much here.
    #[test]
    fn one_agents_notes_are_not_another_agents() {
        let gateway = gateway_over(&[]);
        let journal = &gateway.inner.journal;

        journal
            .remember("agent-alpha", "edge", "alpha's edge", 1)
            .expect("alpha writes");
        journal
            .remember("agent-beta", "edge", "beta's edge", 1)
            .expect("beta writes");

        assert_eq!(
            journal
                .recall("agent-alpha", "edge")
                .expect("recall")
                .expect("note")
                .value,
            "alpha's edge"
        );
        assert_eq!(journal.recall_all("agent-beta").expect("all").len(), 1);
    }

    /// Item 12's three single-order types, as an agent would name them.
    #[test]
    fn the_order_types_deserialize_from_their_wire_names() {
        let limit = place_params(serde_json::json!({
            "symbol": "BTC", "is_buy": true, "size": "1", "reason": "why",
            "order_type": "limit", "limit_px": "100", "tif": "alo"
        }));
        assert!(matches!(
            limit.order,
            PlaceKind::Limit { ref limit_px, tif: PlaceTif::Alo } if limit_px == "100"
        ));

        let market = place_params(serde_json::json!({
            "symbol": "BTC", "is_buy": false, "size": "1", "reason": "why",
            "order_type": "market"
        }));
        assert!(matches!(market.order, PlaceKind::Market));

        let stop = place_params(serde_json::json!({
            "symbol": "BTC", "is_buy": false, "size": "1", "reason": "why",
            "order_type": "stop_market", "trigger_px": "90"
        }));
        assert!(matches!(
            stop.order,
            PlaceKind::StopMarket { ref trigger_px, tpsl: PlaceTpsl::Sl } if trigger_px == "90"
        ));
    }

    /// `tif` defaults to the one that rests. An order that silently became IOC
    /// would be cancelled instead of working, which is the expensive direction
    /// to guess wrong in.
    #[test]
    fn a_limit_order_without_a_tif_rests() {
        let p = place_params(serde_json::json!({
            "symbol": "BTC", "is_buy": true, "size": "1", "reason": "why",
            "order_type": "limit", "limit_px": "100"
        }));
        assert!(matches!(
            p.order,
            PlaceKind::Limit {
                tif: PlaceTif::Gtc,
                ..
            }
        ));
    }

    /// The combinations the type makes unspellable. A stop with no trigger and
    /// a market order with a limit price are refused by deserialization, not
    /// by a runtime check that could be forgotten.
    #[test]
    fn an_order_type_cannot_be_given_the_wrong_fields() {
        let stop_without_trigger = serde_json::from_value::<PlaceParams>(serde_json::json!({
            "symbol": "BTC", "is_buy": true, "size": "1", "reason": "why",
            "order_type": "stop_market"
        }));
        assert!(
            stop_without_trigger.is_err(),
            "a stop needs a trigger price"
        );

        let limit_without_price = serde_json::from_value::<PlaceParams>(serde_json::json!({
            "symbol": "BTC", "is_buy": true, "size": "1", "reason": "why",
            "order_type": "limit"
        }));
        assert!(limit_without_price.is_err(), "a limit needs a price");

        let no_type = serde_json::from_value::<PlaceParams>(serde_json::json!({
            "symbol": "BTC", "is_buy": true, "size": "1", "reason": "why"
        }));
        assert!(no_type.is_err(), "an order type is not optional");
    }

    /// Every time-in-force reaches the wire spelling the signer hashes.
    #[test]
    fn each_tif_maps_to_its_wire_variant() {
        assert_eq!(Tif::from(PlaceTif::Gtc), Tif::Gtc);
        assert_eq!(Tif::from(PlaceTif::Ioc), Tif::Ioc);
        assert_eq!(Tif::from(PlaceTif::Alo), Tif::Alo);
        assert_eq!(Tpsl::from(PlaceTpsl::Sl), Tpsl::Sl);
        assert_eq!(Tpsl::from(PlaceTpsl::Tp), Tpsl::Tp);
    }

    /// `AGENTS.md` invariant 1, for the crate that now has four tools that
    /// act.
    ///
    /// The type already carries most of it: [`Gateway::submit`] takes a
    /// `Cleared` by value, `Cleared` is not `Clone`, and only
    /// `GuardrailEngine::evaluate` and `clear_cancel` produce one — so no tool
    /// here can sign without a clearance. What no type expresses is the
    /// residual `oppen-core`'s own module doc names: `sign_unchecked` and
    /// `AgentKey::sign_l1_action` are `pub` for spec item 33's manual escape
    /// hatch, and a caller **in another crate** can write its own permissive
    /// `PreSignCheck`. This crate is that other crate, so all three are
    /// checked here.
    ///
    /// The needles are built at runtime so this file does not match itself,
    /// and comment lines are skipped so the doc comment above is not read as
    /// a call site.
    #[test]
    fn no_call_site_in_oppen_mcp_reaches_the_signer_unchecked() {
        fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
                .expect("the crate's own src directory is readable")
                .map(|e| e.expect("directory entry").path())
                .collect();
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    rust_sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let needles = [
            format!("sign_{}", "unchecked"),
            format!("sign_{}", "l1_action"),
            format!("Pre{}Check", "Sign"),
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        assert!(files.len() > 3, "the source walk found almost nothing");

        let mut offenders = Vec::new();
        for path in &files {
            let text = std::fs::read_to_string(path).expect("source file is utf-8");
            for (n, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                for needle in &needles {
                    if code.contains(needle.as_str()) {
                        offenders.push(format!("{}:{}: {}", path.display(), n + 1, code));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "every signature from oppen-mcp goes through Gateway::submit, which consumes a \
             Cleared (AGENTS.md invariant 1). Ungated call sites:\n{}",
            offenders.join("\n")
        );
    }

    /// Two mints must not collide: item 19's reconcile-by-cloid answers for
    /// the wrong order if they do.
    #[test]
    fn minted_cloids_are_distinct() {
        let a = mint_cloid().expect("entropy");
        let b = mint_cloid().expect("entropy");
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(a.as_str().len(), b.as_str().len());
    }
}
