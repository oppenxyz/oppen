//! L1 actions: the msgpack-hashed, agent-wallet-signed request bodies.
//!
//! The `type` tag is emitted first and struct fields follow in declaration
//! order, which is what `rmp_serde::to_vec_named` hashes. Optional fields
//! are omitted, never `null` (`docs/hl-signing.md` §2.3). User-signed
//! actions (approveAgent, approveBuilderFee, transfers) are not here: they
//! are EIP-712 typed data signed by the master wallet — see
//! [`crate::signing::user_signed`].

use serde::{Deserialize, Serialize};

use crate::Address;
use crate::wire::{BuilderInfo, CancelByCloidWire, CancelWire, Grouping, OrderWire};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    Order {
        orders: Vec<OrderWire>,
        grouping: Grouping,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        builder: Option<BuilderInfo>,
    },
    Cancel {
        cancels: Vec<CancelWire>,
    },
    CancelByCloid {
        cancels: Vec<CancelByCloidWire>,
    },
    /// Dead-man's switch. `time` absent disarms it.
    ScheduleCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<u64>,
    },
    UpdateLeverage {
        asset: u32,
        is_cross: bool,
        leverage: u32,
    },
    /// `ntli` is a 6-decimal integer (`docs/hl-signing.md` §2.4).
    UpdateIsolatedMargin {
        asset: u32,
        is_buy: bool,
        ntli: i64,
    },
    CreateSubAccount {
        name: String,
    },
    SubAccountTransfer {
        sub_account_user: Address,
        is_deposit: bool,
        usd: u64,
    },
    ClaimRewards,
}

impl Action {
    /// The exact bytes the L1 hashes: `rmp_serde::to_vec_named` over the
    /// struct, keys in declaration order, no floats.
    pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_tag_first_and_optional_fields_omitted() {
        let json = serde_json::to_string(&Action::ScheduleCancel { time: None }).unwrap();
        assert_eq!(json, r#"{"type":"scheduleCancel"}"#);
        let json = serde_json::to_string(&Action::ScheduleCancel {
            time: Some(123456789),
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"scheduleCancel","time":123456789}"#);
        let json = serde_json::to_string(&Action::ClaimRewards).unwrap();
        assert_eq!(json, r#"{"type":"claimRewards"}"#);
        let json = serde_json::to_string(&Action::UpdateLeverage {
            asset: 0,
            is_cross: true,
            leverage: 5,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"updateLeverage","asset":0,"isCross":true,"leverage":5}"#
        );
    }
}
