//! Info-endpoint response types. Field names are the venue's; decimals
//! arrive as strings and are parsed into [`Decimal`] (`serde-with-str`).
//! Unknown fields are ignored so a venue-side addition never breaks
//! deserialization.
//!
//! Nullability here is not defensive style, it is a load-bearing property.
//! `serde` fails the **whole** response, not one row, when a field typed
//! `Decimal` arrives as `null` — so a single nulled asset would blank the
//! entire 233-asset universe. `docs/specs/fair-value.md` §14.4 correction 2
//! records which fields the venue actually nulls; every one of them is an
//! [`Option`] with a `#[serde(default)]` so an absent field degrades the
//! same way a null one does.

use std::fmt;

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

/// Seconds in one Hyperliquid funding interval. Funding is charged hourly
/// at `F₈ₕ / 8` (`docs/specs/fair-value.md` §3.1).
pub const FUNDING_INTERVAL_S: u64 = 3600;

/// Seconds remaining until the next hourly funding boundary.
///
/// `docs/specs/fair-value.md` §14.5 (`charts.md` §5.2 correction): there is
/// **no ctx field for this on either transport or either network**, and
/// `predictedFundings.HlPerp.nextFundingTime` names a boundary that has
/// already passed — measured 1,064–1,898 s in the past and identical across
/// all 233 coins, so a `now >= nextFundingTime` refresh trigger built on it
/// fires forever. Derive it from the clock instead.
///
/// Exactly on a boundary this returns [`FUNDING_INTERVAL_S`], never `0`, so
/// a countdown built on it cannot latch at zero.
pub const fn next_funding_s(unix_epoch_s: u64) -> u64 {
    FUNDING_INTERVAL_S - (unix_epoch_s % FUNDING_INTERVAL_S)
}

/// The `funding` field of [`AssetCtx`]: `g` applied to the **hour-to-date
/// running mean** premium.
///
/// `docs/specs/fair-value.md` §14.4 correction 1. This is *not* `g` applied
/// to [`AssetCtx::premium`], which is the instantaneous 5-second sample:
/// over 68 s of BTC this value spanned 9.05e-7 while `premium` spanned
/// 4.05e-4, **448× larger**. §5.3 labels the field `_1h`, which invites
/// feeding it into the §3.2 funding inverse `g⁻¹`; that is wrong by
/// construction, because `g⁻¹` is defined on an instantaneous premium and
/// this value is an hour-to-date mean of one.
///
/// The newtype exists so the substitution cannot be made by accident. Live
/// carry reads [`AssetCtx::premium`] directly; historical carry reads the
/// uncensored [`FundingHistoryRow::premium`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(transparent)]
pub struct HourToDateRate1h(Decimal);

impl HourToDateRate1h {
    /// Wrap a rate that is known to be an hour-to-date mean. Named for what
    /// the value is, so a call site holding an instantaneous premium reads
    /// as obviously wrong.
    pub const fn from_hour_to_date_1h(rate: Decimal) -> Self {
        HourToDateRate1h(rate)
    }

    /// The hourly rate the venue would charge if the hour closed now.
    ///
    /// This is a funding **payment** rate. It is never a premium, and it
    /// must never be passed to the §3.2 inverse `g⁻¹`.
    pub const fn hour_to_date_1h(self) -> Decimal {
        self.0
    }
}

impl fmt::Display for HourToDateRate1h {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Per-asset market context, aligned by index with `Meta::universe`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    /// Funding accrued over the hour so far. See [`HourToDateRate1h`]: this
    /// is not `g(premium)` and never enters a funding inverse.
    pub funding: HourToDateRate1h,
    /// Zero is the venue's marker for "this asset has no book". See
    /// [`AssetCtx::has_book`].
    pub open_interest: Decimal,
    pub prev_day_px: Decimal,
    pub day_ntl_vlm: Decimal,
    /// Instantaneous 5-second premium sample, the input `g` averages over
    /// the hour. **This, not [`AssetCtx::funding`], is what live carry
    /// reads** (§14.4 correction 1).
    ///
    /// `null` on 56 of 233 mainnet assets (measured 2026-09-03).
    #[serde(default)]
    pub premium: Option<Decimal>,
    pub oracle_px: Decimal,
    pub mark_px: Decimal,
    /// `null` on 56 of 233 mainnet assets. Read it through
    /// [`AssetCtx::mid_px_no_fallback`]; there is no substitute value.
    #[serde(default)]
    pub mid_px: Option<Decimal>,
    /// `[bid impact, ask impact]`. `null` on 56 of 233 mainnet assets, and
    /// independently null on assets that do have a mid — testnet SAGA
    /// carries `midPx` and non-zero open interest with `impactPxs: null` —
    /// so the §3.1 premium formula is unconstructible whenever this is
    /// `None`, regardless of what the other fields say.
    #[serde(default)]
    pub impact_pxs: Option<[Decimal; 2]>,
}

impl AssetCtx {
    /// Whether this asset has a book at all.
    ///
    /// `docs/specs/fair-value.md` §14.4 correction 2: the invariant is
    /// `openInterest == 0`, holding on 233/233 mainnet assets on
    /// 2026-09-03. It is **not** `isDelisted` — testnet PURR is
    /// live-but-null: `isDelisted` absent, open interest `0.0`, all three
    /// nullable ctx fields `null`. An engine gated on `isDelisted` would
    /// have tried to build `micro` and `carry` for it.
    ///
    /// 24% of the main dex and 38.6% of the full mainnet universe has no
    /// book, so this is the common case and the §4.3 slice-and-renormalize
    /// degradation path is day-one code, not an edge case.
    pub fn has_book(&self) -> bool {
        !self.open_interest.is_zero()
    }

    /// The venue's mid, or `None`. **There is no fallback.**
    ///
    /// `docs/specs/fair-value.md` §14.4 correction 3: `allMids` returns a
    /// value for all 56 nulled mainnet assets, and that value is `markPx` —
    /// a frozen last print. Measured 2026-09-03: FRIEND reads `4.72` from
    /// `allMids` against an `oraclePx` of `0.47734`, a **9.9× stale price**.
    /// Substituting it would inject a fictitious component into the §4.2
    /// combination, which §4.3 forbids outright: a missing component is
    /// dropped and `β` renormalized over the survivors, never defaulted.
    ///
    /// The name is the prohibition. Anything that reaches past this into
    /// [`crate::InfoClient::all_mids`] to fill a `None` is a defect.
    pub fn mid_px_no_fallback(&self) -> Option<Decimal> {
        self.mid_px
    }
}

/// `metaAndAssetCtxs` response: a two-element array.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MetaAndAssetCtxs(pub Meta, pub Vec<AssetCtx>);

impl MetaAndAssetCtxs {
    pub fn meta(&self) -> &Meta {
        &self.0
    }

    pub fn ctxs(&self) -> &[AssetCtx] {
        &self.1
    }

    /// Whether every universe entry has a matching ctx. [`Self::iter`] zips,
    /// so a misaligned response would silently truncate rather than
    /// misattribute; check this before trusting a scan's coverage count.
    pub fn is_aligned(&self) -> bool {
        self.0.universe.len() == self.1.len()
    }

    /// `(asset_id, info, ctx)` in universe order.
    ///
    /// The position **is** the on-chain asset id (`docs/spec.md` item 8), so
    /// the array is never compacted, sorted or filtered in place —
    /// §14.4 correction 2. Filter with [`Self::tradable`], which preserves
    /// the id it yields. Ordering is the response's own, which makes this
    /// deterministic in a way iterating [`crate::Universe`] is not.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &AssetInfo, &AssetCtx)> {
        self.0
            .universe
            .iter()
            .zip(self.1.iter())
            .enumerate()
            .map(|(index, (info, ctx))| (index as u32, info, ctx))
    }

    /// Only the assets with a book, by the [`AssetCtx::has_book`] invariant,
    /// each still carrying its true on-chain asset id.
    pub fn tradable(&self) -> impl Iterator<Item = (u32, &AssetInfo, &AssetCtx)> {
        self.iter().filter(|(_, _, ctx)| ctx.has_book())
    }
}

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

/// `bbo` WS channel payload: `{coin, time, bbo: [bid, ask]}`.
///
/// Shape verified against mainnet on 2026-09-03:
/// `{"coin":"BTC","time":1788490137025,"bbo":[{"px":"80657.0","sz":"1.53005","n":19},…]}`
/// — the same `{px, sz, n}` [`Level`] the book uses.
///
/// This is the source for the `micro` component, not `l2Book`
/// (`docs/specs/fair-value.md` §14.4 correction 4): default `l2Book` WS
/// pushes at a 5.4 s median gap, failing §5.2's own 2 s staleness threshold
/// on every sample, while `bbo` delivers at 0.10–0.13 s. It also carries a
/// `time`, which `activeAssetCtx` and `fastAssetCtxs` do not (§14.1) — that
/// timestamp is what makes a fixed-clock sampler alignable at all.
///
/// The venue only emits when the BBO changes on a block, so silence is not
/// staleness by itself and must be judged against [`Bbo::time`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Bbo {
    pub coin: String,
    /// Venue timestamp, ms.
    pub time: u64,
    /// `[bid, ask]`. A side is `None` when the book is empty on that side —
    /// `l2Book` answers `[[],[]]` for a bookless asset such as FRIEND, so an
    /// empty side is representable and must not fail the whole message the
    /// way a null ctx field would (§14.4 correction 2).
    #[serde(default)]
    pub bbo: [Option<Level>; 2],
}

impl Bbo {
    pub fn bid(&self) -> Option<&Level> {
        self.bbo[0].as_ref()
    }

    pub fn ask(&self) -> Option<&Level> {
        self.bbo[1].as_ref()
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

/// Rows returned by one `fundingHistory` request. The venue caps a page at
/// this many rows and truncates from the **newest** end with no cursor, so a
/// window wider than 500 hours silently loses its tail
/// (`docs/specs/fair-value.md` §14.4 correction 12).
pub const FUNDING_HISTORY_PAGE_LIMIT: usize = 500;

/// One `fundingHistory` row: `{coin, fundingRate, premium, time}`.
///
/// `docs/specs/fair-value.md` §14.3 calls [`Self::premium`] "the single most
/// valuable finding" of the live audit: it is the **uncensored** hour-average
/// premium published alongside the censored rate, and `g(premium)` reproduces
/// `fundingRate` to 5e-11 over 4,627 records. That collapses the §3.2
/// censored-interval machinery to a point observation for all historical work.
///
/// It matters because [`Self::funding_rate`] is mostly a constant: 77.2% of
/// prints are pinned to the `0.01%/8h` mechanism rate in aggregate (BTC 90.5%
/// per the audit, 91.7% over the 240 rows measured 2026-09-03). Anything
/// fitted on `fundingRate` is fitted on that constant, and §14.4 correction 11
/// restates §10.2's audit as "confirm the estimator never touches
/// `fundingRate` at all".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingHistoryRow {
    pub coin: String,
    /// The published rate, **censored**: pinned to the mechanism constant
    /// whenever the hour-average premium sits in the §3.2 dead zone.
    pub funding_rate: Decimal,
    /// The uncensored hour-average premium. Non-null on 4,488 rows across 33
    /// mainnet coins measured 2026-09-03, including bookless ones.
    pub premium: Decimal,
    /// Funding boundary, ms. Gaps are `{3599, 3600}` seconds with ±142 ms
    /// jitter and 2023 history contains 77 genuine 8-hour holes, so an exact
    /// 3600 s spacing assertion is not a valid health check
    /// (§14.4 correction 12).
    pub time: u64,
}

/// One page of `fundingHistory`, keeping the venue's two "no rows" answers
/// distinguishable.
///
/// `docs/specs/fair-value.md` §14.4 correction 12: a `null` body means an
/// **unlisted coin** and arrives as a deterministic HTTP 500; genuine no-data
/// is `[]` with HTTP 200. Measured 2026-09-03: `coin: "NOTACOIN"` → HTTP 500,
/// body `null`; `coin: "BTC"` with a future `startTime` → HTTP 200, body `[]`.
/// **Never map `null` to an empty page and advance the cursor** — a backfill
/// that does walks a misspelled coin backwards through all of history
/// recording nothing and reporting success.
///
/// The enum is the enforcement: there is no `Vec` to fall out of a `null`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FundingHistoryPage {
    /// HTTP 200 with an array. Empty means genuinely no funding in the
    /// window, which is a fact worth recording.
    Rows(Vec<FundingHistoryRow>),
    /// HTTP 500 with a `null` body: the coin is not listed on this network.
    UnlistedCoin,
}

impl FundingHistoryPage {
    pub fn rows(&self) -> &[FundingHistoryRow] {
        match self {
            FundingHistoryPage::Rows(rows) => rows,
            FundingHistoryPage::UnlistedCoin => &[],
        }
    }

    /// `startTime` for the next page, or `None` when the walk is finished.
    ///
    /// Pagination is **forward**: rows come oldest-first and the next
    /// `startTime` is the last row's `time` (§14.4 correction 12). That
    /// bound is **inclusive** — verified 2026-09-03, requesting
    /// `startTime = 1787626800017` returned that exact row first — so
    /// consecutive pages overlap by one row and a caller must dedupe on
    /// [`FundingHistoryRow::time`].
    ///
    /// A short page is the end of the data, so this stops rather than
    /// spinning on the same timestamp. [`Self::UnlistedCoin`] returns `None`:
    /// the cursor must not advance past a coin that does not exist.
    pub fn next_start_ms(&self) -> Option<u64> {
        match self {
            FundingHistoryPage::Rows(rows) if rows.len() >= FUNDING_HISTORY_PAGE_LIMIT => {
                rows.last().map(|row| row.time)
            }
            _ => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// A verbatim prefix of the live mainnet `metaAndAssetCtxs` response,
    /// captured 2026-09-03. It is a **prefix**, so indexes 0–3 are the real
    /// on-chain asset ids, and index 3 (MATIC) is one of the 56 assets the
    /// venue nulls. `marginTables` is emptied only to keep the fixture short;
    /// it is an ignored field.
    const MAINNET_PREFIX: &str = r#"[{"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40,"marginTableId":56},{"szDecimals":4,"name":"ETH","maxLeverage":25,"marginTableId":55},{"szDecimals":2,"name":"ATOM","maxLeverage":5,"marginTableId":5},{"szDecimals":1,"name":"MATIC","maxLeverage":20,"marginTableId":20,"isDelisted":true}],"marginTables":[],"collateralToken":0},[{"funding":"0.0000052764","openInterest":"36295.80812","prevDayPx":"77759.0","dayNtlVlm":"4490636711.4322738647","premium":"-0.0005540084","oraclePx":"80684.7","markPx":"80639.0","midPx":"80639.5","impactPxs":["80635.5","80640.0"],"dayBaseVlm":"56265.69083"},{"funding":"0.0000125","openInterest":"897134.8736000003","prevDayPx":"2405.6","dayNtlVlm":"1275317514.6673593521","premium":"-0.0001199856","oraclePx":"2500.3","markPx":"2499.93","midPx":"2499.95","impactPxs":["2499.9","2500.0"],"dayBaseVlm":"516297.5593000001"},{"funding":"-0.0000604394","openInterest":"1846225.26","prevDayPx":"1.4616","dayNtlVlm":"1148752.838546","premium":"-0.0009976721","oraclePx":"1.5035","markPx":"1.5008","midPx":"1.5006","impactPxs":["1.4996","1.502"],"dayBaseVlm":"764932.66"},{"funding":"0.0","openInterest":"0.0","prevDayPx":"0.37621","dayNtlVlm":"0.0","premium":null,"oraclePx":"0.3754","markPx":"0.37621","midPx":null,"impactPxs":null,"dayBaseVlm":"0.0"}]]"#;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("test literal parses")
    }

    /// §14.4 correction 2. The three nullable fields arrive as `null`, and a
    /// non-`Option` field would fail the whole response rather than one row.
    #[test]
    fn null_ctx_fields_do_not_fail_the_response() {
        let response: MetaAndAssetCtxs =
            serde_json::from_str(MAINNET_PREFIX).expect("nulled asset must not fail the response");
        assert!(response.is_aligned());
        assert_eq!(response.ctxs().len(), 4);

        let matic = &response.ctxs()[3];
        assert_eq!(matic.premium, None);
        assert_eq!(matic.mid_px, None);
        assert_eq!(matic.impact_pxs, None);

        let btc = &response.ctxs()[0];
        assert_eq!(btc.premium, Some(d("-0.0005540084")));
        assert_eq!(btc.mid_px_no_fallback(), Some(d("80639.5")));
        assert_eq!(btc.impact_pxs, Some([d("80635.5"), d("80640.0")]));
    }

    /// An absent field must degrade exactly like a null one: `serde(default)`
    /// is what stops a venue-side removal from blanking the universe.
    #[test]
    fn absent_nullable_fields_deserialize_as_none() {
        let json = r#"{"funding":"0.0","openInterest":"0.0","prevDayPx":"1.0","dayNtlVlm":"0.0","oraclePx":"1.0","markPx":"1.0"}"#;
        let ctx: AssetCtx = serde_json::from_str(json).expect("absent fields are None");
        assert_eq!(ctx.premium, None);
        assert_eq!(ctx.mid_px, None);
        assert_eq!(ctx.impact_pxs, None);
    }

    /// §14.4 correction 2: the invariant for "has no book" is
    /// `openInterest == 0`, and the universe is never compacted.
    #[test]
    fn tradability_is_open_interest_and_ids_survive_filtering() {
        let response: MetaAndAssetCtxs = serde_json::from_str(MAINNET_PREFIX).expect("fixture");

        let all: Vec<u32> = response.iter().map(|(id, _, _)| id).collect();
        assert_eq!(all, vec![0, 1, 2, 3], "index is the on-chain asset id");

        let tradable: Vec<(u32, &str)> = response
            .tradable()
            .map(|(id, info, _)| (id, info.name.as_str()))
            .collect();
        assert_eq!(tradable, vec![(0, "BTC"), (1, "ETH"), (2, "ATOM")]);

        assert!(response.ctxs()[0].has_book());
        assert!(!response.ctxs()[3].has_book(), "openInterest 0.0 = no book");
    }

    /// §14.4 correction 1: `funding` is typed apart from every other rate so
    /// it cannot reach `g⁻¹`, and it is 448× narrower in span than `premium`.
    #[test]
    fn hour_to_date_funding_is_a_distinct_type() {
        let response: MetaAndAssetCtxs = serde_json::from_str(MAINNET_PREFIX).expect("fixture");
        let btc = &response.ctxs()[0];

        assert_eq!(
            btc.funding,
            HourToDateRate1h::from_hour_to_date_1h(d("0.0000052764"))
        );
        assert_eq!(btc.funding.hour_to_date_1h(), d("0.0000052764"));
        // Display is preserved so the value still renders as the venue sent it.
        assert_eq!(btc.funding.to_string(), "0.0000052764");
        // The two are different objects: g(premium) is not this number.
        assert_ne!(
            btc.funding.hour_to_date_1h(),
            btc.premium.expect("BTC premium")
        );
    }

    /// §14.5: derived, because no ctx field carries it and
    /// `predictedFundings.nextFundingTime` points into the past.
    #[test]
    fn next_funding_is_derived_from_the_clock() {
        assert_eq!(next_funding_s(0), 3600, "on a boundary, never 0");
        assert_eq!(next_funding_s(1), 3599);
        assert_eq!(next_funding_s(3599), 1);
        assert_eq!(next_funding_s(3600), 3600);
        // Real captures, cross-checked against each other: the BTC bbo tick
        // at 1788490137 s sits 2937 s past the funding boundary 1788487200,
        // which is exactly the `time` of the newest live fundingHistory row.
        assert_eq!(next_funding_s(1_788_490_137), 663);
        assert_eq!(1_788_490_137 + 663, 1_788_490_800);
        assert_eq!(1_788_490_800 % FUNDING_INTERVAL_S, 0, "lands on a boundary");
        assert_eq!(1_788_487_200 % FUNDING_INTERVAL_S, 0, "a real funding row");
        for s in 0..7200u64 {
            let remaining = next_funding_s(s);
            assert!((1..=FUNDING_INTERVAL_S).contains(&remaining));
            assert_eq!((s + remaining) % FUNDING_INTERVAL_S, 0);
        }
    }

    /// §14.4 correction 4. Payload captured verbatim from mainnet 2026-09-03.
    #[test]
    fn bbo_payload_matches_the_live_shape() {
        let json = r#"{"coin":"BTC","time":1788490137025,"bbo":[{"px":"80657.0","sz":"1.53005","n":19},{"px":"80658.0","sz":"0.59555","n":7}]}"#;
        let bbo: Bbo = serde_json::from_str(json).expect("live bbo payload");
        assert_eq!(bbo.coin, "BTC");
        assert_eq!(bbo.time, 1_788_490_137_025);
        assert_eq!(bbo.bid().expect("bid").px, d("80657.0"));
        assert_eq!(bbo.ask().expect("ask").sz, d("0.59555"));
        assert_eq!(bbo.bid().expect("bid").n, 19);
        assert!(bbo.bid().expect("bid").px < bbo.ask().expect("ask").px);
    }

    /// An empty book side must not fail the message the way a null ctx field
    /// would; `l2Book` answers `[[],[]]` for a bookless asset.
    #[test]
    fn bbo_tolerates_an_empty_side() {
        let json = r#"{"coin":"FRIEND","time":1788490146994,"bbo":[null,null]}"#;
        let bbo: Bbo = serde_json::from_str(json).expect("one-sided book");
        assert!(bbo.bid().is_none() && bbo.ask().is_none());

        let json =
            r#"{"coin":"FRIEND","time":1788490146994,"bbo":[{"px":"1.0","sz":"2.0","n":1},null]}"#;
        let bbo: Bbo = serde_json::from_str(json).expect("bid only");
        assert!(bbo.bid().is_some() && bbo.ask().is_none());
    }

    /// §14.3: `fundingHistory` publishes the uncensored premium next to the
    /// censored rate. Rows captured verbatim from mainnet 2026-09-03.
    #[test]
    fn funding_history_carries_the_uncensored_premium() {
        let json = r#"[{"coin":"BTC","fundingRate":"0.0000125","premium":"0.0001446433","time":1787626800017},{"coin":"BTC","fundingRate":"0.0000043313","premium":"-0.0004653492","time":1788487200002}]"#;
        let rows: Vec<FundingHistoryRow> = serde_json::from_str(json).expect("live rows");
        assert_eq!(rows.len(), 2);
        // The first row is pinned to the mechanism constant and carries no
        // information; its premium does.
        assert_eq!(rows[0].funding_rate, d("0.0000125"));
        assert_eq!(rows[0].premium, d("0.0001446433"));
        assert_eq!(rows[1].premium, d("-0.0004653492"));
        assert_eq!(rows[1].time - rows[0].time, 860_399_985);
    }

    /// §14.4 correction 12: `null` is an unlisted coin, `[]` is genuine
    /// no-data, and only the first must stop the walk.
    #[test]
    fn unlisted_coin_never_advances_the_cursor() {
        assert_eq!(FundingHistoryPage::UnlistedCoin.next_start_ms(), None);
        assert!(FundingHistoryPage::UnlistedCoin.rows().is_empty());

        let empty = FundingHistoryPage::Rows(Vec::new());
        assert_eq!(empty.next_start_ms(), None, "a short page is the end");
        assert!(empty.rows().is_empty());
        assert_ne!(empty, FundingHistoryPage::UnlistedCoin);
    }

    /// Forward pagination: a full page hands back its last row's timestamp,
    /// a short page stops, and the inclusive bound means pages overlap by one.
    #[test]
    fn full_pages_paginate_forward_by_the_last_row_time() {
        let row = |time| FundingHistoryRow {
            coin: "BTC".into(),
            funding_rate: Decimal::ZERO,
            premium: Decimal::ZERO,
            time,
        };
        let full: Vec<FundingHistoryRow> = (0..FUNDING_HISTORY_PAGE_LIMIT as u64)
            .map(|i| row(1_000 + i * 3_600_000))
            .collect();
        let last = full[FUNDING_HISTORY_PAGE_LIMIT - 1].time;
        let page = FundingHistoryPage::Rows(full);
        assert_eq!(page.next_start_ms(), Some(last));
        assert_eq!(page.rows().len(), FUNDING_HISTORY_PAGE_LIMIT);

        let short = FundingHistoryPage::Rows(vec![row(1), row(2)]);
        assert_eq!(short.next_start_ms(), None);
    }
}
