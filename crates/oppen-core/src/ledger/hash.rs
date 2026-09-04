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
//!
//! The canonical form of a payload is written by [`canonical_json`] in this
//! file rather than by `serde_json::to_string`. `serde_json`'s map ordering is
//! controlled by its `preserve_order` Cargo feature, which any dependency
//! anywhere in the workspace can switch on through feature unification. The
//! preimage of every row in every existing database must not be a third-party
//! crate's feature resolution, so the bytes are emitted here: keys in sorted
//! byte order, no whitespace, integers only.

use std::collections::BTreeMap;

use serde_json::Value;
use sha3::{Digest, Keccak256};

use crate::Network;

use super::{LedgerError, Result, network_key};

/// How deep a payload may nest before it is refused.
///
/// A recursive walk over an attacker-supplied value is a stack overflow, and a
/// stack overflow is a panic on an input path (`AGENTS.md` conventions). The
/// limit matches `serde_json`'s own parse recursion limit, so nothing that
/// arrived over the MCP wire can hit it — only a value built in-process.
const MAX_DEPTH: usize = 128;

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

/// The canonical JSON text of a value: the bytes that get stored, hashed and
/// exported.
///
/// Three rules, all of them load-bearing:
///
/// * **Object keys in sorted byte order.** Two callers that build the same
///   logical payload in a different order must produce the same hash, and that
///   must not depend on a Cargo feature.
/// * **No whitespace.** There is one encoding of a value, not a family of them.
/// * **Integers only.** `AGENTS.md` says money is `Decimal`, never `f64`, and
///   this is the one table kept forever (`docs/decisions.md` D-e). A float in a
///   chained audit row is a price that no longer means what it said, so it is
///   refused rather than rounded. Prices and sizes belong here as decimal
///   strings.
pub(crate) fn canonical_json(value: &Value) -> Result<String> {
    let mut out = String::new();
    let mut pointer = String::new();
    write_value(value, &mut out, &mut pointer, 0)?;
    Ok(out)
}

/// Emit one value, tracking the RFC 6901 pointer so a rejection names the field.
fn write_value(value: &Value, out: &mut String, pointer: &mut String, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(LedgerError::PayloadTooDeep {
            pointer: pointer.clone(),
        });
    }
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(signed) = number.as_i64() {
                out.push_str(&signed.to_string());
            } else if let Some(unsigned) = number.as_u64() {
                out.push_str(&unsigned.to_string());
            } else {
                return Err(LedgerError::FloatInPayload {
                    pointer: pointer.clone(),
                });
            }
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            let mark = pointer.len();
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                pointer.push('/');
                pointer.push_str(&index.to_string());
                write_value(item, out, pointer, depth + 1)?;
                pointer.truncate(mark);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            // Collected into a `BTreeMap` rather than iterated in place: with
            // `serde_json`'s `preserve_order` feature on, `Map`'s own iteration
            // order is insertion order, and the whole point here is that the
            // preimage does not move when a dependency flips that feature.
            let sorted: BTreeMap<&str, &Value> = fields
                .iter()
                .map(|(key, item)| (key.as_str(), item))
                .collect();
            out.push('{');
            let mark = pointer.len();
            for (index, (key, item)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                pointer.push('/');
                push_pointer_token(key, pointer);
                write_value(item, out, pointer, depth + 1)?;
                pointer.truncate(mark);
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Write a JSON string literal.
///
/// The escaping matches `serde_json`'s exactly — `"`, `\`, the five short
/// control escapes, `\u00xx` for the remaining C0 controls, and raw UTF-8 for
/// everything else — so replacing the serialiser did not move a single hash in
/// an existing database. The pinned vectors in `tests.rs` are the proof.
fn write_string(value: &str, out: &mut String) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            control if (control as u32) < 0x20 => {
                let code = control as u32;
                out.push_str("\\u00");
                out.push(char::from_digit((code >> 4) & 0xF, 16).unwrap_or('0'));
                out.push(char::from_digit(code & 0xF, 16).unwrap_or('0'));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Append one RFC 6901 pointer token, escaping `~` and `/` so a rejection names
/// exactly one field even when a key contains a separator.
fn push_pointer_token(key: &str, pointer: &mut String) {
    for character in key.chars() {
        match character {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            other => pointer.push(other),
        }
    }
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
