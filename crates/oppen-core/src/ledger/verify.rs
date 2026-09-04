//! Chain verification: walk from genesis and report the first broken link.
//!
//! `docs/spec.md` item 29 — the ledger is tamper-evident, not tamper-proof. The
//! database file belongs to the user and anyone with the user's shell can edit
//! it. What the chain buys is that an edit cannot be hidden, and that the report
//! names the row where the history stops being trustworthy.

use std::collections::BTreeSet;

use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use super::{Anchor, EventKind, LedgerError, Result, hash};

/// How far the chain may legitimately run ahead of the anchor.
///
/// `note_head` writes the anchor after the row commits, so a crash in that
/// window leaves exactly one unwitnessed row. Anything beyond that is not a
/// crash.
pub const ANCHOR_LAG_TOLERANCE: u64 = 1;

/// Why the walk stopped.
///
/// Each variant is a distinct forensic story, which is the point: "row 4,812 is
/// missing" and "row 4,812 was rewritten" call for different responses, and a
/// single `chain_broken` boolean would collapse them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BreakReason {
    /// The chain is LONGER than the anchor by more than the one row a crash
    /// between commit and anchor-write can explain.
    ///
    /// This is the forged-tail case. An attacker with write access to the
    /// database can recompute a row hash from the open-source preimage and
    /// *append* — laundering a nulled payload behind a tombstone event it
    /// wrote itself — rather than rewriting history. Comparing only at the
    /// anchored seq accepts that, because a prefix still matches. So the
    /// anchor must bound the head from above as well as below.
    #[error("chain is {head_seq} rows but the anchor witnessed {anchor_seq}; {ahead} unwitnessed")]
    ChainAheadOfAnchor {
        /// The seq the anchor last witnessed.
        anchor_seq: u64,
        /// The seq the chain now claims.
        head_seq: u64,
        /// How far ahead, beyond the tolerated crash lag.
        ahead: u64,
    },
    /// The ledger was opened with an anchor and the anchor is now gone.
    ///
    /// `open_anchored` populates the anchor at open, adopting the current head
    /// when the sidecar is empty. A later read of `None` is therefore positive
    /// proof the sidecar was removed, not an unanchored ledger — and folding
    /// the two together would be a fail-open on exactly the file an attacker
    /// would delete first.
    #[error("this ledger was anchored and the anchor is missing")]
    AnchorMissing,
    /// A row is missing: the walk expected `expected` and found the reporting
    /// seq instead. Rows are never deleted (`docs/decisions.md` D-e), so this
    /// is deletion, not retention.
    #[error("expected seq {expected}, which is missing")]
    SeqGap {
        /// The seq the walk expected to see next.
        expected: u64,
    },
    /// This row does not point at the previous row's hash. Something was
    /// inserted, removed or reordered before it.
    #[error("prev_hash is {found}, expected {expected}")]
    PrevHashMismatch {
        /// The hash of the row before this one.
        expected: String,
        /// The `prev_hash` actually stored on this row.
        found: String,
    },
    /// The stored hash is not the hash of the row's own chained fields: a
    /// field on this row was edited, or the hash column was.
    #[error("stored hash is {found}, recomputes to {expected}")]
    RowHashMismatch {
        /// The hash recomputed from the row's chained fields.
        expected: String,
        /// The hash stored on the row.
        found: String,
    },
    /// The payload beside the chain is not the payload the chain committed to.
    /// This is the tamper case redaction is careful not to look like.
    #[error("payload hashes to {found}, chain committed to {expected}")]
    PayloadHashMismatch {
        /// The payload hash stored in the chained row.
        expected: String,
        /// The hash of the payload bytes actually present.
        found: String,
    },
    /// The payload column holds something that is neither text, a blob, nor
    /// null. It cannot be hashed, so the commitment cannot be checked.
    #[error("payload column is not text, blob or null")]
    PayloadUnreadable,
    /// The payload is gone and no *chained* redaction covers it. An authorised
    /// redaction appends an [`EventKind::PayloadRedacted`] row naming the seq
    /// it nulled, and that row is inside the hash chain; a silent NULL is
    /// someone deleting evidence.
    ///
    /// The `redacted_at` column is deliberately not consulted. It is not in the
    /// row-hash preimage, so anyone who can null a payload can set it in the
    /// same `UPDATE` — evidence an attacker can write is not evidence.
    #[error("payload is null and no chained redaction covers it")]
    UnrecordedTombstone,
    /// The chain itself verifies but the head no longer points at its last
    /// row, so the next append would build on the wrong hash.
    #[error(
        "chain head is ({found_seq}, {found_hash}), last row is ({expected_seq}, {expected_hash})"
    )]
    HeadMismatch {
        /// Seq of the last row actually present.
        expected_seq: u64,
        /// Hash of the last row actually present.
        expected_hash: String,
        /// Seq recorded in the head.
        found_seq: u64,
        /// Hash recorded in the head.
        found_hash: String,
    },
    /// The chain no longer reaches, or no longer agrees with, the head that was
    /// anchored outside the database file.
    ///
    /// Covers both shapes of rewriting history from the end: the chain is now
    /// shorter than the anchor (a truncated suffix, with `chain_head` rewound
    /// to match so [`BreakReason::HeadMismatch`] stays quiet), and the chain is
    /// long enough but the row at the anchored seq hashes to something else (a
    /// forged row with every hash after it recomputed).
    #[error(
        "anchored head is ({anchor_seq}, {anchor_hash}), chain has ({found_seq}, {found_hash})"
    )]
    HeadBehindAnchor {
        /// Seq recorded outside the database.
        anchor_seq: u64,
        /// Row hash recorded outside the database.
        anchor_hash: String,
        /// Seq the chain actually reaches, or the anchored seq when the chain
        /// is long enough but disagrees.
        found_seq: u64,
        /// The hash actually found there.
        found_hash: String,
    },
}

/// The first broken link, with the seq it was found at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChainBreak {
    /// The row the walk stopped at. For `SeqGap` this is the row that was
    /// found where the missing one should have been; for `HeadMismatch` it is
    /// the last row present.
    pub seq: u64,
    /// What was wrong with it.
    pub reason: BreakReason,
}

/// Result of walking the whole chain.
///
/// Drives the "chain broken" banner in `docs/spec.md` item 29 and the
/// verification section of the audit export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChainReport {
    /// How many rows verified before the walk stopped (or in total, if intact).
    pub rows_checked: u64,
    /// Seq recorded in the chain head; `0` on an empty ledger.
    pub head_seq: u64,
    /// Hash recorded in the chain head; the genesis hash on an empty ledger.
    pub head_hash: String,
    /// The first break, or `None` when the chain verifies end to end.
    pub first_break: Option<ChainBreak>,
}

impl ChainReport {
    /// Whether the chain verified end to end. The banner reads this; nothing
    /// else should have to know how a report is shaped.
    pub fn is_intact(&self) -> bool {
        self.first_break.is_none()
    }
}

/// Columns the walk needs, in preimage order so the reader below stays honest.
///
/// `redacted_at` and `redaction_reason` are absent on purpose: neither is in
/// the row-hash preimage, so neither can be evidence of anything. A redaction
/// is proven by the chained [`EventKind::PayloadRedacted`] row that names it.
const WALK_COLUMNS: &str = "seq, ts_ms, kind, agent_id, payload, payload_hash, \
     prev_hash, hash, snapshot_id, snapshot_hash";

/// The seq a `payload_redacted` row says it covers, if it says anything.
///
/// The bytes are the ones the chain committed to — this is only ever called
/// after the row's own hash and its payload hash have both verified — so the
/// claim is as trustworthy as the chain itself. A body that does not parse, or
/// that names no seq, simply excuses nothing.
fn redacted_seq(payload: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(payload)
        .ok()?
        .get("redacted_seq")?
        .as_u64()
}

/// Walk the chain from genesis and report the first broken link.
///
/// Streams the rows rather than collecting them: an audited ledger is expected
/// to be long-lived, and a verification that needs the whole history resident
/// is a verification that stops being run. The only state that grows is the set
/// of null payloads not yet excused by a chained redaction, which is bounded by
/// the redaction count and empties as the walk passes each tombstone.
///
/// `anchor` is the head remembered outside the database file, when there is
/// one. Without it the last row of the chain is whatever the file says it is;
/// see [`super::anchor`] for exactly what it buys.
/// The head this database claims, without walking the chain.
///
/// Used when verification stops before the walk — a removed anchor, for
/// instance — so the report still names the head under scrutiny.
pub(crate) fn head_of(conn: &Connection) -> Result<(u64, String)> {
    let (seq_raw, hash): (i64, String) =
        conn.query_row("SELECT seq, hash FROM chain_head WHERE id = 0", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    Ok((
        u64::try_from(seq_raw).map_err(|_| LedgerError::SeqOutOfRange)?,
        hash,
    ))
}

pub(crate) fn walk(
    conn: &Connection,
    genesis: &str,
    anchor: Option<&Anchor>,
) -> Result<ChainReport> {
    let (head_seq_raw, head_hash): (i64, String) =
        conn.query_row("SELECT seq, hash FROM chain_head WHERE id = 0", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let head_seq = u64::try_from(head_seq_raw).map_err(|_| LedgerError::SeqOutOfRange)?;

    let mut statement = conn.prepare(&format!(
        "SELECT {WALK_COLUMNS} FROM events ORDER BY seq ASC"
    ))?;
    let mut rows = statement.query([])?;

    let mut expected_seq: u64 = 1;
    let mut expected_prev = genesis.to_owned();
    let mut rows_checked: u64 = 0;
    let mut last: Option<(u64, String)> = None;
    // Rows whose payload is gone and which no chained redaction has explained
    // yet. A `BTreeSet` rather than a `HashSet` so the seq reported is the
    // lowest one and is the same on every run.
    let mut unexplained_nulls: BTreeSet<u64> = BTreeSet::new();
    // The hash found at the anchored seq, captured on the way past.
    let mut hash_at_anchor: Option<String> = None;

    while let Some(row) = rows.next()? {
        let seq_raw: i64 = row.get(0)?;
        let seq = u64::try_from(seq_raw).map_err(|_| LedgerError::SeqOutOfRange)?;
        if seq != expected_seq {
            return Ok(broken(
                rows_checked,
                head_seq,
                head_hash,
                seq,
                BreakReason::SeqGap {
                    expected: expected_seq,
                },
            ));
        }

        let ts_ms: i64 = row.get(1)?;
        let kind: String = row.get(2)?;
        let agent_id: Option<String> = row.get(3)?;
        let payload_hash: String = row.get(5)?;
        let prev_hash: String = row.get(6)?;
        let stored_hash: String = row.get(7)?;
        let snapshot_id: Option<String> = row.get(8)?;
        let snapshot_hash: Option<String> = row.get(9)?;

        if prev_hash != expected_prev {
            return Ok(broken(
                rows_checked,
                head_seq,
                head_hash,
                seq,
                BreakReason::PrevHashMismatch {
                    expected: expected_prev,
                    found: prev_hash,
                },
            ));
        }

        let recomputed = hash::row_hash(&hash::RowHashInput {
            prev_hash: &prev_hash,
            seq,
            kind: &kind,
            ts_ms,
            agent_id: agent_id.as_deref(),
            payload_hash: &payload_hash,
            snapshot_id: snapshot_id.as_deref(),
            snapshot_hash: snapshot_hash.as_deref(),
        });
        if recomputed != stored_hash {
            return Ok(broken(
                rows_checked,
                head_seq,
                head_hash,
                seq,
                BreakReason::RowHashMismatch {
                    expected: recomputed,
                    found: stored_hash,
                },
            ));
        }

        // The payload sits beside the chain (R5). Present must hash to what the
        // row committed to; absent is held against the row until a chained
        // redaction explains it, because the only thing that can excuse a
        // missing payload is a record an attacker would have to forge a hash to
        // write.
        match row.get_ref(4).map(|value| value.as_bytes_or_null()) {
            Ok(Ok(None)) => {
                unexplained_nulls.insert(seq);
            }
            Ok(Ok(Some(bytes))) => {
                let actual = hash::payload_hash(bytes);
                if actual != payload_hash {
                    return Ok(broken(
                        rows_checked,
                        head_seq,
                        head_hash,
                        seq,
                        BreakReason::PayloadHashMismatch {
                            expected: payload_hash,
                            found: actual,
                        },
                    ));
                }
                // Hash-verified, so this row is what was written. A redaction
                // is always appended after the row it nulls, so one forward
                // pass is enough.
                if kind == EventKind::PayloadRedacted.as_str()
                    && let Some(covered) = redacted_seq(bytes)
                {
                    unexplained_nulls.remove(&covered);
                }
            }
            Ok(Err(_)) => {
                return Ok(broken(
                    rows_checked,
                    head_seq,
                    head_hash,
                    seq,
                    BreakReason::PayloadUnreadable,
                ));
            }
            Err(error) => return Err(error.into()),
        }

        if anchor.is_some_and(|anchor| anchor.seq == seq) {
            hash_at_anchor = Some(stored_hash.clone());
        }

        rows_checked += 1;
        expected_seq = seq + 1;
        expected_prev = stored_hash.clone();
        last = Some((seq, stored_hash));
    }

    // Reported after the walk rather than at the row, because the redaction
    // that would have explained it is a *later* row. Seqs are contiguous by
    // here — a gap returns above — so the number of rows that verified before
    // the offending one is exactly `seq - 1`.
    if let Some(&seq) = unexplained_nulls.first() {
        return Ok(broken(
            seq.saturating_sub(1),
            head_seq,
            head_hash,
            seq,
            BreakReason::UnrecordedTombstone,
        ));
    }

    let (last_seq, last_hash) = last.unwrap_or((0, genesis.to_owned()));

    // The anchor is checked before the head: a rewound `chain_head` is exactly
    // what a truncation fixes up to keep `HeadMismatch` quiet, so the anchor is
    // the more informative story when both fire.
    if let Some(anchor) = anchor {
        let found = if anchor.seq > last_seq {
            Some((last_seq, last_hash.clone()))
        } else {
            let at_anchor = if anchor.seq == 0 {
                genesis.to_owned()
            } else {
                hash_at_anchor.clone().unwrap_or_default()
            };
            (at_anchor != anchor.hash).then_some((anchor.seq, at_anchor))
        };
        if let Some((found_seq, found_hash)) = found {
            return Ok(broken(
                rows_checked,
                head_seq,
                head_hash,
                anchor.seq,
                BreakReason::HeadBehindAnchor {
                    anchor_seq: anchor.seq,
                    anchor_hash: anchor.hash.clone(),
                    found_seq,
                    found_hash,
                },
            ));
        }

        // Bound the head from ABOVE as well. `note_head` runs after the
        // commit, so a crash can legitimately leave the anchor exactly one row
        // behind; anything further is rows the anchor never witnessed, which
        // is what a forged tail looks like.
        let ahead = last_seq.saturating_sub(anchor.seq);
        if ahead > ANCHOR_LAG_TOLERANCE {
            return Ok(broken(
                rows_checked,
                head_seq,
                head_hash,
                anchor.seq,
                BreakReason::ChainAheadOfAnchor {
                    anchor_seq: anchor.seq,
                    head_seq: last_seq,
                    ahead: ahead - ANCHOR_LAG_TOLERANCE,
                },
            ));
        }
    }

    if last_seq != head_seq || last_hash != head_hash {
        return Ok(ChainReport {
            rows_checked,
            head_seq,
            head_hash: head_hash.clone(),
            first_break: Some(ChainBreak {
                seq: last_seq,
                reason: BreakReason::HeadMismatch {
                    expected_seq: last_seq,
                    expected_hash: last_hash,
                    found_seq: head_seq,
                    found_hash: head_hash,
                },
            }),
        });
    }

    Ok(ChainReport {
        rows_checked,
        head_seq,
        head_hash,
        first_break: None,
    })
}

/// Assemble a report that stops at `seq`. Kept as one helper so every early
/// return reports `rows_checked` the same way: rows that verified, not rows read.
fn broken(
    rows_checked: u64,
    head_seq: u64,
    head_hash: String,
    seq: u64,
    reason: BreakReason,
) -> ChainReport {
    ChainReport {
        rows_checked,
        head_seq,
        head_hash,
        first_break: Some(ChainBreak { seq, reason }),
    }
}
