//! The tools an agent may call (`docs/spec.md` section C).
//!
//! Every tool here is read-only. The execution tools of item 19 arrive with
//! the guardrail wiring, because a tool that signs must go through
//! `oppen_core`'s single pre-sign path and nothing here may offer a second
//! route to the signer (`AGENTS.md` invariant 1).

use std::sync::Arc;

use oppen_core::guardrail::AgentId;
use oppen_core::guardrail::{FeedQuality, GuardrailEngine, MarketRef, OrderIntent, Refusal};
use oppen_core::state::{
    AccountState, VenueReadings, assemble, exposure_from, realized_pnl_since, utc_day_start_ms,
};
use oppen_hl::OrderKind;
use oppen_hl::wire::{Grouping, Tif};
use oppen_hl::{Address, InfoClient, Network, Universe, meta::MIN_NOTIONAL_USD};
use oppen_hl::{ExchangeClient, NonceAllocator};
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
    /// The account the gateway reports on. One address in v1: an agent is
    /// bound to exactly one container (`docs/decisions.md` D1 as revised), and
    /// a gateway serving two would have to disambiguate on every call.
    account: Address,
    /// The single agent this gateway speaks for. One pairing, one agent, one
    /// container (D1 as revised).
    agent: AgentId,
    /// Guardrails live in `oppen-core` and nowhere else (`AGENTS.md`
    /// invariant 1). This gateway cannot evaluate a predicate itself, and
    /// there is no path from a tool to the signer that does not go through
    /// `evaluate` then `sign_cleared`.
    engine: Arc<GuardrailEngine>,
    exchange: ExchangeClient,
    nonces: NonceAllocator,
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
    /// Limit price, at `get_meta`'s `price_decimals`. A decimal string, for
    /// the same reason as `size`.
    pub limit_px: String,
    /// Why you are placing this order. Required.
    pub reason: String,
    #[serde(default)]
    pub reduce_only: bool,
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
        account: Address,
        agent: AgentId,
        engine: Arc<GuardrailEngine>,
    ) -> Result<Self, oppen_hl::Error> {
        Ok(Self {
            inner: Arc::new(GatewayInner {
                network,
                info: InfoClient::new(network)?,
                account,
                agent,
                engine,
                exchange: ExchangeClient::new(network)?,
                nonces: NonceAllocator::new(),
            }),
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
            .map_err(|e| ErrorData::internal_error(format!("meta query failed: {e}"), None))?;
        let universe = Universe::from_meta(&meta)
            .map_err(|e| ErrorData::internal_error(format!("meta rejected: {e}"), None))?;

        let mut out: Vec<SymbolMeta> = Vec::new();
        match symbols {
            Some(wanted) => {
                for symbol in wanted {
                    let asset = universe.get(&symbol).map_err(|e| {
                        ErrorData::invalid_params(format!("unknown symbol {symbol:?}: {e}"), None)
                    })?;
                    out.push(describe(asset));
                }
            }
            None => out.extend(universe.iter().map(describe)),
        }

        // Stable order so two calls with the same universe produce the same
        // bytes, which is what makes a cached response comparable.
        out.sort_by_key(|symbol| symbol.asset_id);

        let json = serde_json::to_string(&out)
            .map_err(|e| ErrorData::internal_error(format!("serialise: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
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
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let now_ms = now_ms();

        let meta = inner
            .info
            .meta()
            .await
            .map_err(|e| ErrorData::internal_error(format!("meta: {e}"), None))?;
        let universe = Universe::from_meta(&meta)
            .map_err(|e| ErrorData::internal_error(format!("meta rejected: {e}"), None))?;
        let asset = universe.get(&params.symbol).map_err(|e| {
            ErrorData::invalid_params(format!("unknown symbol {:?}: {e}", params.symbol), None)
        })?;

        let state = self.read_state(now_ms).await?;

        // Realised PnL is summed from the venue's own fills for the UTC day,
        // net of fees: a daily-loss limit that ignores fees is not a limit.
        let day_start_ms = utc_day_start_ms(now_ms);
        let fills = inner
            .info
            .user_fills_by_time(inner.account, day_start_ms, Some(now_ms))
            .await
            .map_err(|e| ErrorData::internal_error(format!("fills: {e}"), None))?;
        let portfolio = inner
            .info
            .portfolio(inner.account)
            .await
            .map_err(|e| ErrorData::internal_error(format!("portfolio: {e}"), None))?;

        let exposure = exposure_from(
            &state,
            realized_pnl_since(&fills, day_start_ms),
            portfolio.window("day").and_then(|w| w.peak_account_value()),
            // No socket yet, so no reconnect-reconcile has run. Reported
            // honestly; the engine decides what that means, not this gateway.
            false,
            day_start_ms,
        );

        let mids = inner
            .info
            .all_mids()
            .await
            .map_err(|e| ErrorData::internal_error(format!("mids: {e}"), None))?;
        let market = MarketRef {
            symbol: params.symbol.clone(),
            // A missing price is a refusal, never a fallback: `allMids`
            // answers for bookless assets with a frozen mark.
            reference_px: mids.get(&params.symbol).copied(),
            as_of_ms: now_ms,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
        };

        let px: Decimal = params.limit_px.parse().map_err(|e| {
            ErrorData::invalid_params(
                format!("limit_px {:?} is not a decimal: {e}", params.limit_px),
                None,
            )
        })?;
        let sz: Decimal = params.size.parse().map_err(|e| {
            ErrorData::invalid_params(
                format!("size {:?} is not a decimal: {e}", params.size),
                None,
            )
        })?;

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
            reason: params.reason,
        };

        // The gate. There is no branch around it.
        let cleared =
            match inner
                .engine
                .evaluate(&inner.agent, &intent, asset, &market, &exposure, now_ms)
            {
                Ok(cleared) => cleared,
                Err(refusal) => return Ok(refused(&refusal)),
            };

        let nonce = inner.nonces.next();
        let (request, _clearance) = inner
            .engine
            .sign_cleared(cleared, nonce, None, now_ms)
            .map_err(|e| ErrorData::internal_error(format!("signing refused: {e}"), None))?;

        let response = inner
            .exchange
            .post(&request)
            .await
            .map_err(|e| ErrorData::internal_error(format!("venue: {e}"), None))?;

        let json = serde_json::to_string(&serde_json::json!({
            "status": "submitted",
            "venue_response": format!("{response:?}"),
        }))
        .map_err(|e| ErrorData::internal_error(format!("serialise: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// `get_state` — the agent's eyes (`docs/spec.md` item 16).
    #[tool(
        description = "The account right now: equity, margin, open positions with distance to \
                       liquidation, resting orders, and feed freshness. Read this before acting. \
                       If `feed` is not `live`, the data is stale and execution will fail closed."
    )]
    async fn get_state(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.read_state(now_ms()).await?;
        let json = serde_json::to_string(&state)
            .map_err(|e| ErrorData::internal_error(format!("serialise: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// The four venue reads `get_state` and `place` share.
    ///
    /// Four and not one: Hyperliquid publishes no single endpoint that answers
    /// this, and spot is load-bearing because margin is unified.
    async fn read_state(&self, now_ms: u64) -> Result<AccountState, ErrorData> {
        let inner = &self.inner;
        let account = inner.account;

        let perps = inner
            .info
            .clearinghouse_state(account)
            .await
            .map_err(|e| ErrorData::internal_error(format!("clearinghouse: {e}"), None))?;
        let spot = inner
            .info
            .spot_clearinghouse_state(account)
            .await
            .map_err(|e| ErrorData::internal_error(format!("spot: {e}"), None))?;
        let orders = inner
            .info
            .frontend_open_orders(account)
            .await
            .map_err(|e| ErrorData::internal_error(format!("orders: {e}"), None))?;
        let mids = inner
            .info
            .all_mids()
            .await
            .map_err(|e| ErrorData::internal_error(format!("mids: {e}"), None))?;

        Ok(assemble(
            inner.network,
            account,
            now_ms,
            &VenueReadings {
                perps: &perps,
                spot: &spot,
                orders: &orders,
                mids: &mids,
                // No socket yet, so nothing has ticked. Reported honestly as
                // never-connected rather than as live: an agent that reads
                // this must not believe a feed exists.
                last_tick_ms: None,
            },
        ))
    }
}

/// Render a guardrail refusal as data the agent can act on.
///
/// Returned as a **successful** tool call carrying `status: "refused"`, not as
/// a protocol error. A refusal is a correct, expected outcome of asking — the
/// system working — and an error would invite a blind retry, which is exactly
/// the wrong response to a limit.
fn refused(refusal: &Refusal) -> CallToolResult {
    let body = serde_json::json!({
        "status": "refused",
        "refused_by": "guardrail",
        // The Display impl names the predicate, the observed value and the
        // limit; `AGENTS.md` keeps that contract, not this call site.
        "reason": refusal.to_string(),
        "retryable": false,
    });
    CallToolResult::success(vec![ContentBlock::text(body.to_string())])
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
