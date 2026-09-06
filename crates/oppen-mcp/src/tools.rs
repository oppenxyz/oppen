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

use std::sync::Arc;

use oppen_core::alert::{AlertStore, Condition, Direction};
use oppen_core::book;
use oppen_core::features::quotes::QuoteCache;
use oppen_core::features::{book_features, funding_features, vol_features};
use oppen_core::feed::FeedSession;
use oppen_core::guardrail::{Cleared, FeedQuality, GuardrailEngine, MarketRef, OrderIntent};
use oppen_core::journal::Journal;
use oppen_core::ledger::EventViews;
use oppen_core::state::{
    AccountState, VenueReadings, assemble, exposure_from, realized_pnl_since, utc_day_start_ms,
};
use oppen_hl::info::OrderRef;
use oppen_hl::types::{Candle, OrderStatusResponse, ReferencePrices};
use oppen_hl::wire::{CancelWire, Cloid, Grouping, Tif, Tpsl};
use oppen_hl::{Address, InfoClient, Network, Universe, meta::MIN_NOTIONAL_USD};
use oppen_hl::{ExchangeClient, ExchangeResponse, NonceAllocator, OrderKind, Status};

use crate::auth::Binding;

/// Basis points per unit. A bound 1% wide is 100 bps.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);
use crate::outcome::{self, CancelFailure, Reply, ToolError};
use rmcp::RoleServer;
use rmcp::service::RequestContext;
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
    /// Hands out one agent's read-only slice of the one ledger
    /// (`docs/spec.md` D6), built per request for whoever the token names. An
    /// [`EventViews`] and never a `Ledger`: `redact`, `upsert_sub_account` and
    /// the gap surface are not spellable from here, so `AGENTS.md` invariant 3
    /// is a compile error rather than a review note.
    events: EventViews,
}

/// The inputs `GuardrailEngine::evaluate` needs, gathered once per call.
struct EvaluationContext {
    universe: Universe,
    state: AccountState,
    exposure: oppen_core::guardrail::Exposure,
    market: MarketRef,
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

#[tool_router]
impl Gateway {
    pub fn new(
        network: Network,
        engine: Arc<GuardrailEngine>,
        events: EventViews,
        journal: Arc<Journal>,
        feed: Arc<FeedSession>,
        alerts: Arc<AlertStore>,
        quotes: Arc<QuoteCache>,
    ) -> Result<Self, oppen_hl::Error> {
        Ok(Self {
            inner: Arc::new(GatewayInner {
                network,
                info: InfoClient::new(network)?,
                engine,
                exchange: ExchangeClient::new(network)?,
                nonces: NonceAllocator::new(),
                journal,
                feed,
                alerts,
                quotes,
                events,
            }),
        })
    }

    /// Who this request's token names (`docs/spec.md` item 15).
    ///
    /// `rmcp` republishes the request's `http::request::Parts` into the tool's
    /// context, and the door put the [`Binding`] there after authenticating.
    /// An absent binding is not a caller error to explain: the door refuses
    /// every unauthenticated request, so reaching a tool without one means the
    /// gateway was mounted without its guard. Fail closed and say so.
    fn bound(ctx: &RequestContext<RoleServer>) -> Result<Binding, ToolError> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Binding>())
            .cloned()
            .ok_or(ToolError::Unavailable {
                what: "pairing",
                detail: "this request carries no pairing binding; the gateway is \
                         mounted without its door"
                    .to_owned(),
            })
    }

    pub fn network(&self) -> Network {
        self.inner.network
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
        let inner = &self.inner;
        let bound = Self::bound(&ctx)?;
        let now_ms = now_ms();

        let context = self
            .evaluation_context(bound.account, &params.symbol, now_ms)
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
        let (px, kind) = match &params.order {
            PlaceKind::Limit { limit_px, tif } => (
                limit_px
                    .parse::<Decimal>()
                    .map_err(|e| ToolError::invalid("limit_px", e))?,
                OrderKind::Limit { tif: (*tif).into() },
            ),
            PlaceKind::Market => {
                // A missing mid is a refusal, not a guess: this is the gateway
                // failing to build an order rather than a guardrail verdict.
                let mid = context
                    .market
                    .reference_px
                    .ok_or_else(|| ToolError::Unavailable {
                        what: "reference price",
                        detail: format!(
                            "no mid for {}; refusing to price a market order",
                            params.symbol
                        ),
                    })?;
                (
                    self.crossing_price(&bound, asset, mid, params.is_buy)?,
                    OrderKind::Limit { tif: Tif::Ioc },
                )
            }
            PlaceKind::StopMarket { trigger_px, tpsl } => {
                let trigger_px = trigger_px
                    .parse::<Decimal>()
                    .map_err(|e| ToolError::invalid("trigger_px", e))?;
                // Priced from the *trigger*, not today's mid: that is where
                // the book will be when this fills, and it is the reference
                // the engine measures the slippage cap against.
                (
                    self.crossing_price(&bound, asset, trigger_px, params.is_buy)?,
                    OrderKind::Trigger {
                        is_market: true,
                        trigger_px,
                        tpsl: (*tpsl).into(),
                    },
                )
            }
        };

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
        };

        // The gate. There is no branch around it.
        let cleared = match inner.engine.evaluate(
            &bound.agent,
            &intent,
            asset,
            &context.market,
            &context.exposure,
            now_ms,
        ) {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let response = self.submit(cleared, Some(&cloid), now_ms).await?;
        Ok(order_outcome(response, Some(cloid.as_str().to_owned()))?.into_result())
    }

    /// `cancel` — one resting order (`docs/spec.md` item 19).
    #[tool(
        description = "Cancel one resting order by oid or by the cloid place returned. Requires \
                       a reason. Cancels are risk-reducing: they are cleared while the kill \
                       switch is engaged and they cost no order-rate token."
    )]
    async fn cancel(
        &self,
        Parameters(params): Parameters<CancelParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let bound = Self::bound(&ctx)?;
        let now_ms = now_ms();

        // Which resting order this names, and on which asset. The asset id is
        // part of both cancel wires, so an oid alone is not enough and the
        // open-order list is the only place it can come from.
        let orders = inner
            .info
            .frontend_open_orders(bound.account)
            .await
            .map_err(|e| ToolError::unavailable("orders", e))?;
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
        let asset = universe
            .get(&order.coin)
            .map_err(|e| ToolError::unavailable("universe", e))?;

        let cleared = match inner.engine.clear_cancel(
            &bound.agent,
            vec![CancelWire {
                a: asset.index,
                o: order.oid,
            }],
            &params.reason,
            now_ms,
        ) {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let response = self.submit(cleared, order.cloid.as_ref(), now_ms).await?;
        Ok(cancel_outcome(
            response,
            vec![(
                Some(order.oid),
                order.cloid.as_ref().map(|c| c.as_str().to_owned()),
            )],
        )
        .into_result())
    }

    /// `cancel_all` — every resting order, or every one on a symbol
    /// (`docs/spec.md` item 19).
    #[tool(
        description = "Cancel every resting order, or every one on a symbol. Requires a reason. \
                       Partial success is normal — an order that filled a moment ago cannot be \
                       cancelled — so the result itemises what the venue would not take."
    )]
    async fn cancel_all(
        &self,
        Parameters(params): Parameters<SymbolActionParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let bound = Self::bound(&ctx)?;
        let now_ms = now_ms();

        let orders = inner
            .info
            .frontend_open_orders(bound.account)
            .await
            .map_err(|e| ToolError::unavailable("orders", e))?;
        let universe = self.universe().await?;

        let targets: Vec<_> = orders
            .into_iter()
            .filter(|o| params.symbol.as_ref().is_none_or(|s| *s == o.coin))
            .collect();
        // Nothing resting is the state the caller wanted, and the engine
        // refuses an empty cancel as an input mismatch — so this answers
        // without asking it.
        if targets.is_empty() {
            return Ok(outcome::canceled(0, Vec::new()).into_result());
        }

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

        let cleared = match inner
            .engine
            .clear_cancel(&bound.agent, wires, &params.reason, now_ms)
        {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let response = self.submit(cleared, None, now_ms).await?;
        Ok(cancel_outcome(response, named).into_result())
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
        let inner = &self.inner;
        let bound = Self::bound(&ctx)?;
        let now_ms = now_ms();

        let context = self
            .evaluation_context(bound.account, &params.symbol, now_ms)
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
            // The operator's bound, shared with `place`'s market order.
            self.operator_slippage_bps(&bound)?,
            asset,
            mint_cloid()?,
        );

        // The same gate as `place`. A close is not privileged: it is refused
        // when the account is unreconciled or the feed is stale, exactly as an
        // opening order is.
        let cleared = match inner.engine.evaluate(
            &bound.agent,
            &intent,
            asset,
            &context.market,
            &context.exposure,
            now_ms,
        ) {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let cloid = intent.cloid.clone();
        let response = self.submit(cleared, cloid.as_ref(), now_ms).await?;
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
        let bound = Self::bound(&ctx)?;
        let now_ms = now_ms();

        let context = self
            .evaluation_context(bound.account, &params.symbol, now_ms)
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
        };
        let verdict = inner.engine.preflight(
            &bound.agent,
            &intent,
            asset,
            &context.market,
            &context.exposure,
            now_ms,
        );

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

        // `unknown_oid` is a real answer and the important one after a
        // timeout: the venue never saw the order, so it is safe to place
        // again. Anything else means it did, and resending would double it.
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
                       If `feed` is not `live`, the data is stale and execution will fail closed."
    )]
    async fn get_state(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let bound = Self::bound(&ctx)?;
        let state = self.read_state(bound.account, now_ms()).await?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&state).expect("AccountState serializes"),
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
        now_ms: u64,
    ) -> Result<ExchangeResponse, ToolError> {
        let inner = &self.inner;
        let nonce = inner.nonces.next();
        let (request, _clearance) = inner
            .engine
            .sign_cleared(cleared, nonce, None, now_ms)
            .map_err(|e| ToolError::unavailable("signer", e))?;

        inner.exchange.post(&request).await.map_err(|e| match e {
            oppen_hl::Error::Venue { status, message } => ToolError::venue(status, message),
            transport => ToolError::TimeoutUnknownOutcome {
                cloid: cloid.map(|c| c.as_str().to_owned()),
                detail: transport.to_string(),
            },
        })
    }

    /// Everything [`GuardrailEngine::evaluate`] needs, assembled once.
    ///
    /// `place` and `close_position` both call it, and they must: two
    /// assemblies of the same inputs drift, and the one that drifts is the one
    /// that decides whether an order is refused.
    async fn evaluation_context(
        &self,
        account: Address,
        symbol: &str,
        now_ms: u64,
    ) -> Result<EvaluationContext, ToolError> {
        let inner = &self.inner;
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

        let exposure = exposure_from(
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

        // A missing price is a refusal, never a fallback. This used to read
        // `allMids`, which answers for an unquoted asset with a frozen last
        // print — so the refusal this comment describes never happened and the
        // caps below were measured against a price that could be 9.9× off.
        let reference_px = self.reference_pxs().await?.get(symbol);
        let market = MarketRef {
            symbol: symbol.to_owned(),
            reference_px,
            as_of_ms: now_ms,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
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
    /// one conversion to a fraction lives in [`Gateway::crossing_price`].
    fn operator_slippage_bps(&self, bound: &Binding) -> Result<Decimal, ToolError> {
        let guardrails =
            self.inner
                .engine
                .guardrails(&bound.agent)
                .ok_or_else(|| ToolError::Unavailable {
                    what: "guardrails",
                    detail: format!("{} is not a paired agent", bound.agent),
                })?;
        Ok(guardrails.max_slippage_bps)
    }

    /// A price that crosses the book from `reference_px`, within that bound.
    ///
    /// Rounded toward the reference, so pricing *at* the operator's limit
    /// cannot be refused *for* that limit (`docs/decisions.md` C5).
    fn crossing_price(
        &self,
        bound: &Binding,
        asset: &oppen_hl::meta::Asset,
        reference_px: Decimal,
        is_buy: bool,
    ) -> Result<Decimal, ToolError> {
        let slippage = self.operator_slippage_bps(bound)? / BPS;
        Ok(asset.slippage_price_bounded(reference_px, is_buy, slippage))
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

/// Map an order response onto item 19's synchronous result.
///
/// A venue `error` status is a `venue_error` rather than a refusal: the nonce
/// was spent and the request was seen, which is a different fact about the
/// world than a guardrail saying no.
fn order_outcome(response: ExchangeResponse, cloid: Option<String>) -> Result<Reply, ToolError> {
    // One order in, so one status out. A venue that answers a single order
    // with none of them has not accepted it, and reporting that as success
    // would invent a fill.
    let Some(status) = response.statuses.into_iter().next() else {
        return Err(ToolError::venue(
            200,
            "the venue accepted the request but reported no order status".to_owned(),
        ));
    };
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
fn cancel_outcome(response: ExchangeResponse, named: Vec<(Option<u64>, Option<String>)>) -> Reply {
    let requested = named.len();
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
    outcome::canceled(requested, failed)
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
fn close_intent(
    symbol: &str,
    reason: &str,
    position_size: Decimal,
    reference_px: Decimal,
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
        ExchangeResponse { statuses }
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
        assert!(matches!(err, ToolError::VenueError { .. }), "{err:?}");
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
            response(vec![
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
        let body = body(reply.into_result());
        assert!(body.contains(r#""requested":3,"canceled":2"#), "{body}");
        assert!(body.contains(r#""oid":2,"cloid":"0xcc""#), "{body}");
        assert!(!body.contains(r#""oid":1"#), "{body}");
        assert!(!body.contains(r#""oid":3"#), "{body}");
    }

    #[test]
    fn a_cancel_the_venue_took_in_full_reports_no_failures() {
        let reply = cancel_outcome(
            response(vec![Status::Success, Status::Success]),
            vec![(Some(1), None), (Some(2), None)],
        );
        assert_eq!(
            body(reply.into_result()),
            r#"{"contract_version":0,"status":"canceled","requested":2,"canceled":2,"failed":[]}"#
        );
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

    /// The one that doubles a position if it is backwards.
    #[test]
    fn closing_a_long_sells_and_closing_a_short_buys() {
        let asset = test_asset(4);
        let long = close_intent(
            "BTC",
            "flat",
            d("1.5"),
            d("100"),
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
        let intent = close_intent("BTC", "flat", d("2"), d("100"), d("50"), &asset, a_cloid());
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
        let buy = close_intent("T", "flat", d("-1"), d("100"), d("0.6"), &asset, a_cloid());
        assert!(buy.is_buy);
        assert!(
            buy.px <= d("100.006"),
            "priced at {} against a 0.6 bp bound on a 100 mid",
            buy.px
        );

        let sell = close_intent("T", "flat", d("1"), d("100"), d("0.6"), &asset, a_cloid());
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
        let intent = close_intent("BTC", "flat", d("1"), d("100"), d("50"), &asset, a_cloid());
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
        use oppen_core::guardrail::{GuardrailEngine, SqliteGuardrailStore};
        use oppen_core::keys::KeychainKeyStore;
        use oppen_core::ledger::{EventKind, Ledger, LedgerAuditSink, NewEvent};

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
        let engine = std::sync::Arc::new(
            GuardrailEngine::new(
                std::sync::Arc::new(
                    SqliteGuardrailStore::open(dir.path().join("guardrails.db")).expect("store"),
                ),
                std::sync::Arc::new(LedgerAuditSink::new(ledger.clone())),
                std::sync::Arc::new(KeychainKeyStore::new(Network::Testnet)),
                Network::Testnet,
            )
            .expect("engine"),
        );
        let journal = std::sync::Arc::new(
            oppen_core::journal::Journal::open(dir.path().join("journal.db")).expect("journal"),
        );
        // The tempdir must outlive the gateway; leaking the handle is fine in a
        // test process that is about to exit.
        std::mem::forget(dir);
        Gateway::new(
            Network::Testnet,
            engine,
            EventViews::new(ledger),
            journal,
            std::sync::Arc::new(oppen_core::feed::FeedSession::new()),
            std::sync::Arc::new(oppen_core::alert::AlertStore::open(":memory:").expect("alerts")),
            std::sync::Arc::new(oppen_core::features::quotes::QuoteCache::new()),
        )
        .expect("gateway")
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
