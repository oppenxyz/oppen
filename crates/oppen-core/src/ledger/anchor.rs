//! Where the chain head is remembered *outside* the database file.
//!
//! # The hole this closes
//!
//! `docs/spec.md` item 29 promises tamper evidence. Walking the chain proves a
//! row was not rewritten and that no row is missing from the middle — but the
//! walk's only reference for "and this is the end" is `chain_head`, which lives
//! in the same user-writable file as `events`. Deleting the last eight rows and
//! rewinding `chain_head` to match therefore verified clean, and the next
//! append re-issued those seqs with different content. Erasing the most recent
//! history is exactly the edit an operator would most want to hide, so it was
//! the one case tamper evidence did not cover.
//!
//! The fix is to keep `(seq, hash)` somewhere the writer of the SQLite file
//! does not automatically reach, and to refuse to believe a chain that has gone
//! backwards relative to it.
//!
//! # What the file sidecar does and does not stop
//!
//! [`FileAnchor`] is the default because `oppen-core` holds no keychain
//! dependency (`docs/decisions.md` R1 keeps this crate headless, and adding one
//! here is a later phase). It is **not a security boundary**. Stated plainly:
//!
//! * It **does** catch a `DELETE FROM events WHERE seq > n` plus a rewound
//!   `chain_head` — the whole suffix-truncation attack — done by anyone who
//!   edits the database and does not know the sidecar exists.
//! * It **does** catch a full-chain rewrite: rehashing every row from a forged
//!   row forward produces a valid chain with a different hash at the anchored
//!   seq, which the anchor names.
//! * It **does** catch restoring an older copy of the database over a newer one.
//! * It **does not** stop anyone who deletes or edits the sidecar as well. It
//!   is an ordinary file next to an ordinary file, owned by the same user.
//! * It **does not** authenticate anything: there is no key and no MAC, so a
//!   forged sidecar is indistinguishable from a real one.
//!
//! The keychain-backed implementation — the same pattern `docs/spec.md` item 3
//! already uses for the HMAC-checked guardrail config — is a later phase and
//! slots in behind [`HeadAnchor`] without touching the ledger. Until it lands,
//! the honest claim is "an edit to the database alone cannot be hidden", not
//! "an edit cannot be hidden".

use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{LedgerError, Result};

/// A remembered chain head: the last seq and its row hash.
///
/// `seq == 0` with the genesis hash is the anchor of an empty chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Seq of the last row the chain was known to have.
    pub seq: u64,
    /// That row's chain hash, or the genesis hash when `seq` is `0`.
    pub hash: String,
}

/// Somewhere to keep the chain head that is not the database file.
///
/// Behind a trait because the storage that actually makes this a security
/// boundary — the OS keychain — cannot be a dependency of this crate today
/// (`docs/decisions.md` R1). The ledger only ever asks for two operations, so
/// the keychain implementation is a later phase rather than a redesign.
pub trait HeadAnchor: Debug + Send + Sync {
    /// Read the anchor, or `None` if none has been written yet.
    fn load(&self) -> Result<Option<Anchor>>;

    /// Record a new head. Called after every committed append.
    fn store(&self, anchor: &Anchor) -> Result<()>;
}

/// The default anchor: a small JSON file beside the database.
///
/// Read [the module docs](self) before treating this as protection. It raises
/// the cost of erasing history from "edit one file" to "edit two files"; it
/// does not make the history unforgeable.
#[derive(Debug, Clone)]
pub struct FileAnchor {
    path: PathBuf,
}

impl FileAnchor {
    /// The sidecar for a database file: the same path with `.anchor` appended.
    pub fn beside(database: &Path) -> Self {
        let mut path = database.as_os_str().to_owned();
        path.push(".anchor");
        Self {
            path: PathBuf::from(path),
        }
    }

    /// An anchor at an explicit path, for a caller keeping it on other media.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Where this anchor is stored.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl HeadAnchor for FileAnchor {
    fn load(&self) -> Result<Option<Anchor>> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(LedgerError::Io(error)),
        }
    }

    /// Write to a temporary file, flush it to the platter, then rename.
    ///
    /// A torn anchor is worse than a missing one: it fails to parse and every
    /// later verification errors instead of reporting. The `sync_all` before
    /// the rename is what makes a crash leave either the old anchor or the new
    /// one. It costs one fsync per appended event, on top of the `synchronous =
    /// FULL` commit the ledger already pays for.
    fn store(&self, anchor: &Anchor) -> Result<()> {
        let encoded = serde_json::to_vec(anchor)?;
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".new");
        let temporary = PathBuf::from(temporary);
        {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}
