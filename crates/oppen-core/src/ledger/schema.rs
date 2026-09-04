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

/// Every migration in order. The index of a statement is the `user_version` it
/// produces, so `MIGRATIONS.len()` is the version this build writes.
const MIGRATIONS: &[&str] = &[V1];

/// Bring the database up to the schema this build expects.
///
/// Refuses a database written by a newer build rather than guessing at it: a
/// forward-migrated ledger opened by an older oppen would drop columns from the
/// hash preimage and report the whole chain as broken.
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
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

/// Apply one migration and stamp the version it produces.
///
/// `PRAGMA user_version` takes no bound parameter, so the version is formatted
/// in. The value is a `usize` from this file's own constant, never user input.
fn apply(conn: &Connection, statements: &str, version: usize) -> Result<()> {
    conn.execute_batch(statements)?;
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
    Ok(())
}
