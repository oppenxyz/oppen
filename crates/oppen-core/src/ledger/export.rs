//! Audit export.
//!
//! `docs/spec.md` D6 — `get_events`, the activity stream and the audit log are
//! the same table, so the export is a dump of that table and nothing else.
//! CSV is for the spreadsheet an accountant or an auditor will actually open;
//! JSON Lines is for anything that has to read the payload back as structure
//! (`docs/specs/history.md` 3.5).
//!
//! Both stream row by row. An export that has to hold the whole ledger in
//! memory is an export that fails on the ledger that most needs exporting.

use std::io::Write;

use rusqlite::Connection;

use super::{Event, Result, SELECT_EVENT_COLUMNS, event_from_row};

/// Column header, and the field order every CSV row follows. The payload is
/// last because it is the only unbounded field: a human scanning the file in a
/// terminal sees the chain columns before the line wraps.
const CSV_HEADER: &str = "seq,ts_ms,kind,agent_id,payload_hash,prev_hash,hash,snapshot_id,snapshot_hash,redacted_at,redaction_reason,payload";

/// Write the whole chain as RFC 4180 CSV. Returns the number of event rows
/// written, not counting the header.
pub(crate) fn to_csv<W: Write>(conn: &Connection, out: &mut W) -> Result<u64> {
    writeln!(out, "{CSV_HEADER}")?;
    for_each(conn, |event| {
        let payload = match &event.payload {
            Some(value) => serde_json::to_string(value)?,
            None => String::new(),
        };
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            event.seq,
            event.ts_ms,
            field(event.kind.as_str()),
            opt(event.agent_id.as_deref()),
            field(&event.payload_hash),
            field(&event.prev_hash),
            field(&event.hash),
            opt(event.snapshot_id.as_deref()),
            opt(event.snapshot_hash.as_deref()),
            event.redacted_at.map_or(String::new(), |ts| ts.to_string()),
            opt(event.redaction_reason.as_deref()),
            field(&payload),
        )?;
        Ok(())
    })
}

/// Write the whole chain as JSON Lines, one event object per line.
///
/// Key order is the declaration order of [`Event`], which is stable across
/// builds — `AGENTS.md` invariant 6 asks for deterministic JSON, and a diff
/// between two exports should show changed rows, not reshuffled keys.
pub(crate) fn to_jsonl<W: Write>(conn: &Connection, out: &mut W) -> Result<u64> {
    for_each(conn, |event| {
        serde_json::to_writer(&mut *out, event)?;
        out.write_all(b"\n")?;
        Ok(())
    })
}

/// Stream every event in chain order through `sink`.
fn for_each<F>(conn: &Connection, mut sink: F) -> Result<u64>
where
    F: FnMut(&Event) -> Result<()>,
{
    let mut statement = conn.prepare(&format!(
        "SELECT {SELECT_EVENT_COLUMNS} FROM events ORDER BY seq ASC"
    ))?;
    let mut rows = statement.query([])?;
    let mut written = 0u64;
    while let Some(row) = rows.next()? {
        let event = event_from_row(row)?;
        sink(&event)?;
        written += 1;
    }
    Ok(written)
}

/// Quote a CSV field only when it needs quoting, doubling any embedded quote.
fn field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// An absent value is an empty CSV field. A literal `null` would be
/// indistinguishable from an agent whose id is the string "null".
fn opt(value: Option<&str>) -> String {
    value.map_or_else(String::new, field)
}
