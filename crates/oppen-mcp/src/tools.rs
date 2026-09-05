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

use oppen_core::guardrail::AgentId;
use oppen_core::guardrail::{Cleared, FeedQuality, GuardrailEngine, MarketRef, OrderIntent};
use oppen_core::state::{
    AccountState, VenueReadings, assemble, exposure_from, realized_pnl_since, utc_day_start_ms,
};
use oppen_hl::info::OrderRef;
use oppen_hl::types::OrderStatusResponse;
use oppen_hl::wire::{CancelWire, Cloid, Grouping, Tif};
use oppen_hl::{Address, InfoClient, Network, Universe, meta::MIN_NOTIONAL_USD};
use oppen_hl::{ExchangeClient, ExchangeResponse, NonceAllocator, OrderKind, Status};

use crate::outcome::{self, CancelFailure, Reply, ToolError};
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
    /// Optional. One is minted when absent, and either way it comes back on
    /// the result — item 19's reconcile-by-cloid needs one to exist.
    #[serde(default)]
    pub cloid: Option<String>,
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
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let now_ms = now_ms();

        let meta = inner
            .info
            .meta()
            .await
            .map_err(|e| ToolError::unavailable("meta", e))?;
        let universe =
            Universe::from_meta(&meta).map_err(|e| ToolError::unavailable("universe", e))?;
        let asset = universe
            .get(&params.symbol)
            .map_err(|e| ToolError::invalid("symbol", e))?;

        let state = self.read_state(now_ms).await?;

        // Realised PnL is summed from the venue's own fills for the UTC day,
        // net of fees: a daily-loss limit that ignores fees is not a limit.
        let day_start_ms = utc_day_start_ms(now_ms);
        let fills = inner
            .info
            .user_fills_by_time(inner.account, day_start_ms, Some(now_ms))
            .await
            .map_err(|e| ToolError::unavailable("fills", e))?;
        let portfolio = inner
            .info
            .portfolio(inner.account)
            .await
            .map_err(|e| ToolError::unavailable("portfolio", e))?;

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
            .map_err(|e| ToolError::unavailable("mids", e))?;
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

        let px: Decimal = params
            .limit_px
            .parse()
            .map_err(|e| ToolError::invalid("limit_px", e))?;
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

        let intent = OrderIntent {
            symbol: params.symbol.clone(),
            is_buy: params.is_buy,
            px,
            sz,
            kind: OrderKind::Limit { tif: Tif::Gtc },
            reduce_only: params.reduce_only,
            cloid: Some(cloid.clone()),
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
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let now_ms = now_ms();

        // Which resting order this names, and on which asset. The asset id is
        // part of both cancel wires, so an oid alone is not enough and the
        // open-order list is the only place it can come from.
        let orders = inner
            .info
            .frontend_open_orders(inner.account)
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
            &inner.agent,
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
    ) -> Result<CallToolResult, ErrorData> {
        let inner = &self.inner;
        let now_ms = now_ms();

        let orders = inner
            .info
            .frontend_open_orders(inner.account)
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
            .clear_cancel(&inner.agent, wires, &params.reason, now_ms)
        {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal).into_result()),
        };

        let response = self.submit(cleared, None, now_ms).await?;
        Ok(cancel_outcome(response, named).into_result())
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
    ) -> Result<CallToolResult, ErrorData> {
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
            .order_status(self.inner.account, reference)
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
    async fn get_state(&self) -> Result<CallToolResult, ErrorData> {
        let state = self.read_state(now_ms()).await?;
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
    async fn read_state(&self, now_ms: u64) -> Result<AccountState, ErrorData> {
        let inner = &self.inner;
        let account = inner.account;

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
        let mids = inner
            .info
            .all_mids()
            .await
            .map_err(|e| ToolError::unavailable("mids", e))?;

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

    /// `AGENTS.md` invariant 1, for the crate that now has three tools that
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
