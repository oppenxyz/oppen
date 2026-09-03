//! Operator-level order description → validated wire order.

use rust_decimal::Decimal;

use crate::meta::{Asset, ValidationError};
use crate::wire::WireError;
use crate::wire::{Cloid, OrderType, OrderWire, Tif, Tpsl, WireFloat};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderKind {
    Limit {
        tif: Tif,
    },
    Trigger {
        is_market: bool,
        trigger_px: Decimal,
        tpsl: Tpsl,
    },
}

/// What an operator or agent asks for, in decimals. Rounded and validated
/// against the asset before it becomes an [`OrderWire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderSpec {
    pub is_buy: bool,
    pub px: Decimal,
    pub sz: Decimal,
    pub kind: OrderKind,
    pub reduce_only: bool,
    pub cloid: Option<Cloid>,
}

#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Wire(#[from] WireError),
}

impl OrderSpec {
    /// Rounds price and size to the asset's rules, validates, and builds
    /// the wire order. Rounding happens first so a caller can pass a raw
    /// mid-derived price.
    pub fn to_wire(&self, asset: &Asset) -> Result<OrderWire, OrderError> {
        let px = asset.round_price(self.px);
        let sz = asset.round_size(self.sz);
        asset.validate_order(px, sz)?;
        let t = match &self.kind {
            OrderKind::Limit { tif } => OrderType::Limit { tif: *tif },
            OrderKind::Trigger {
                is_market,
                trigger_px,
                tpsl,
            } => {
                let trigger_px = asset.round_price(*trigger_px);
                asset.validate_price(trigger_px)?;
                OrderType::Trigger {
                    is_market: *is_market,
                    trigger_px: WireFloat::from_decimal(trigger_px)?,
                    tpsl: *tpsl,
                }
            }
        };
        Ok(OrderWire {
            a: asset.index,
            b: self.is_buy,
            p: WireFloat::from_decimal(px)?,
            s: WireFloat::from_decimal(sz)?,
            r: self.reduce_only,
            t,
            c: self.cloid.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetInfo;
    use std::str::FromStr;

    #[test]
    fn spec_rounds_then_validates() {
        let btc = Asset {
            index: 1,
            info: AssetInfo {
                name: "BTC".into(),
                sz_decimals: 5,
                max_leverage: 40,
                margin_table_id: 0,
                is_delisted: false,
                only_isolated: false,
            },
        };
        let spec = OrderSpec {
            is_buy: true,
            px: Decimal::from_str("41505.123").unwrap(),
            sz: Decimal::from_str("0.0012345678").unwrap(),
            kind: OrderKind::Limit { tif: Tif::Alo },
            reduce_only: false,
            cloid: None,
        };
        let wire = spec.to_wire(&btc).unwrap();
        assert_eq!(wire.p.as_str(), "41505");
        assert_eq!(wire.s.as_str(), "0.00123");
        assert_eq!(wire.a, 1);
        let tiny = OrderSpec {
            sz: Decimal::from_str("0.0001").unwrap(),
            ..spec
        };
        assert!(matches!(
            tiny.to_wire(&btc),
            Err(OrderError::Validation(ValidationError::MinNotional { .. }))
        ));
    }
}
