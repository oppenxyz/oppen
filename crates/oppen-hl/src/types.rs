//! Info-endpoint response types. Field names are the venue's; decimals
//! arrive as strings and are parsed into [`Decimal`] (`serde-with-str`).
//! Unknown fields are ignored so a venue-side addition never breaks
//! deserialization.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::wire::Cloid;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetInfo {
    pub name: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    #[serde(default)]
    pub margin_table_id: u32,
    #[serde(default)]
    pub is_delisted: bool,
    #[serde(default)]
    pub only_isolated: bool,
}

/// `meta` response. `universe[i]` has asset id `i` on the first perp dex.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub universe: Vec<AssetInfo>,
}

/// Per-asset market context, aligned by index with `Meta::universe`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    /// Current hourly funding rate as a fraction.
    pub funding: Decimal,
    pub open_interest: Decimal,
    pub prev_day_px: Decimal,
    pub day_ntl_vlm: Decimal,
    pub premium: Option<Decimal>,
    pub oracle_px: Decimal,
    pub mark_px: Decimal,
    pub mid_px: Option<Decimal>,
    /// `[bid impact, ask impact]` when present.
    pub impact_pxs: Option<[Decimal; 2]>,
}

/// `metaAndAssetCtxs` response: a two-element array.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MetaAndAssetCtxs(pub Meta, pub Vec<AssetCtx>);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Level {
    pub px: Decimal,
    pub sz: Decimal,
    /// Number of orders at the level.
    pub n: u32,
}

/// `l2Book` response. `levels[0]` bids, `levels[1]` asks, best first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct L2Book {
    pub coin: String,
    pub time: u64,
    pub levels: [Vec<Level>; 2],
}

impl L2Book {
    pub fn bids(&self) -> &[Level] {
        &self.levels[0]
    }

    pub fn asks(&self) -> &[Level] {
        &self.levels[1]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Candle {
    /// Open time, ms.
    pub t: u64,
    /// Close time, ms.
    #[serde(rename = "T")]
    pub t_close: u64,
    pub s: String,
    pub i: String,
    pub o: Decimal,
    pub c: Decimal,
    pub h: Decimal,
    pub l: Decimal,
    pub v: Decimal,
    pub n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Leverage {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: u32,
    pub raw_usd: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CumFunding {
    pub all_time: Decimal,
    pub since_open: Decimal,
    pub since_change: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub coin: String,
    /// Signed size: negative is short.
    pub szi: Decimal,
    pub entry_px: Option<Decimal>,
    pub position_value: Decimal,
    pub unrealized_pnl: Decimal,
    pub return_on_equity: Decimal,
    pub liquidation_px: Option<Decimal>,
    pub margin_used: Decimal,
    pub max_leverage: u32,
    pub leverage: Leverage,
    pub cum_funding: CumFunding,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetPosition {
    #[serde(rename = "type")]
    pub kind: String,
    pub position: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    pub account_value: Decimal,
    pub total_ntl_pos: Decimal,
    pub total_raw_usd: Decimal,
    pub total_margin_used: Decimal,
}

/// `clearinghouseState` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    pub margin_summary: MarginSummary,
    pub cross_margin_summary: MarginSummary,
    pub cross_maintenance_margin_used: Decimal,
    pub withdrawable: Decimal,
    pub asset_positions: Vec<AssetPosition>,
    pub time: u64,
}

/// `A` ask (sell), `B` bid (buy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Side {
    A,
    B,
}

impl Side {
    pub fn is_buy(self) -> bool {
        matches!(self, Side::B)
    }
}

/// `frontendOpenOrders` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    pub coin: String,
    pub side: Side,
    pub limit_px: Decimal,
    pub sz: Decimal,
    pub orig_sz: Decimal,
    pub oid: u64,
    pub timestamp: u64,
    pub order_type: String,
    pub reduce_only: bool,
    pub is_trigger: bool,
    pub trigger_px: Option<Decimal>,
    pub trigger_condition: Option<String>,
    pub is_position_tpsl: bool,
    pub cloid: Option<Cloid>,
}

/// `userFills` / `userFillsByTime` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub coin: String,
    pub px: Decimal,
    pub sz: Decimal,
    pub side: Side,
    pub time: u64,
    pub start_position: Decimal,
    pub dir: String,
    pub closed_pnl: Decimal,
    pub hash: String,
    pub oid: u64,
    pub crossed: bool,
    pub fee: Decimal,
    pub fee_token: String,
    pub builder_fee: Option<Decimal>,
    pub tid: u64,
    pub cloid: Option<Cloid>,
}

/// `orderStatus` info response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum OrderStatusResponse {
    Order { order: OrderStatusEntry },
    UnknownOid,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusEntry {
    pub order: OrderStatusOrder,
    /// `open`, `filled`, `canceled`, `triggered`, `rejected`, `marginCanceled`, ...
    pub status: String,
    pub status_timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusOrder {
    pub coin: String,
    pub side: Side,
    pub limit_px: Decimal,
    pub sz: Decimal,
    pub oid: u64,
    pub timestamp: u64,
    pub orig_sz: Decimal,
    pub cloid: Option<Cloid>,
}

/// `userRateLimit` response (`docs/spec.md` item 10).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRateLimit {
    pub cum_vlm: Decimal,
    pub n_requests_used: u64,
    pub n_requests_cap: u64,
    pub n_requests_surplus: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PredictedFunding {
    pub funding_rate: Decimal,
    pub next_funding_time: u64,
    pub funding_interval_hours: Option<u32>,
}

/// One `predictedFundings` row: `[coin, [[venue, funding], ...]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PredictedFundings(pub String, pub Vec<(String, Option<PredictedFunding>)>);

impl PredictedFundings {
    pub fn coin(&self) -> &str {
        &self.0
    }

    /// Hyperliquid's own prediction (`HlPerp`).
    pub fn hyperliquid(&self) -> Option<&PredictedFunding> {
        self.1
            .iter()
            .find(|(venue, _)| venue == "HlPerp")
            .and_then(|(_, f)| f.as_ref())
    }
}

/// `subAccounts` entry. Shape taken from the docs; the live probe address
/// had none, so P2's first sub-account round trip verifies it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubAccount {
    pub name: String,
    pub sub_account_user: crate::Address,
    pub master: crate::Address,
    pub clearinghouse_state: ClearinghouseState,
}
