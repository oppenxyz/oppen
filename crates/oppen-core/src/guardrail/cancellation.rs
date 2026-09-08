//! Frozen discretionary cancellation evidence; never a cleanup capability.

use std::collections::BTreeSet;

use oppen_hl::wire::{CancelWire, Cloid};
use oppen_hl::{Action, Address};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{Refusal, Unevaluable};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelTarget {
    pub symbol: String,
    pub asset_index: u32,
    pub oid: u64,
    pub cloid: Option<Cloid>,
    pub is_buy: bool,
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub sz: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub orig_sz: Decimal,
    pub timestamp: u64,
    pub order_type: String,
    pub reduce_only: bool,
    pub is_trigger: bool,
    #[serde(
        serialize_with = "rust_decimal::serde::str_option::serialize",
        deserialize_with = "optional_decimal"
    )]
    pub trigger_px: Option<Decimal>,
    pub trigger_condition: Option<String>,
    pub is_position_tpsl: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelIntent {
    pub targets: Vec<CancelTarget>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelContext {
    pub account: Address,
    /// Actual completion time of the fresh account HTTP snapshot.
    pub observed_at_ms: u64,
    /// Observed rows scoped to the requested OIDs. Missing rows are not filtered
    /// from the retained intent; exact comparison refuses them below.
    pub targets: Vec<CancelTarget>,
}

fn optional_decimal<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Decimal>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|value| Decimal::from_str_exact(&value).map_err(serde::de::Error::custom))
        .transpose()
}

fn changed(detail: &str) -> Refusal {
    Unevaluable::ApprovalReviewChanged {
        detail: detail.into(),
    }
    .into()
}

impl CancelIntent {
    pub(crate) fn validate(&self) -> Result<(), Refusal> {
        if self.targets.is_empty() {
            return Err(changed("cancellation requires frozen targets"));
        }
        let mut identities = BTreeSet::new();
        for target in &self.targets {
            if target.symbol.is_empty()
                || target.symbol.chars().any(char::is_control)
                || target.limit_px <= Decimal::ZERO
                || target.sz <= Decimal::ZERO
                || target.orig_sz < target.sz
                || target.timestamp > i64::MAX as u64
                || target.trigger_px.is_some_and(|px| px < Decimal::ZERO)
                || !identities.insert(target.oid)
            {
                return Err(changed("invalid or duplicate cancellation target"));
            }
        }
        Ok(())
    }

    pub(crate) fn action(&self) -> Action {
        Action::Cancel {
            cancels: self
                .targets
                .iter()
                .map(|target| CancelWire {
                    a: target.asset_index,
                    o: target.oid,
                })
                .collect(),
        }
    }

    pub(super) fn check_context(
        &self,
        context: &CancelContext,
        account: Address,
        max_age_ms: u64,
        now_ms: u64,
    ) -> Result<(), Refusal> {
        self.validate()?;
        if context.account != account {
            return Err(Unevaluable::RouteAuthority {
                detail: "cancellation snapshot account differs from route".into(),
            }
            .into());
        }
        if now_ms < context.observed_at_ms {
            return Err(Unevaluable::ClockWentBackwards {
                now_ms,
                last_ms: context.observed_at_ms,
            }
            .into());
        }
        let age_ms = now_ms - context.observed_at_ms;
        if age_ms > max_age_ms {
            return Err(Unevaluable::StaleClearance { age_ms, max_age_ms }.into());
        }
        let mut seen = BTreeSet::new();
        if context
            .targets
            .iter()
            .any(|target| !seen.insert(target.oid))
        {
            return Err(changed("duplicate identities in cancellation snapshot"));
        }
        for target in &self.targets {
            let actual = context.targets.iter().find(|actual| {
                actual.oid == target.oid && actual.asset_index == target.asset_index
            });
            if actual != Some(target) {
                return Err(changed(
                    "frozen cancellation target missing or changed; request a fresh proposal",
                ));
            }
        }
        Ok(())
    }
}
