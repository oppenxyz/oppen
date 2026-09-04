//! The tools an agent may call (`docs/spec.md` section C).
//!
//! Every tool here is read-only. The execution tools of item 19 arrive with
//! the guardrail wiring, because a tool that signs must go through
//! `oppen_core`'s single pre-sign path and nothing here may offer a second
//! route to the signer (`AGENTS.md` invariant 1).

use std::sync::Arc;

use oppen_core::state::{AccountState, VenueReadings, assemble};
use oppen_hl::{Address, InfoClient, Network, Universe, meta::MIN_NOTIONAL_USD};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
};
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
    pub fn new(network: Network, account: Address) -> Result<Self, oppen_hl::Error> {
        Ok(Self {
            inner: Arc::new(GatewayInner {
                network,
                info: InfoClient::new(network)?,
                account,
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

    /// `get_state` — the agent's eyes (`docs/spec.md` item 16).
    #[tool(
        description = "The account right now: equity, margin, open positions with distance to \
                       liquidation, resting orders, and feed freshness. Read this before acting. \
                       If `feed` is not `live`, the data is stale and execution will fail closed."
    )]
    async fn get_state(&self) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let account = inner.account;

        // Four reads, not one: Hyperliquid publishes no single endpoint that
        // answers this, and spot is load-bearing because margin is unified.
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

        let state: AccountState = assemble(
            inner.network,
            account,
            now_ms(),
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
        );

        let json = serde_json::to_string(&state)
            .map_err(|e| ErrorData::internal_error(format!("serialise: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }
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
