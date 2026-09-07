//! Hyperliquid protocol client.
//!
//! Owns everything that talks to the venue: action signing (msgpack hash +
//! EIP-712), the info and exchange REST clients, websocket subscriptions,
//! per-signer nonce allocation, and the asset-meta / order-validation layer
//! that rounds prices and sizes before anything is signed.
//!
//! This crate is the only place in oppen that ever holds a private key in
//! memory. See `AGENTS.md` invariant 2.
//!
//! Every wire rule implemented here cites `docs/hl-signing.md`; the official
//! SDK vectors in `tests/vectors/signing.json` pin the behaviour.

pub mod action;
pub mod address;
pub mod exchange;
pub mod info;
pub mod meta;
pub mod order;
pub mod signing;
pub mod types;
pub mod wire;
pub mod ws;

// A stalled venue must not retain a container's execution lock indefinitely.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub use action::Action;
pub use address::Address;
pub use exchange::{ExchangeClient, ExchangeRequest, ExchangeResponse, NonceAllocator, Status};
pub use info::{InfoClient, OrderRef};
pub use meta::{Asset, Universe, ValidationError};
pub use order::{OrderKind, OrderSpec};
pub use signing::{AgentKey, Signature};

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
    /// network — see `docs/hl-signing.md` §3.1.
    pub fn hyperliquid_chain(self) -> &'static str {
        match self {
            Network::Testnet => "Testnet",
            Network::Mainnet => "Mainnet",
        }
    }

    /// `source` field of the phantom agent used to sign L1 actions. The
    /// phantom-agent domain chain id is the constant 1337 on both networks;
    /// only this byte carries the network (`docs/hl-signing.md` §2.2).
    pub fn phantom_agent_source(self) -> &'static str {
        match self {
            Network::Testnet => "b",
            Network::Mainnet => "a",
        }
    }
}

/// Errors from the protocol layer. Every variant is a distinct, typed
/// failure so callers never have to parse a message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Wire(#[from] wire::WireError),
    #[error("invalid address: {0}")]
    Address(String),
    #[error("invalid private key")]
    InvalidKey,
    #[error("msgpack encoding failed: {0}")]
    Msgpack(#[from] rmp_serde::encode::Error),
    #[error("http transport: {0}")]
    Http(#[from] reqwest::Error),
    /// Non-2xx or an invalid info response. Not proof of exchange rejection.
    #[error("venue rejected the request (http {status}): {message}")]
    Venue { status: u16, message: String },
    /// A parsed top-level exchange rejection, safe to release from in-flight exposure.
    #[error("exchange rejected the request: {message}")]
    ExchangeRejected { message: String },
    /// The response cannot establish whether a submitted action was applied.
    #[error("invalid exchange response: {0}")]
    InvalidExchangeResponse(String),
    #[error("websocket: {0}")]
    Ws(String),
}
