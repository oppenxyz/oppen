//! Schema and migrations for the ledger database.
//!
//! One file per network (`docs/decisions.md` R4), so this schema is applied
//! twice on a machine running testnet and mainnet side by side and the two
//! never share a rowid space.
//!
//! Migrations are a plain ordered list keyed off SQLite's `user_version`. A
//! heavier migration framework would buy nothing here: the ledger is
//! append-only, so a migration can add tables and indices but must never
//! rewrite a chained row.

use rusqlite::Connection;

use super::{LedgerError, Result};

/// The initial schema: the chain, its head, the prunable side tables and the
/// sub-account registry.
///
/// Everything chained lives in `events`. Candles, book snapshots and
/// projections stay out of it (`docs/decisions.md` R5) — a chain over
/// recomputable data would make pruning them indistinguishable from tampering.
const V1: &str = r#"
CREATE TABLE ledger_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- The one append-only, hash-chained event table. `docs/spec.md` D6: this is the
-- single source for get_events, the activity stream and the audit export.
-- `seq` is an explicit INTEGER PRIMARY KEY (a rowid alias) assigned from
-- chain_head inside the append transaction, never left to SQLite's rowid
-- allocator, because a reused rowid would silently rewind every agent cursor.
CREATE TABLE events (
    seq              INTEGER PRIMARY KEY,
    ts_ms            INTEGER NOT NULL,
    kind             TEXT    NOT NULL,
    agent_id         TEXT,
    -- Canonical JSON. NULL only once redacted, which redacted_at records.
    payload          TEXT,
    payload_hash     TEXT    NOT NULL,
    prev_hash        TEXT    NOT NULL,
    hash             TEXT    NOT NULL UNIQUE,
    redacted_at      INTEGER,
    redaction_reason TEXT,
    -- docs/decisions.md R6: decision-time market context by reference.
    snapshot_id      TEXT,
    snapshot_hash    TEXT
);
CREATE INDEX events_ts_ms ON events (ts_ms);
CREATE INDEX events_kind ON events (kind, seq);
CREATE INDEX events_agent ON events (agent_id, seq);

-- The head is a table rather than SELECT MAX(seq) so that the next seq and the
-- previous hash are read under the same row lock the insert takes, and so that
-- deleting the last row cannot hand its seq out a second time.
CREATE TABLE chain_head (
    id   INTEGER PRIMARY KEY CHECK (id = 0),
    seq  INTEGER NOT NULL,
    hash TEXT    NOT NULL
);

-- docs/decisions.md R6 and D-e: the body of a decision-time snapshot is
-- prunable, the reference to it in the chained row is not.
CREATE TABLE book_snapshots (
    snapshot_id   TEXT PRIMARY KEY,
    snapshot_hash TEXT    NOT NULL,
    ts_ms         INTEGER NOT NULL,
    coin          TEXT    NOT NULL,
    body          TEXT    NOT NULL
);
CREATE INDEX book_snapshots_ts_ms ON book_snapshots (ts_ms);

-- docs/spec.md item 9 and specs/history.md 3.3: every disconnect writes a gap
-- with its window so a missing fill is distinguishable from an absent one.
CREATE TABLE feed_gaps (
    gap_id           INTEGER PRIMARY KEY,
    scope            TEXT    NOT NULL,
    opened_ts_ms     INTEGER NOT NULL,
    closed_ts_ms     INTEGER,
    reconciled_ts_ms INTEGER,
    open_seq         INTEGER NOT NULL REFERENCES events (seq),
    close_seq        INTEGER REFERENCES events (seq),
    note             TEXT
);
CREATE INDEX feed_gaps_unreconciled ON feed_gaps (reconciled_ts_ms, opened_ts_ms);
-- A scope has at most one gap open at a time. A flapping socket that opened a
-- second gap orphaned the first: nothing closes a gap but its own id, so it
-- stayed on the reconciler's work list and under the staleness overlay
-- (`docs/spec.md` item 34) forever. Reconnect handling has to be idempotent,
-- and this makes a second open gap unrepresentable rather than merely
-- discouraged.
CREATE UNIQUE INDEX feed_gaps_one_open_per_scope ON feed_gaps (scope) WHERE closed_ts_ms IS NULL;

-- docs/decisions.md R2: the owner discriminator exists before any row
-- references a sub-account, because it is free today and impossible to add
-- cleanly once the ledger has rows pointing here. Both owner columns are
-- nullable and null together: R3 discovers every sub-account under the master,
-- and a discovered account has no agent or workflow owner at all.
CREATE TABLE sub_accounts (
    address              TEXT PRIMARY KEY,
    name                 TEXT    NOT NULL,
    owner_type           TEXT CHECK (owner_type IN ('agent', 'workflow')),
    owner_id             TEXT,
    recorded             INTEGER NOT NULL DEFAULT 0,
    provisioned_by_oppen INTEGER NOT NULL DEFAULT 0,
    active               INTEGER NOT NULL DEFAULT 1,
    created_ts_ms        INTEGER NOT NULL,
    CHECK ((owner_type IS NULL) = (owner_id IS NULL))
);
"#;

/// `events.idem_key` and the partial unique index that enforces it.
///
/// Idempotence for fills used to be an in-memory set of every `tid` in the
/// chain, consulted before the append. That is check-then-write, and the window
/// between the two is exactly when a resumed socket replays its subscribe
/// snapshot. A duplicate that lands in an append-only chain cannot be taken back
/// out, so the check and the write have to be one operation, and the database is
/// the only place they can be. `feed_gaps_one_open_per_scope` above already
/// makes the same move for gaps.
///
/// The index is **partial** because only a row with a venue identifier to key on
/// carries a key. Everything else stores NULL, and NULLs never collide in a
/// SQLite unique index, so nothing else in the chain is constrained by this.
///
/// `idem_key` is deliberately **not** in the row-hash preimage. It is a function
/// of the payload the chain already commits to, so chaining it would add nothing
/// and would invalidate every hash already written — including the ones this
/// migration backfills.
///
/// The backfill keys the fill rows an older build wrote, so upgrading does not
/// re-record history the chain already holds. It keys the **first** row per
/// `(account, tid)` only: a chain that already carries a duplicate keeps it —
/// append-only means it cannot be removed — and the index then stops a third. A
/// fill row that carries neither field at its payload root gets no key; it
/// predates `Ledger::record_fill` being the one door for fills.
const V2: &str = r#"
ALTER TABLE events ADD COLUMN idem_key TEXT;

UPDATE events
   SET idem_key = 'fill:' || json_extract(payload, '$.account')
                          || ':' || json_extract(payload, '$.tid')
 WHERE seq IN (
     SELECT MIN(seq) FROM events
      WHERE kind = 'fill'
        AND json_extract(payload, '$.account') IS NOT NULL
        AND json_extract(payload, '$.tid') IS NOT NULL
      GROUP BY json_extract(payload, '$.account'), json_extract(payload, '$.tid')
 );

CREATE UNIQUE INDEX events_idem_key ON events (idem_key) WHERE idem_key IS NOT NULL;
"#;

// A version boundary is necessary even without a new table: older runtimes
// must refuse a ledger whose durable reservations they cannot enforce.
const V3: &str = r#"
CREATE INDEX events_submission_account
    ON events (json_extract(payload, '$.account'), seq)
    WHERE kind IN ('submission_started', 'submission_resolved');
"#;

/// Every migration in order. `MIGRATIONS.len()` is the version this build writes.
// Reader barrier: a V3 runtime cannot enforce the cumulative pilot budget.
// No row rewrite or new table is needed for these additional chained events.
// V5 also excludes readers that cannot enforce durable pairing revocation.
// V6 requires authenticated registry authority at the signing boundary.
// V7 requires authenticated policy and revision-bound runtime admission.
// V8 requires authenticated pilot consent; legacy history is never auto-adopted.
// V9 requires authenticated, one-shot approval lifecycle authority.
// V10: older writers cannot preserve original request evidence in approvals.
// V11: reviewed candidate commitments and their actual receipt binding.
const MIGRATIONS: &[&str] = &[V1, V2, V3, "", "", "", "", "", "", "", ""];

pub(super) const CURRENT_VERSION: usize = MIGRATIONS.len();

/// Bring the database up to the schema this build expects.
///
/// Refuses a database written by a newer build rather than guessing at it: a
/// forward-migrated ledger opened by an older oppen would drop columns from the
/// hash preimage and report the whole chain as broken.
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let current = supported_version(conn)?;
    for (index, statements) in MIGRATIONS.iter().enumerate().skip(current) {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match apply(conn, statements, index + 1) {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(error) => {
                conn.execute_batch("ROLLBACK")?;
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Shared by the writer/migrator and the console's read-only opener.
pub(super) fn supported_version(conn: &Connection) -> Result<usize> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let current = usize::try_from(current).map_err(|_| LedgerError::SchemaTooNew {
        found: current,
        supported: MIGRATIONS.len(),
    })?;
    if current > MIGRATIONS.len() {
        return Err(LedgerError::SchemaTooNew {
            found: current as i64,
            supported: MIGRATIONS.len(),
        });
    }
    Ok(current)
}

/// Apply one migration and stamp the version it produces.
///
/// `PRAGMA user_version` takes no bound parameter, so the version is formatted
/// in. The value is a `usize` from this file's own constant, never user input.
fn apply(conn: &Connection, statements: &str, version: usize) -> Result<()> {
    conn.execute_batch(statements)?;
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
    Ok(())
}
