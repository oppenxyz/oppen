//! `POST /exchange` request envelope and per-signer nonce allocation
//! (`docs/hl-signing.md` §8–9).

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::{Action, Address, AgentKey, Error, Network, Signature};

/// The JSON body of an L1 exchange request, shaped like the python SDK's
/// `_post_action` (both optional keys present, `null` when unset).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeRequest {
    pub action: Action,
    pub nonce: u64,
    pub signature: Signature,
    pub vault_address: Option<Address>,
    pub expires_after: Option<u64>,
}

impl ExchangeRequest {
    /// Builds and signs a request. This is the only constructor: there is
    /// no way to assemble an `ExchangeRequest` without going through the
    /// signer, so guardrails wrapping this call cover every order path.
    pub fn sign(
        key: &AgentKey,
        action: Action,
        nonce: u64,
        vault_address: Option<Address>,
        expires_after: Option<u64>,
        network: Network,
    ) -> Result<Self, Error> {
        let signature =
            key.sign_l1_action(&action, nonce, vault_address, expires_after, network)?;
        Ok(ExchangeRequest {
            action,
            nonce,
            signature,
            vault_address,
            expires_after,
        })
    }
}

/// One monotonic nonce source per signer. The L1 keeps the 100 highest
/// nonces per signer and rejects reuse, and an agent wallet shares one
/// nonce set across every account it signs for — so oppen keeps exactly
/// one allocator per agent key and never hands the same value out twice.
#[derive(Debug, Default)]
pub struct NonceAllocator {
    last: Mutex<u64>,
}

impl NonceAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// `max(now_ms, last + 1)`: wall-clock when it has moved on, otherwise
    /// the next integer, so bursts inside one millisecond stay unique.
    pub fn next(&self) -> u64 {
        self.next_at(now_ms())
    }

    fn next_at(&self, now: u64) -> u64 {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let nonce = now.max(*last + 1);
        *last = nonce;
        nonce
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_are_strictly_increasing_within_one_millisecond() {
        let n = NonceAllocator::new();
        assert_eq!(n.next_at(1_000), 1_000);
        assert_eq!(n.next_at(1_000), 1_001);
        assert_eq!(n.next_at(1_000), 1_002);
        assert_eq!(n.next_at(5_000), 5_000);
        assert_eq!(n.next_at(4_000), 5_001);
    }

    #[test]
    fn envelope_has_python_shape() {
        let key =
            AgentKey::from_hex("0123456789012345678901234567890123456789012345678901234567890123")
                .unwrap();
        let req =
            ExchangeRequest::sign(&key, Action::ClaimRewards, 1, None, None, Network::Testnet)
                .unwrap();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["action"]["type"], "claimRewards");
        assert_eq!(json["nonce"], 1);
        assert!(json["vaultAddress"].is_null());
        assert!(json["expiresAfter"].is_null());
        assert!(json["signature"]["r"].as_str().unwrap().starts_with("0x"));
        assert!(matches!(json["signature"]["v"].as_u64(), Some(27 | 28)));
    }
}
