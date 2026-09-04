//! Chain verification: walk from genesis and report the first broken link.
//!
//! `docs/spec.md` item 29 — the ledger is tamper-evident, not tamper-proof. The
//! database file belongs to the user and anyone with the user's shell can edit
//! it. What the chain buys is that an edit cannot be hidden, and that the report
//! names the row where the history stops being trustworthy.

use rusqlite::Connection;
use serde::Serialize;

use super::{LedgerError, Result, hash};

/// Why the walk stopped.
///
/// Each variant is a distinct forensic story, which is the point: "row 4,812 is
/// missing" and "row 4,812 was rewritten" call for different responses, and a
/// single `chain_broken` boolean would collapse them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum BreakReason {
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
    /// The payload is gone but no redaction was recorded. An authorised
    /// redaction stamps `redacted_at` and appends its own chained event; a
    /// silent NULL is someone deleting evidence.
    #[error("payload is null but no redaction was recorded")]
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
const WALK_COLUMNS: &str = "seq, ts_ms, kind, agent_id, payload, payload_hash, \
     prev_hash, hash, snapshot_id, snapshot_hash, redacted_at";

/// Walk the chain from genesis and report the first broken link.
///
/// Streams the rows rather than collecting them: an audited ledger is expected
/// to be long-lived, and a verification that needs the whole history resident
/// is a verification that stops being run.
pub(crate) fn walk(conn: &Connection, genesis: &str) -> Result<ChainReport> {
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
        let redacted_at: Option<i64> = row.get(10)?;

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

        // The payload sits beside the chain (R5). Absent is legitimate only
        // when a redaction was recorded; present must hash to what the row
        // committed to.
        match row.get_ref(4).map(|value| value.as_bytes_or_null()) {
            Ok(Ok(None)) => {
                if redacted_at.is_none() {
                    return Ok(broken(
                        rows_checked,
                        head_seq,
                        head_hash,
                        seq,
                        BreakReason::UnrecordedTombstone,
                    ));
                }
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

        rows_checked += 1;
        expected_seq = seq + 1;
        expected_prev = stored_hash.clone();
        last = Some((seq, stored_hash));
    }

    let (last_seq, last_hash) = last.unwrap_or((0, genesis.to_owned()));
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
