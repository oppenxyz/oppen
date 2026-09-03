//! Hyperliquid protocol client.
//!
//! Owns everything that talks to the venue: action signing (msgpack hash +
//! EIP-712), the info and exchange REST clients, websocket subscriptions,
//! per-signer nonce allocation, and the asset-meta / order-validation layer
//! that rounds prices and sizes before anything is signed.
//!
//! This crate is the only place in oppen that ever holds a private key in
//! memory. See `AGENTS.md` invariant 2.

/// Which Hyperliquid network a client talks to. Testnet is the default
/// everywhere in oppen; mainnet is an explicit, persisted operator choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Testnet,
    Mainnet,
}

impl Network {
    pub fn api_url(self) -> &'static str {
        match self {
            Network::Testnet => "https://api.hyperliquid-testnet.xyz",
            Network::Mainnet => "https://api.hyperliquid.xyz",
        }
    }

    pub fn ws_url(self) -> &'static str {
        match self {
            Network::Testnet => "wss://api.hyperliquid-testnet.xyz/ws",
            Network::Mainnet => "wss://api.hyperliquid.xyz/ws",
        }
    }

    /// Value of the `hyperliquidChain` field on user-signed (EIP-712)
    /// actions. This field, not the domain chain id, binds a signature to a
    /// network — see `docs/hl-signing.md`.
    pub fn hyperliquid_chain(self) -> &'static str {
        match self {
            Network::Testnet => "Testnet",
            Network::Mainnet => "Mainnet",
        }
    }

    /// `source` field of the phantom agent used to sign L1 actions. The
    /// phantom-agent domain chain id is the constant 1337 on both networks;
    /// only this byte carries the network.
    pub fn phantom_agent_source(self) -> &'static str {
        match self {
            Network::Testnet => "b",
            Network::Mainnet => "a",
        }
    }
}
