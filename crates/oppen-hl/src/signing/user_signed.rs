//! User-signed actions oppen needs in v1: `approveAgent` and
//! `approveBuilderFee`. Both are signed by the master wallet over
//! WalletConnect (spec D5); this module builds the JSON action and the
//! typed data, and never signs (`docs/hl-signing.md` §3).

use serde::{Serialize, Serializer};

use super::eip712::{FieldValue, TypedData};
use crate::{Address, Network};

pub const DOMAIN_NAME: &str = "HyperliquidSignTransaction";

/// `signatureChainId` travels as a hex string (`docs/hl-signing.md` §3.1).
fn chain_id_hex<S: Serializer>(id: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0x{id:x}"))
}

/// `approveAgent` (`docs/hl-signing.md` §3.3). An unnamed agent signs
/// `agentName: ""` and omits the key on the wire, like the python SDK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApproveAgent {
    #[serde(rename = "type")]
    kind: &'static str,
    pub hyperliquid_chain: &'static str,
    #[serde(serialize_with = "chain_id_hex")]
    pub signature_chain_id: u64,
    pub agent_address: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub nonce: u64,
}

impl ApproveAgent {
    pub fn new(
        network: Network,
        signature_chain_id: u64,
        agent_address: Address,
        agent_name: Option<String>,
        nonce: u64,
    ) -> Self {
        ApproveAgent {
            kind: "approveAgent",
            hyperliquid_chain: network.hyperliquid_chain(),
            signature_chain_id,
            agent_address,
            agent_name,
            nonce,
        }
    }

    pub fn typed_data(&self) -> TypedData {
        TypedData {
            domain_name: DOMAIN_NAME.into(),
            chain_id: self.signature_chain_id,
            primary_type: "HyperliquidTransaction:ApproveAgent".into(),
            fields: vec![
                (
                    "hyperliquidChain".into(),
                    FieldValue::String(self.hyperliquid_chain.into()),
                ),
                (
                    "agentAddress".into(),
                    FieldValue::Address(self.agent_address),
                ),
                (
                    "agentName".into(),
                    FieldValue::String(self.agent_name.clone().unwrap_or_default()),
                ),
                ("nonce".into(), FieldValue::Uint64(self.nonce)),
            ],
        }
    }
}

/// `approveBuilderFee` (`docs/hl-signing.md` §3.4). `max_fee_rate` is a
/// percent string such as `"0.001%"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApproveBuilderFee {
    #[serde(rename = "type")]
    kind: &'static str,
    pub hyperliquid_chain: &'static str,
    #[serde(serialize_with = "chain_id_hex")]
    pub signature_chain_id: u64,
    pub max_fee_rate: String,
    pub builder: Address,
    pub nonce: u64,
}

impl ApproveBuilderFee {
    pub fn new(
        network: Network,
        signature_chain_id: u64,
        max_fee_rate: String,
        builder: Address,
        nonce: u64,
    ) -> Self {
        ApproveBuilderFee {
            kind: "approveBuilderFee",
            hyperliquid_chain: network.hyperliquid_chain(),
            signature_chain_id,
            max_fee_rate,
            builder,
            nonce,
        }
    }

    pub fn typed_data(&self) -> TypedData {
        TypedData {
            domain_name: DOMAIN_NAME.into(),
            chain_id: self.signature_chain_id,
            primary_type: "HyperliquidTransaction:ApproveBuilderFee".into(),
            fields: vec![
                (
                    "hyperliquidChain".into(),
                    FieldValue::String(self.hyperliquid_chain.into()),
                ),
                (
                    "maxFeeRate".into(),
                    FieldValue::String(self.max_fee_rate.clone()),
                ),
                ("builder".into(), FieldValue::Address(self.builder)),
                ("nonce".into(), FieldValue::Uint64(self.nonce)),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_agent_omits_name_on_the_wire_but_signs_empty_string() {
        let a = ApproveAgent::new(Network::Testnet, 0x66eee, Address::ZERO, None, 7);
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["type"], "approveAgent");
        assert_eq!(json["signatureChainId"], "0x66eee");
        assert!(json.get("agentName").is_none());
        let td = a.typed_data();
        assert_eq!(td.fields[2].1, FieldValue::String(String::new()));
        assert_eq!(td.chain_id, 421614);
    }
}
