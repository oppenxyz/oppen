//! oppen MCP gateway.
//!
//! Runs in-process with the core so that every tool call shares the single
//! guardrail-then-sign path. Streamable HTTP on loopback only: Origin and
//! Host are validated, a bearer token is required on every request, and
//! revoking a token closes its live sessions.
//!
//! Tools (v1): get_state, get_meta, get_events, get_features, preflight,
//! place, cancel, cancel_all, close_position, get_order_status, remember,
//! recall, set_alert. Every failure is a typed error with retryability
//! semantics — see `docs/mcp-contract.md`.

pub mod auth;
pub mod guard;
pub(crate) mod outcome;
pub mod server;
pub mod tools;

pub use oppen_core::Network;
