//! Asset universe plus the rounding and validation every order goes
//! through before it is signed (`docs/spec.md` item 8, `docs/hl-signing.md`
//! §6–7).

use std::collections::HashMap;

use rust_decimal::Decimal;

use crate::types::{AssetInfo, Meta};

/// Perp prices: 5 significant figures and at most `6 - szDecimals`
/// decimals. Spot would be 8; v1 is perps only.
pub const PERP_MAX_DECIMALS: u32 = 6;
pub const PRICE_SIG_FIGS: u32 = 5;
/// Orders below this notional are rejected by the venue.
pub const MIN_NOTIONAL_USD: Decimal = Decimal::from_parts(10, 0, 0, false, 0);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// A builder-deployed (HIP-3) universe was handed to the validator-dex
    /// path, where the array position would be signed as the asset id. v1 is
    /// the validator dex only — see [`Universe::from_meta`].
    #[error(
        "{0:?} is a builder-deployed (HIP-3) asset; v1 trades the validator dex only, and its \
         asset ids are numbered differently (docs/hl-signing.md \u{a7}6)"
    )]
    BuilderDeployedDex(String),
    #[error("unknown asset {0:?}")]
    UnknownAsset(String),
    #[error("asset {0:?} is delisted")]
    Delisted(String),
    #[error("price {0} is not positive")]
    NonPositivePrice(Decimal),
    #[error("size {0} is not positive")]
    NonPositiveSize(Decimal),
    #[error("price {px} has more than {PRICE_SIG_FIGS} significant figures")]
    PriceSigFigs { px: Decimal },
    #[error("price {px} has more than {max} decimals for this asset")]
    PriceDecimals { px: Decimal, max: u32 },
    #[error("size {sz} has more than {sz_decimals} decimals for this asset")]
    SizeDecimals { sz: Decimal, sz_decimals: u32 },
    #[error("notional {notional} is below the {MIN_NOTIONAL_USD} minimum")]
    MinNotional { notional: Decimal },
}

/// One tradable perp with its numeric asset id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub index: u32,
    pub info: AssetInfo,
}

impl Asset {
    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn sz_decimals(&self) -> u32 {
        self.info.sz_decimals
    }

    pub fn max_price_decimals(&self) -> u32 {
        PERP_MAX_DECIMALS.saturating_sub(self.info.sz_decimals)
    }

    /// The SDK's market-order price: 5 significant figures, then at most
    /// `6 - szDecimals` decimals (`_slippage_price`). Ties round to even
    /// in decimal, where the python SDK rounds a binary float; both yield
    /// a valid price.
    pub fn round_price(&self, px: Decimal) -> Decimal {
        let sf = px.round_sf(PRICE_SIG_FIGS).unwrap_or(px);
        sf.round_dp(self.max_price_decimals()).normalize()
    }

    pub fn round_size(&self, sz: Decimal) -> Decimal {
        sz.round_dp(self.info.sz_decimals).normalize()
    }

    /// `mid × (1 ± slippage)`, rounded as the venue expects.
    pub fn slippage_price(&self, mid: Decimal, is_buy: bool, slippage: Decimal) -> Decimal {
        let factor = if is_buy {
            Decimal::ONE + slippage
        } else {
            Decimal::ONE - slippage
        };
        self.round_price(mid * factor)
    }

    /// Doc rules verbatim: integers always pass; otherwise ≤ 5 significant
    /// figures and ≤ `6 - szDecimals` decimals.
    pub fn validate_price(&self, px: Decimal) -> Result<(), ValidationError> {
        if px <= Decimal::ZERO {
            return Err(ValidationError::NonPositivePrice(px));
        }
        let px = px.normalize();
        if px.scale() == 0 {
            return Ok(());
        }
        let max = self.max_price_decimals();
        if px.scale() > max {
            return Err(ValidationError::PriceDecimals { px, max });
        }
        if significant_figures(px) > PRICE_SIG_FIGS {
            return Err(ValidationError::PriceSigFigs { px });
        }
        Ok(())
    }

    pub fn validate_size(&self, sz: Decimal) -> Result<(), ValidationError> {
        if sz <= Decimal::ZERO {
            return Err(ValidationError::NonPositiveSize(sz));
        }
        let sz = sz.normalize();
        if sz.scale() > self.info.sz_decimals {
            return Err(ValidationError::SizeDecimals {
                sz,
                sz_decimals: self.info.sz_decimals,
            });
        }
        Ok(())
    }

    /// Full pre-sign check for a limit order.
    pub fn validate_order(&self, px: Decimal, sz: Decimal) -> Result<(), ValidationError> {
        if self.info.is_delisted {
            return Err(ValidationError::Delisted(self.info.name.clone()));
        }
        self.validate_price(px)?;
        self.validate_size(sz)?;
        let notional = px * sz;
        if notional < MIN_NOTIONAL_USD {
            return Err(ValidationError::MinNotional {
                notional: notional.normalize(),
            });
        }
        Ok(())
    }
}

fn significant_figures(x: Decimal) -> u32 {
    let digits = x.normalize().mantissa().unsigned_abs().to_string();
    digits.trim_start_matches('0').len() as u32
}

/// The perp universe of one network, keyed by coin name.
#[derive(Debug, Clone, Default)]
pub struct Universe {
    by_name: HashMap<String, Asset>,
}

impl Universe {
    /// Every entry is kept, including delisted and bookless assets.
    ///
    /// The universe array is **never compacted**: a position in it is the
    /// on-chain asset id (`docs/spec.md` item 8,
    /// `docs/specs/fair-value.md` §14.4 correction 2). Dropping the 56
    /// bookless mainnet assets here would renumber every asset after the
    /// first one dropped and send orders to the wrong instrument. Filter at
    /// the point of use on the paired ctx —
    /// [`crate::types::AssetCtx::can_build_micro`],
    /// [`crate::types::AssetCtx::can_build_carry`] or
    /// [`crate::types::AssetCtx::has_book`] — which
    /// [`crate::types::MetaAndAssetCtxs::iter`] hands you already paired, so
    /// an asset can never be judged against another asset's context.
    /// Builds a universe from a **validator-dex** `meta` response.
    ///
    /// Refuses a builder-deployed (HIP-3) response. On the validator dex the
    /// array position is the on-chain asset id, but a HIP-3 asset's id is
    /// `100000 + dex_index * 10000 + index` (`docs/hl-signing.md` §6), so
    /// signing an order against the raw position would name a completely
    /// different instrument — position 1 is ETH on the validator dex.
    ///
    /// The fence is total and free, because the venue prefixes every HIP-3
    /// asset with its dex: a mainnet `{"type":"meta","dex":"xyz"}` names its
    /// assets `xyz:TSLA`, `xyz:NVDA`. Anything carrying a colon is refused
    /// rather than mis-numbered. v1 is the validator dex only
    /// (`docs/specs/fair-value.md` §14.4 correction 15).
    pub fn from_meta(meta: &Meta) -> Result<Self, ValidationError> {
        if let Some(prefixed) = meta.universe.iter().find(|info| info.name.contains(':')) {
            return Err(ValidationError::BuilderDeployedDex(prefixed.name.clone()));
        }
        let by_name = meta
            .universe
            .iter()
            .enumerate()
            .map(|(index, info)| {
                (
                    info.name.clone(),
                    Asset {
                        index: index as u32,
                        info: info.clone(),
                    },
                )
            })
            .collect();
        Ok(Universe { by_name })
    }

    pub fn get(&self, coin: &str) -> Result<&Asset, ValidationError> {
        self.by_name
            .get(coin)
            .ok_or_else(|| ValidationError::UnknownAsset(coin.to_owned()))
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Iteration order is the map's and therefore unspecified. Anything
    /// hashed, serialized or rendered in a fixed order must iterate
    /// [`crate::types::MetaAndAssetCtxs::iter`] instead, which walks the
    /// response's own universe order (`AGENTS.md`, determinism).
    pub fn iter(&self) -> impl Iterator<Item = &Asset> {
        self.by_name.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn asset(sz_decimals: u32) -> Asset {
        Asset {
            index: 0,
            info: AssetInfo {
                name: "TEST".into(),
                sz_decimals,
                max_leverage: 50,
                margin_table_id: 0,
                is_delisted: false,
                only_isolated: false,
            },
        }
    }

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// DOC-TICK examples verbatim.
    #[test]
    fn price_rules_match_the_docs() {
        let a = asset(0);
        assert!(a.validate_price(d("1234.5")).is_ok());
        assert!(a.validate_price(d("1234.56")).is_err());
        assert!(a.validate_price(d("0.001234")).is_ok());
        assert!(a.validate_price(d("0.0012345")).is_err());
        assert!(a.validate_price(d("123456")).is_ok());
        assert!(a.validate_price(d("12345.6")).is_err());
        let b = asset(1);
        assert!(b.validate_price(d("0.01234")).is_ok());
        assert!(b.validate_price(d("0.012345")).is_err());
        assert!(b.validate_price(d("0")).is_err());
    }

    #[test]
    fn size_rules_match_the_docs() {
        let a = asset(3);
        assert!(a.validate_size(d("1.001")).is_ok());
        assert!(a.validate_size(d("1.0001")).is_err());
        assert!(a.validate_size(d("1.0010")).is_ok());
        assert!(a.validate_size(d("0")).is_err());
    }

    #[test]
    fn rounding_matches_the_sdk() {
        let btc = asset(5);
        assert_eq!(btc.round_price(d("83010.5")), d("83010"));
        assert_eq!(btc.round_price(d("83010.5") * d("1.01")), d("83841"));
        let sol = asset(2);
        assert_eq!(sol.round_price(d("104.985")), d("104.98"));
        assert_eq!(sol.round_price(d("104.975")), d("104.98"));
        assert_eq!(sol.round_price(d("104.9851")), d("104.99"));
        assert_eq!(sol.round_price(d("104.985") * d("0.99")), d("103.94"));
        assert_eq!(sol.round_size(d("1.2349")), d("1.23"));
        assert_eq!(sol.slippage_price(d("100"), true, d("0.01")), d("101"));
        assert_eq!(sol.slippage_price(d("100"), false, d("0.01")), d("99"));
        let low = asset(0);
        assert_eq!(low.round_price(d("0.00123456")), d("0.001235"));
    }

    #[test]
    fn min_notional_and_delisting() {
        let a = asset(2);
        assert_eq!(
            a.validate_order(d("100"), d("0.05")),
            Err(ValidationError::MinNotional { notional: d("5") })
        );
        assert!(a.validate_order(d("100"), d("0.1")).is_ok());
        let mut gone = asset(2);
        gone.info.is_delisted = true;
        assert!(matches!(
            gone.validate_order(d("100"), d("1")),
            Err(ValidationError::Delisted(_))
        ));
    }

    /// The signed asset id, not merely a doc comment. A HIP-3 universe must
    /// never reach `from_meta`, because position 1 there would be signed as
    /// asset 1 — which is ETH on the validator dex. Names captured from the
    /// live mainnet `{"type":"meta","dex":"xyz"}` on 2026-09-04.
    #[test]
    fn a_builder_dex_universe_is_refused_rather_than_misnumbered() {
        let meta: Meta = serde_json::from_str(
            r#"{"universe":[
                {"name":"xyz:XYZ100","szDecimals":4,"maxLeverage":5,"marginTableId":5},
                {"name":"xyz:TSLA","szDecimals":2,"maxLeverage":5,"marginTableId":5},
                {"name":"xyz:NVDA","szDecimals":2,"maxLeverage":5,"marginTableId":5}
            ]}"#,
        )
        .expect("hip-3 meta parses");
        match Universe::from_meta(&meta) {
            Err(ValidationError::BuilderDeployedDex(name)) => assert_eq!(name, "xyz:XYZ100"),
            other => panic!("a HIP-3 universe must be refused, got {other:?}"),
        }
    }

    #[test]
    fn universe_assigns_indexes_in_meta_order() {
        let meta: Meta = serde_json::from_str(include_str!("../tests/fixtures/meta.json")).unwrap();
        let u = Universe::from_meta(&meta).expect("validator dex");
        assert_eq!(u.get("SOL").unwrap().index, 0);
        assert_eq!(u.get("BTC").unwrap().index, 1);
        assert_eq!(u.get("BTC").unwrap().sz_decimals(), 5);
        assert!(u.get("MATIC").unwrap().info.is_delisted);
        assert!(matches!(
            u.get("NOPE"),
            Err(ValidationError::UnknownAsset(_))
        ));
    }

    /// Testnet PURR and SAGA, captured verbatim on 2026-09-03. PURR is the
    /// live-but-null counterexample that makes `isDelisted` the wrong
    /// invariant (`docs/specs/fair-value.md` §14.4 correction 2); SAGA is a
    /// second one the audit does not mention — it has open interest, volume
    /// and a `midPx` but a `null` `premium` and `impactPxs`.
    const TESTNET_PURR: &str = r#"{"info":{"szDecimals":0,"name":"PURR","maxLeverage":3,"marginTableId":3,"onlyIsolated":true,"marginMode":"strictIsolated"},"ctx":{"funding":"0.0","openInterest":"0.0","prevDayPx":"2.0","dayNtlVlm":"0.0","premium":null,"oraclePx":"4.60235","markPx":"2.0","midPx":null,"impactPxs":null,"dayBaseVlm":"0.0"}}"#;
    const TESTNET_SAGA: &str = r#"{"info":{"szDecimals":1,"name":"SAGA","maxLeverage":3,"marginTableId":3},"ctx":{"funding":"0.0","openInterest":"1146084.0","prevDayPx":"0.01518","dayNtlVlm":"38000.72228","premium":null,"oraclePx":"0.01482","markPx":"0.01483","midPx":"0.01513","impactPxs":null,"dayBaseVlm":"2496048.2999999998"}}"#;

    #[derive(serde::Deserialize)]
    struct Pair {
        info: AssetInfo,
        ctx: crate::types::AssetCtx,
    }

    fn pair(json: &str) -> (Asset, crate::types::AssetCtx) {
        let p: Pair = serde_json::from_str(json).unwrap();
        (
            Asset {
                index: 0,
                info: p.info,
            },
            p.ctx,
        )
    }

    /// The invariant is open interest, not `isDelisted`.
    ///
    /// The judgement is made on the ctx alone. `Asset::is_tradable(&self,
    /// ctx)` used to forward here, and its `&self` bought nothing except the
    /// ability to pass a mismatched pair — which this test itself used to do,
    /// asking a fabricated `asset(2)` about PURR's context. There is no
    /// `Asset` in this test now because there is nothing an `Asset` could
    /// contribute to the answer.
    #[test]
    fn book_presence_is_open_interest_not_is_delisted() {
        let (purr, ctx) = pair(TESTNET_PURR);
        assert!(!purr.info.is_delisted, "PURR carries no isDelisted flag");
        assert!(
            !ctx.has_book(),
            "zero open interest means no book, whatever isDelisted says"
        );
        assert!(!ctx.can_build_micro() && !ctx.can_build_carry());
        assert_eq!(ctx.premium, None);
        assert_eq!(ctx.mid_px_no_fallback(), None);

        // A real book, and both components constructible.
        let live: crate::types::AssetCtx = serde_json::from_str(
            r#"{"funding":"0.0000052764","openInterest":"36295.80812","prevDayPx":"77759.0","dayNtlVlm":"4490636711.43","premium":"-0.0005540084","oraclePx":"80684.7","markPx":"80639.0","midPx":"80639.5","impactPxs":["80635.5","80640.0"]}"#,
        )
        .unwrap();
        assert!(live.has_book() && live.can_build_micro() && live.can_build_carry());
    }

    /// The three nullable fields are not co-null in practice: SAGA has open
    /// interest and a mid while `impactPxs` is `null`, so "has a book" does
    /// not imply the §3.1 premium inputs exist.
    #[test]
    fn a_book_does_not_guarantee_impact_prices() {
        let (_saga, ctx) = pair(TESTNET_SAGA);
        assert!(ctx.has_book(), "SAGA has 1,146,084 open interest");
        assert!(ctx.can_build_micro());
        assert!(!ctx.can_build_carry(), "carry is unconstructible here");
        assert!(ctx.mid_px_no_fallback().is_some());
        assert_eq!(ctx.premium, None);
        assert_eq!(ctx.impact_pxs, None);
    }

    /// Filtering must never renumber: the asset id is the array position.
    #[test]
    fn bookless_assets_keep_their_asset_id() {
        let meta: Meta = serde_json::from_str(include_str!("../tests/fixtures/meta.json")).unwrap();
        let u = Universe::from_meta(&meta).expect("validator dex");
        assert_eq!(
            u.len(),
            meta.universe.len(),
            "the universe is not compacted"
        );
        // MATIC is delisted and sits at index 3; ETH before it keeps index 2.
        assert_eq!(u.get("ETH").unwrap().index, 2);
        assert_eq!(u.get("MATIC").unwrap().index, 3);
    }
}
