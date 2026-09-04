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

use sha3::{Digest, Sha3_256};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use zeroize::Zeroizing;

/// Bytes of entropy in a pairing token. 256 bits: the token is a bearer
/// credential with nothing but the OS accept queue in front of it.
const TOKEN_BYTES: usize = 32;

/// The stable public name of a pairing. Appears in the ledger and the
/// approvals UI; safe to log, unlike the token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PairingId(u64);

impl fmt::Display for PairingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pairing-{}", self.0)
    }
}

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

struct Record {
    digest: [u8; 32],
    /// Flips to `true` on revoke. Every live session for this pairing holds a
    /// receiver.
    revoked_tx: watch::Sender<bool>,
}

/// The set of pairings the gateway will accept, and the authority that revokes
/// them.
#[derive(Default)]
pub struct TokenStore {
    records: HashMap<PairingId, Record>,
    next_id: u64,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a pairing. The caller shows [`IssuedToken::reveal`] once and then
    /// drops it; from here the store can only recognise the token, never
    /// reproduce it.
    pub fn issue(&mut self) -> Result<IssuedToken, EntropyUnavailable> {
        let mut raw = Zeroizing::new([0u8; TOKEN_BYTES]);
        getrandom::getrandom(raw.as_mut()).map_err(|e| EntropyUnavailable(e.to_string()))?;
        let secret = Zeroizing::new(hex_of(raw.as_ref()));

        self.next_id += 1;
        let id = PairingId(self.next_id);

        let (revoked_tx, _) = watch::channel(false);
        self.records.insert(
            id,
            Record {
                digest: digest_of(secret.as_bytes()),
                revoked_tx,
            },
        );

        Ok(IssuedToken { id, secret })
    }

    /// Authenticate a presented bearer value.
    ///
    /// Hashes the candidate and compares digests in constant time against every
    /// record. The loop deliberately does not break on a match: an early exit
    /// would make the response time depend on the matching pairing's position
    /// in the map.
    pub fn authenticate(&self, presented: &str) -> Result<Session, AuthError> {
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
            revoked: record.revoked_tx.subscribe(),
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
    pub fn revoke(&mut self, id: PairingId) -> bool {
        match self.records.get(&id) {
            Some(record) => {
                // `send_replace`, not `send`: `send` returns `Err` and leaves
                // the value untouched when no receiver is alive, so a pairing
                // with no session open at this instant would stay live.
                !record.revoked_tx.send_replace(true)
            }
            None => false,
        }
    }
}

/// An authenticated connection's handle to its pairing.
#[derive(Debug)]
pub struct Session {
    pub id: PairingId,
    revoked: watch::Receiver<bool>,
}

impl Session {
    /// Resolves when the pairing is revoked. A streaming handler selects on
    /// this so revocation closes the connection instead of waiting for the
    /// agent's next request.
    pub async fn closed(&mut self) {
        // `changed()` errors only once every sender is dropped, which happens
        // when the store drops the record — also a reason to close.
        while self.revoked.changed().await.is_ok() {
            if *self.revoked.borrow() {
                return;
            }
        }
    }
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

    #[test]
    fn a_minted_token_authenticates_and_names_its_pairing() {
        let mut store = TokenStore::new();
        let issued = store.issue().expect("entropy");
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

    #[test]
    fn the_store_does_not_hold_the_token() {
        let mut store = TokenStore::new();
        let issued = store.issue().expect("entropy");
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
        let mut store = TokenStore::new();
        let _issued = store.issue().expect("entropy");
        assert_eq!(
            store.authenticate(&"0".repeat(64)).err(),
            Some(AuthError::Unauthenticated)
        );
    }

    #[test]
    fn an_empty_and_a_short_token_do_not_panic() {
        let mut store = TokenStore::new();
        let _issued = store.issue().expect("entropy");
        for candidate in ["", "0x", "deadbeef", &"f".repeat(200)] {
            assert!(
                store.authenticate(candidate).is_err(),
                "{candidate:?} authenticated"
            );
        }
    }

    #[test]
    fn a_revoked_token_stops_authenticating() {
        let mut store = TokenStore::new();
        let issued = store.issue().expect("entropy");
        assert!(store.authenticate(issued.reveal()).is_ok());
        assert!(
            store.revoke(issued.id),
            "revoke reports the pairing was live"
        );
        assert_eq!(
            store.authenticate(issued.reveal()).err(),
            Some(AuthError::Revoked),
            "a revoked pairing must be distinguishable from one that never existed"
        );
        assert!(
            !store.revoke(issued.id),
            "revoking twice reports it was already revoked"
        );
    }

    #[tokio::test]
    async fn revocation_closes_a_live_session() {
        let mut store = TokenStore::new();
        let issued = store.issue().expect("entropy");
        let mut session = store.authenticate(issued.reveal()).expect("authenticates");

        // The session is open: `closed()` must not resolve yet.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), session.closed())
                .await
                .is_err(),
            "closed() resolved before the pairing was revoked"
        );

        store.revoke(issued.id);

        tokio::time::timeout(std::time::Duration::from_millis(50), session.closed())
            .await
            .expect("revocation did not close the live session");
    }

    #[test]
    fn two_pairings_get_distinct_ids_and_distinct_tokens() {
        let mut store = TokenStore::new();
        let a = store.issue().expect("entropy");
        let b = store.issue().expect("entropy");
        assert_ne!(a.id, b.id);
        assert_ne!(a.reveal(), b.reveal());
        assert_eq!(store.authenticate(a.reveal()).expect("a").id, a.id);
        assert_eq!(store.authenticate(b.reveal()).expect("b").id, b.id);
    }

    #[test]
    fn revoking_one_pairing_leaves_the_other_working() {
        let mut store = TokenStore::new();
        let a = store.issue().expect("entropy");
        let b = store.issue().expect("entropy");
        store.revoke(a.id);
        assert!(store.authenticate(a.reveal()).is_err());
        assert!(
            store.authenticate(b.reveal()).is_ok(),
            "revoke hit the wrong pairing"
        );
    }

    #[test]
    fn the_debug_impl_redacts_the_secret() {
        let mut store = TokenStore::new();
        let issued = store.issue().expect("entropy");
        let rendered = format!("{issued:?}");
        assert!(
            !rendered.contains(issued.reveal()),
            "Debug leaked the token: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }
}
