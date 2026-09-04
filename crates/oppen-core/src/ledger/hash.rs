//! Chain hashing for the event ledger.
//!
//! `docs/decisions.md` R5 chains **content hashes, not content**: a row commits
//! to `hash(payload)` and the payload sits beside the chain in the same row.
//! That is what lets a payload be tombstoned later without breaking
//! verification — narrowing the chain by rehashing would destroy the only
//! property the chain exists for (`docs/spec.md` item 29).
//!
//! Every preimage here is domain-separated and length-prefixed. Bare
//! concatenation is ambiguous: `("ab", "c")` and `("a", "bc")` would hash the
//! same, which is a forgery primitive in a structure whose entire purpose is
//! tamper evidence.

use sha3::{Digest, Keccak256};

use crate::Network;

use super::network_key;

/// Domain tag for a chained row hash. Versioned because changing the preimage
/// changes every hash in every existing database, so it has to be a visible,
/// deliberate migration rather than a silent behaviour change.
const ROW_DOMAIN: &[u8] = b"oppen.ledger.row.v1";

/// Domain tag for the genesis hash. Separate from `ROW_DOMAIN` so a genesis
/// value can never be mistaken for, or collide with, a row hash.
const GENESIS_DOMAIN: &[u8] = b"oppen.ledger.genesis.v1";

/// Domain tag for a payload hash. Separate again, so the hash the chain commits
/// to can never be confused with the hash of a row.
const PAYLOAD_DOMAIN: &[u8] = b"oppen.ledger.payload.v1";

/// Absorb one tagged, length-prefixed, presence-flagged field.
///
/// `None` is encoded as a distinct single byte rather than as an empty value,
/// so an absent `agent_id` and an empty-string `agent_id` produce different
/// hashes.
fn put(h: &mut Keccak256, tag: &[u8], value: Option<&[u8]>) {
    h.update((tag.len() as u64).to_be_bytes());
    h.update(tag);
    match value {
        None => h.update([0u8]),
        Some(v) => {
            h.update([1u8]);
            h.update((v.len() as u64).to_be_bytes());
            h.update(v);
        }
    }
}

/// The hash the first row's `prev_hash` must equal.
///
/// It binds the network name (`docs/decisions.md` R4: one database file and one
/// hash chain per network). A mainnet file opened as testnet therefore fails
/// verification at row 1 instead of silently serving mainnet rows under a
/// testnet cursor, which R4 calls the worst bug this product can ship.
pub(crate) fn genesis_hash(network: Network) -> String {
    let mut h = Keccak256::new();
    h.update(GENESIS_DOMAIN);
    put(&mut h, b"network", Some(network_key(network).as_bytes()));
    hex::encode(h.finalize())
}

/// Hash of a payload's canonical JSON bytes.
///
/// The bytes hashed are exactly the bytes stored, so verification never depends
/// on re-serialising a parsed value the same way twice (`docs/decisions.md` R5).
pub(crate) fn payload_hash(canonical_json: &[u8]) -> String {
    let mut h = Keccak256::new();
    h.update(PAYLOAD_DOMAIN);
    put(&mut h, b"payload", Some(canonical_json));
    hex::encode(h.finalize())
}

/// Everything a chained row commits to.
///
/// `docs/decisions.md` R5 requires `prev_hash`, `seq`, `kind`, `ts` and
/// `payload_hash`; R6 additionally requires the decision-time snapshot
/// reference to live in the chained row, and `agent_id` is chained because it
/// survives redaction and would otherwise be freely editable attribution.
pub(crate) struct RowHashInput<'a> {
    /// Hash of the row before this one, or the genesis hash for `seq == 1`.
    pub prev_hash: &'a str,
    /// This row's position in the chain — the agent's `get_events` cursor
    /// (`docs/spec.md` D6).
    pub seq: u64,
    /// Wire name of the event kind, hashed as the stored string so that a
    /// kind this build does not know about still verifies.
    pub kind: &'a str,
    /// Unix milliseconds. Millisecond precision matches Hyperliquid's own
    /// timestamps; `AGENTS.md` invariant 6 forbids inventing finer precision.
    pub ts_ms: i64,
    /// Which agent the row is attributed to, if any. Operator actions have none.
    pub agent_id: Option<&'a str>,
    /// Hash of the payload, per R5.
    pub payload_hash: &'a str,
    /// Decision-time book snapshot id (`docs/decisions.md` R6), nullable.
    pub snapshot_id: Option<&'a str>,
    /// Decision-time book snapshot hash (`docs/decisions.md` R6), nullable.
    pub snapshot_hash: Option<&'a str>,
}

/// Compute a row's chain hash.
pub(crate) fn row_hash(input: &RowHashInput<'_>) -> String {
    let mut h = Keccak256::new();
    h.update(ROW_DOMAIN);
    put(&mut h, b"prev", Some(input.prev_hash.as_bytes()));
    put(&mut h, b"seq", Some(&input.seq.to_be_bytes()));
    put(&mut h, b"kind", Some(input.kind.as_bytes()));
    put(&mut h, b"ts_ms", Some(&input.ts_ms.to_be_bytes()));
    put(&mut h, b"agent_id", input.agent_id.map(str::as_bytes));
    put(&mut h, b"payload_hash", Some(input.payload_hash.as_bytes()));
    put(&mut h, b"snapshot_id", input.snapshot_id.map(str::as_bytes));
    put(
        &mut h,
        b"snapshot_hash",
        input.snapshot_hash.map(str::as_bytes),
    );
    hex::encode(h.finalize())
}
