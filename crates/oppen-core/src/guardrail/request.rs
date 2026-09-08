//! Original request evidence, never an approval or signing capability.

use oppen_hl::meta::Asset;
use oppen_hl::order::OrderKind;
use oppen_hl::wire::{Tif, Tpsl};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{Exposure, MarketRef, OrderIntent, Refusal, Unevaluable};

pub(super) fn review_candidate(
    intent: &OrderIntent,
    asset: &Asset,
    market: &MarketRef,
    exposure: &Exposure,
) -> Result<OrderIntent, Refusal> {
    let mut candidate = intent.clone();
    let Some(original) = &intent.original else {
        return Ok(candidate);
    };
    if !original.matches_normalization(intent, asset) {
        return Err(Unevaluable::OriginalRequestMismatch.into());
    }
    let slippage_bps = match original.kind {
        RequestedOrderKind::Market { slippage_bps } => slippage_bps,
        RequestedOrderKind::ClosePosition {
            position_size,
            slippage_bps,
        } => {
            if exposure.agent.position_szi(&intent.symbol) != position_size {
                return Err(Unevaluable::ApprovalReviewChanged {
                    detail: "position close size or direction changed".into(),
                }
                .into());
            }
            slippage_bps
        }
        RequestedOrderKind::Limit { .. } | RequestedOrderKind::StopMarket { .. } => {
            return Ok(candidate);
        }
    };
    let reference_px = market
        .reference_px
        .filter(|px| *px > Decimal::ZERO)
        .ok_or_else(|| {
            Refusal::from(Unevaluable::ApprovalReviewChanged {
                detail: "review requires a positive current reference price".into(),
            })
        })?;
    let overflow = || Unevaluable::ArithmeticOverflow {
        field: "review price".into(),
    };
    let slippage = slippage_bps
        .checked_div(Decimal::from(10_000))
        .ok_or_else(overflow)?;
    let factor = if intent.is_buy {
        Decimal::ONE.checked_add(slippage)
    } else {
        Decimal::ONE.checked_sub(slippage)
    }
    .ok_or_else(overflow)?;
    let raw = reference_px.checked_mul(factor).ok_or_else(overflow)?;
    if raw <= Decimal::ZERO {
        return Err(Unevaluable::ApprovalReviewChanged {
            detail: "review price bound is not positive".into(),
        }
        .into());
    }
    // The existing venue rounding helper uses these same operations unchecked;
    // establishing their range first preserves its exact rounding semantics.
    candidate.px = asset.slippage_price_bounded(reference_px, intent.is_buy, slippage);
    candidate.original = Some(OriginalRequest {
        kind: original.kind.clone(),
        reference_px: Some(reference_px),
        reference_at_ms: market.as_of_ms,
    });
    Ok(candidate)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalRequest {
    pub kind: RequestedOrderKind,
    #[serde(
        serialize_with = "rust_decimal::serde::str_option::serialize",
        deserialize_with = "optional_decimal"
    )]
    pub reference_px: Option<Decimal>,
    pub reference_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestedOrderKind {
    Limit {
        #[serde(with = "rust_decimal::serde::str")]
        limit_px: Decimal,
        tif: Tif,
    },
    Market {
        #[serde(with = "rust_decimal::serde::str")]
        slippage_bps: Decimal,
    },
    StopMarket {
        #[serde(with = "rust_decimal::serde::str")]
        trigger_px: Decimal,
        tpsl: Tpsl,
        #[serde(with = "rust_decimal::serde::str")]
        slippage_bps: Decimal,
    },
    ClosePosition {
        #[serde(with = "rust_decimal::serde::str")]
        position_size: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        slippage_bps: Decimal,
    },
}

fn optional_decimal<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Decimal>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|value| Decimal::from_str_exact(&value).map_err(serde::de::Error::custom))
        .transpose()
}

impl OriginalRequest {
    pub(crate) fn matches_intent(&self, intent: &OrderIntent) -> bool {
        if self.reference_px.is_some_and(|px| px <= Decimal::ZERO)
            || self.reference_at_ms > i64::MAX as u64
        {
            return false;
        }
        match self.kind {
            RequestedOrderKind::Limit { limit_px, tif } => {
                limit_px > Decimal::ZERO
                    && intent.px == limit_px
                    && matches!(intent.kind, OrderKind::Limit { tif: actual } if actual == tif)
            }
            RequestedOrderKind::Market { slippage_bps } => {
                slippage_bps >= Decimal::ZERO
                    && self.reference_px.is_some()
                    && matches!(intent.kind, OrderKind::Limit { tif: Tif::Ioc })
            }
            RequestedOrderKind::StopMarket {
                trigger_px,
                tpsl,
                slippage_bps,
            } => {
                trigger_px > Decimal::ZERO
                    && slippage_bps >= Decimal::ZERO
                    && matches!(intent.kind, OrderKind::Trigger { is_market: true, trigger_px: actual, tpsl: actual_tpsl } if actual == trigger_px && actual_tpsl == tpsl)
            }
            RequestedOrderKind::ClosePosition {
                position_size,
                slippage_bps,
            } => {
                position_size != Decimal::ZERO
                    && slippage_bps >= Decimal::ZERO
                    && self.reference_px.is_some()
                    && intent.reduce_only
                    && intent.sz == position_size.abs()
                    && intent.is_buy == position_size.is_sign_negative()
                    && matches!(intent.kind, OrderKind::Limit { tif: Tif::Ioc })
            }
        }
    }

    pub(crate) fn matches_normalization(&self, intent: &OrderIntent, asset: &Asset) -> bool {
        if !self.matches_intent(intent) {
            return false;
        }
        let (reference, bps) = match self.kind {
            RequestedOrderKind::Limit { .. } => return true,
            RequestedOrderKind::Market { slippage_bps }
            | RequestedOrderKind::ClosePosition { slippage_bps, .. } => {
                (self.reference_px, slippage_bps)
            }
            RequestedOrderKind::StopMarket {
                trigger_px,
                slippage_bps,
                ..
            } => (Some(trigger_px), slippage_bps),
        };
        reference.is_some_and(|px| {
            let slippage = bps / Decimal::from(10_000);
            let factor = if intent.is_buy {
                Decimal::ONE.checked_add(slippage)
            } else {
                Decimal::ONE.checked_sub(slippage)
            };
            factor
                .and_then(|factor| px.checked_mul(factor))
                .is_some_and(|raw| raw > Decimal::ZERO)
                && asset.slippage_price_bounded(px, intent.is_buy, slippage) == intent.px
        })
    }
}
