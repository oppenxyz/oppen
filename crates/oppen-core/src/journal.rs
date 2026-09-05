//! The per-agent scratchpad (`docs/spec.md` item 21).
//!
//! Agents are amnesiac across sessions: a model that reasoned its way to
//! "ETH funding inverts around 03:00 UTC" on Tuesday starts Wednesday with
//! nothing. This is where it puts that, and gets it back.
//!
//! **Not the ledger.** The ledger is append-only, hash-chained and verified,
//! and those properties are the whole of its claim (`AGENTS.md` invariant 7).
//! A journal is mutable by design — remembering the same key twice replaces
//! it — so it lives in its own per-network file rather than putting a
//! rewritable table inside the file whose point is that nothing is rewritten.
//! Per-network for the same reason the ledger is (`docs/decisions.md` R4): a
//! note reasoned from testnet prices is not a mainnet note.
//!
//! **Scoped per agent**, the way events are (C6). One agent's notes are its
//! own; nothing here lets an agent read another's.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// The longest note the journal will hold.
///
/// Same reasoning and the same order of magnitude as the guardrail reason
/// bound: a write that costs no rate token and is kept forever is unlimited
/// free disk if nothing bounds it. Four times the reason limit, because a
/// note is meant to carry more than a sentence.
pub const MAX_VALUE_BYTES: usize = 8_192;

/// The longest key. A key is a label, not a payload.
pub const MAX_KEY_BYTES: usize = 256;

/// How many notes one agent may keep.
///
/// Bounded for the same reason as the value: without a ceiling, an agent that
/// remembers on a loop fills the disk. Generous enough that no honest use
/// meets it, and the refusal names the limit so an agent that does can
/// replace a key rather than guess.
pub const MAX_ENTRIES_PER_AGENT: usize = 1_000;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the key is {len_bytes} bytes, past the {MAX_KEY_BYTES}-byte limit")]
    KeyTooLong { len_bytes: usize },
    #[error("the note is {len_bytes} bytes, past the {MAX_VALUE_BYTES}-byte limit")]
    ValueTooLong { len_bytes: usize },
    #[error("the key is empty")]
    EmptyKey,
    #[error(
        "this agent already keeps {MAX_ENTRIES_PER_AGENT} notes; replace one by \
         remembering its key again"
    )]
    Full,
}

/// One remembered note.
///
/// Field order is the wire order (`AGENTS.md` invariant 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub key: String,
    pub value: String,
    /// When it was last written, in unix milliseconds. A note that replaced an
    /// earlier one carries the newer time: the journal keeps what is true now,
    /// not a history — the ledger is where history lives.
    pub written_at_ms: i64,
}

/// The per-network journal file.
#[derive(Debug)]
pub struct Journal {
    conn: Mutex<Connection>,
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// The same schema and the same SQL, without a file. Test-only: the app
    /// opens the per-network database file.
    #[cfg(test)]
    fn in_memory() -> Result<Self, JournalError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, JournalError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS journal (
                 agent          TEXT NOT NULL,
                 key            TEXT NOT NULL,
                 value          TEXT NOT NULL,
                 written_at_ms  INTEGER NOT NULL,
                 PRIMARY KEY (agent, key)
             );",
        )?;
        Ok(Journal {
            conn: Mutex::new(conn),
        })
    }

    /// Write a note, replacing any note this agent already keeps under `key`.
    ///
    /// Replacing rather than appending is the point: a scratchpad holds what
    /// is true now. What happened, and when, is the ledger's job.
    pub fn remember(
        &self,
        agent: &str,
        key: &str,
        value: &str,
        now_ms: i64,
    ) -> Result<(), JournalError> {
        if key.is_empty() {
            return Err(JournalError::EmptyKey);
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(JournalError::KeyTooLong {
                len_bytes: key.len(),
            });
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(JournalError::ValueTooLong {
                len_bytes: value.len(),
            });
        }

        let conn = self.lock();
        // Counted before the write and only for a key that is not already
        // held, so an agent at the ceiling can still correct a note it has —
        // being full must not also mean being stuck with what is there.
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM journal WHERE agent = ?1 AND key = ?2)",
            params![agent, key],
            |row| row.get(0),
        )?;
        if !exists {
            let held: i64 = conn.query_row(
                "SELECT COUNT(*) FROM journal WHERE agent = ?1",
                params![agent],
                |row| row.get(0),
            )?;
            if held as usize >= MAX_ENTRIES_PER_AGENT {
                return Err(JournalError::Full);
            }
        }

        conn.execute(
            "INSERT INTO journal (agent, key, value, written_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (agent, key) DO UPDATE SET value = ?3, written_at_ms = ?4",
            params![agent, key, value, now_ms],
        )?;
        Ok(())
    }

    /// One note by key, or `None` when this agent keeps none under it.
    pub fn recall(&self, agent: &str, key: &str) -> Result<Option<Note>, JournalError> {
        let conn = self.lock();
        let note = conn
            .query_row(
                "SELECT key, value, written_at_ms FROM journal WHERE agent = ?1 AND key = ?2",
                params![agent, key],
                row_to_note,
            )
            .optional()?;
        Ok(note)
    }

    /// Every note this agent keeps, newest first.
    ///
    /// Ordered by write time so an agent reading its whole journal sees what
    /// it last thought about first; the key breaks ties so two notes written
    /// in the same millisecond still have one order (invariant 6).
    pub fn recall_all(&self, agent: &str) -> Result<Vec<Note>, JournalError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT key, value, written_at_ms FROM journal WHERE agent = ?1
             ORDER BY written_at_ms DESC, key ASC",
        )?;
        let notes = statement
            .query_map(params![agent], row_to_note)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(notes)
    }

    /// A poisoned lock means a previous caller panicked mid-statement. The
    /// journal holds no invariant a panic could half-break — every write is a
    /// single statement — so the guard is taken back rather than failing the
    /// call and losing an agent's notes to an unrelated bug.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn row_to_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        key: row.get(0)?,
        value: row.get(1)?,
        written_at_ms: row.get(2)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_756_000_000_000;

    fn journal() -> Journal {
        Journal::in_memory().expect("journal")
    }

    #[test]
    fn a_note_survives_being_written_and_read_back() {
        let j = journal();
        j.remember("a", "funding", "ETH inverts around 03:00 UTC", NOW)
            .expect("remember");
        let note = j.recall("a", "funding").expect("recall").expect("present");
        assert_eq!(note.value, "ETH inverts around 03:00 UTC");
        assert_eq!(note.written_at_ms, NOW);
    }

    /// The scratchpad holds what is true now. An agent correcting itself must
    /// not have to invent a new key, and must not end up with both.
    #[test]
    fn remembering_the_same_key_replaces_it() {
        let j = journal();
        j.remember("a", "k", "first", NOW).expect("first");
        j.remember("a", "k", "second", NOW + 1).expect("second");

        assert_eq!(
            j.recall("a", "k").expect("recall").expect("present").value,
            "second"
        );
        assert_eq!(j.recall_all("a").expect("all").len(), 1, "not appended");
    }

    /// `docs/decisions.md` C6's rule, in the other store: one agent's notes
    /// are its own. A journal that leaked would leak reasoning, which is the
    /// most private thing an agent writes.
    #[test]
    fn one_agent_cannot_read_anothers_notes() {
        let j = journal();
        j.remember("alpha", "edge", "alpha's edge", NOW).expect("a");
        j.remember("beta", "edge", "beta's edge", NOW).expect("b");

        assert_eq!(
            j.recall("alpha", "edge")
                .expect("recall")
                .expect("note")
                .value,
            "alpha's edge"
        );
        assert_eq!(j.recall_all("alpha").expect("all").len(), 1);
        assert!(
            j.recall("gamma", "edge").expect("recall").is_none(),
            "an agent with no notes reads none of anybody else's"
        );
    }

    /// The same key under two agents is two notes, not a collision.
    #[test]
    fn the_same_key_under_two_agents_is_two_notes() {
        let j = journal();
        j.remember("alpha", "k", "one", NOW).expect("a");
        j.remember("beta", "k", "two", NOW).expect("b");
        assert_eq!(j.recall("alpha", "k").expect("r").expect("n").value, "one");
        assert_eq!(j.recall("beta", "k").expect("r").expect("n").value, "two");
    }

    #[test]
    fn notes_come_back_newest_first_with_a_stable_tie_break() {
        let j = journal();
        j.remember("a", "old", "1", NOW).expect("w");
        j.remember("a", "zebra", "2", NOW + 10).expect("w");
        j.remember("a", "apple", "3", NOW + 10).expect("w");

        let notes = j.recall_all("a").expect("all");
        let keys: Vec<&str> = notes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(keys, ["apple", "zebra", "old"]);
    }

    /// A write that costs no rate token and is kept forever is unlimited free
    /// disk if nothing bounds it — the same hole the reason bound closes.
    #[test]
    fn an_oversized_note_is_refused_with_its_limit_named() {
        let j = journal();
        let huge = "x".repeat(MAX_VALUE_BYTES + 1);
        match j.remember("a", "k", &huge, NOW) {
            Err(JournalError::ValueTooLong { len_bytes }) => {
                assert_eq!(len_bytes, MAX_VALUE_BYTES + 1);
            }
            other => panic!("expected ValueTooLong, got {other:?}"),
        }
        assert!(
            j.recall("a", "k").expect("recall").is_none(),
            "nothing kept"
        );

        let long_key = "k".repeat(MAX_KEY_BYTES + 1);
        assert!(matches!(
            j.remember("a", &long_key, "v", NOW),
            Err(JournalError::KeyTooLong { .. })
        ));
        assert!(matches!(
            j.remember("a", "", "v", NOW),
            Err(JournalError::EmptyKey)
        ));
    }

    /// Exactly at the limit is allowed; the refusal is for going past it.
    #[test]
    fn a_note_exactly_at_the_limit_is_kept() {
        let j = journal();
        let exact = "x".repeat(MAX_VALUE_BYTES);
        j.remember("a", "k", &exact, NOW).expect("at the limit");
        assert_eq!(
            j.recall("a", "k").expect("r").expect("n").value.len(),
            MAX_VALUE_BYTES
        );
    }

    /// Being full must not also mean being stuck with what is there: an agent
    /// at the ceiling can still correct a note it already keeps.
    #[test]
    fn a_full_journal_refuses_new_keys_and_still_accepts_corrections() {
        let j = journal();
        for n in 0..MAX_ENTRIES_PER_AGENT {
            j.remember("a", &format!("k{n}"), "v", NOW).expect("fill");
        }
        assert!(matches!(
            j.remember("a", "one-more", "v", NOW),
            Err(JournalError::Full)
        ));

        j.remember("a", "k0", "corrected", NOW + 1)
            .expect("replacing an existing key is not a new entry");
        assert_eq!(
            j.recall("a", "k0").expect("r").expect("n").value,
            "corrected"
        );

        // And the ceiling is per agent, not global.
        j.remember("b", "mine", "v", NOW)
            .expect("another agent has its own room");
    }

    #[test]
    fn recalling_what_was_never_written_is_none_rather_than_an_error() {
        let j = journal();
        assert!(j.recall("a", "nothing").expect("recall").is_none());
        assert!(j.recall_all("a").expect("all").is_empty());
    }
}
