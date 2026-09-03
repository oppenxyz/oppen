//! Minimal EIP-712 encoder covering the field types Hyperliquid uses:
//! `string`, `address`, `uint64`, `bool`, `bytes32` (`docs/hl-signing.md`
//! §3.2). Produces both the signing digest (for tests and local agent
//! keys) and the `eth_signTypedData_v4` JSON handed to the master wallet.

use serde_json::{Map, Value, json};

use super::keccak256;
use crate::Address;

const DOMAIN_TYPE: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

pub fn domain_separator(
    name: &str,
    version: &str,
    chain_id: u64,
    verifying_contract: &Address,
) -> [u8; 32] {
    let mut encoded = Vec::with_capacity(160);
    encoded.extend_from_slice(&keccak256(DOMAIN_TYPE.as_bytes()));
    encoded.extend_from_slice(&keccak256(name.as_bytes()));
    encoded.extend_from_slice(&keccak256(version.as_bytes()));
    encoded.extend_from_slice(&word_u64(chain_id));
    encoded.extend_from_slice(&word_address(verifying_contract));
    keccak256(&encoded)
}

/// `keccak(0x19 0x01 ‖ domainSeparator ‖ structHash)`.
pub fn digest(domain_separator: [u8; 32], struct_hash: [u8; 32]) -> [u8; 32] {
    let mut input = [0u8; 66];
    input[0] = 0x19;
    input[1] = 0x01;
    input[2..34].copy_from_slice(&domain_separator);
    input[34..].copy_from_slice(&struct_hash);
    keccak256(&input)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Address,
    Uint64,
    Bool,
    Bytes32,
}

impl FieldType {
    pub fn solidity_name(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Address => "address",
            FieldType::Uint64 => "uint64",
            FieldType::Bool => "bool",
            FieldType::Bytes32 => "bytes32",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "string" => FieldType::String,
            "address" => FieldType::Address,
            "uint64" => FieldType::Uint64,
            "bool" => FieldType::Bool,
            "bytes32" => FieldType::Bytes32,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    String(String),
    Address(Address),
    Uint64(u64),
    Bool(bool),
    Bytes32([u8; 32]),
}

impl FieldValue {
    fn field_type(&self) -> FieldType {
        match self {
            FieldValue::String(_) => FieldType::String,
            FieldValue::Address(_) => FieldType::Address,
            FieldValue::Uint64(_) => FieldType::Uint64,
            FieldValue::Bool(_) => FieldType::Bool,
            FieldValue::Bytes32(_) => FieldType::Bytes32,
        }
    }

    /// The 32-byte ABI word: dynamic `string` is hashed, everything else
    /// is left-padded.
    fn word(&self) -> [u8; 32] {
        match self {
            FieldValue::String(s) => keccak256(s.as_bytes()),
            FieldValue::Address(a) => word_address(a),
            FieldValue::Uint64(n) => word_u64(*n),
            FieldValue::Bool(b) => {
                let mut w = [0u8; 32];
                w[31] = u8::from(*b);
                w
            }
            FieldValue::Bytes32(b) => *b,
        }
    }

    fn json(&self) -> Value {
        match self {
            FieldValue::String(s) => Value::String(s.clone()),
            FieldValue::Address(a) => Value::String(a.to_string()),
            FieldValue::Uint64(n) => json!(n),
            FieldValue::Bool(b) => Value::Bool(*b),
            FieldValue::Bytes32(b) => Value::String(format!("0x{}", hex::encode(b))),
        }
    }
}

/// One typed struct plus its domain. Fields are in signing order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedData {
    pub domain_name: String,
    pub chain_id: u64,
    pub primary_type: String,
    pub fields: Vec<(String, FieldValue)>,
}

impl TypedData {
    pub fn type_string(&self) -> String {
        let fields: Vec<String> = self
            .fields
            .iter()
            .map(|(name, value)| format!("{} {name}", value.field_type().solidity_name()))
            .collect();
        format!("{}({})", self.primary_type, fields.join(","))
    }

    pub fn struct_hash(&self) -> [u8; 32] {
        let mut encoded = Vec::with_capacity(32 * (1 + self.fields.len()));
        encoded.extend_from_slice(&keccak256(self.type_string().as_bytes()));
        for (_, value) in &self.fields {
            encoded.extend_from_slice(&value.word());
        }
        keccak256(&encoded)
    }

    pub fn signing_hash(&self) -> [u8; 32] {
        let domain = domain_separator(&self.domain_name, "1", self.chain_id, &Address::ZERO);
        digest(domain, self.struct_hash())
    }

    /// The `eth_signTypedData_v4` payload, shaped like the python SDK's
    /// `user_signed_payload` (explicit `EIP712Domain` type included).
    pub fn to_json(&self) -> Value {
        let type_entries: Vec<Value> = self
            .fields
            .iter()
            .map(
                |(name, value)| json!({ "name": name, "type": value.field_type().solidity_name() }),
            )
            .collect();
        let mut message = Map::new();
        for (name, value) in &self.fields {
            message.insert(name.clone(), value.json());
        }
        json!({
            "types": {
                "EIP712Domain": [
                    { "name": "name", "type": "string" },
                    { "name": "version", "type": "string" },
                    { "name": "chainId", "type": "uint256" },
                    { "name": "verifyingContract", "type": "address" },
                ],
                &self.primary_type: type_entries,
            },
            "primaryType": self.primary_type,
            "domain": {
                "name": self.domain_name,
                "version": "1",
                "chainId": self.chain_id,
                "verifyingContract": Address::ZERO.to_string(),
            },
            "message": Value::Object(message),
        })
    }
}

fn word_u64(n: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&n.to_be_bytes());
    w
}

fn word_address(a: &Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_bytes());
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_string_follows_field_order() {
        let td = TypedData {
            domain_name: "HyperliquidSignTransaction".into(),
            chain_id: 421614,
            primary_type: "HyperliquidTransaction:ApproveAgent".into(),
            fields: vec![
                (
                    "hyperliquidChain".into(),
                    FieldValue::String("Testnet".into()),
                ),
                ("agentAddress".into(), FieldValue::Address(Address::ZERO)),
                ("agentName".into(), FieldValue::String(String::new())),
                ("nonce".into(), FieldValue::Uint64(1)),
            ],
        };
        assert_eq!(
            td.type_string(),
            "HyperliquidTransaction:ApproveAgent(string hyperliquidChain,address agentAddress,string agentName,uint64 nonce)"
        );
        let json = td.to_json();
        assert_eq!(json["primaryType"], "HyperliquidTransaction:ApproveAgent");
        assert_eq!(json["domain"]["chainId"], 421614);
        assert_eq!(json["message"]["nonce"], 1);
        assert_eq!(json["types"]["EIP712Domain"].as_array().unwrap().len(), 4);
        let primary = json["types"]["HyperliquidTransaction:ApproveAgent"]
            .as_array()
            .unwrap();
        assert_eq!(primary.len(), 4);
        assert_eq!(
            primary[1],
            json!({ "name": "agentAddress", "type": "address" })
        );
    }
}
