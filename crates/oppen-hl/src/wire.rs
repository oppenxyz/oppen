//! Wire-level value types for L1 actions.
//!
//! Field names and order are the msgpack bytes that get hashed, so every
//! struct here is declared in exactly the order the official SDKs emit
//! (`docs/hl-signing.md` §2.3). Prices and sizes are strings normalized by
//! [`float_to_wire`]; a trailing zero is a different hash.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::Address;

/// Maximum decimals of any price or size on the wire (`docs/hl-signing.md` §4).
pub const WIRE_DECIMALS: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("{0} has more than {WIRE_DECIMALS} decimals")]
    TooManyDecimals(Decimal),
    #[error("{0} does not fit {1} decimals")]
    NotRepresentable(Decimal, u32),
    #[error("cloid must be 0x followed by 32 hex chars, got {0:?}")]
    InvalidCloid(String),
    #[error("{0:?} is not a normalized wire number")]
    NotNormalized(String),
}

/// Normalizes a decimal into the string the L1 hashes: at most 8 decimals,
/// trailing zeros stripped, no trailing dot, never exponent notation, and
/// `-0` rendered as `0`. Mirrors `float_to_wire` / `float_to_string_for_hashing`.
pub fn float_to_wire(x: Decimal) -> Result<String, WireError> {
    let n = x.normalize();
    if n.scale() > WIRE_DECIMALS {
        return Err(WireError::TooManyDecimals(x));
    }
    if n.is_zero() {
        return Ok("0".to_owned());
    }
    Ok(n.to_string())
}

/// Scales a decimal to an integer with `decimals` places, as `float_to_int`
/// does for `usd`/`ntli`-style fields. Errors instead of rounding.
pub fn decimal_to_int(x: Decimal, decimals: u32) -> Result<i64, WireError> {
    let n = x.normalize();
    if n.scale() > decimals {
        return Err(WireError::NotRepresentable(x, decimals));
    }
    let scaled = n
        .checked_mul(Decimal::from(10i64.pow(decimals)))
        .ok_or(WireError::NotRepresentable(x, decimals))?;
    i64::try_from(scaled.normalize().mantissa())
        .map_err(|_| WireError::NotRepresentable(x, decimals))
}

/// A price or size already in wire form. The only constructors go through
/// [`float_to_wire`], so a trailing zero or an exponent cannot reach the
/// hash (`docs/hl-signing.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WireFloat(String);

impl WireFloat {
    pub fn from_decimal(x: Decimal) -> Result<Self, WireError> {
        float_to_wire(x).map(WireFloat)
    }

    /// Accepts a string only if it is already normalized.
    pub fn parse(s: &str) -> Result<Self, WireError> {
        let x = s
            .parse::<Decimal>()
            .map_err(|_| WireError::NotNormalized(s.to_owned()))?;
        let wire = float_to_wire(x)?;
        if wire != s {
            return Err(WireError::NotNormalized(s.to_owned()));
        }
        Ok(WireFloat(wire))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WireFloat {
    type Error = WireError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        WireFloat::parse(&s)
    }
}

impl From<WireFloat> for String {
    fn from(w: WireFloat) -> Self {
        w.0
    }
}

/// Client order id: 128 bits as `0x` + 32 lowercase hex chars.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Cloid(String);

impl Cloid {
    pub fn parse(s: &str) -> Result<Self, WireError> {
        let hex_part = s
            .strip_prefix("0x")
            .ok_or_else(|| WireError::InvalidCloid(s.to_owned()))?;
        if hex_part.len() != 32 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(WireError::InvalidCloid(s.to_owned()));
        }
        Ok(Cloid(format!("0x{}", hex_part.to_ascii_lowercase())))
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Cloid(format!("0x{}", hex::encode(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Cloid {
    type Error = WireError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Cloid::parse(&s)
    }
}

impl From<Cloid> for String {
    fn from(c: Cloid) -> Self {
        c.0
    }
}

/// Time in force. Wire spelling is title-case (`docs/hl-signing.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tif {
    /// Add liquidity only: cancelled instead of matching.
    Alo,
    /// Immediate or cancel: unfilled remainder cancelled.
    Ioc,
    /// Good til cancelled.
    Gtc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tpsl {
    Tp,
    Sl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grouping {
    #[serde(rename = "na")]
    Na,
    #[serde(rename = "normalTpsl")]
    NormalTpsl,
    #[serde(rename = "positionTpsl")]
    PositionTpsl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    Limit {
        tif: Tif,
    },
    Trigger {
        #[serde(rename = "isMarket")]
        is_market: bool,
        #[serde(rename = "triggerPx")]
        trigger_px: WireFloat,
        tpsl: Tpsl,
    },
}

/// One order as hashed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderWire {
    pub a: u32,
    pub b: bool,
    pub p: WireFloat,
    pub s: WireFloat,
    pub r: bool,
    pub t: OrderType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c: Option<Cloid>,
}

/// Builder fee attachment: `f` is tenths of a basis point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderInfo {
    pub b: Address,
    pub f: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelWire {
    pub a: u32,
    pub o: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelByCloidWire {
    pub asset: u32,
    pub cloid: Cloid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn wire(s: &str) -> String {
        float_to_wire(Decimal::from_str(s).unwrap()).unwrap()
    }

    /// RS-HELPERS `float_to_string_for_hashing_test`, verbatim table.
    #[test]
    fn matches_official_float_table() {
        assert_eq!(wire("0."), "0");
        assert_eq!(wire("-0."), "0");
        assert_eq!(wire("-0.0000"), "0");
        assert_eq!(wire("0.00076000"), "0.00076");
        assert_eq!(wire("0.00000001"), "0.00000001");
        assert_eq!(wire("0.12345678"), "0.12345678");
        assert_eq!(wire("87654321.12345678"), "87654321.12345678");
        assert_eq!(wire("987654321.00000000"), "987654321");
        assert_eq!(wire("87654321.1234"), "87654321.1234");
        assert_eq!(wire("987654321.0"), "987654321");
        assert_eq!(wire("2000.0"), "2000");
        assert_eq!(wire("100"), "100");
        assert_eq!(wire("1670.1"), "1670.1");
        assert_eq!(wire("0.0147"), "0.0147");
    }

    #[test]
    fn never_exponent_notation() {
        assert_eq!(wire("100000000"), "100000000");
        assert_eq!(wire("0.00000001"), "0.00000001");
    }

    #[test]
    fn rejects_more_than_eight_decimals() {
        let x = Decimal::from_str("0.000000001").unwrap();
        assert_eq!(float_to_wire(x), Err(WireError::TooManyDecimals(x)));
    }

    /// PY-TESTS `test_float_to_int_for_hashing`. The source's first case
    /// (`123123123123` → `1.23e19`) exceeds `i64`; no wire integer field
    /// carries that magnitude, so a same-shape in-range case stands in.
    #[test]
    fn decimal_to_int_matches_python() {
        let d = |s: &str| Decimal::from_str(s).unwrap();
        assert_eq!(
            decimal_to_int(d("12312312312"), 8).unwrap(),
            1231231231200000000
        );
        assert_eq!(decimal_to_int(d("0.00001231"), 8).unwrap(), 1231);
        assert_eq!(decimal_to_int(d("1.033"), 8).unwrap(), 103300000);
        assert!(decimal_to_int(d("0.000012312312"), 8).is_err());
        assert_eq!(decimal_to_int(d("10.5"), 6).unwrap(), 10_500_000);
    }

    #[test]
    fn wire_float_rejects_unnormalized_input() {
        assert_eq!(WireFloat::parse("2000").unwrap().as_str(), "2000");
        assert_eq!(
            WireFloat::from_decimal(Decimal::from_str("2000.0").unwrap())
                .unwrap()
                .as_str(),
            "2000"
        );
        for bad in ["2000.0", "1e3", "0.10", "+1", ".5", "abc", "-0"] {
            assert!(WireFloat::parse(bad).is_err(), "{bad} accepted");
        }
        let json: Result<WireFloat, _> = serde_json::from_str(r#""100.0""#);
        assert!(json.is_err());
    }

    #[test]
    fn cloid_validation() {
        let c = Cloid::parse("0x1234567890ABCDEF1234567890abcdef").unwrap();
        assert_eq!(c.as_str(), "0x1234567890abcdef1234567890abcdef");
        assert!(Cloid::parse("1234567890abcdef1234567890abcdef").is_err());
        assert!(Cloid::parse("0x1234").is_err());
        assert!(Cloid::parse("0x1234567890abcdef1234567890abcdeg").is_err());
    }

    #[test]
    fn order_type_wire_shape() {
        let limit = serde_json::to_string(&OrderType::Limit { tif: Tif::Ioc }).unwrap();
        assert_eq!(limit, r#"{"limit":{"tif":"Ioc"}}"#);
        let trigger = serde_json::to_string(&OrderType::Trigger {
            is_market: true,
            trigger_px: WireFloat::parse("0.8").unwrap(),
            tpsl: Tpsl::Tp,
        })
        .unwrap();
        assert_eq!(
            trigger,
            r#"{"trigger":{"isMarket":true,"triggerPx":"0.8","tpsl":"tp"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Grouping::PositionTpsl).unwrap(),
            r#""positionTpsl""#
        );
    }
}
