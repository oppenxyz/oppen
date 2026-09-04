//! The tools an agent may call (`docs/spec.md` section C).
//!
//! Every tool here is read-only. The execution tools of item 19 arrive with
//! the guardrail wiring, because a tool that signs must go through
//! `oppen_core`'s single pre-sign path and nothing here may offer a second
//! route to the signer (`AGENTS.md` invariant 1).

use std::sync::Arc;

use oppen_hl::{InfoClient, Network, Universe, meta::MIN_NOTIONAL_USD};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::tool::Parameters,
    model::{CallToolResult, Content},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

/// Shared, cheap to clone: `rmcp` builds one service per session.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<GatewayInner>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

struct GatewayInner {
    network: Network,
    info: InfoClient,
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
    pub fn new(network: Network) -> Result<Self, oppen_hl::Error> {
        Ok(Self {
            inner: Arc::new(GatewayInner {
                network,
                info: InfoClient::new(network)?,
            }),
            tool_router: Self::tool_router(),
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
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
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
        rmcp::model::ServerInfo {
            // Without this the handshake advertises no capabilities and an
            // agent never learns the tools exist. `#[tool_handler]` generates
            // the routes; it does not announce them.
            capabilities: rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: rmcp::model::Implementation {
                name: "oppen".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            instructions: Some(
                "oppen — a local-first Hyperliquid terminal. You are one agent among several; \
                 the human supervises. Every action you take is guardrail-checked before it is \
                 signed and is recorded in an append-only ledger. Read AGENTS.md in the oppen \
                 repository before trading."
                    .into(),
            ),
            ..Default::default()
        }
    }
}
