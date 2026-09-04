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
use serde::{Deserialize, Deserializer, Serialize};

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

/// `meta` response. `universe[i]` has asset id `i` on the validator-operated
/// perp dex; see [`PerpDex`] for what the index means anywhere else.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub universe: Vec<AssetInfo>,
}

/// A coin name that is **not** a HIP-3 builder-deployed asset.
///
/// `docs/specs/fair-value.md` §14.4 correction 15 fences v1 to the
/// validator-operated dex. A `<dex>:<coin>` asset is a different market with
/// a different funding mechanism: the premium is taken on the **midpoint**
/// rather than §3.1's impact-price difference (`xyz:EUR` reads 0.00039570 one
/// way and 0.00020645 the other), the clamp is 3e-4 rather than §3.2's 5e-4
/// and is published nowhere, the per-asset multipliers are deployer-set,
/// mutable, undated and unversioned, and the interest rate `i` may be
/// negative. That is 38.6% of the mainnet universe across 10 live dexes, and
/// every one of them answers the same endpoints with the same JSON shapes,
/// so nothing about a HIP-3 response looks wrong on arrival.
///
/// The newtype is the fence. It is required to call
/// [`crate::InfoClient::funding_history`] and it is the type of
/// [`FundingHistoryRow::coin`], so §14.3's "`g(premium)` reproduces
/// `fundingRate` to 5e-11" — which is a fact about the validator dex only —
/// cannot be asserted over rows that came from somewhere else. Measured
/// 2026-09-04 against the same reconstruction: BTC 0/200 mismatches,
/// `xyz:TSLA` **200/200** with a worst error of 9.35e-5 (9.3 bp per hour),
/// `hyna:BTC` 53/200 with a worst error of 2.50e-5.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValidatorDexCoin(String);

/// The v1 scope fence (`docs/specs/fair-value.md` §14.4 correction 15).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error(
        "{0:?} is a HIP-3 builder-deployed asset; v1 covers the validator-operated dex only (docs/specs/fair-value.md §14.4 correction 15)"
    )]
    OutOfScopeDex(String),
}

impl ValidatorDexCoin {
    /// Accept a validator-dex coin, refuse a `<dex>:<coin>` one.
    ///
    /// `docs/hl-signing.md` §6: "builder-deployed perps always have name in
    /// the format `{dex}:{coin}`", so the separator is the whole test.
    pub fn new(coin: impl Into<String>) -> Result<Self, ScopeError> {
        let coin = coin.into();
        if coin.contains(':') {
            return Err(ScopeError::OutOfScopeDex(coin));
        }
        Ok(ValidatorDexCoin(coin))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ValidatorDexCoin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for ValidatorDexCoin {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// A HIP-3 row must not deserialize into a type whose documented invariants
/// are validator-dex facts, so the check runs on the wire and not afterwards.
impl<'de> Deserialize<'de> for ValidatorDexCoin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let coin = String::deserialize(deserializer)?;
        ValidatorDexCoin::new(coin).map_err(serde::de::Error::custom)
    }
}

/// Which perp dex a `metaAndAssetCtxs` response describes.
///
/// The array position in `meta.universe` is only the on-chain asset id on the
/// validator dex. `docs/hl-signing.md` §6: a builder-deployed perp's id is
/// `100000 + perp_dex_index * 10000 + index_in_meta` — `test:ABC` on testnet
/// has `perp_dex_index = 1`, `index_in_meta = 0`, `asset = 110000`.
///
/// `{"type":"metaAndAssetCtxs","dex":"xyz"}` returns the byte-identical shape
/// the validator dex returns, so a HIP-3 response deserializes into
/// [`MetaAndAssetCtxs`] without complaint and its positions 0, 1, 2 are on
/// chain BTC, ETH and ATOM. Naming the dex is therefore required to get an id
/// at all: see [`MetaAndAssetCtxs::iter_on`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PerpDex {
    /// The validator-operated dex, where the asset id is the array position.
    Validator,
    /// A HIP-3 builder-deployed dex, by its `perpDexs` index.
    Builder(u32),
}

impl PerpDex {
    /// The on-chain asset id of the entry at `index_in_meta`
    /// (`docs/hl-signing.md` §6).
    ///
    /// Saturating, because an out-of-range dex index is a caller bug and an
    /// order path must never panic on one.
    pub const fn asset_id(self, index_in_meta: u32) -> u32 {
        match self {
            PerpDex::Validator => index_in_meta,
            PerpDex::Builder(dex_index) => 100_000u32
                .saturating_add(dex_index.saturating_mul(10_000))
                .saturating_add(index_in_meta),
        }
    }
}

/// Seconds in one Hyperliquid funding interval. Funding is charged hourly
/// at `F₈ₕ / 8` (`docs/specs/fair-value.md` §3.1).
pub const FUNDING_INTERVAL_S: u64 = 3600;

/// Seconds remaining until the next hourly funding boundary, from a
/// **millisecond** epoch — the unit every other timestamp in this module
/// carries ([`Bbo::time`], [`L2Book::time`], [`FundingHistoryRow::time`],
/// [`Candle::t`], [`Fill::time`], [`ClearinghouseState::time`],
/// [`OpenOrder::timestamp`]).
///
/// It takes ms rather than seconds because a seconds-taking version fails
/// silently on the only values a caller has at hand: `1_788_490_137_025` ms
/// read as seconds answers 575 where the true answer is 663.
///
/// `docs/specs/fair-value.md` §14.5 (`charts.md` §5.2 correction): there is
/// **no ctx field for this on either transport or either network**, and
/// `predictedFundings.HlPerp.nextFundingTime` names a boundary that has
/// already passed — measured 1,064–1,898 s in the past and identical across
/// all 233 coins, so a `now >= nextFundingTime` refresh trigger built on it
/// fires forever. Derive it from the clock instead. See [`StaleBoundaryMs`].
///
/// Exactly on a boundary this returns [`FUNDING_INTERVAL_S`], never `0`, so
/// a countdown built on it cannot latch at zero.
pub const fn next_funding_s_from_ms(unix_epoch_ms: u64) -> u64 {
    FUNDING_INTERVAL_S - ((unix_epoch_ms / 1_000) % FUNDING_INTERVAL_S)
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
    /// Whether the venue has nulled this asset's book-derived fields.
    ///
    /// **This is not a trading permission and not a component gate.** It is
    /// the one claim `docs/specs/fair-value.md` §14.4 correction 2 actually
    /// makes: `openInterest == 0` identifies the nulled set, holding on
    /// 233/233 mainnet assets on 2026-09-03. It is **not** `isDelisted` —
    /// testnet PURR is live-but-null: `isDelisted` absent, open interest
    /// `0.0`, all three nullable ctx fields `null`.
    ///
    /// Two reasons not to gate on it. Open interest measures **positions**,
    /// not resting orders, so a perp listed today with a live book and no
    /// fills yet reads `false` here. And the converse fails too: testnet SAGA
    /// carries 1,146,084 open interest and a `midPx` of 0.01513 with `premium`
    /// and `impactPxs` both `null`, so it reads `true` here while `carry` is
    /// unconstructible. Ask [`AssetCtx::can_build_micro`] and
    /// [`AssetCtx::can_build_carry`], which are the two questions §4.3's
    /// slice-and-renormalize path actually asks.
    ///
    /// 24% of the main dex and 38.6% of the full mainnet universe has no
    /// book, so degradation is the common case and §4.3 is day-one code.
    pub fn has_book(&self) -> bool {
        !self.open_interest.is_zero()
    }

    /// Whether the `micro` component of §4.2 can be built for this asset.
    ///
    /// §4.3: a missing component is dropped and `β` renormalized over the
    /// survivors, never defaulted — so this is the question the sampler asks,
    /// per asset, per sample.
    pub fn can_build_micro(&self) -> bool {
        self.mid_px.is_some()
    }

    /// Whether the `carry` component of §4.2 can be built for this asset.
    ///
    /// Needs both legs: the instantaneous [`AssetCtx::premium`] that §14.4
    /// correction 1 says live carry reads, and the [`AssetCtx::impact_pxs`]
    /// the §3.1 premium is defined on. The two are **not** co-null — testnet
    /// SAGA has open interest, volume and a mid with `impactPxs: null` — so
    /// neither [`AssetCtx::has_book`] nor a mid answers this.
    pub fn can_build_carry(&self) -> bool {
        self.premium.is_some() && self.impact_pxs.is_some()
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
///
/// The response does **not** say which dex it describes.
/// `{"type":"metaAndAssetCtxs","dex":"xyz"}` returns the same two-element
/// shape with the same keys, so this type deserializes a HIP-3 universe
/// exactly as happily as the validator one. That is why the asset id is not
/// available from a position alone — see [`Self::iter_on`] and [`PerpDex`].
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

    /// `(info, ctx)` pairs in universe order, **without** an asset id.
    ///
    /// The array is never compacted, sorted or filtered in place (§14.4
    /// correction 2) and the ordering is the response's own, which makes this
    /// deterministic in a way iterating [`crate::Universe`] is not. Getting
    /// an id needs [`Self::iter_on`], because a position is only an id once
    /// the dex is known.
    pub fn iter(&self) -> impl Iterator<Item = (&AssetInfo, &AssetCtx)> {
        self.0.universe.iter().zip(self.1.iter())
    }

    /// `(asset_id, info, ctx)` in universe order, for a response known to
    /// come from `dex`.
    ///
    /// The id is [`PerpDex::asset_id`] of the array position, which is the
    /// position itself only on [`PerpDex::Validator`] (`docs/spec.md` item 8).
    /// On a HIP-3 dex the same positions 0, 1, 2 are asset ids 110000, 110001
    /// and 110002 while the bare positions name BTC, ETH and ATOM on the
    /// validator dex — an order signed against the wrong one reaches a
    /// different instrument, so the dex is an argument rather than an
    /// assumption. `docs/specs/fair-value.md` §14.4 correction 15 fences v1
    /// to [`PerpDex::Validator`]; the parameter exists so that fence is
    /// visible at every call site instead of implied by a doc comment.
    pub fn iter_on(&self, dex: PerpDex) -> impl Iterator<Item = (u32, &AssetInfo, &AssetCtx)> {
        self.iter()
            .enumerate()
            .map(move |(index, (info, ctx))| (dex.asset_id(index as u32), info, ctx))
    }

    /// The assets whose book-derived fields the venue has not nulled, by
    /// [`AssetCtx::has_book`].
    ///
    /// Named for what it measures. It is **not** a tradability filter and not
    /// a component gate — see [`AssetCtx::can_build_micro`] and
    /// [`AssetCtx::can_build_carry`].
    pub fn with_book(&self) -> impl Iterator<Item = (&AssetInfo, &AssetCtx)> {
        self.iter().filter(|(_, ctx)| ctx.has_book())
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
    /// `[bid, ask]`, normalised from whatever the venue sends.
    ///
    /// The null-side shape is **unobserved**, not measured: subscribing `bbo`
    /// and `l2Book` for MATIC, FRIEND, RNDR, FTM, MKR, HPOS and BTC on the
    /// mainnet socket for 75 s produced `l2Book` frames with `levels: [[],[]]`
    /// for every bookless coin and **zero `bbo` frames** for any of them; only
    /// BTC emitted, always two-sided. `[Option<Level>; 2]` was a defensive
    /// guess at a shape nobody has seen.
    ///
    /// A fixed-length array is the least forgiving representation of an
    /// unknown shape, and §14.4 correction 2's lesson is that the cost of
    /// getting this wrong is the **whole** frame: `"bbo":[]` against
    /// `[Option<Level>; 2]` is `invalid length 0, expected an array of
    /// length 2`, which loses the timestamp and both sides rather than one.
    /// So lengths 0, 1 and 2, an absent key and an explicit `null` all
    /// deserialize, a missing side reads as `None`, and anything past
    /// position 1 is ignored.
    #[serde(default, deserialize_with = "deserialize_bbo_sides")]
    pub bbo: [Option<Level>; 2],
}

/// Normalise the `bbo` array to two sides without ever failing on its length.
fn deserialize_bbo_sides<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<[Option<Level>; 2], D::Error> {
    let sides = Option::<Vec<Option<Level>>>::deserialize(deserializer)?.unwrap_or_default();
    let mut normalised: [Option<Level>; 2] = [None, None];
    for (slot, side) in normalised.iter_mut().zip(sides) {
        *slot = side;
    }
    Ok(normalised)
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
    /// **Not the account's buying power.** Hyperliquid margin is unified: spot
    /// USDC collateralises perps directly, and this field reports only what has
    /// been committed to the perps clearinghouse. On an account with 999 USDC of
    /// spot and no position it reads `0.0` while the venue's own order ticket
    /// reads `Available to Trade 999.00 USDC`. Verified live on both networks
    /// 2026-09-04 (`docs/runbooks/testnet-provisioning.md` §0).
    ///
    /// Do not wire this into `oppen-core`'s `AccountSnapshot::equity_usd` or
    /// any guardrail denominator. A leverage cap divided by this value treats a
    /// funded account as empty, which fails closed today and would fail *open*
    /// the moment a fallback is added.
    ///
    /// The unified figure is `perps accountValue + spot holdings at mark`, and
    /// `portfolio`'s `accountValueHistory` is the venue's own answer to the
    /// same question. **`webData2.cumLedger` is not it** — it is cumulative net
    /// deposits. Verified on mainnet 2026-09-04:
    /// `accountValue 29.699177 = cumLedger 29.69 + pnl 0.009177`. It equals
    /// equity only while PnL is exactly zero, which is why it looked correct on
    /// a fresh testnet account and would have drifted from the first fill.
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

/// One spot token balance from `spotClearinghouseState`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotBalance {
    pub coin: String,
    pub token: u32,
    /// Everything held, including the part reserved by resting spot orders.
    pub total: Decimal,
    /// The part reserved by resting spot orders and therefore not free.
    pub hold: Decimal,
}

/// `spotClearinghouseState` response.
///
/// Needed because Hyperliquid margin is unified: spot USDC collateralises
/// perps, so an account's equity is not visible from [`ClearinghouseState`]
/// alone (see [`MarginSummary::account_value`]).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotClearinghouseState {
    pub balances: Vec<SpotBalance>,
}

impl SpotClearinghouseState {
    /// The USDC balance, or zero when the account holds none.
    ///
    /// Absent and zero are the same answer here: an account that has never
    /// held USDC and one that spent it all have the same buying power.
    pub fn usdc_total(&self) -> Decimal {
        self.balances
            .iter()
            .find(|balance| balance.coin == "USDC")
            .map(|balance| balance.total)
            .unwrap_or_default()
    }
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

/// `predictedFundings.HlPerp.nextFundingTime`: a boundary that has already
/// passed, so it is not a clock and carries no ordering.
///
/// `docs/specs/fair-value.md` §14.5, re-verified live 2026-09-04: all 233
/// `HlPerp` entries report a boundary **in the past** — 1,749.97 s behind the
/// capture clock, byte-identical across every coin — while `BinPerp` and
/// `BybitPerp` on the same payload point forward. A `now >= next_funding_time`
/// refresh trigger written against it fires on every tick, forever.
///
/// The type has no `PartialOrd` and no `From<StaleBoundaryMs> for u64` on
/// purpose: the comparison the venue invites is the defect, and the only way
/// out of the newtype is an accessor that names what it is at the call site.
/// The real countdown is [`next_funding_s_from_ms`], derived from the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct StaleBoundaryMs(u64);

impl StaleBoundaryMs {
    /// The raw venue value, ms. Fine to display or log; never a deadline.
    pub const fn venue_reported_boundary_ms_do_not_compare_to_now(self) -> u64 {
        self.0
    }
}

impl fmt::Display for StaleBoundaryMs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PredictedFunding {
    pub funding_rate: Decimal,
    /// See [`StaleBoundaryMs`]: on `HlPerp` this names a boundary that has
    /// already passed, identically for every coin.
    pub next_funding_time: StaleBoundaryMs,
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

/// One `fundingHistory` row from the **validator-operated dex**:
/// `{coin, fundingRate, premium, time}`.
///
/// `docs/specs/fair-value.md` §14.3 calls [`Self::premium`] "the single most
/// valuable finding" of the live audit: it is the **uncensored** hour-average
/// premium published alongside the censored rate, and `g(premium)` reproduces
/// `fundingRate` to 5e-11 over 4,627 records. That collapses the §3.2
/// censored-interval machinery to a point observation for all historical work.
///
/// That reconstruction is a property of the validator dex, not of the
/// endpoint. §14.4 correction 15: HIP-3 takes its premium on the midpoint,
/// clamps at 3e-4, and lets the deployer move `i` and the multipliers with no
/// change timestamp, so the same `g` reproduces nothing there — measured
/// 2026-09-04, `xyz:TSLA` misses on 200/200 rows by up to 9.35e-5 (9.3 bp per
/// hour) and `hyna:BTC` on 53/200 by up to 2.50e-5, while BTC misses 0/200.
/// Those rows deserialize cleanly against every other field, which is why
/// [`Self::coin`] is a [`ValidatorDexCoin`]: a `<dex>:<coin>` row fails here,
/// on the wire, instead of being recorded as history whose stated invariant
/// is false.
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
    /// Refuses a `<dex>:<coin>` name at deserialization time; see
    /// [`ValidatorDexCoin`].
    pub coin: ValidatorDexCoin,
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
    /// The rows, or `None` when the coin is not listed.
    ///
    /// It returns an [`Option`] rather than an empty slice because an empty
    /// slice is exactly the `Vec` this enum exists to keep out of a `null`.
    /// A backfill written the obvious way — `if page.rows().is_empty() {
    /// mark_complete() }` — would otherwise reproduce §14.4 correction 12's
    /// bug in full: a misspelled or unlisted coin reports "no more history",
    /// the walk terminates reporting success, and under D-e the UI states a
    /// completeness date that is wrong with no error raised anywhere.
    /// [`Self::next_start_ms`] keeps the **cursor** safe; this keeps the
    /// **completeness signal** safe, and they are different questions.
    pub fn rows(&self) -> Option<&[FundingHistoryRow]> {
        match self {
            FundingHistoryPage::Rows(rows) => Some(rows),
            FundingHistoryPage::UnlistedCoin => None,
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
    fn book_presence_is_open_interest_and_ids_survive_filtering() {
        let response: MetaAndAssetCtxs = serde_json::from_str(MAINNET_PREFIX).expect("fixture");

        let all: Vec<u32> = response
            .iter_on(PerpDex::Validator)
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(all, vec![0, 1, 2, 3], "validator dex: index is the id");

        let with_book: Vec<&str> = response
            .with_book()
            .map(|(info, _)| info.name.as_str())
            .collect();
        assert_eq!(with_book, vec!["BTC", "ETH", "ATOM"]);

        assert!(response.ctxs()[0].has_book());
        assert!(!response.ctxs()[3].has_book(), "openInterest 0.0 = no book");
    }

    /// §14.4 correction 15 and `docs/hl-signing.md` §6. The same
    /// `metaAndAssetCtxs` shape comes back for `{"dex":"xyz"}`, where the
    /// array positions are **not** the asset ids. Naming the dex is the only
    /// way to get an id, so the HIP-3 arithmetic cannot be skipped by
    /// accident. The regression is the old `iter()`, which handed out the
    /// bare position as "the on-chain asset id" unconditionally: on this
    /// fixture it would call `xyz:TSLA` asset 1, which is ETH.
    #[test]
    fn a_hip3_response_does_not_yield_validator_asset_ids() {
        // Verbatim shape of `{"type":"metaAndAssetCtxs","dex":"xyz"}`,
        // trimmed to three assets: same keys, same nesting, HIP-3 names.
        const HIP3: &str = r#"[{"universe":[{"szDecimals":2,"name":"XYZ100","maxLeverage":5},{"szDecimals":2,"name":"TSLA","maxLeverage":5},{"szDecimals":2,"name":"NVDA","maxLeverage":5}]},[{"funding":"0.0","openInterest":"1.0","prevDayPx":"1.0","dayNtlVlm":"1.0","premium":"0.0001","oraclePx":"1.0","markPx":"1.0","midPx":"1.0","impactPxs":["1.0","1.0"]},{"funding":"0.0","openInterest":"1.0","prevDayPx":"1.0","dayNtlVlm":"1.0","premium":"0.0001","oraclePx":"1.0","markPx":"1.0","midPx":"1.0","impactPxs":["1.0","1.0"]},{"funding":"0.0","openInterest":"1.0","prevDayPx":"1.0","dayNtlVlm":"1.0","premium":"0.0001","oraclePx":"1.0","markPx":"1.0","midPx":"1.0","impactPxs":["1.0","1.0"]}]]"#;
        let hip3: MetaAndAssetCtxs =
            serde_json::from_str(HIP3).expect("a HIP-3 response has the same shape");
        assert!(hip3.is_aligned());

        // `xyz` is perp dex index 1 in this fixture's world.
        let ids: Vec<(u32, &str)> = hip3
            .iter_on(PerpDex::Builder(1))
            .map(|(id, info, _)| (id, info.name.as_str()))
            .collect();
        assert_eq!(
            ids,
            vec![(110_000, "XYZ100"), (110_001, "TSLA"), (110_002, "NVDA")],
            "hl-signing.md §6: 100000 + dex*10000 + index"
        );
        // The bare positions belong to entirely different instruments.
        let validator: Vec<u32> = hip3
            .iter_on(PerpDex::Validator)
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(validator, vec![0, 1, 2]);
        assert_ne!(ids[1].0, validator[1], "position 1 is ETH on the main dex");
    }

    /// `docs/hl-signing.md` §6, including its worked example, and no panic on
    /// a dex index large enough to overflow the arithmetic.
    #[test]
    fn asset_ids_follow_the_dex_offset_and_saturate() {
        assert_eq!(PerpDex::Validator.asset_id(0), 0);
        assert_eq!(PerpDex::Validator.asset_id(233), 233);
        // "test:ABC on testnet has perp_dex_index = 1, index_in_meta = 0,
        // asset = 110000".
        assert_eq!(PerpDex::Builder(1).asset_id(0), 110_000);
        assert_eq!(PerpDex::Builder(9).asset_id(7), 190_007);
        assert_eq!(PerpDex::Builder(u32::MAX).asset_id(u32::MAX), u32::MAX);
    }

    /// §4.3 asks two questions — can I build `micro`, can I build `carry` —
    /// and `has_book` answers neither. Testnet SAGA, captured verbatim
    /// 2026-09-03, is the counterexample: open interest and a mid with a null
    /// premium and null impact prices.
    #[test]
    fn saga_has_a_book_but_no_carry() {
        const TESTNET_SAGA: &str = r#"{"funding":"0.0","openInterest":"1146084.0","prevDayPx":"0.01518","dayNtlVlm":"38000.72228","premium":null,"oraclePx":"0.01482","markPx":"0.01483","midPx":"0.01513","impactPxs":null,"dayBaseVlm":"2496048.2999999998"}"#;
        let saga: AssetCtx = serde_json::from_str(TESTNET_SAGA).expect("SAGA ctx");
        assert!(saga.has_book(), "1,146,084 open interest");
        assert!(saga.can_build_micro(), "midPx 0.01513");
        assert!(
            !saga.can_build_carry(),
            "premium and impactPxs are both null: §3.1 is unconstructible"
        );

        let response: MetaAndAssetCtxs = serde_json::from_str(MAINNET_PREFIX).expect("fixture");
        let btc = &response.ctxs()[0];
        assert!(btc.can_build_micro() && btc.can_build_carry());
        let matic = &response.ctxs()[3];
        assert!(!matic.can_build_micro() && !matic.can_build_carry());
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
    ///
    /// It takes **milliseconds**, the unit every timestamp in this module
    /// carries. The regression is the seconds-taking version: fed the ms
    /// value below it answered 575 instead of 663 — a plausible wrong number,
    /// no panic, no type error.
    #[test]
    fn next_funding_is_derived_from_the_clock_in_ms() {
        assert_eq!(next_funding_s_from_ms(0), 3600, "on a boundary, never 0");
        assert_eq!(next_funding_s_from_ms(1_000), 3599);
        assert_eq!(
            next_funding_s_from_ms(999),
            3600,
            "sub-second is the same s"
        );
        assert_eq!(next_funding_s_from_ms(3_599_000), 1);
        assert_eq!(next_funding_s_from_ms(3_600_000), 3600);
        // The real BTC bbo tick, verbatim in ms as the venue sent it: it sits
        // 2937 s past the funding boundary 1788487200, which is exactly the
        // `time` of the newest live fundingHistory row.
        assert_eq!(next_funding_s_from_ms(1_788_490_137_025), 663);
        assert_eq!(1_788_490_137 + 663, 1_788_490_800);
        assert_eq!(1_788_490_800 % FUNDING_INTERVAL_S, 0, "lands on a boundary");
        assert_eq!(1_788_487_200 % FUNDING_INTERVAL_S, 0, "a real funding row");
        for s in 0..7200u64 {
            let ms = s * 1_000 + 500;
            let remaining = next_funding_s_from_ms(ms);
            assert!((1..=FUNDING_INTERVAL_S).contains(&remaining));
            assert_eq!((s + remaining) % FUNDING_INTERVAL_S, 0);
        }
    }

    /// §14.5. `HlPerp` names a boundary already in the past, identically for
    /// every coin, while the CEX rows on the same payload point forward. The
    /// newtype is what stops `now >= next_funding_time` from compiling.
    #[test]
    fn hl_predicted_funding_boundary_is_in_the_past() {
        // Two rows verbatim from `tests/fixtures/predictedFundings.json`,
        // whose session clock is the `time` of `tests/fixtures/l2Book.json`.
        const CAPTURED_AT_MS: u64 = 1_788_460_183_896;
        let json = r#"[["BTC",[["BinPerp",{"fundingRate":"0.00009858","nextFundingTime":1788480000000,"fundingIntervalHours":8}],["HlPerp",{"fundingRate":"0.0012026746","nextFundingTime":1788458400000,"fundingIntervalHours":1}]]],["ETH",[["HlPerp",{"fundingRate":"-0.0010026023","nextFundingTime":1788458400000,"fundingIntervalHours":1}]]]]"#;
        let rows: Vec<PredictedFundings> = serde_json::from_str(json).expect("live rows");

        let boundaries: Vec<u64> = rows
            .iter()
            .map(|row| {
                row.hyperliquid()
                    .expect("HlPerp row")
                    .next_funding_time
                    .venue_reported_boundary_ms_do_not_compare_to_now()
            })
            .collect();
        assert_eq!(
            boundaries[0], boundaries[1],
            "identical across coins, which a real per-coin boundary would not be"
        );
        assert!(
            boundaries[0] < CAPTURED_AT_MS,
            "the venue's next boundary is already past"
        );
        let behind_s = (CAPTURED_AT_MS - boundaries[0]) / 1_000;
        assert_eq!(behind_s, 1_783);
        assert!(
            (1_064..=1_898).contains(&behind_s),
            "inside the §14.5 measured band"
        );
        // The same payload's CEX row points forward, so this is HlPerp's
        // defect and not a stale fixture.
        let binance = rows[0].1.iter().find(|(v, _)| v == "BinPerp");
        let binance = binance
            .and_then(|(_, f)| f.as_ref())
            .expect("BinPerp row")
            .next_funding_time
            .venue_reported_boundary_ms_do_not_compare_to_now();
        assert!(binance > CAPTURED_AT_MS);
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
    ///
    /// The null-side shape was never observed on the wire (75 s across seven
    /// bookless mainnet coins produced zero `bbo` frames), so every plausible
    /// encoding of "a side is missing" has to survive. Under the old
    /// fixed-length array, `"bbo":[]` failed with `invalid length 0, expected
    /// an array of length 2` and lost the **whole** frame — timestamp and
    /// both sides — which is §14.4 correction 2's lesson exactly.
    #[test]
    fn bbo_tolerates_every_shape_of_a_missing_side() {
        let bid = r#"{"px":"1.0","sz":"2.0","n":1}"#;
        let cases = [
            (
                r#"{"coin":"F","time":1,"bbo":[null,null]}"#.to_owned(),
                false,
            ),
            (r#"{"coin":"F","time":1,"bbo":[]}"#.to_owned(), false),
            (r#"{"coin":"F","time":1,"bbo":null}"#.to_owned(), false),
            (r#"{"coin":"F","time":1}"#.to_owned(), false),
            (format!(r#"{{"coin":"F","time":1,"bbo":[{bid}]}}"#), true),
            (
                format!(r#"{{"coin":"F","time":1,"bbo":[{bid},null]}}"#),
                true,
            ),
        ];
        for (json, has_bid) in cases {
            let bbo: Bbo = serde_json::from_str(&json).unwrap_or_else(|e| {
                panic!("a missing side must not lose the frame: {json} -> {e}")
            });
            assert_eq!(bbo.time, 1, "the timestamp survives: {json}");
            assert_eq!(bbo.bid().is_some(), has_bid, "{json}");
            assert!(bbo.ask().is_none(), "{json}");
        }

        // A longer array is normalised rather than refused.
        let json = format!(r#"{{"coin":"F","time":1,"bbo":[{bid},{bid},{bid}]}}"#);
        let bbo: Bbo = serde_json::from_str(&json).expect("extra sides are ignored");
        assert!(bbo.bid().is_some() && bbo.ask().is_some());
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
        assert_eq!(rows[0].coin, *"BTC");
    }

    /// §14.4 correction 15. A HIP-3 funding row deserialises cleanly against
    /// every other field, and the reconstruction property this type documents
    /// is false for it, so it must not become a `FundingHistoryRow` at all.
    #[test]
    fn a_hip3_funding_row_is_refused_on_the_wire() {
        let json = r#"[{"coin":"xyz:TSLA","fundingRate":"0.0000125","premium":"0.0001446433","time":1787626800017}]"#;
        let err = serde_json::from_str::<Vec<FundingHistoryRow>>(json)
            .expect_err("a HIP-3 row must not deserialize into a validator-dex row");
        assert!(
            err.to_string().contains("HIP-3"),
            "the error must name the scope fence, got {err}"
        );

        // Every other field is well-formed, which is why nothing downstream
        // would have caught it.
        let value: serde_json::Value = serde_json::from_str(json).expect("valid json");
        assert_eq!(value[0]["premium"], "0.0001446433");

        assert_eq!(
            ValidatorDexCoin::new("xyz:TSLA"),
            Err(ScopeError::OutOfScopeDex("xyz:TSLA".to_owned()))
        );
        assert_eq!(
            ValidatorDexCoin::new("BTC").expect("BTC is on the validator dex"),
            ValidatorDexCoin::new("BTC").expect("BTC")
        );
    }

    /// §14.4 correction 12: `null` is an unlisted coin, `[]` is genuine
    /// no-data, and only the first must stop the walk.
    ///
    /// `rows()` is an `Option` because the empty slice it used to return for
    /// `UnlistedCoin` is precisely the `Vec` this enum exists to keep out of
    /// a `null`: `if page.rows().is_empty() { mark_complete() }` marked a
    /// misspelled coin complete and reported success.
    #[test]
    fn unlisted_coin_has_no_rows_and_never_advances_the_cursor() {
        assert_eq!(FundingHistoryPage::UnlistedCoin.next_start_ms(), None);
        assert_eq!(
            FundingHistoryPage::UnlistedCoin.rows(),
            None,
            "an unlisted coin has no rows; it is not a coin with zero rows"
        );

        let empty = FundingHistoryPage::Rows(Vec::new());
        assert_eq!(empty.next_start_ms(), None, "a short page is the end");
        assert_eq!(
            empty.rows(),
            Some(&[][..]),
            "genuine no-data is an empty page, which is a fact worth recording"
        );
        assert_ne!(empty.rows(), FundingHistoryPage::UnlistedCoin.rows());
        assert_ne!(empty, FundingHistoryPage::UnlistedCoin);
    }

    /// Forward pagination: a full page hands back its last row's timestamp,
    /// a short page stops, and the inclusive bound means pages overlap by one.
    #[test]
    fn full_pages_paginate_forward_by_the_last_row_time() {
        let row = |time| FundingHistoryRow {
            coin: ValidatorDexCoin::new("BTC").expect("BTC"),
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
        assert_eq!(page.rows().expect("rows").len(), FUNDING_HISTORY_PAGE_LIMIT);

        let short = FundingHistoryPage::Rows(vec![row(1), row(2)]);
        assert_eq!(short.next_start_ms(), None);
    }
}
