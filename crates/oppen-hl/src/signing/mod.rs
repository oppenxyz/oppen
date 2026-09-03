//! Action signing.
//!
//! Two schemes exist (`docs/hl-signing.md` §1). L1 actions are hashed as
//! `msgpack(action) ‖ nonce ‖ vault marker ‖ [expiresAfter]`, wrapped in the
//! phantom `Agent` EIP-712 struct on chain id 1337, and signed by the agent
//! wallet held in [`AgentKey`]. User-signed actions are EIP-712 typed data
//! on the wallet's real chain and are signed by the master wallet outside
//! the app (spec D5); [`user_signed`] only builds the payloads.
//!
//! The phantom-agent construction and the float normalizer follow the
//! MIT-licensed `hyperliquid-rust-sdk` (see `NOTICE`).

pub mod eip712;
pub mod user_signed;

use std::fmt;

use k256::ecdsa::{RecoveryId, SigningKey, VerifyingKey};
use serde::Serialize;
use sha3::{Digest, Keccak256};
use zeroize::Zeroizing;

use crate::{Action, Address, Error, Network};

/// EIP-712 domain of the phantom agent: constant on both networks.
const PHANTOM_DOMAIN_NAME: &str = "Exchange";
const PHANTOM_CHAIN_ID: u64 = 1337;
const AGENT_TYPE: &str = "Agent(string source,bytes32 connectionId)";

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// `keccak(msgpack ‖ nonce_be64 ‖ 0x00 | 0x01‖vault ‖ [0x00‖expires_be64])`
/// — `docs/hl-signing.md` §2.1. The result is the phantom agent's
/// `connectionId`.
pub fn action_hash(
    action_msgpack: &[u8],
    nonce: u64,
    vault_address: Option<Address>,
    expires_after: Option<u64>,
) -> [u8; 32] {
    let mut data = Vec::with_capacity(action_msgpack.len() + 8 + 21 + 9);
    data.extend_from_slice(action_msgpack);
    data.extend_from_slice(&nonce.to_be_bytes());
    match vault_address {
        None => data.push(0x00),
        Some(vault) => {
            data.push(0x01);
            data.extend_from_slice(vault.as_bytes());
        }
    }
    if let Some(expires) = expires_after {
        data.push(0x00);
        data.extend_from_slice(&expires.to_be_bytes());
    }
    keccak256(&data)
}

/// EIP-712 signing digest of the phantom `Agent { source, connectionId }`
/// struct (`docs/hl-signing.md` §2.2). Only `source` carries the network.
pub fn phantom_agent_digest(connection_id: [u8; 32], network: Network) -> [u8; 32] {
    let domain =
        eip712::domain_separator(PHANTOM_DOMAIN_NAME, "1", PHANTOM_CHAIN_ID, &Address::ZERO);
    let mut encoded = Vec::with_capacity(96);
    encoded.extend_from_slice(&keccak256(AGENT_TYPE.as_bytes()));
    encoded.extend_from_slice(&keccak256(network.phantom_agent_source().as_bytes()));
    encoded.extend_from_slice(&connection_id);
    eip712::digest(domain, keccak256(&encoded))
}

/// A secp256k1 signature in Ethereum form: `v` is 27 or 28.
#[derive(Clone, PartialEq, Eq)]
pub struct Signature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u8,
}

impl Signature {
    /// `r ‖ s ‖ v` — the 65-byte layout alloy prints.
    pub fn to_bytes(&self) -> [u8; 65] {
        let mut out = [0u8; 65];
        out[..32].copy_from_slice(&self.r);
        out[32..64].copy_from_slice(&self.s);
        out[64] = self.v;
        out
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.to_bytes()))
    }
}

/// Minimal-length hex, as both official SDKs send `r` and `s`
/// (`docs/hl-signing.md` §9).
fn min_hex(bytes: &[u8; 32]) -> String {
    let full = hex::encode(bytes);
    let trimmed = full.trim_start_matches('0');
    if trimmed.is_empty() {
        "0x0".to_owned()
    } else {
        format!("0x{trimmed}")
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("Signature", 3)?;
        st.serialize_field("r", &min_hex(&self.r))?;
        st.serialize_field("s", &min_hex(&self.s))?;
        st.serialize_field("v", &self.v)?;
        st.end()
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// An agent (API) wallet private key. The only key material oppen ever
/// holds; zeroized on drop, never printed.
pub struct AgentKey {
    key: SigningKey,
    address: Address,
}

impl AgentKey {
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, Error> {
        let key = SigningKey::from_slice(bytes).map_err(|_| Error::InvalidKey)?;
        let address = address_of(key.verifying_key());
        Ok(AgentKey { key, address })
    }

    /// Parses `0x`-prefixed or bare 64-char hex. The intermediate buffer is
    /// zeroized.
    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let hex_part = s.strip_prefix("0x").unwrap_or(s);
        let mut buf = Zeroizing::new([0u8; 32]);
        hex::decode_to_slice(hex_part, buf.as_mut()).map_err(|_| Error::InvalidKey)?;
        Self::from_bytes(&buf)
    }

    /// The agent wallet address the master approves via `approveAgent`.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Signs a 32-byte digest. `v` is `27 + recovery id`.
    pub fn sign_digest(&self, digest: &[u8; 32]) -> Signature {
        let (sig, recid): (k256::ecdsa::Signature, RecoveryId) =
            self.key.sign_prehash_recoverable(digest);
        Signature {
            r: sig.r().to_bytes().into(),
            s: sig.s().to_bytes().into(),
            v: 27 + recid.to_byte(),
        }
    }

    /// Signs a precomputed action hash (`connectionId`) for `network`.
    pub fn sign_connection_id(&self, connection_id: [u8; 32], network: Network) -> Signature {
        self.sign_digest(&phantom_agent_digest(connection_id, network))
    }

    /// The single L1 signing path: msgpack → action hash → phantom agent →
    /// signature. Guardrails run in `oppen-core` immediately before this.
    pub fn sign_l1_action(
        &self,
        action: &Action,
        nonce: u64,
        vault_address: Option<Address>,
        expires_after: Option<u64>,
        network: Network,
    ) -> Result<Signature, Error> {
        let msgpack = action.to_msgpack()?;
        let connection_id = action_hash(&msgpack, nonce, vault_address, expires_after);
        Ok(self.sign_connection_id(connection_id, network))
    }
}

impl fmt::Debug for AgentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AgentKey({})", self.address)
    }
}

fn address_of(key: &VerifyingKey) -> Address {
    let point = key.to_sec1_point(false);
    let hash = keccak256(&point.as_bytes()[1..]);
    let mut out = [0u8; 20];
    out.copy_from_slice(&hash[12..]);
    Address::from_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_expected_address() {
        // Well-known Ethereum test key 0x0123…0123.
        let key = AgentKey::from_hex(
            "0x0123456789012345678901234567890123456789012345678901234567890123",
        )
        .unwrap();
        assert_eq!(
            key.address().to_string(),
            "0x14791697260e4c9a71f18484c9f997b308e59325"
        );
    }

    #[test]
    fn signature_json_uses_minimal_hex() {
        let sig = Signature {
            r: [0u8; 32],
            s: [0xffu8; 32],
            v: 28,
        };
        let json = serde_json::to_value(&sig).unwrap();
        assert_eq!(json["r"], "0x0");
        assert_eq!(json["s"], format!("0x{}", "f".repeat(64)));
        assert_eq!(json["v"], 28);
        let mut r = [0u8; 32];
        r[31] = 0x0a;
        assert_eq!(
            serde_json::to_value(Signature { r, s: r, v: 27 }).unwrap()["r"],
            "0xa"
        );
    }

    #[test]
    fn debug_never_prints_key_material() {
        let key =
            AgentKey::from_hex("0123456789012345678901234567890123456789012345678901234567890123")
                .unwrap();
        let dbg = format!("{key:?}");
        assert!(!dbg.contains("0123456789012345678901234567890123456789012345678901234567890123"));
    }
}
