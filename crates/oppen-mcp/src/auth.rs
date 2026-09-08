//! Pairing tokens for the MCP gateway (`docs/spec.md` items 14 and 15).
//!
//! Three properties this module exists to hold, all from item 15's
//! "default-deny pairing … token revocation closes live connections":
//!
//! 1. **The store never holds a token.** It holds a SHA3-256 digest of one. A
//!    memory dump, a log line or a `Debug` print of [`TokenStore`] cannot yield
//!    a credential that still works.
//! 2. **Comparison is constant-time.** Presented tokens are hashed and the
//!    digests compared with [`subtle`], so response timing does not leak how
//!    much of a guessed prefix was right.
//! 3. **Revocation is immediate and reaches live sessions.** Revoking flips a
//!    [`watch`] channel every open session holds, so a streaming connection
//!    that authenticated an hour ago is closed rather than left running until
//!    it next makes a request.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub use oppen_core::ledger::{PairingBinding as Binding, PairingId};
use oppen_core::ledger::{PairingError, PairingJournal};
use oppen_hl::Network;

use sha3::{Digest, Sha3_256};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use zeroize::Zeroizing;

/// Bytes of entropy in a pairing token. 256 bits: the token is a bearer
/// credential with nothing but the OS accept queue in front of it.
const TOKEN_BYTES: usize = 32;

/// A freshly minted token, returned once at pairing time and never recoverable
/// afterwards — the store keeps only its digest.
///
/// The secret is wrapped in [`Zeroizing`] so it is wiped when the caller drops
/// it, and [`fmt::Debug`] is written by hand so the secret cannot reach a log
/// through a `#[derive]` on some enclosing type.
pub struct IssuedToken {
    pub id: PairingId,
    secret: Zeroizing<String>,
}

impl IssuedToken {
    /// The bearer value to hand to the agent. Call once, at the approve
    /// dialog; oppen has no way to show it again.
    pub fn reveal(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedToken")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Why an `Authorization` header did not authenticate.
///
/// One variant per caller behaviour, per `AGENTS.md` leanness rule 5. A
/// malformed header and an unknown token are both "this request is not from a
/// paired agent" and share [`AuthError::Unauthenticated`], because answering
/// them differently would tell an unauthenticated caller whether a token
/// exists. `Revoked` is separate only because the operator revoked it
/// deliberately and the console says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("not a paired agent")]
    Unauthenticated,
    #[error("pairing was revoked")]
    Revoked,
}

/// The OS CSPRNG was unavailable, so no token was minted.
///
/// Fails closed: a weak bearer token is unrepresentable rather than merely
/// discouraged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("OS entropy unavailable, refusing to mint a pairing token: {0}")]
pub struct EntropyUnavailable(String);

#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error(transparent)]
    Entropy(#[from] EntropyUnavailable),
    #[error(transparent)]
    Persistence(#[from] PairingError),
    #[error("pairing authority is closed or its clock is invalid; reopen required")]
    Closed,
}

struct Record {
    digest: [u8; 32],
    /// Flips to `true` on revoke. Every live session for this pairing holds a
    /// receiver.
    revoked_tx: watch::Sender<bool>,
    /// Who this token speaks for. Fixed at pairing: rebinding a live token to
    /// a different agent would move an agent's authority without the operator
    /// re-approving it, so a new binding is a new pairing.
    binding: Binding,
}

/// The set of pairings the gateway will accept, and the authority that revokes
/// them.
pub struct TokenStore {
    records: HashMap<PairingId, Record>,
    journal: Arc<PairingJournal>,
    closed: bool,
    #[cfg(test)]
    before_publish: Option<fn()>,
}

impl TokenStore {
    /// Consume the exclusive durable owner. Cached records are valid under
    /// that lease; this is not detection of out-of-band database tampering.
    ///
    /// Performs synchronous disk I/O. Async operator callers must run this
    /// off async worker threads and await completion before publishing the store.
    pub fn open(journal: PairingJournal) -> Result<Self, PairingError> {
        let records = journal
            .records()?
            .into_iter()
            .map(|record| {
                let (revoked_tx, _) = watch::channel(record.revoked_at_ms.is_some());
                (
                    record.id,
                    Record {
                        digest: record.digest,
                        revoked_tx,
                        binding: record.binding,
                    },
                )
            })
            .collect();
        Ok(Self {
            records,
            journal: Arc::new(journal),
            closed: false,
            #[cfg(test)]
            before_publish: None,
        })
    }

    pub fn network(&self) -> Network {
        self.journal.network()
    }

    /// Whether a single-account desktop pump covers every retained binding.
    /// Revoked records still count because supervision may need their cleanup.
    /// An empty store has no cancellation target, so it cannot supervise one.
    pub fn supports_binding(&self, binding: &Binding) -> bool {
        !self.closed
            && !self.records.is_empty()
            && self
                .records
                .values()
                .all(|record| record.binding == *binding)
    }

    // The earliest live same-binding pairing is selected once for a native
    // review. Confirmation pins that ID and never falls back after revocation.
    pub(crate) fn operator_authority(
        &self,
        binding: &Binding,
    ) -> Result<SessionAuthority, AuthError> {
        if self.closed {
            return Err(AuthError::Unauthenticated);
        }
        let (id, record) = self
            .records
            .iter()
            .filter(|(_, record)| record.binding == *binding && !*record.revoked_tx.borrow())
            .min_by_key(|(id, _)| id.issued_seq)
            .ok_or(AuthError::Unauthenticated)?;
        Ok(SessionAuthority {
            id: *id,
            binding: record.binding.clone(),
            revoked: record.revoked_tx.subscribe(),
            _journal: self.journal.clone(),
        })
    }

    pub(crate) fn check_authority(&self, authority: &SessionAuthority) -> Result<(), AuthError> {
        if self.closed || !Arc::ptr_eq(&self.journal, &authority._journal) {
            return Err(AuthError::Unauthenticated);
        }
        let record = self
            .records
            .get(&authority.id)
            .ok_or(AuthError::Unauthenticated)?;
        if record.binding != authority.binding {
            return Err(AuthError::Unauthenticated);
        }
        if *record.revoked_tx.borrow() {
            return Err(AuthError::Revoked);
        }
        Ok(())
    }

    /// Owned bindings for pause enforcement, including revoked pairings whose
    /// resting orders still need cancellation. No credentials leave the store.
    pub(crate) fn bindings(&self) -> Vec<Binding> {
        self.records
            .values()
            .map(|record| record.binding.clone())
            .collect()
    }

    /// Mint a pairing for one named agent on one account.
    ///
    /// The caller shows [`IssuedToken::reveal`] once and then drops it; from
    /// here the store can only recognise the token, never reproduce it. Item
    /// 15's approve dialog is what calls this: it names the agent, binds the
    /// container, and assigns the guardrails on the engine before the token
    /// exists. There is no unbound token — default-deny means an agent oppen
    /// has not been told about has no credential to present.
    ///
    /// Performs synchronous disk I/O. Async operator callers must run this
    /// off async worker threads and await completion before publishing a result.
    /// Canceling the awaiting future does not cancel or roll back durable issuance.
    pub fn issue(&mut self, binding: Binding) -> Result<IssuedToken, TokenStoreError> {
        let mut mutation = Mutation::begin(self)?;
        let mut raw = Zeroizing::new([0u8; TOKEN_BYTES]);
        getrandom::getrandom(raw.as_mut()).map_err(|e| EntropyUnavailable(e.to_string()))?;
        let secret = Zeroizing::new(hex_of(raw.as_ref()));
        let digest = digest_of(secret.as_bytes());
        let persisted = mutation.store.journal.issue(binding, digest, now_ms()?)?;
        #[cfg(test)]
        mutation.before_publish();
        let id = persisted.id;
        let (revoked_tx, _) = watch::channel(false);
        mutation.store.records.insert(
            id,
            Record {
                digest: persisted.digest,
                revoked_tx,
                binding: persisted.binding,
            },
        );
        mutation.complete = true;
        Ok(IssuedToken { id, secret })
    }

    /// Authenticate a presented bearer value.
    ///
    /// Hashes the candidate and compares digests in constant time against every
    /// record. The loop deliberately does not break on a match: an early exit
    /// would make the response time depend on the matching pairing's position
    /// in the map.
    pub fn authenticate(&self, presented: &str) -> Result<Session, AuthError> {
        if self.closed {
            return Err(AuthError::Unauthenticated);
        }
        let candidate = digest_of(presented.as_bytes());

        let mut matched: Option<(PairingId, &Record)> = None;
        for (id, record) in &self.records {
            if bool::from(record.digest.ct_eq(&candidate)) {
                matched = Some((*id, record));
            }
        }

        let (id, record) = matched.ok_or(AuthError::Unauthenticated)?;
        if *record.revoked_tx.borrow() {
            return Err(AuthError::Revoked);
        }
        Ok(Session {
            id,
            binding: record.binding.clone(),
            authority: SessionAuthority {
                id,
                binding: record.binding.clone(),
                revoked: record.revoked_tx.subscribe(),
                _journal: self.journal.clone(),
            },
        })
    }

    /// Revoke a pairing. Returns whether it was live before this call.
    ///
    /// The record is **marked, not removed**. Two reasons, and the first is the
    /// load-bearing one: a removed record drops its [`watch::Sender`], and a
    /// dropped sender closes live sessions as a side effect of deallocation
    /// rather than as a decision. Keeping the record makes
    /// [`Session::closed`] observe the flag itself. The second is that a
    /// revoked pairing must stay distinguishable from one that never existed,
    /// so the console can say which it was — that is
    /// [`AuthError::Revoked`], and removing the record would make the variant
    /// unconstructible.
    ///
    /// Performs synchronous disk I/O. Async operator callers must run this
    /// off async worker threads and await completion before publishing a result.
    /// Canceling the awaiting future does not cancel or roll back durable revocation.
    pub fn revoke(&mut self, id: PairingId) -> Result<bool, TokenStoreError> {
        let mut mutation = Mutation::begin(self)?;
        let changed = mutation.store.journal.revoke(id, now_ms()?)?;
        #[cfg(test)]
        mutation.before_publish();
        if changed {
            if let Some(record) = mutation.store.records.get(&id) {
                // `send_replace`, not `send`: `send` returns `Err` and leaves
                // the value untouched when no receiver is alive, so a pairing
                // with no session open at this instant would stay live.
                record.revoked_tx.send_replace(true);
            } else {
                return Err(TokenStoreError::Closed);
            }
        }
        mutation.complete = true;
        Ok(changed)
    }
}

impl fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenStore")
            .field("network", &self.network())
            .field("records", &self.records.len())
            .field("closed", &self.closed)
            .finish()
    }
}

/// Errors and unwinding between durable commit and publication revoke all
/// current in-memory authority. Only a verified reopen may recover it.
struct Mutation<'a> {
    store: &'a mut TokenStore,
    complete: bool,
}

impl<'a> Mutation<'a> {
    fn begin(store: &'a mut TokenStore) -> Result<Self, TokenStoreError> {
        if store.closed {
            return Err(TokenStoreError::Closed);
        }
        Ok(Self {
            store,
            complete: false,
        })
    }

    #[cfg(test)]
    fn before_publish(&mut self) {
        if let Some(hook) = self.store.before_publish.take() {
            hook();
        }
    }
}

impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.store.closed = true;
            for record in self.store.records.values() {
                record.revoked_tx.send_replace(true);
            }
        }
    }
}

/// An authenticated connection's handle to its pairing.
#[derive(Debug)]
pub struct Session {
    pub id: PairingId,
    /// Who the presented token names. Every tool acts as this agent.
    pub binding: Binding,
    authority: SessionAuthority,
}

impl Session {
    pub(crate) fn authority(&self) -> SessionAuthority {
        self.authority.clone()
    }

    /// Resolves when the pairing is revoked. A streaming handler selects on
    /// this so revocation closes the connection instead of waiting for the
    /// agent's next request.
    pub async fn closed(&mut self) {
        self.authority.closed().await;
    }
}

/// A live task retains the journal's exclusive lease, never its mutators.
#[derive(Clone)]
pub(crate) struct SessionAuthority {
    pub(crate) id: PairingId,
    binding: Binding,
    revoked: watch::Receiver<bool>,
    _journal: Arc<PairingJournal>,
}

impl fmt::Debug for SessionAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionAuthority")
            .field("id", &self.id)
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

impl SessionAuthority {
    pub(crate) fn binding(&self) -> &Binding {
        &self.binding
    }

    pub(crate) async fn closed(&mut self) {
        if *self.revoked.borrow() {
            return;
        }
        // `changed()` errors only once every sender is dropped, which happens
        // when the store drops the record — also a reason to close.
        while self.revoked.changed().await.is_ok() {
            if *self.revoked.borrow() {
                return;
            }
        }
    }
}

fn now_ms() -> Result<u64, TokenStoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TokenStoreError::Closed)?;
    u64::try_from(elapsed.as_millis()).map_err(|_| TokenStoreError::Closed)
}

fn digest_of(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha3_256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn hex_of(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_core::guardrail::AgentId;
    use oppen_core::keys::HmacKey;
    use oppen_core::ledger::Ledger;

    fn fixture() -> (tempfile::TempDir, TokenStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = reopen(dir.path(), Network::Testnet);
        (dir, store)
    }

    #[test]
    fn native_review_selects_one_live_pairing_and_never_retargets_its_lease() {
        let (_dir, mut store) = fixture();
        let first = store.issue(binding("alpha")).unwrap();
        let second = store.issue(binding("alpha")).unwrap();
        let pinned = store.operator_authority(&binding("alpha")).unwrap();
        assert_eq!(pinned.id, first.id);
        assert!(store.operator_authority(&binding("beta")).is_err());
        store.revoke(first.id).unwrap();
        assert_eq!(store.check_authority(&pinned), Err(AuthError::Revoked));
        assert_eq!(
            store.operator_authority(&binding("alpha")).unwrap().id,
            second.id
        );
        let (_other_dir, mut other) = fixture();
        other.issue(binding("alpha")).unwrap();
        assert_eq!(
            other.check_authority(&pinned),
            Err(AuthError::Unauthenticated)
        );
        store.revoke(second.id).unwrap();
        assert!(store.supports_binding(&binding("alpha")));
        assert!(store.operator_authority(&binding("alpha")).is_err());
    }

    #[test]
    fn native_pairing_read_guard_serializes_revocation_and_closed_store_refuses() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("alpha")).unwrap();
        let pinned = store.operator_authority(&binding("alpha")).unwrap();
        let store = std::sync::RwLock::new(store);
        {
            let guard = store.try_read().unwrap();
            guard.check_authority(&pinned).unwrap();
            assert!(store.try_write().is_err());
        }
        store.write().unwrap().revoke(issued.id).unwrap();
        assert_eq!(
            store.read().unwrap().check_authority(&pinned),
            Err(AuthError::Revoked)
        );
        store.write().unwrap().closed = true;
        assert_eq!(
            store.read().unwrap().check_authority(&pinned),
            Err(AuthError::Unauthenticated)
        );
        assert!(
            store
                .read()
                .unwrap()
                .operator_authority(&binding("alpha"))
                .is_err()
        );
    }

    #[test]
    fn authentication_and_revocation_survive_physical_reopen() {
        let (dir, mut store) = fixture();
        let issued = store.issue(binding("alpha")).unwrap();
        let id = issued.id;
        assert_eq!(store.network(), Network::Testnet);
        assert_eq!(id.network, Network::Testnet);
        assert!(id.issued_seq > 0);
        drop(store);

        let mut store = reopen(dir.path(), Network::Testnet);
        let session = store.authenticate(issued.reveal()).unwrap();
        assert_eq!(session.id, id);
        assert_eq!(session.binding, binding("alpha"));
        drop(session);
        assert!(store.revoke(id).unwrap());
        assert!(!store.revoke(id).unwrap());
        drop(store);

        let store = reopen(dir.path(), Network::Testnet);
        assert_eq!(
            store.authenticate(issued.reveal()).err(),
            Some(AuthError::Revoked)
        );
        assert_eq!(store.bindings(), [binding("alpha")]);
    }

    #[test]
    fn durable_ids_and_authority_are_network_scoped() {
        for network in [Network::Testnet, Network::Mainnet] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = reopen(dir.path(), network);
            let issued = store.issue(binding("alpha")).unwrap();
            assert_eq!(store.network(), network);
            assert_eq!(issued.id.network, network);
            drop(store);
            assert!(
                reopen(dir.path(), network)
                    .authenticate(issued.reveal())
                    .is_ok()
            );
        }
    }

    fn owner_is_locked(path: &std::path::Path) -> bool {
        let ledger = Arc::new(Ledger::open_at(&path.join("ledger.db"), Network::Testnet).unwrap());
        PairingJournal::open(ledger, Arc::new(HmacKey::from_bytes([42; 32]))).is_err()
    }

    #[tokio::test]
    async fn session_and_task_authority_hold_the_lease_after_store_drop() {
        let (dir, mut store) = fixture();
        let issued = store.issue(binding("alpha")).unwrap();
        let mut session = store.authenticate(issued.reveal()).unwrap();
        let mut task = session.authority();
        assert_eq!(task.id, issued.id);
        assert_eq!(task.binding(), &binding("alpha"));
        drop(store);
        tokio::time::timeout(std::time::Duration::from_millis(50), session.closed())
            .await
            .unwrap();
        assert!(owner_is_locked(dir.path()), "Session still owns the lease");
        drop(session);
        tokio::time::timeout(std::time::Duration::from_millis(50), task.closed())
            .await
            .unwrap();
        assert!(
            owner_is_locked(dir.path()),
            "actual task still owns the lease"
        );
        drop(task);
        let store = reopen(dir.path(), Network::Testnet);
        assert!(
            store.authenticate(issued.reveal()).is_ok(),
            "store drop is not durable revocation"
        );
    }

    #[tokio::test]
    async fn authority_cloned_after_revocation_is_already_closed() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("alpha")).unwrap();
        let session = store.authenticate(issued.reveal()).unwrap();
        store.revoke(issued.id).unwrap();
        let mut task = session.authority();
        tokio::time::timeout(std::time::Duration::from_millis(50), task.closed())
            .await
            .unwrap();
        // Polling a second time must not wait for a new watch version.
        tokio::time::timeout(std::time::Duration::from_millis(50), task.closed())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn persistence_io_failure_closes_all_sessions_and_requires_reopen() {
        for revoke in [false, true] {
            let (dir, mut store) = fixture();
            let alpha = store.issue(binding("alpha")).unwrap();
            let beta = store.issue(binding("beta")).unwrap();
            let mut a = store.authenticate(alpha.reveal()).unwrap();
            let mut b = store.authenticate(beta.reveal()).unwrap();
            // A directory where the coordination file must be makes the next
            // ledger mutation fail with real I/O, before watch publication.
            let lock = dir.path().join("ledger.db.lock");
            std::fs::remove_file(&lock).unwrap();
            std::fs::create_dir(&lock).unwrap();
            let result = if revoke {
                store.revoke(alpha.id).map(|_| ())
            } else {
                store.issue(binding("gamma")).map(drop)
            };
            assert!(matches!(result, Err(TokenStoreError::Persistence(_))));
            assert!(store.authenticate(alpha.reveal()).is_err());
            assert!(store.authenticate(beta.reveal()).is_err());
            assert!(matches!(
                store.issue(binding("gamma")),
                Err(TokenStoreError::Closed)
            ));
            assert!(matches!(
                store.revoke(beta.id),
                Err(TokenStoreError::Closed)
            ));
            tokio::time::timeout(std::time::Duration::from_millis(50), a.closed())
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_millis(50), b.closed())
                .await
                .unwrap();
            drop((a, b, store));
            std::fs::remove_dir(&lock).unwrap();
            let store = reopen(dir.path(), Network::Testnet);
            assert!(store.authenticate(alpha.reveal()).is_ok());
            assert!(store.authenticate(beta.reveal()).is_ok());
        }
    }

    #[tokio::test]
    async fn panic_after_durable_commit_before_publication_poison_closes_every_session() {
        for revoke in [false, true] {
            let (dir, mut store) = fixture();
            let alpha = store.issue(binding("alpha")).unwrap();
            let beta = store.issue(binding("beta")).unwrap();
            let mut a = store.authenticate(alpha.reveal()).unwrap();
            let mut b = store.authenticate(beta.reveal()).unwrap();
            store.before_publish = Some(|| panic!("test: committed but not published"));
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if revoke {
                    store.revoke(alpha.id).map(|_| ())
                } else {
                    store.issue(binding("gamma")).map(drop)
                }
            }));
            assert!(panic.is_err());
            assert!(store.authenticate(alpha.reveal()).is_err());
            assert!(store.authenticate(beta.reveal()).is_err());
            assert!(matches!(
                store.issue(binding("delta")),
                Err(TokenStoreError::Closed)
            ));
            assert!(matches!(
                store.revoke(beta.id),
                Err(TokenStoreError::Closed)
            ));
            tokio::time::timeout(std::time::Duration::from_millis(50), a.closed())
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_millis(50), b.closed())
                .await
                .unwrap();
            drop((a, b, store));
            let store = reopen(dir.path(), Network::Testnet);
            assert!(store.authenticate(beta.reveal()).is_ok());
            if revoke {
                assert_eq!(
                    store.authenticate(alpha.reveal()).err(),
                    Some(AuthError::Revoked)
                );
            } else {
                assert!(store.authenticate(alpha.reveal()).is_ok());
                assert_eq!(store.bindings().len(), 3, "commit preceded the panic");
            }
        }
    }

    #[test]
    fn persisted_digest_is_not_a_bearer_and_exports_and_debug_do_not_reveal_secrets() {
        let (dir, mut store) = fixture();
        let issued = store.issue(binding("alpha")).unwrap();
        let records = store.journal.records().unwrap();
        let digest = hex_of(&records[0].digest);
        assert_eq!(
            store.authenticate(&digest).err(),
            Some(AuthError::Unauthenticated)
        );
        let session = store.authenticate(issued.reveal()).unwrap();
        for rendered in [
            format!("{store:?}"),
            format!("{issued:?}"),
            format!("{session:?}"),
            format!("{:?}", store.bindings()),
            format!("{records:?}"),
        ] {
            assert!(!rendered.contains(issued.reveal()), "bearer leaked");
        }
        let ledger = Ledger::open_at(&dir.path().join("ledger.db"), Network::Testnet).unwrap();
        let mut jsonl = Vec::new();
        ledger.export_jsonl(&mut jsonl).unwrap();
        let mut csv = Vec::new();
        ledger.export_csv(&mut csv).unwrap();
        for exported in [jsonl, csv] {
            assert!(
                !String::from_utf8(exported)
                    .unwrap()
                    .contains(issued.reveal())
            );
        }
        let agent_rows = Arc::new(ledger)
            .agent_view("alpha")
            .get_events(0, 100)
            .unwrap();
        assert!(
            !serde_json::to_string(&agent_rows)
                .unwrap()
                .contains(&digest)
        );
    }

    #[test]
    fn opening_store_reverifies_journal_history() {
        let dir = tempfile::tempdir().unwrap();
        let ledger =
            Arc::new(Ledger::open_at(&dir.path().join("ledger.db"), Network::Testnet).unwrap());
        let journal =
            PairingJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([42; 32]))).unwrap();
        let issued = journal.issue(binding("alpha"), [1; 32], 1).unwrap();
        ledger
            .redact(
                issued.id.issued_seq,
                "test invalidated authority evidence",
                2,
            )
            .unwrap();
        assert!(
            TokenStore::open(journal).is_err(),
            "open must not accept an old verified cache"
        );
    }

    fn reopen(path: &std::path::Path, network: Network) -> TokenStore {
        let ledger = Arc::new(Ledger::open_at(&path.join("ledger.db"), network).unwrap());
        let journal =
            PairingJournal::open(ledger, Arc::new(HmacKey::from_bytes([42; 32]))).unwrap();
        TokenStore::open(journal).unwrap()
    }

    /// A binding for a named agent. The pairing under test is about the token,
    /// not the identity, so every test that does not care uses this.
    fn binding(agent: &str) -> Binding {
        Binding {
            agent: AgentId::new(agent),
            account: "0xbf829199c1ae7f0caf21fb6fc45e10edff25b7d2"
                .parse()
                .expect("address"),
        }
    }

    #[test]
    fn single_account_support_checks_revoked_bindings_and_closed_authority() {
        let (_dir, mut store) = fixture();
        let expected = binding("alpha");
        assert!(!store.supports_binding(&expected));
        store.issue(expected.clone()).unwrap();
        assert!(store.supports_binding(&expected));
        let other = store.issue(binding("beta")).unwrap();
        store.revoke(other.id).unwrap();
        assert!(!store.supports_binding(&expected));
        let (_dir, mut store) = fixture();
        store.closed = true;
        assert!(!store.supports_binding(&expected));
    }

    #[test]
    fn bindings_keep_revoked_records_and_are_an_owned_snapshot() {
        let (_dir, mut store) = fixture();
        let alpha = store.issue(binding("alpha")).expect("token");
        store.issue(binding("beta")).expect("token");
        assert!(store.revoke(alpha.id).unwrap());
        let snapshot = store.bindings();
        store.issue(binding("gamma")).expect("token");
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.contains(&binding("alpha")));
        assert!(snapshot.contains(&binding("beta")));
        assert!(!snapshot.contains(&binding("gamma")));
        assert_eq!(
            store.authenticate(alpha.reveal()).err(),
            Some(AuthError::Revoked)
        );
    }

    #[test]
    fn a_minted_token_authenticates_and_names_its_pairing() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        let session = store.authenticate(issued.reveal()).expect("authenticates");
        assert_eq!(session.id, issued.id);
    }

    /// SHA3-256("") from FIPS 202. Pins the algorithm so a stand-in that
    /// truncates or copies its input fails here rather than silently storing
    /// recoverable material.
    #[test]
    fn the_digest_is_sha3_256() {
        let expected = "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a";
        assert_eq!(hex_of(&digest_of(b"")), expected);
    }

    /// Item 15: a token names one agent on one account, and authenticating
    /// hands that back. Everything downstream acts as whoever this says.
    #[test]
    fn authenticating_returns_the_binding_the_pairing_was_minted_with() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        let session = store.authenticate(issued.reveal()).expect("authenticates");
        assert_eq!(session.binding, binding("agent-alpha"));
    }

    /// Two pairings are two identities, not two keys to the same one.
    #[test]
    fn two_pairings_carry_their_own_agents() {
        let (_dir, mut store) = fixture();
        let alpha = store.issue(binding("agent-alpha")).expect("entropy");
        let beta = store.issue(binding("agent-beta")).expect("entropy");

        let a = store.authenticate(alpha.reveal()).expect("alpha");
        let b = store.authenticate(beta.reveal()).expect("beta");
        assert_eq!(a.binding.agent.as_str(), "agent-alpha");
        assert_eq!(b.binding.agent.as_str(), "agent-beta");
        assert_ne!(a.id, b.id);

        // Revoking one leaves the other acting as itself.
        store.revoke(alpha.id).unwrap();
        assert!(store.authenticate(alpha.reveal()).is_err());
        assert_eq!(
            store
                .authenticate(beta.reveal())
                .expect("beta still paired")
                .binding
                .agent
                .as_str(),
            "agent-beta"
        );
    }

    #[test]
    fn the_store_does_not_hold_the_token() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        let secret = issued.reveal().to_owned();
        let stored = store.records.values().next().expect("one record").digest;

        // No prefix of the token survives in the stored bytes: a digest that
        // copied or truncated its input would fail here.
        for window in 4..=16 {
            let needle = &secret.as_bytes()[..window];
            assert!(
                !stored.windows(window).any(|w| w == needle),
                "{window} bytes of the token appear in the stored digest"
            );
        }
        // And the token is not recoverable by re-encoding the digest either.
        assert!(!secret.contains(&hex_of(&stored)));
    }

    #[test]
    fn a_wrong_token_is_unauthenticated() {
        let (_dir, mut store) = fixture();
        let _issued = store.issue(binding("agent-alpha")).expect("entropy");
        assert_eq!(
            store.authenticate(&"0".repeat(64)).err(),
            Some(AuthError::Unauthenticated)
        );
    }

    #[test]
    fn an_empty_and_a_short_token_do_not_panic() {
        let (_dir, mut store) = fixture();
        let _issued = store.issue(binding("agent-alpha")).expect("entropy");
        for candidate in ["", "0x", "deadbeef", &"f".repeat(200)] {
            assert!(
                store.authenticate(candidate).is_err(),
                "{candidate:?} authenticated"
            );
        }
    }

    #[test]
    fn a_revoked_token_stops_authenticating() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        assert!(store.authenticate(issued.reveal()).is_ok());
        assert!(
            store.revoke(issued.id).unwrap(),
            "revoke reports the pairing was live"
        );
        assert_eq!(
            store.authenticate(issued.reveal()).err(),
            Some(AuthError::Revoked),
            "a revoked pairing must be distinguishable from one that never existed"
        );
        assert!(
            !store.revoke(issued.id).unwrap(),
            "revoking twice reports it was already revoked"
        );
    }

    #[tokio::test]
    async fn revocation_closes_a_live_session() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        let mut session = store.authenticate(issued.reveal()).expect("authenticates");

        // The session is open: `closed()` must not resolve yet.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), session.closed())
                .await
                .is_err(),
            "closed() resolved before the pairing was revoked"
        );

        store.revoke(issued.id).unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(50), session.closed())
            .await
            .expect("revocation did not close the live session");
    }

    #[test]
    fn two_pairings_get_distinct_ids_and_distinct_tokens() {
        let (_dir, mut store) = fixture();
        let a = store.issue(binding("agent-alpha")).expect("entropy");
        let b = store.issue(binding("agent-alpha")).expect("entropy");
        assert_ne!(a.id, b.id);
        assert_ne!(a.reveal(), b.reveal());
        assert_eq!(store.authenticate(a.reveal()).expect("a").id, a.id);
        assert_eq!(store.authenticate(b.reveal()).expect("b").id, b.id);
    }

    #[test]
    fn revoking_one_pairing_leaves_the_other_working() {
        let (_dir, mut store) = fixture();
        let a = store.issue(binding("agent-alpha")).expect("entropy");
        let b = store.issue(binding("agent-alpha")).expect("entropy");
        store.revoke(a.id).unwrap();
        assert!(store.authenticate(a.reveal()).is_err());
        assert!(
            store.authenticate(b.reveal()).is_ok(),
            "revoke hit the wrong pairing"
        );
    }

    #[test]
    fn the_debug_impl_redacts_the_secret() {
        let (_dir, mut store) = fixture();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        let rendered = format!("{issued:?}");
        assert!(
            !rendered.contains(issued.reveal()),
            "Debug leaked the token: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }
}
