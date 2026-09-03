//! 20-byte account address. Always rendered as lowercase `0x` hex: the L1
//! lowercases addresses when it parses them as bytes, so signing anything
//! else risks a hash mismatch (`docs/hl-signing.md` §2.4).

use std::fmt;

use crate::Error;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address([u8; 20]);

impl Address {
    pub const ZERO: Address = Address([0u8; 20]);

    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Address(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Parses `0x`-prefixed or bare 40-char hex, any case.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let hex_part = s.strip_prefix("0x").unwrap_or(s);
        if hex_part.len() != 40 {
            return Err(Error::Address(s.to_owned()));
        }
        let mut out = [0u8; 20];
        hex::decode_to_slice(hex_part, &mut out).map_err(|_| Error::Address(s.to_owned()))?;
        Ok(Address(out))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::str::FromStr for Address {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Address::parse(s)
    }
}

impl serde::Serialize for Address {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Address {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Address::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_lowercase() {
        let a = Address::parse("0x0D1d9635D0640821d15e323ac8AdADfA9c111414").unwrap();
        assert_eq!(a.to_string(), "0x0d1d9635d0640821d15e323ac8adadfa9c111414");
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            "\"0x0d1d9635d0640821d15e323ac8adadfa9c111414\""
        );
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(Address::parse("0x1234").is_err());
        assert!(Address::parse("0x0d1d9635d0640821d15e323ac8adadfa9c11141").is_err());
    }
}
