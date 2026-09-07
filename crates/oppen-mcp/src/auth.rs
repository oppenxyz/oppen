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

use oppen_core::guardrail::AgentId;
use oppen_hl::Address;

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

/// What a pairing token names (`docs/spec.md` item 15).
///
/// A token is not an anonymous key to the gateway: it names one agent, bound
/// to one venue account (D1 as revised — one container per agent). Every tool
/// call resolves this from the token presented, so two paired agents on the
/// same gateway act as themselves, under their own guardrails, and see only
/// their own events (`docs/decisions.md` C6). Assigning the guardrails is the
/// operator's half of the approve dialog and lives on the engine; this is the
/// identity those guardrails are keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub agent: AgentId,
    pub account: Address,
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
    /// Who this token speaks for. Fixed at pairing: rebinding a live token to
    /// a different agent would move an agent's authority without the operator
    /// re-approving it, so a new binding is a new pairing.
    binding: Binding,
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
    pub fn issue(&mut self, binding: Binding) -> Result<IssuedToken, EntropyUnavailable> {
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
                binding,
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
            binding: record.binding.clone(),
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
    /// Who the presented token names. Every tool acts as this agent.
    pub binding: Binding,
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
    fn bindings_keep_revoked_records_and_are_an_owned_snapshot() {
        let mut store = TokenStore::new();
        let alpha = store.issue(binding("alpha")).expect("token");
        store.issue(binding("beta")).expect("token");
        assert!(store.revoke(alpha.id));
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
        let mut store = TokenStore::new();
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
        let mut store = TokenStore::new();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        let session = store.authenticate(issued.reveal()).expect("authenticates");
        assert_eq!(session.binding, binding("agent-alpha"));
    }

    /// Two pairings are two identities, not two keys to the same one.
    #[test]
    fn two_pairings_carry_their_own_agents() {
        let mut store = TokenStore::new();
        let alpha = store.issue(binding("agent-alpha")).expect("entropy");
        let beta = store.issue(binding("agent-beta")).expect("entropy");

        let a = store.authenticate(alpha.reveal()).expect("alpha");
        let b = store.authenticate(beta.reveal()).expect("beta");
        assert_eq!(a.binding.agent.as_str(), "agent-alpha");
        assert_eq!(b.binding.agent.as_str(), "agent-beta");
        assert_ne!(a.id, b.id);

        // Revoking one leaves the other acting as itself.
        store.revoke(alpha.id);
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
        let mut store = TokenStore::new();
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
        let mut store = TokenStore::new();
        let _issued = store.issue(binding("agent-alpha")).expect("entropy");
        assert_eq!(
            store.authenticate(&"0".repeat(64)).err(),
            Some(AuthError::Unauthenticated)
        );
    }

    #[test]
    fn an_empty_and_a_short_token_do_not_panic() {
        let mut store = TokenStore::new();
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
        let mut store = TokenStore::new();
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
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
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
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
        let a = store.issue(binding("agent-alpha")).expect("entropy");
        let b = store.issue(binding("agent-alpha")).expect("entropy");
        assert_ne!(a.id, b.id);
        assert_ne!(a.reveal(), b.reveal());
        assert_eq!(store.authenticate(a.reveal()).expect("a").id, a.id);
        assert_eq!(store.authenticate(b.reveal()).expect("b").id, b.id);
    }

    #[test]
    fn revoking_one_pairing_leaves_the_other_working() {
        let mut store = TokenStore::new();
        let a = store.issue(binding("agent-alpha")).expect("entropy");
        let b = store.issue(binding("agent-alpha")).expect("entropy");
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
        let issued = store.issue(binding("agent-alpha")).expect("entropy");
        let rendered = format!("{issued:?}");
        assert!(
            !rendered.contains(issued.reveal()),
            "Debug leaked the token: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }
}
