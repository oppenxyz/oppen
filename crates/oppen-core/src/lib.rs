//! oppen core.
//!
//! The append-only, hash-chained event ledger (the single source for
//! `get_events`, the activity stream and the audit export), the guardrail
//! engine that runs immediately before signing, the kill switch and
//! dead-man's switch, condition alerts, the per-agent journal, and the
//! quant feature computations agents read through MCP.
//!
//! Guardrails live here and nowhere else. See `AGENTS.md` invariant 1.

pub use oppen_hl::Network;
