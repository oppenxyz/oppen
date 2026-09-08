//! OS keychain storage for the two secrets oppen holds: agent wallet private
//! keys and the guardrail-config HMAC key.
//!
//! `docs/spec.md` item 2 puts keys in the OS keychain; item 3 HMAC-checks the
//! guardrail configuration with a key from that same keychain. This module owns
//! the storage and the HMAC primitive. It does not own the guardrail
//! configuration, which lives in [`crate::guardrail`], and it does not own key
//! generation, which is `oppen-hl`'s job — key material arrives here already
//! generated and leaves only as an [`oppen_hl::AgentKey`].
//!
//! # What this protects against, and what it does not
//!
//! `docs/threat-model.md` is the normative text and this module must not
//! promise more than it does: a same-user process reads these secrets on
//! Windows and Linux, and the macOS prompt stops casual access rather than a
//! determined process. The keychain is *containment*, not a boundary. The only
//! containment property that survives a compromised machine is the venue's: an
//! agent (API) wallet cannot withdraw.
//!
//! # Invariants this file carries
//!
//! - `AGENTS.md` invariant 2: no private key reaches TypeScript. Key material
//!   read here is handed straight to `oppen-hl` and is never serialized into a
//!   Tauri command result, a log line or an MCP response.
//! - `docs/decisions.md` R4: testnet and mainnet agent keys are different
//!   secrets. The network is part of the keychain *service* name, so a
//!   cross-network read is unrepresentable rather than merely filtered.
//! - `docs/decisions.md` D-b: agent approvals expire after 90 days with
//!   warnings from 14 days out, and an agent address is never reused across a
//!   rotation. [`KeyStore::rotate_agent_key`] mints a new entry at a new
//!   generation, and both it and [`KeyStore::create_agent_key`] refuse an
//!   address this store has installed before — `docs/specs/onboarding.md` §7.3
//!   says the old address is retired **permanently**, so the memory that
//!   answers that question is one keychain entry per address, scoped to the
//!   network rather than to the agent id, and is never trimmed, never evicted
//!   and not removed by [`KeyStore::delete_agent`].

use std::sync::{
    Mutex, MutexGuard, PoisonError,
    atomic::{Ordering, compiler_fence},
};

#[cfg(test)]
use std::collections::BTreeMap;

use hmac::{Hmac, KeyInit, Mac};
use oppen_hl::{Address, AgentKey, Network};
use serde::{Deserialize, Serialize};
use sha3::Sha3_256;

use crate::guardrail::AgentId;

/// Keychain service holding every testnet secret.
///
/// Derived from the Tauri bundle identifier `xyz.oppen.desktop` with the
/// `.desktop` component dropped: `docs/decisions.md` R1 keeps `oppen-core`
/// runnable headless, so the service that owns the keys cannot be named after
/// the GUI that is only one of its front ends.
///
/// **This string is frozen.** Changing it orphans every key already stored on
/// every installed machine, and an orphaned agent key is an agent that can no
/// longer cancel its own resting orders.
const SERVICE_TESTNET: &str = "xyz.oppen.testnet";

/// Keychain service holding every mainnet secret. Frozen, for the same reason.
///
/// The network is in the *service* rather than in the entry name because
/// `docs/decisions.md` R4 calls a mainnet number that is actually a testnet
/// number the worst bug this product can ship. A per-network service makes that
/// collision impossible to write and visible to an operator auditing Keychain
/// Access or Credential Manager.
const SERVICE_MAINNET: &str = "xyz.oppen.mainnet";

/// Entry-name prefix for one generation of one agent's wallet key.
const PREFIX_AGENT_KEY: &str = "agent-key";
/// Entry-name prefix for an agent's non-secret wallet record.
const PREFIX_AGENT_RECORD: &str = "agent-record";
/// Entry-name prefix for the permanent mark that an address has been installed
/// on this network. One entry per address, holding the generation it was
/// installed at.
const PREFIX_AGENT_ADDRESS: &str = "agent-address";
/// Entry name of the guardrail-config HMAC key (`docs/spec.md` item 3).
const ENTRY_GUARDRAIL_HMAC: &str = "guardrail-hmac";

/// Separator inside an entry name. Also the one byte an [`AgentId`] may not
/// contain, which is what keeps `agent-key/<id>/<generation>` unambiguous.
const ENTRY_SEPARATOR: char = '/';

/// Longest [`AgentId`] accepted into an entry name. `AgentId::new` takes any
/// `String`, so the bound is applied here rather than assumed: 64 bytes is
/// longer than any identifier the pairing flow mints and short enough that the
/// entry name stays inside every platform's target-name limit.
const MAX_AGENT_ID_BYTES: usize = 64;

/// How long an `approveAgent` approval oppen requests is valid for: 90 days
/// (`docs/decisions.md` D-b). Long enough not to be a chore, short enough that
/// an abandoned deployment stops trading within a quarter.
const AGENT_APPROVAL_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1_000;

/// How long before `valid_until` the console and `get_state` start warning:
/// 14 days (`docs/decisions.md` D-b).
const AGENT_EXPIRY_WARNING_MS: u64 = 14 * 24 * 60 * 60 * 1_000;

/// `valid_until` oppen should request for an approval signed at `now_ms`.
///
/// Saturating rather than wrapping: a clock far enough in the future to
/// overflow should produce a permanently-valid approval, never a
/// silently-expired one.
pub fn default_valid_until_ms(now_ms: u64) -> u64 {
    now_ms.saturating_add(AGENT_APPROVAL_TTL_MS)
}

/// Highest generation an agent may reach, and so exactly how many key entries
/// [`KeyStore::delete_agent`] sweeps.
///
/// The ceiling is what lets the revoke path be a fixed, record-independent
/// sweep. `generation` is read back out of the *record*, a non-secret entry any
/// same-user process can edit (this module's header, and
/// `docs/threat-model.md`), so a delete guided by it removes whatever that
/// number says and reports success: too few generations if it was edited down
/// or the record was lost, and 4,294,967,296 credential-store deletes if it was
/// edited up. Both leave a live agent key behind, which is the fail-*open*
/// direction. With a ceiling the delete asks the record nothing and removes
/// every generation that could exist.
///
/// 1,024 rotations is roughly 252 years at `docs/decisions.md` D-b's 90-day
/// approval, so nothing legitimate reaches it, and 1,026 deletes is bounded
/// work on a path an operator takes once per revoked agent. Past it,
/// [`KeyStoreError::RotationLimit`] rather than silence.
pub(crate) const MAX_GENERATION: u32 = 1_024;

/// Secret text held only as long as it is needed, overwritten on drop.
///
/// The hand-off type between the keychain and [`oppen_hl::AgentKey::from_hex`],
/// which zeroizes its own decode buffer. Without it the *hex* lingers: the
/// keychain hands back an ordinary `String` whose allocation is freed with the
/// private key still in it.
///
/// **This is a stand-in for `zeroize::Zeroizing<String>` and is weaker.**
/// `oppen-core` declares no `zeroize`, so the overwrite is a plain fill plus a
/// compiler fence that an aggressive optimizer may elide, and it cannot reach
/// the copy `keyring` made in its own decode path. Swap it the moment the
/// dependency exists.
pub struct SecretText(Vec<u8>);

impl SecretText {
    /// Takes ownership of `text`. The `String`'s allocation is moved, not
    /// copied, so no second buffer holding the secret is created here.
    pub fn new(text: String) -> Self {
        SecretText(text.into_bytes())
    }

    /// Borrows the secret as `&str` for the one call that needs it.
    ///
    /// Fails rather than lossily converting: a keychain entry that is not UTF-8
    /// is a corrupt entry, and guessing at a private key is worse than refusing
    /// to load it.
    pub(crate) fn as_str(&self) -> Result<&str, KeyStoreError> {
        std::str::from_utf8(&self.0).map_err(|_| KeyStoreError::Corrupt {
            detail: "secret is not valid UTF-8".to_owned(),
        })
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        self.0.fill(0);
        // Best effort: stops the compiler reordering the overwrite past the
        // deallocation it is about to become dead code for.
        compiler_fence(Ordering::SeqCst);
    }
}

impl std::fmt::Debug for SecretText {
    /// Redacted. A `Debug` that printed the value would put an agent private
    /// key into any log line that formatted an error containing one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretText(<{} bytes redacted>)", self.0.len())
    }
}

/// Every way a key operation fails, typed so no caller has to parse a message
/// (`AGENTS.md` invariant 8).
#[derive(thiserror::Error)]
pub enum KeyStoreError {
    /// The platform credential store failed or is unavailable. On Linux this is
    /// most often a locked or absent Secret Service.
    #[error("keychain backend: {0}")]
    Backend(#[from] keyring::Error),

    #[error("no keychain entry named {entry}")]
    Missing { entry: String },

    /// An agent already has a wallet record. Creating a second one would
    /// overwrite the first, and `docs/decisions.md` D-b requires a rotation to
    /// mint a new entry instead.
    #[error("agent {agent} already has a wallet; rotate instead of creating")]
    AlreadyExists { agent: String },

    /// An [`AgentId`] that cannot be part of an entry name. Rejected rather
    /// than escaped: an id containing the separator could name another agent's
    /// entry.
    #[error("agent id {id:?} is not usable in a keychain entry name: {reason}")]
    InvalidAgentId { id: String, reason: &'static str },

    /// A stored entry could not be read back as what it claims to be.
    #[error("corrupt keychain entry: {detail}")]
    Corrupt { detail: String },

    /// The stored bytes are not a usable secp256k1 private key.
    #[error("stored agent key is not a valid private key")]
    InvalidKey,

    /// An install tried to use an address this store has installed before.
    /// Hyperliquid prunes an agent when it is replaced and the nonce state goes
    /// with it, so a reused address can be replayed against: refused, never
    /// warned about. Raised by creation as well as by rotation, and scoped to
    /// the network rather than to the agent id, because
    /// `docs/specs/onboarding.md` §7.3 retires the *address* permanently —
    /// neither deleting an agent nor offering the key under a second agent id
    /// un-retires it.
    #[error("agent address {address} was already used on this network; addresses are never reused")]
    AddressReused { address: Address },

    /// This agent id has reached [`MAX_GENERATION`], or its record claims a
    /// generation past it.
    ///
    /// One variant for both because the caller does the same thing either way:
    /// this id can rotate no further, so provision a new agent. This module
    /// cannot write a record above the ceiling, so reading one means the entry
    /// was edited: named rather than reported as a missing key.
    /// [`KeyStore::delete_agent`] never raises it — revoking must not depend on
    /// the record being intact.
    #[error("agent generation {generation} is past the {MAX_GENERATION}-rotation limit")]
    RotationLimit { generation: u32 },

    /// The stored key does not derive the address in the agent's record.
    ///
    /// The write path pairs the two, so a mismatch means the entries were
    /// edited apart after that — and the two sides disagreeing is exactly the
    /// silent mis-signing [`KeyStore::create_agent_key`] refuses to create.
    /// Both addresses are carried rather than formatted into a message,
    /// because an operator deciding whether to rotate or to restore needs to
    /// see which side moved (`AGENTS.md` invariant 8).
    #[error("stored agent key derives {derived}, but its record names {record}")]
    AddressMismatch { record: Address, derived: Address },

    /// No OS entropy source is reachable, so no key was generated.
    ///
    /// Fails closed on purpose: a guardrail HMAC key from a weak source is
    /// worse than no key, because it looks like protection.
    #[error("no OS entropy source available: {detail}")]
    EntropyUnavailable { detail: String },

    #[error("wallet record encoding: {0}")]
    Encoding(#[from] serde_json::Error),

    /// The HMAC key length was rejected by the MAC construction. Structurally
    /// unreachable for a 32-byte key; typed so the primitive never panics.
    #[error("hmac key rejected by the mac construction")]
    BadMacKey,
}

impl std::fmt::Debug for KeyStoreError {
    /// Delegates to [`Display`](std::fmt::Display) instead of deriving.
    ///
    /// `keyring::Error` derives `Debug`, and its `BadEncoding(Vec<u8>)` and
    /// `BadDataFormat(Vec<u8>, _)` variants carry **the raw credential blob the
    /// store just read** — on Windows that is the stored secret's bytes. A
    /// derived `Debug` here would carry them through [`KeyStoreError::Backend`]
    /// into every `{:?}`: `tracing::error!(?e)`, an `unwrap`/`expect` panic
    /// payload, a Tauri command's error result. `AGENTS.md` invariant 2 says no
    /// key material reaches a log line, and `Display` never renders the blob.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// A fully-qualified keychain entry: the per-network service plus the account
/// name inside it.
///
/// Public because it appears in [`KeyStore`]'s signatures, but opaque: every
/// constructor and accessor is `pub(crate)`, so no caller outside this crate
/// can assemble a name that crosses networks (`docs/decisions.md` R4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryName {
    service: &'static str,
    account: String,
}

impl EntryName {
    /// The keychain service, which carries the network.
    pub(crate) fn service(&self) -> &'static str {
        self.service
    }

    /// The account (entry) name inside the service.
    pub(crate) fn account(&self) -> &str {
        &self.account
    }

    /// One generation of one agent's wallet key.
    ///
    /// The generation is part of the name because `docs/decisions.md` D-b
    /// forbids reusing an agent address across a rotation: a rotation writes a
    /// name that has never existed, so it cannot overwrite the key it replaces.
    pub(crate) fn agent_key(
        network: Network,
        agent: &AgentId,
        generation: u32,
    ) -> Result<Self, KeyStoreError> {
        let id = checked_agent_id(agent)?;
        Ok(EntryName {
            service: service_of(network),
            account: format!(
                "{PREFIX_AGENT_KEY}{ENTRY_SEPARATOR}{id}{ENTRY_SEPARATOR}{generation}"
            ),
        })
    }

    /// An agent's wallet record: which generation is current, its address and
    /// its approval window. Not secret, but it belongs next to the key it
    /// describes so deleting an agent deletes both.
    pub(crate) fn agent_record(network: Network, agent: &AgentId) -> Result<Self, KeyStoreError> {
        let id = checked_agent_id(agent)?;
        Ok(EntryName {
            service: service_of(network),
            account: format!("{PREFIX_AGENT_RECORD}{ENTRY_SEPARATOR}{id}"),
        })
    }

    /// The permanent mark that `address` has been installed on this network.
    ///
    /// Keyed by the address so the reuse question is one read rather than a
    /// scan, and so the answer is complete: nothing trims this, so
    /// `docs/specs/onboarding.md` §7.3's "retired permanently" is literally
    /// what the store implements. An [`Address`] renders as lowercase `0x` hex
    /// and cannot contain [`ENTRY_SEPARATOR`], so the name stays unambiguous.
    ///
    /// Deliberately *not* keyed by the agent id. §7.3 retires the address, not
    /// the pairing, and a mark filed under `alpha` would let the same key be
    /// installed again as `beta` — the same pruned nonce set, one container
    /// over. The network is still in the service, because
    /// `docs/decisions.md` R4 keeps the two networks' secrets apart.
    pub(crate) fn agent_address(network: Network, address: &Address) -> Self {
        EntryName {
            service: service_of(network),
            account: format!("{PREFIX_AGENT_ADDRESS}{ENTRY_SEPARATOR}{address}"),
        }
    }

    /// The guardrail-config HMAC key (`docs/spec.md` item 3). One per network,
    /// because the guardrail configuration is per network too.
    pub(crate) fn guardrail_hmac(network: Network) -> Self {
        EntryName {
            service: service_of(network),
            account: ENTRY_GUARDRAIL_HMAC.to_owned(),
        }
    }
}

/// The keychain service for `network` (`docs/decisions.md` R4).
fn service_of(network: Network) -> &'static str {
    match network {
        Network::Testnet => SERVICE_TESTNET,
        Network::Mainnet => SERVICE_MAINNET,
    }
}

/// Validates an [`AgentId`] for use inside an entry name.
///
/// `AgentId::new` accepts any `String`, so without this an id containing the
/// separator — `"a/0"` — would produce the same entry name as another agent's
/// generation, and reading one agent's key would return another's.
pub(crate) fn checked_agent_id(agent: &AgentId) -> Result<&str, KeyStoreError> {
    let id = agent.as_str();
    let reason = if id.is_empty() {
        "empty"
    } else if id.len() > MAX_AGENT_ID_BYTES {
        "longer than 64 bytes"
    } else if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        "characters outside [A-Za-z0-9._-]"
    } else {
        return Ok(id);
    };
    Err(KeyStoreError::InvalidAgentId {
        id: id.to_owned(),
        reason,
    })
}

/// The non-secret record describing an agent's current wallet.
///
/// Serialized in declaration order into one keychain entry, so the stored bytes
/// are byte-identical for equal values (`AGENTS.md` invariant 6). It is what
/// lets the console and `get_state` answer "how long has this agent got"
/// without ever touching the key.
///
/// Every field is fixed-width, so the encoded record has a small constant
/// worst case and cannot grow into a platform's credential-blob cap. The agent
/// id is deliberately *not* a field: the entry name already carries it, and a
/// copy of the id inside the value is a second answer that can disagree with
/// the first. The address history is not a field either — it is one entry per
/// address ([`EntryName::agent_address`]), which is what makes it permanent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWallet {
    /// Rotation counter. `0` is the first wallet; each rotation adds one and
    /// names a keychain entry that has never existed before. Bounded by
    /// [`MAX_GENERATION`].
    pub generation: u32,
    /// The agent address the master approved, derived from the stored key
    /// rather than supplied, so the record cannot disagree with the signer.
    pub address: Address,
    /// When this wallet was recorded, in ms since the epoch.
    pub approved_at_ms: u64,
    /// The approval's `valid_until`, in ms since the epoch. Stored rather than
    /// derived: `docs/decisions.md` D-b sets what oppen *requests*, but the
    /// signed approval is what the venue enforces.
    pub valid_until_ms: u64,
}

/// Where an approval sits relative to `docs/decisions.md` D-b's 90-day window.
///
/// Three states rather than a bare duration because the console, the event
/// taxonomy (`docs/spec.md` item 18's agent-wallet expiry warnings) and
/// `get_state` all need the same threshold, and a threshold computed in three
/// places drifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExpiryState {
    /// More than 14 days left.
    Valid { remaining_ms: u64 },
    /// Inside D-b's 14-day warning window. The operator should be told.
    Expiring { remaining_ms: u64 },
    /// `valid_until` has passed. D-b: signing fails, oppen halts the agent and
    /// cancels resting orders, and **leaves positions open** — force-closing on
    /// a calendar event is a destructive action triggered by a clock.
    Expired { elapsed_ms: u64 },
}

impl AgentWallet {
    /// Where this approval sits in D-b's window at `now_ms`.
    ///
    /// Saturating both ways, so a clock that jumped backwards reports a large
    /// remaining time rather than underflowing into `Expired`.
    pub fn expiry(&self, now_ms: u64) -> ExpiryState {
        if now_ms >= self.valid_until_ms {
            return ExpiryState::Expired {
                elapsed_ms: now_ms.saturating_sub(self.valid_until_ms),
            };
        }
        let remaining_ms = self.valid_until_ms.saturating_sub(now_ms);
        if remaining_ms <= AGENT_EXPIRY_WARNING_MS {
            ExpiryState::Expiring { remaining_ms }
        } else {
            ExpiryState::Valid { remaining_ms }
        }
    }
}

/// Length of the guardrail HMAC key and of the tags it produces, in bytes.
const HMAC_KEY_LEN: usize = 32;
/// Length of an HMAC-SHA3-256 tag, in bytes.
const HMAC_TAG_LEN: usize = 32;

/// The key behind `docs/spec.md` item 3: the guardrail configuration is
/// HMAC-checked so that editing the SQLite file cannot silently raise a limit.
///
/// `docs/threat-model.md` is explicit that this makes tampering **detectable,
/// not preventable** — the same-user process that can edit the database can
/// also read this key out of the keychain. What it buys is that a limit changed
/// outside oppen does not pass unnoticed.
///
/// HMAC-SHA3-256 because `sha3` is already the workspace's hash — the ledger
/// chain and the L1 action hash both use Keccak — and adding a second hash
/// family for one MAC is a dependency with no argument behind it.
pub struct HmacKey([u8; HMAC_KEY_LEN]);

impl HmacKey {
    /// Wraps caller-supplied key bytes. Public as the other half of a
    /// fail-closed branch: where no OS entropy source is reachable
    /// [`KeyStore::ensure_hmac_key`] refuses rather than inventing a key, and
    /// this plus [`KeyStore::store_hmac_key`] is how a front end with its own
    /// RNG supplies one (`docs/spec.md` item 3).
    pub fn from_bytes(bytes: [u8; HMAC_KEY_LEN]) -> Self {
        HmacKey(bytes)
    }

    /// Generates a key from the OS CSPRNG.
    ///
    /// On unix this reads `/dev/urandom`, the kernel CSPRNG on both macOS and
    /// Linux, which does not block. **On other platforms — Windows included —
    /// this returns [`KeyStoreError::EntropyUnavailable`]**, because
    /// `oppen-core` declares no `getrandom`-style dependency and there is no
    /// `std` API for OS entropy. That is a real gap, not a design choice; it
    /// fails closed so a weak key is unrepresentable, and the fix is one
    /// dependency line. Until then a Windows front end must call
    /// [`HmacKey::from_bytes`] with entropy it obtained itself and store it via
    /// [`KeyStore::store_hmac_key`].
    fn generate() -> Result<Self, KeyStoreError> {
        let mut bytes = [0u8; HMAC_KEY_LEN];
        os_entropy(&mut bytes)?;
        Ok(HmacKey(bytes))
    }

    /// Tags `message`. The guardrail module decides *what* is signed; this is
    /// only the primitive.
    ///
    /// Returns a `Result` rather than panicking on a key length the MAC could
    /// reject. HMAC accepts any key length, so the error branch is structurally
    /// dead, but a dead branch is cheaper than an `expect` on a signing path.
    pub fn sign(&self, message: &[u8]) -> Result<[u8; HMAC_TAG_LEN], KeyStoreError> {
        let mut mac =
            Hmac::<Sha3_256>::new_from_slice(&self.0).map_err(|_| KeyStoreError::BadMacKey)?;
        mac.update(message);
        Ok(mac.finalize().into_bytes().into())
    }

    /// Constant-time check that `tag` is this key's tag over `message`.
    ///
    /// Returns `false` for a wrong tag, a wrong length and for any internal
    /// failure: a verifier that could not run has not verified anything, and on
    /// this path "unknown" has to read as "no".
    pub fn verify(&self, message: &[u8], tag: &[u8]) -> bool {
        let Ok(mut mac) = Hmac::<Sha3_256>::new_from_slice(&self.0) else {
            return false;
        };
        mac.update(message);
        mac.verify_slice(tag).is_ok()
    }
}

impl Drop for HmacKey {
    fn drop(&mut self) {
        self.0.fill(0);
        compiler_fence(Ordering::SeqCst);
    }
}

impl std::fmt::Debug for HmacKey {
    /// Redacted, for the same reason as [`SecretText`]'s.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HmacKey(<redacted>)")
    }
}

/// Fills `dst` from the OS CSPRNG. See `HmacKey::generate` for the platform
/// gap this leaves open.
#[cfg(unix)]
pub(crate) fn os_entropy(dst: &mut [u8]) -> Result<(), KeyStoreError> {
    use std::io::Read;

    let mut file =
        std::fs::File::open("/dev/urandom").map_err(|e| KeyStoreError::EntropyUnavailable {
            detail: format!("opening /dev/urandom: {e}"),
        })?;
    file.read_exact(dst)
        .map_err(|e| KeyStoreError::EntropyUnavailable {
            detail: format!("reading /dev/urandom: {e}"),
        })
}

/// No `std` OS-entropy API exists off unix and this crate declares no RNG
/// dependency, so key generation fails closed here rather than inventing
/// entropy. See `HmacKey::generate`.
#[cfg(not(unix))]
pub(crate) fn os_entropy(_dst: &mut [u8]) -> Result<(), KeyStoreError> {
    Err(KeyStoreError::EntropyUnavailable {
        detail: "oppen-core has no OS RNG dependency on this platform; \
                 supply the key with HmacKey::from_bytes"
            .to_owned(),
    })
}

/// Read/write access to one network's secrets.
///
/// The three required methods are raw entry access; everything an operator or
/// the guardrail engine calls is a provided method built on them, so the
/// agent-wallet rules (one record per agent, rotation mints a new generation,
/// an address is never reinstalled) are implemented once and hold for the
/// in-memory store used by tests exactly as they do for the keychain.
///
/// `Send + Sync` because `docs/decisions.md` R1 has the core running headless
/// with the console as a client: a store is shared across the async runtime's
/// worker threads.
pub trait KeyStore: Send + Sync {
    /// Which network's secrets this store holds. Every entry name is built from
    /// it (`docs/decisions.md` R4).
    fn network(&self) -> Network;

    /// Writes `secret` at `entry`, replacing whatever was there.
    ///
    /// Deliberately unconditional: the *rules* about what may be replaced live
    /// in the provided methods, where they can be read in one place, rather
    /// than in two backend implementations.
    fn write(&self, entry: &EntryName, secret: &str) -> Result<(), KeyStoreError>;

    /// Reads `entry`, or `None` when nothing is stored there.
    ///
    /// Absence is `Ok(None)` rather than an error because "this agent has no
    /// wallet yet" is the normal first-run state, not a failure.
    fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError>;

    /// Deletes `entry`. Deleting something that is not there succeeds, so
    /// cleanup after a partial write is idempotent.
    fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError>;

    /// Whether this machine's keychain can be read at all.
    ///
    /// The first thing that breaks on a fresh machine is not a missing wallet
    /// but an unreachable store: a locked login keychain on macOS, no keyring
    /// daemon on a headless Linux box, a denied prompt. Every other call here
    /// then fails for a reason that has nothing to do with what the operator
    /// was trying to do, so the console asks this first and says so plainly.
    ///
    /// **A read, and only a read.** It probes the guardrail HMAC entry —
    /// present on any configured machine, absent on a fresh one — and treats
    /// both answers as success, because what is being established is that the
    /// store *answered*, not that anything is in it. Nothing is written, so
    /// asking cannot create the state it reports on, and no secret leaves the
    /// crate: the probe discards what it read.
    fn reachable(&self) -> Result<(), KeyStoreError> {
        self.read(&EntryName::guardrail_hmac(self.network()))?;
        Ok(())
    }

    /// The agent's wallet record, or `None` if it has no wallet.
    fn agent_wallet(&self, agent: &AgentId) -> Result<Option<AgentWallet>, KeyStoreError> {
        let entry = EntryName::agent_record(self.network(), agent)?;
        let Some(stored) = self.read(&entry)? else {
            return Ok(None);
        };
        let record: AgentWallet = serde_json::from_str(stored.as_str()?)?;
        // Validated here rather than at each use: the number comes from an
        // entry the threat model treats as externally editable, and every path
        // that acts on a record reaches it through this one read.
        if record.generation > MAX_GENERATION {
            return Err(KeyStoreError::RotationLimit {
                generation: record.generation,
            });
        }
        Ok(Some(record))
    }

    /// Records an agent's first wallet at generation 0.
    ///
    /// Takes the key by value so the caller's copy is overwritten when this
    /// returns, and derives the address from the key rather than accepting one:
    /// a record that disagreed with its key would name an address the master
    /// never approved, and every order signed under it would be rejected with
    /// the venue's opaque "User or API Wallet does not exist".
    ///
    /// Refuses if the agent already has a record. `docs/decisions.md` D-b
    /// requires a new agent address per rotation, so replacing a wallet is
    /// [`KeyStore::rotate_agent_key`] and never an overwrite.
    ///
    /// Also refuses an address this store has ever installed, under any agent
    /// id. Deleting an agent removes its keys but not the address marks, so
    /// neither re-creating an agent under the same id nor creating a second one
    /// can be handed an address whose nonce state the venue has already pruned.
    fn create_agent_key(
        &self,
        agent: &AgentId,
        key_hex: SecretText,
        valid_until_ms: u64,
        now_ms: u64,
    ) -> Result<AgentWallet, KeyStoreError> {
        let _guard = lock_agent_wallets();
        if self.agent_wallet(agent)?.is_some() {
            return Err(KeyStoreError::AlreadyExists {
                agent: agent.as_str().to_owned(),
            });
        }
        let normalized = normalize_key_hex(&key_hex)?;
        let address = address_of_key(&normalized)?;
        if address_used(self, &address)? {
            return Err(KeyStoreError::AddressReused { address });
        }
        let record = AgentWallet {
            generation: 0,
            address,
            approved_at_ms: now_ms,
            valid_until_ms,
        };
        write_wallet(self, agent, &normalized, &record)?;
        Ok(record)
    }

    /// Installs a new agent wallet at generation `n + 1`.
    ///
    /// This is the API shape `docs/decisions.md` D-b asks for: a rotation
    /// *mints* an entry rather than overwriting one. The previous generation's
    /// key stays in the keychain so the retiring agent can still cancel its own
    /// resting orders.
    ///
    /// Refuses an address this store has installed before — every one of them,
    /// for as long as the store exists. Hyperliquid prunes a replaced agent
    /// along with its nonce state, so reinstalling an old address hands out a
    /// signer whose replay window has been reset.
    fn rotate_agent_key(
        &self,
        agent: &AgentId,
        key_hex: SecretText,
        valid_until_ms: u64,
        now_ms: u64,
    ) -> Result<AgentWallet, KeyStoreError> {
        let _guard = lock_agent_wallets();
        let current = require_wallet(self, agent)?;
        let normalized = normalize_key_hex(&key_hex)?;
        let address = address_of_key(&normalized)?;
        if address_used(self, &address)? {
            return Err(KeyStoreError::AddressReused { address });
        }
        // Saturating rather than plain: `agent_wallet` already refuses a record
        // past MAX_GENERATION, but a `+ 1` here would make this path's safety
        // depend on that read, and `agent_wallet` is an overridable provided
        // method on a public trait. Wrapping would recompute generation 0 and
        // overwrite the first key entry.
        let generation = current.generation.saturating_add(1);
        if generation > MAX_GENERATION {
            return Err(KeyStoreError::RotationLimit { generation });
        }

        let record = AgentWallet {
            generation,
            address,
            approved_at_ms: now_ms,
            valid_until_ms,
        };
        write_wallet(self, agent, &normalized, &record)?;
        Ok(record)
    }

    /// Loads the agent's current key as an [`oppen_hl::AgentKey`].
    ///
    /// The hex never becomes a plain `String` on the way: it is wrapped in
    /// [`SecretText`] as it leaves the keychain and handed to
    /// `AgentKey::from_hex`, which zeroizes its own decode buffer. This is the
    /// hand-off `ROADMAP.md` item [2] names.
    ///
    /// Loading an *expired* wallet is not refused here. Expiry is a policy the
    /// guardrail engine applies — D-b halts the agent and cancels resting
    /// orders, which itself needs a usable key — and a store that refused to
    /// load would make that cleanup impossible. Callers check
    /// [`AgentWallet::expiry`].
    ///
    /// The key is checked against the record's address before it is returned.
    /// [`KeyStore::create_agent_key`] pairs them on the way in, but the two
    /// entries are separately editable afterwards and this is the read that has
    /// to hold the guarantee: a key that derives some other address signs
    /// actions the master never approved.
    fn load_agent_key(&self, agent: &AgentId) -> Result<AgentKey, KeyStoreError> {
        self.load_agent_key_with_wallet(agent).map(|(key, _)| key)
    }

    /// Return the wallet record used to select this exact key generation.
    /// Signing authority must compare this record, not a separate read of the
    /// current wallet that could observe a concurrent rotation.
    fn load_agent_key_with_wallet(
        &self,
        agent: &AgentId,
    ) -> Result<(AgentKey, AgentWallet), KeyStoreError> {
        let record = require_wallet(self, agent)?;
        let entry = EntryName::agent_key(self.network(), agent, record.generation)?;
        let stored = self.read(&entry)?.ok_or_else(|| KeyStoreError::Missing {
            entry: entry.account().to_owned(),
        })?;
        let key = AgentKey::from_hex(stored.as_str()?).map_err(|_| KeyStoreError::InvalidKey)?;
        let derived = key.address();
        if derived != record.address {
            return Err(KeyStoreError::AddressMismatch {
                record: record.address,
                derived,
            });
        }
        Ok((key, record))
    }

    /// Removes every one of an agent's keys and its record, and **keeps its
    /// address history**.
    ///
    /// Keeping it is the point: `docs/specs/onboarding.md` §7.3 retires an
    /// address permanently, so an agent re-created under the same id must not
    /// be handed one back. The entries kept are non-secret marks; every entry
    /// that holds key material goes.
    ///
    /// Takes [`AGENT_WALLET_LOCK`] because it competes with the two writers for
    /// the same entries: without it a rotation already inside the lock writes
    /// its key after this has started, and the delete reports success with an
    /// agent private key still in the store.
    ///
    /// Sweeps every generation [`MAX_GENERATION`] allows rather than the range
    /// the record names. Revoking is how an operator takes an agent's signer
    /// away, so it must not believe a number it cannot verify: the record is
    /// externally editable, and one edited or missing field would otherwise
    /// leave the higher generations installed while this returned `Ok`. It also
    /// subsumes the key a crashed [`write_wallet`] orphans one generation past
    /// the record. The cost is a fixed 1,026 deletes of entries that are mostly
    /// absent, on a path taken once per revoked agent.
    ///
    /// The record goes first, and that ordering is the sweep's price: 1,026
    /// backend calls is a wide enough window for a credential store to become
    /// unreachable partway, and the record is what [`KeyStore::load_agent_key`]
    /// needs. Removed first, a revoke that then fails leaves an agent that
    /// cannot sign; removed last, it leaves one that still can.
    fn delete_agent(&self, agent: &AgentId) -> Result<(), KeyStoreError> {
        let _guard = lock_agent_wallets();
        self.remove(&EntryName::agent_record(self.network(), agent)?)?;
        for generation in 0..=MAX_GENERATION {
            self.remove(&EntryName::agent_key(self.network(), agent, generation)?)?;
        }
        Ok(())
    }

    /// The guardrail HMAC key, or `None` on a machine that has never run oppen
    /// on this network. Stored as 64 lowercase hex characters.
    fn load_hmac_key(&self) -> Result<Option<HmacKey>, KeyStoreError> {
        let Some(stored) = self.read(&EntryName::guardrail_hmac(self.network()))? else {
            return Ok(None);
        };
        let mut bytes = [0u8; HMAC_KEY_LEN];
        hex::decode_to_slice(stored.as_str()?, &mut bytes).map_err(|_| KeyStoreError::Corrupt {
            detail: "hmac key entry is not 32 bytes of hex".to_owned(),
        })?;
        Ok(Some(HmacKey(bytes)))
    }

    /// Stores `key`, replacing any existing one.
    ///
    /// Replacing invalidates every guardrail-config tag written under the old
    /// key, which then reads as tampering. That is the correct alarm — the
    /// configuration really is no longer the one that was authenticated — so
    /// the caller must re-tag the configuration in the same operation.
    ///
    /// The hex goes through [`SecretText`] so the encoded copy is overwritten.
    fn store_hmac_key(&self, key: &HmacKey) -> Result<(), KeyStoreError> {
        let encoded = SecretText::new(hex::encode(key.0));
        self.write(
            &EntryName::guardrail_hmac(self.network()),
            encoded.as_str()?,
        )
    }

    /// Loads the guardrail HMAC key, generating and storing one on first run
    /// (`docs/spec.md` item 3).
    ///
    /// Not atomic against a second oppen process racing it on the same machine:
    /// both would generate, the later write would win, and configuration tagged
    /// by the loser would then fail verification. That surfaces as a tamper
    /// alarm rather than as a silent bypass, which is the failure direction
    /// this subsystem is supposed to have.
    fn ensure_hmac_key(&self) -> Result<HmacKey, KeyStoreError> {
        if let Some(existing) = self.load_hmac_key()? {
            return Ok(existing);
        }
        let key = HmacKey::generate()?;
        self.store_hmac_key(&key)?;
        Ok(key)
    }
}

/// Serializes [`KeyStore::create_agent_key`], [`KeyStore::rotate_agent_key`]
/// and [`KeyStore::delete_agent`] against each other.
///
/// The two writers read the wallet record, decide a generation and an address
/// *from what they read*, and write three entries back; the delete reads the
/// same record and removes what it names. [`KeyStore`] is `Send + Sync` so the store is
/// shared across the runtime's worker threads, and without this two rotations
/// of one agent interleave: both see generation *n*, both mint *n + 1*, the
/// second key entry overwrites the first, and the surviving record names an
/// address whose key is gone. A delete interleaved with a rotation is worse in
/// a different direction: it reports success while the key the rotation minted
/// after it read the record is still in the credential store.
///
/// One process-wide lock rather than one per store or per agent: a rotation
/// happens once per agent per 90-day approval, so there is no contention to
/// design around and this is the version that is obviously correct. It does not
/// reach across processes — no platform credential store offers a
/// compare-and-swap — and `ensure_hmac_key` already documents that same
/// boundary for its own race.
static AGENT_WALLET_LOCK: Mutex<()> = Mutex::new(());

/// Takes [`AGENT_WALLET_LOCK`], recovering a poisoned one.
///
/// Poisoning means a thread panicked mid-sequence, and [`write_wallet`]'s
/// ordering already makes that recoverable: a key entry with no record pointing
/// at it is unusable and the next attempt overwrites it. Refusing every later
/// rotation for the rest of the process is the worse failure, not the safer
/// one.
fn lock_agent_wallets() -> MutexGuard<'static, ()> {
    AGENT_WALLET_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The agent's record, or [`KeyStoreError::Missing`] if it has none. Shared by
/// the two paths that cannot proceed without one.
fn require_wallet<S: KeyStore + ?Sized>(
    store: &S,
    agent: &AgentId,
) -> Result<AgentWallet, KeyStoreError> {
    store
        .agent_wallet(agent)?
        .ok_or_else(|| KeyStoreError::Missing {
            entry: format!("{PREFIX_AGENT_RECORD}{ENTRY_SEPARATOR}{agent}"),
        })
}

/// Whether this store has ever installed `address`, under any agent id.
///
/// One read of one entry, and complete for the life of the store: the mark is
/// written before the key it belongs to and nothing ever removes it.
fn address_used<S: KeyStore + ?Sized>(store: &S, address: &Address) -> Result<bool, KeyStoreError> {
    Ok(store
        .read(&EntryName::agent_address(store.network(), address))?
        .is_some())
}

/// Writes a wallet's address mark, then its key entry, then its record.
///
/// The order is the point, and each step fails in the safe direction. A crash
/// after the mark refuses that address forever, which costs one generated
/// keypair. A crash after the key leaves a key no record points at, which is
/// unusable, which a retry overwrites and which [`KeyStore::delete_agent`]
/// still removes. The reverse of either would leave a record pointing at
/// nothing, or an installed address with no mark against reinstalling it.
///
/// The mark's value is the generation, so an operator reading the credential
/// store can map an address to the key entry that holds it.
fn write_wallet<S: KeyStore + ?Sized>(
    store: &S,
    agent: &AgentId,
    key_hex: &SecretText,
    record: &AgentWallet,
) -> Result<(), KeyStoreError> {
    let network = store.network();
    store.write(
        &EntryName::agent_address(network, &record.address),
        &record.generation.to_string(),
    )?;
    store.write(
        &EntryName::agent_key(network, agent, record.generation)?,
        key_hex.as_str()?,
    )?;
    store.write(
        &EntryName::agent_record(network, agent)?,
        &serde_json::to_string(record)?,
    )
}

/// Canonicalises a private key hex string to bare lowercase 64 characters, so
/// the same key written by the onboarding flow and by an import produce
/// identical entries and a later reader never has to guess at a `0x` prefix.
fn normalize_key_hex(key_hex: &SecretText) -> Result<SecretText, KeyStoreError> {
    let text = key_hex.as_str()?;
    let body = text.strip_prefix("0x").unwrap_or(text);
    if body.len() != 64 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(KeyStoreError::InvalidKey);
    }
    Ok(SecretText::new(body.to_ascii_lowercase()))
}

/// The address of the key in `normalized`, derived through `oppen-hl` so the
/// derivation is the signer's own.
fn address_of_key(normalized: &SecretText) -> Result<Address, KeyStoreError> {
    let key = AgentKey::from_hex(normalized.as_str()?).map_err(|_| KeyStoreError::InvalidKey)?;
    Ok(key.address())
}

/// The real store: Keychain on macOS, Credential Manager on Windows, Secret
/// Service on Linux, via the `keyring` crate. See this module's header for what
/// that does and does not protect.
#[derive(Debug, Clone, Copy)]
pub struct KeychainKeyStore {
    network: Network,
}

impl KeychainKeyStore {
    /// A store over `network`'s keychain service. The network is fixed at
    /// construction rather than passed per call so that no call site can pick
    /// the wrong one (`docs/decisions.md` R4).
    pub fn new(network: Network) -> Self {
        KeychainKeyStore { network }
    }

    fn entry(name: &EntryName) -> Result<keyring::Entry, KeyStoreError> {
        Ok(keyring::Entry::new(name.service(), name.account())?)
    }
}

impl KeyStore for KeychainKeyStore {
    fn network(&self) -> Network {
        self.network
    }

    fn write(&self, entry: &EntryName, secret: &str) -> Result<(), KeyStoreError> {
        Self::entry(entry)?.set_password(secret)?;
        Ok(())
    }

    fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
        match Self::entry(entry)?.get_password() {
            Ok(secret) => Ok(Some(SecretText::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(other) => Err(other.into()),
        }
    }

    fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError> {
        match Self::entry(entry)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(other) => Err(other.into()),
        }
    }
}

/// The test double, and the test seam that earns [`KeyStore`] its place as a
/// trait (`AGENTS.md` leanness rule 2).
///
/// CI has no keychain, and a test suite that skipped itself there would leave
/// the agent-wallet rules — one record per agent, rotation mints a generation,
/// an address is never reinstalled — untested on the only machine that gates a
/// merge. Those rules are provided methods on [`KeyStore`], so exercising them
/// here exercises the same code the keychain store runs. `BTreeMap`, not
/// `HashMap`, because `AGENTS.md` invariant 6 wants deterministic iteration
/// anywhere state is enumerated.
///
/// Test-gated because it holds secrets in process memory for the process's
/// whole life: "never a substitute for the keychain in a shipped build" is a
/// property the compiler enforces here, not a warning in a doc comment. Its
/// `Default` is a testnet store (`AGENTS.md` invariant 5), and a poisoned lock
/// panics — that means another test thread already panicked.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct MemoryKeyStore {
    network: Network,
    entries: Mutex<BTreeMap<(&'static str, String), String>>,
    /// Slept off after every read, outside the map lock. It widens the
    /// read-modify-write window in `create_agent_key` and `rotate_agent_key`
    /// until an interleaving is certain rather than occasional, which is what
    /// makes the concurrency tests fail deterministically without
    /// [`AGENT_WALLET_LOCK`]. Zero unless a test asks for it.
    read_delay: std::time::Duration,
}

#[cfg(test)]
impl MemoryKeyStore {
    pub(crate) fn new(network: Network) -> Self {
        MemoryKeyStore {
            network,
            ..MemoryKeyStore::default()
        }
    }

    /// A store whose reads take `delay`. See the `read_delay` field.
    pub(crate) fn with_read_delay(network: Network, delay: std::time::Duration) -> Self {
        MemoryKeyStore {
            read_delay: delay,
            ..MemoryKeyStore::new(network)
        }
    }

    /// Entry names currently populated, in deterministic order. For tests that
    /// assert a rotation minted rather than replaced.
    pub(crate) fn entry_names(&self) -> Vec<String> {
        self.entries
            .lock()
            .expect("poisoned")
            .keys()
            .map(|(service, account)| format!("{service}:{account}"))
            .collect()
    }
}

#[cfg(test)]
impl KeyStore for MemoryKeyStore {
    fn network(&self) -> Network {
        self.network
    }

    fn write(&self, entry: &EntryName, secret: &str) -> Result<(), KeyStoreError> {
        self.entries.lock().expect("poisoned").insert(
            (entry.service(), entry.account().to_owned()),
            secret.to_owned(),
        );
        Ok(())
    }

    fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
        let found = self
            .entries
            .lock()
            .expect("poisoned")
            .get(&(entry.service(), entry.account().to_owned()))
            .map(|s| SecretText::new(s.clone()));
        // After the map lock is released, so the delay widens the caller's
        // read-modify-write window rather than the store's own critical
        // section.
        std::thread::sleep(self.read_delay);
        Ok(found)
    }

    fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError> {
        self.entries
            .lock()
            .expect("poisoned")
            .remove(&(entry.service(), entry.account().to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Barrier, thread, time::Duration};

    use super::*;

    /// `CRED_MAX_CREDENTIAL_BLOB_SIZE`, the cap
    /// `windows-native-keyring-store` enforces on a stored secret. It measures
    /// the secret re-encoded as UTF-16LE, so ASCII JSON costs two bytes a
    /// character.
    const WINDOWS_CREDENTIAL_BLOB_MAX_BYTES: usize = 2_560;

    /// Four distinct valid secp256k1 scalars. Fixed rather than generated so a
    /// failure reproduces; none of them is a key that has ever held funds.
    const KEY_A: &str = "0123456789012345678901234567890123456789012345678901234567890123";
    const KEY_B: &str = "1123456789012345678901234567890123456789012345678901234567890123";
    const KEY_C: &str = "2123456789012345678901234567890123456789012345678901234567890123";
    const KEY_D: &str = "3123456789012345678901234567890123456789012345678901234567890123";

    const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
    const T0: u64 = 1_757_000_000_000;

    fn secret(hex: &str) -> SecretText {
        SecretText::new(hex.to_owned())
    }

    fn agent(id: &str) -> AgentId {
        AgentId::new(id)
    }

    /// `KeyStoreError` crosses `.await` points in the headless core
    /// (`docs/decisions.md` R1), so it has to be `Send + Sync`. A compile
    /// failure here is the whole assertion. The stores need no line of their
    /// own: `KeyStore: Send + Sync` makes every implementation prove it.
    #[test]
    fn error_is_send_and_sync() {
        fn require<T: Send + Sync + 'static>() {}
        require::<KeyStoreError>();
    }

    // -- naming ------------------------------------------------------------

    #[test]
    fn networks_never_share_an_entry() {
        let a = agent("alpha");
        let testnet = EntryName::agent_key(Network::Testnet, &a, 0).expect("valid id");
        let mainnet = EntryName::agent_key(Network::Mainnet, &a, 0).expect("valid id");
        assert_eq!(testnet.account(), mainnet.account());
        assert_ne!(testnet.service(), mainnet.service());
        assert_eq!(testnet.service(), SERVICE_TESTNET);
        assert_eq!(mainnet.service(), SERVICE_MAINNET);
        assert_ne!(testnet, mainnet);
    }

    #[test]
    fn entry_names_are_stable() {
        let a = agent("alpha");
        assert_eq!(
            EntryName::agent_key(Network::Testnet, &a, 3)
                .expect("valid id")
                .account(),
            "agent-key/alpha/3"
        );
        assert_eq!(
            EntryName::agent_record(Network::Mainnet, &a)
                .expect("valid id")
                .account(),
            "agent-record/alpha"
        );
        assert_eq!(
            EntryName::guardrail_hmac(Network::Testnet).account(),
            "guardrail-hmac"
        );
        // The reuse mark carries no agent id: the address is retired, not the
        // pairing (`docs/specs/onboarding.md` §7.3).
        assert_eq!(
            EntryName::agent_address(Network::Testnet, &Address::from_bytes([0xab; 20])).account(),
            "agent-address/0xabababababababababababababababababababab"
        );
    }

    #[test]
    fn agent_id_with_a_separator_is_refused() {
        // Without validation "a/0" at generation 9 and "a" at generation 0
        // would both be reachable; the point is that neither name is ever
        // built.
        let bad = agent("a/0");
        assert!(matches!(
            EntryName::agent_key(Network::Testnet, &bad, 9),
            Err(KeyStoreError::InvalidAgentId { .. })
        ));
        assert!(matches!(
            EntryName::agent_record(Network::Testnet, &bad),
            Err(KeyStoreError::InvalidAgentId { .. })
        ));
    }

    #[test]
    fn agent_id_bounds_are_enforced() {
        assert!(matches!(
            EntryName::agent_record(Network::Testnet, &agent("")),
            Err(KeyStoreError::InvalidAgentId { .. })
        ));
        let long = "a".repeat(MAX_AGENT_ID_BYTES + 1);
        assert!(matches!(
            EntryName::agent_record(Network::Testnet, &agent(&long)),
            Err(KeyStoreError::InvalidAgentId { .. })
        ));
        let ok = "a".repeat(MAX_AGENT_ID_BYTES);
        assert!(EntryName::agent_record(Network::Testnet, &agent(&ok)).is_ok());
        assert!(EntryName::agent_record(Network::Testnet, &agent("A-b_c.9")).is_ok());
        for bad in ["a b", "a:b", "a\u{e9}", "a\\b", "a%b"] {
            assert!(
                matches!(
                    EntryName::agent_record(Network::Testnet, &agent(bad)),
                    Err(KeyStoreError::InvalidAgentId { .. })
                ),
                "{bad:?} should be refused"
            );
        }
    }

    // -- agent wallets -----------------------------------------------------

    #[test]
    fn create_then_load_round_trips_and_derives_the_address() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let record = store
            .create_agent_key(&a, secret(KEY_A), default_valid_until_ms(T0), T0)
            .expect("create");

        assert_eq!(record.generation, 0);
        assert_eq!(record.valid_until_ms, T0 + AGENT_APPROVAL_TTL_MS);

        let key = store.load_agent_key(&a).expect("load");
        assert_eq!(key.address(), record.address);
        assert_eq!(
            store.agent_wallet(&a).expect("read").as_ref(),
            Some(&record)
        );
    }

    #[test]
    fn loaded_wallet_identifies_the_key_generation_even_if_current_record_rotates() {
        struct RotatingRead {
            inner: MemoryKeyStore,
            agent: AgentId,
            key_entry: EntryName,
            armed: std::sync::atomic::AtomicBool,
        }

        impl KeyStore for RotatingRead {
            fn network(&self) -> Network {
                self.inner.network()
            }

            fn write(&self, entry: &EntryName, value: &str) -> Result<(), KeyStoreError> {
                self.inner.write(entry, value)
            }

            fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError> {
                self.inner.remove(entry)
            }

            fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
                if entry.account() == self.key_entry.account()
                    && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    self.inner.rotate_agent_key(
                        &self.agent,
                        secret(KEY_B),
                        default_valid_until_ms(T0 + 1),
                        T0 + 1,
                    )?;
                }
                self.inner.read(entry)
            }
        }

        let inner = MemoryKeyStore::new(Network::Testnet);
        let agent = agent("alpha");
        let original = inner
            .create_agent_key(&agent, secret(KEY_A), default_valid_until_ms(T0), T0)
            .unwrap();
        let store = RotatingRead {
            key_entry: EntryName::agent_key(Network::Testnet, &agent, 0).unwrap(),
            inner,
            agent: agent.clone(),
            armed: std::sync::atomic::AtomicBool::new(true),
        };
        let (key, loaded_wallet) = store.load_agent_key_with_wallet(&agent).unwrap();
        assert_eq!(loaded_wallet, original);
        assert_eq!(key.address(), original.address);
        let current = store.agent_wallet(&agent).unwrap().unwrap();
        assert_eq!(current.generation, 1);
        assert_ne!(current.address, key.address());
    }

    #[test]
    fn a_prefixed_uppercase_key_normalizes_to_the_same_entry() {
        // A store apiece, because one store refuses the second form as a reuse
        // of the first's address — which is the same fact from the other side.
        // The assertion is on the stored *bytes*: an onboarding write and an
        // import of the same key must leave the entry byte-identical.
        let install = |hex: &str| {
            let store = MemoryKeyStore::new(Network::Testnet);
            let a = agent("alpha");
            let record = store
                .create_agent_key(&a, secret(hex), T0 + DAY_MS, T0)
                .expect("create");
            let stored = store
                .read(&EntryName::agent_key(Network::Testnet, &a, 0).expect("valid id"))
                .expect("read")
                .expect("the key entry");
            (record.address, stored.as_str().expect("utf-8").to_owned())
        };
        let plain = install(KEY_A);
        assert_eq!(plain.1, KEY_A, "the stored key is not bare lowercase hex");
        assert_eq!(plain, install(&format!("0x{}", KEY_A.to_ascii_uppercase())));
    }

    #[test]
    fn a_malformed_key_is_refused_before_anything_is_written() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        for bad in ["", "0x", "zz", KEY_A.trim_end_matches('3')] {
            assert!(matches!(
                store.create_agent_key(&a, secret(bad), T0 + DAY_MS, T0),
                Err(KeyStoreError::InvalidKey)
            ));
        }
        assert!(store.agent_wallet(&a).expect("read").is_none());
        assert!(store.entry_names().is_empty());
    }

    #[test]
    fn creating_twice_is_refused() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        assert!(matches!(
            store.create_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0),
            Err(KeyStoreError::AlreadyExists { .. })
        ));
        // The first wallet is untouched.
        let record = store.agent_wallet(&a).expect("read").expect("present");
        assert_eq!(record.generation, 0);
        assert_eq!(
            record.address,
            AgentKey::from_hex(KEY_A).expect("valid key").address()
        );
    }

    #[test]
    fn rotation_mints_a_new_entry_and_keeps_the_old_one() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let first = store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        let second = store
            .rotate_agent_key(&a, secret(KEY_B), T0 + 90 * DAY_MS, T0 + DAY_MS)
            .expect("rotate");

        assert_eq!(second.generation, 1);
        assert_ne!(second.address, first.address);

        // Generation 0's key was not overwritten: both entries exist.
        let names = store.entry_names();
        assert!(names.contains(&format!("{SERVICE_TESTNET}:agent-key/alpha/0")));
        assert!(names.contains(&format!("{SERVICE_TESTNET}:agent-key/alpha/1")));

        // Loading follows the record to the current generation.
        assert_eq!(
            store.load_agent_key(&a).expect("load").address(),
            second.address
        );
    }

    #[test]
    fn rotation_refuses_a_reused_address() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");

        // The current address.
        assert!(matches!(
            store.rotate_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0),
            Err(KeyStoreError::AddressReused { .. })
        ));

        store
            .rotate_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0)
            .expect("rotate");

        // A retired address.
        assert!(matches!(
            store.rotate_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0),
            Err(KeyStoreError::AddressReused { .. })
        ));
        // A fresh one is fine.
        assert!(
            store
                .rotate_agent_key(&a, secret(KEY_C), T0 + DAY_MS, T0)
                .is_ok()
        );
    }

    #[test]
    fn rotating_an_unknown_agent_is_refused() {
        let store = MemoryKeyStore::new(Network::Testnet);
        assert!(matches!(
            store.rotate_agent_key(&agent("ghost"), secret(KEY_A), T0 + DAY_MS, T0),
            Err(KeyStoreError::Missing { .. })
        ));
    }

    #[test]
    fn an_address_is_never_reinstalled_however_many_rotations_pass() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let key_at = |i: u32| format!("{:02x}{}", (i % 200) + 1, &KEY_A[2..]);

        let first = store
            .create_agent_key(&a, secret(&key_at(0)), T0 + DAY_MS, T0)
            .expect("create");
        // Far past any window a fixed-size retired list could have held.
        const ROTATIONS: u32 = 40;
        for i in 1..=ROTATIONS {
            store
                .rotate_agent_key(&a, secret(&key_at(i)), T0 + DAY_MS, T0 + u64::from(i))
                .expect("rotate");
        }

        // Generation 0's address, forty rotations later.
        assert!(
            matches!(
                store.rotate_agent_key(&a, secret(&key_at(0)), T0 + DAY_MS, T0 + 99),
                Err(KeyStoreError::AddressReused { address }) if address == first.address
            ),
            "an address left the reuse window after {ROTATIONS} rotations"
        );
        // And every generation in between.
        for i in 1..=ROTATIONS {
            assert!(matches!(
                store.rotate_agent_key(&a, secret(&key_at(i)), T0 + DAY_MS, T0 + 99),
                Err(KeyStoreError::AddressReused { .. })
            ));
        }
        let record = store.agent_wallet(&a).expect("read").expect("present");
        assert_eq!(record.generation, ROTATIONS);
    }

    #[test]
    fn delete_then_create_cannot_reinstall_a_retired_address() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let first = store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .rotate_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0 + 1)
            .expect("rotate");
        store.delete_agent(&a).expect("delete");

        // `docs/specs/onboarding.md` §7.3: retired permanently. Deleting the
        // agent removes its keys, not the fact that it used these addresses.
        for used in [KEY_A, KEY_B] {
            assert!(matches!(
                store.create_agent_key(&a, secret(used), T0 + DAY_MS, T0 + 2),
                Err(KeyStoreError::AddressReused { .. })
            ));
        }
        assert!(
            store.agent_wallet(&a).expect("read").is_none(),
            "a refused create left a record behind"
        );
        // A fresh address still works, so the agent id is not bricked.
        let revived = store
            .create_agent_key(&a, secret(KEY_C), T0 + DAY_MS, T0 + 3)
            .expect("create");
        assert_ne!(revived.address, first.address);
        assert_eq!(revived.generation, 0);
    }

    #[test]
    fn a_record_past_the_rotation_limit_is_named_rather_than_acted_on() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let record = store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        // The record is a non-secret entry any same-user process can edit, and
        // `delete_agent` iterates from 0 to this number.
        store
            .write(
                &EntryName::agent_record(Network::Testnet, &a).expect("valid id"),
                &serde_json::to_string(&AgentWallet {
                    generation: u32::MAX,
                    ..record
                })
                .expect("encode"),
            )
            .expect("write");

        // Every path that acts on a record reaches it through `agent_wallet`,
        // so one read refuses for all of them and the edited number is named
        // rather than mistaken for a missing key.
        assert!(matches!(
            store.agent_wallet(&a),
            Err(KeyStoreError::RotationLimit {
                generation: u32::MAX
            })
        ));
        assert!(matches!(
            store.load_agent_key(&a),
            Err(KeyStoreError::RotationLimit { .. })
        ));
        assert!(matches!(
            store.rotate_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0 + 1),
            Err(KeyStoreError::RotationLimit { .. })
        ));
        // Revoking is the exception, and has to be: `delete_agent` reads no
        // record, so an edited generation cannot make an agent undeletable.
        store
            .delete_agent(&a)
            .expect("revoke must not need the record");
        let left = store.entry_names();
        assert!(
            !left.iter().any(|n| n.contains("agent-key/")),
            "a private key survived the revoke of a tampered record: {left:?}"
        );
    }

    /// A store whose key-entry removals fail, the shape a credential store
    /// that becomes unreachable partway through the sweep produces.
    struct KeyRemovalFails(MemoryKeyStore);

    impl KeyStore for KeyRemovalFails {
        fn network(&self) -> Network {
            self.0.network()
        }
        fn write(&self, entry: &EntryName, secret: &str) -> Result<(), KeyStoreError> {
            self.0.write(entry, secret)
        }
        fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
            self.0.read(entry)
        }
        fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError> {
            if entry.account().starts_with(PREFIX_AGENT_KEY) {
                return Err(KeyStoreError::Backend(keyring::Error::NoDefaultStore));
            }
            self.0.remove(entry)
        }
    }

    #[test]
    fn a_revoke_that_fails_partway_leaves_an_agent_that_cannot_sign() {
        let store = KeyRemovalFails(MemoryKeyStore::new(Network::Testnet));
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");

        assert!(matches!(
            store.delete_agent(&a),
            Err(KeyStoreError::Backend(_))
        ));
        // The sweep failed, so the key is still stored — but the record it
        // needs is not, so nothing can load it.
        assert!(
            store
                .read(&EntryName::agent_key(Network::Testnet, &a, 0).expect("valid id"))
                .expect("read")
                .is_some()
        );
        assert!(matches!(
            store.load_agent_key(&a),
            Err(KeyStoreError::Missing { .. })
        ));
    }

    #[test]
    fn a_revoke_ignores_the_generation_the_record_claims() {
        // The record is a non-secret entry any same-user process can edit, and
        // it is not the delete's guide: whether the number is edited down, the
        // whole record is gone, or it is past the ceiling, every generation
        // that could hold a key is swept.
        for (name, tamper) in [
            ("edited down to 0", 0usize),
            ("record removed", 1),
            ("edited up past the ceiling", 2),
        ] {
            let store = MemoryKeyStore::new(Network::Testnet);
            let a = agent("alpha");
            let record = store
                .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
                .expect("create");
            for (i, k) in [KEY_B, KEY_C, KEY_D].into_iter().enumerate() {
                store
                    .rotate_agent_key(&a, secret(k), T0 + DAY_MS, T0 + i as u64)
                    .expect("rotate");
            }
            let entry = EntryName::agent_record(Network::Testnet, &a).expect("valid id");
            match tamper {
                1 => store.remove(&entry).expect("remove"),
                other => {
                    let generation = if other == 0 { 0 } else { u32::MAX };
                    store
                        .write(
                            &entry,
                            &serde_json::to_string(&AgentWallet {
                                generation,
                                ..record
                            })
                            .expect("encode"),
                        )
                        .expect("write");
                }
            }

            store
                .delete_agent(&a)
                .unwrap_or_else(|e| panic!("revoke refused with the record {name}: {e}"));
            let left = store.entry_names();
            let keys: Vec<_> = left.iter().filter(|n| n.contains("agent-key/")).collect();
            assert!(
                keys.is_empty(),
                "with the record {name} the revoke returned Ok and left {keys:?}"
            );
        }
    }

    /// A store whose every read fails, standing in for a locked keychain or a
    /// machine with no keyring daemon.
    struct UnreachableStore;

    impl KeyStore for UnreachableStore {
        fn network(&self) -> Network {
            Network::Testnet
        }
        fn write(&self, _: &EntryName, _: &str) -> Result<(), KeyStoreError> {
            Ok(())
        }
        fn read(&self, _: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
            Err(KeyStoreError::Corrupt {
                detail: "the keychain is locked".to_owned(),
            })
        }
        fn remove(&self, _: &EntryName) -> Result<(), KeyStoreError> {
            Ok(())
        }
    }

    /// **An empty store is a reachable one.** The probe establishes that the
    /// keychain *answered*, not that anything is in it — a fresh machine has
    /// nothing stored and that is the normal first-run state, not a fault. Get
    /// this backwards and the console tells every new operator their keychain
    /// is broken.
    #[test]
    fn a_keychain_that_answers_is_reachable_even_with_nothing_in_it() {
        let store = MemoryKeyStore::default();
        assert!(store.reachable().is_ok(), "an empty store still answered");

        // And one that refuses is not, carrying its own reason for the
        // operator rather than a generic failure.
        let error = UnreachableStore
            .reachable()
            .expect_err("a store that cannot be read is not reachable");
        assert!(error.to_string().contains("locked"), "{error}");
    }

    /// A store whose `agent_wallet` reports a generation this module's write
    /// path cannot produce. [`KeyStore`] is public and `agent_wallet` is a
    /// provided method, so this is a shape a downstream implementation really
    /// can have — and `rotate_agent_key`'s arithmetic must hold on its own
    /// rather than lean on the guard inside the read it happens to call.
    struct UnboundedGenerationStore;

    impl KeyStore for UnboundedGenerationStore {
        fn network(&self) -> Network {
            Network::Testnet
        }
        fn write(&self, _: &EntryName, _: &str) -> Result<(), KeyStoreError> {
            Ok(())
        }
        fn read(&self, _: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
            Ok(None)
        }
        fn remove(&self, _: &EntryName) -> Result<(), KeyStoreError> {
            Ok(())
        }
        fn agent_wallet(&self, _: &AgentId) -> Result<Option<AgentWallet>, KeyStoreError> {
            Ok(Some(AgentWallet {
                generation: u32::MAX,
                address: Address::from_bytes([0; 20]),
                approved_at_ms: T0,
                valid_until_ms: T0 + DAY_MS,
            }))
        }
    }

    #[test]
    fn a_rotation_never_overflows_the_generation_it_was_handed() {
        // A plain `+ 1` panics here in debug and wraps to 0 in release, and
        // generation 0 is an entry that already holds a key.
        assert!(matches!(
            UnboundedGenerationStore.rotate_agent_key(
                &agent("alpha"),
                secret(KEY_A),
                T0 + DAY_MS,
                T0
            ),
            Err(KeyStoreError::RotationLimit {
                generation: u32::MAX
            })
        ));
    }

    #[test]
    fn rotating_past_the_generation_limit_is_refused() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let record = store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .write(
                &EntryName::agent_record(Network::Testnet, &a).expect("valid id"),
                &serde_json::to_string(&AgentWallet {
                    generation: MAX_GENERATION,
                    ..record
                })
                .expect("encode"),
            )
            .expect("write");

        // The last legal generation still reads; it just cannot go on.
        assert_eq!(
            store
                .agent_wallet(&a)
                .expect("read")
                .expect("present")
                .generation,
            MAX_GENERATION
        );
        assert!(matches!(
            store.rotate_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0 + 1),
            Err(KeyStoreError::RotationLimit { generation }) if generation == MAX_GENERATION + 1
        ));
    }

    #[test]
    fn the_largest_wallet_record_fits_the_windows_credential_blob() {
        // Every field is fixed-width, so this *is* the worst case: the highest
        // generation the store accepts and clocks at `u64::MAX`.
        let record = AgentWallet {
            generation: MAX_GENERATION,
            address: Address::from_bytes([0xff; 20]),
            approved_at_ms: u64::MAX,
            valid_until_ms: u64::MAX,
        };
        let blob_bytes = serde_json::to_string(&record)
            .expect("encode")
            .encode_utf16()
            .count()
            * 2;
        assert!(
            blob_bytes <= WINDOWS_CREDENTIAL_BLOB_MAX_BYTES / 2,
            "the record is {blob_bytes} bytes as UTF-16, past half the \
             {WINDOWS_CREDENTIAL_BLOB_MAX_BYTES}-byte Windows credential blob \
             cap — a variable-length field was added"
        );
    }

    #[test]
    fn delete_agent_removes_every_generation() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .rotate_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0)
            .expect("rotate");
        store
            .rotate_agent_key(&a, secret(KEY_C), T0 + DAY_MS, T0)
            .expect("rotate");
        // A second agent proves the delete is scoped.
        store
            .create_agent_key(&agent("beta"), secret(KEY_D), T0 + DAY_MS, T0)
            .expect("create");

        store.delete_agent(&a).expect("delete");

        assert!(store.agent_wallet(&a).expect("read").is_none());
        assert!(matches!(
            store.load_agent_key(&a),
            Err(KeyStoreError::Missing { .. })
        ));
        // No entry holding key material survives, and beta is untouched.
        let left = store.entry_names();
        assert!(
            !left
                .iter()
                .any(|n| n.contains("agent-key/alpha/") || n.contains("agent-record/alpha")),
            "an alpha key or record survived the delete: {left:?}"
        );
        assert!(left.contains(&format!("{SERVICE_TESTNET}:agent-key/beta/0")));
        assert!(left.contains(&format!("{SERVICE_TESTNET}:agent-record/beta")));
        // The address marks stay, so the addresses stay retired.
        assert_eq!(
            left.iter().filter(|n| n.contains("agent-address/")).count(),
            4,
            "alpha's three marks and beta's one must all survive: {left:?}"
        );
        // Deleting again is a no-op rather than an error.
        store.delete_agent(&a).expect("idempotent delete");
    }

    #[test]
    fn a_record_without_its_key_reports_missing_rather_than_loading_junk() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .remove(&EntryName::agent_key(Network::Testnet, &a, 0).expect("valid id"))
            .expect("remove");
        assert!(matches!(
            store.load_agent_key(&a),
            Err(KeyStoreError::Missing { .. })
        ));
    }

    #[test]
    fn a_corrupt_record_is_typed_not_a_panic() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .write(
                &EntryName::agent_record(Network::Testnet, &a).expect("valid id"),
                "{not json",
            )
            .expect("write");
        assert!(matches!(
            store.agent_wallet(&a),
            Err(KeyStoreError::Encoding(_))
        ));
    }

    #[test]
    fn a_record_stored_with_a_non_key_value_fails_to_load_as_a_key() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .write(
                &EntryName::agent_key(Network::Testnet, &a, 0).expect("valid id"),
                "not a key",
            )
            .expect("write");
        assert!(matches!(
            store.load_agent_key(&a),
            Err(KeyStoreError::InvalidKey)
        ));
    }

    #[test]
    fn a_key_that_does_not_derive_its_record_address_is_refused_on_load() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let record = store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        let key_entry = EntryName::agent_key(Network::Testnet, &a, 0).expect("valid id");

        // A perfectly valid *other* key swapped in under the same entry: the
        // write path's pairing check cannot see this, because it already ran.
        store.write(&key_entry, KEY_B).expect("write");
        let b_address = AgentKey::from_hex(KEY_B).expect("valid key").address();
        assert!(
            matches!(
                store.load_agent_key(&a),
                Err(KeyStoreError::AddressMismatch { record: r, derived })
                    if r == record.address && derived == b_address
            ),
            "a swapped key loaded as if it were the approved one"
        );

        // The other direction: the key stands and the record's address is
        // edited. Same refusal — neither side is trusted over the other.
        store.write(&key_entry, KEY_A).expect("write");
        let tampered = AgentWallet {
            address: b_address,
            ..record.clone()
        };
        store
            .write(
                &EntryName::agent_record(Network::Testnet, &a).expect("valid id"),
                &serde_json::to_string(&tampered).expect("encode"),
            )
            .expect("write");
        assert!(
            matches!(
                store.load_agent_key(&a),
                Err(KeyStoreError::AddressMismatch { record: r, derived })
                    if r == b_address && derived == record.address
            ),
            "an edited record loaded a key the record does not name"
        );
    }

    // -- concurrency -------------------------------------------------------

    #[test]
    fn concurrent_rotations_each_mint_their_own_generation() {
        const ROTATIONS: usize = 4;

        let store = MemoryKeyStore::with_read_delay(Network::Testnet, Duration::from_millis(25));
        let a = agent("alpha");
        let key_at = |i: usize| format!("{:02x}{}", i + 1, &KEY_A[2..]);
        store
            .create_agent_key(&a, secret(&key_at(0)), T0 + DAY_MS, T0)
            .expect("create");

        let barrier = Barrier::new(ROTATIONS);
        thread::scope(|scope| {
            for i in 1..=ROTATIONS {
                let (store, a, barrier) = (&store, &a, &barrier);
                let key = key_at(i);
                scope.spawn(move || {
                    barrier.wait();
                    store
                        .rotate_agent_key(a, secret(&key), T0 + DAY_MS, T0 + i as u64)
                        .expect("rotate")
                });
            }
        });

        let record = store.agent_wallet(&a).expect("read").expect("present");
        assert_eq!(
            record.generation, ROTATIONS as u32,
            "rotations shared a generation: {ROTATIONS} ran, the record is at {}",
            record.generation
        );
        // Every generation still has its key. An interleaved pair writes the
        // same entry name twice and one agent loses the signer it needs to
        // cancel its own resting orders.
        for generation in 0..=record.generation {
            let entry = EntryName::agent_key(Network::Testnet, &a, generation).expect("valid id");
            assert!(
                store.read(&entry).expect("read").is_some(),
                "generation {generation} lost its key entry"
            );
        }
        // One address mark per generation: every rotation installed a distinct
        // address, and none was silently overwritten.
        assert_eq!(
            store
                .entry_names()
                .iter()
                .filter(|n| n.contains("agent-address/"))
                .count(),
            ROTATIONS + 1,
            "an address was installed twice"
        );
        assert_eq!(
            store.load_agent_key(&a).expect("load").address(),
            record.address
        );
    }

    #[test]
    fn concurrent_rotations_to_the_same_address_install_it_once() {
        // The reuse check and the write that satisfies it are a
        // read-modify-write like the generation is. Two rotations offered the
        // same key must not both pass the check before either writes its mark.
        let store = MemoryKeyStore::with_read_delay(Network::Testnet, Duration::from_millis(25));
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");

        let barrier = Barrier::new(2);
        let outcomes = thread::scope(|scope| {
            let handles = [1u64, 2].map(|i| {
                let (store, a, barrier) = (&store, &a, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    store.rotate_agent_key(a, secret(KEY_B), T0 + DAY_MS, T0 + i)
                })
            });
            handles.map(|h| h.join().expect("thread did not panic"))
        });

        assert_eq!(
            outcomes.iter().filter(|r| r.is_ok()).count(),
            1,
            "the same address was installed twice"
        );
        assert!(
            outcomes
                .iter()
                .any(|r| matches!(r, Err(KeyStoreError::AddressReused { .. }))),
            "the losing rotation did not report AddressReused"
        );
        let record = store.agent_wallet(&a).expect("read").expect("present");
        assert_eq!(record.generation, 1);
    }

    #[test]
    fn concurrent_creates_leave_exactly_one_wallet() {
        let store = MemoryKeyStore::with_read_delay(Network::Testnet, Duration::from_millis(25));
        let a = agent("alpha");
        let barrier = Barrier::new(2);

        let outcomes = thread::scope(|scope| {
            let handles = [KEY_A, KEY_B].map(|key| {
                let (store, a, barrier) = (&store, &a, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    store.create_agent_key(a, secret(key), T0 + DAY_MS, T0)
                })
            });
            handles.map(|h| h.join().expect("thread did not panic"))
        });

        let created = outcomes.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            created, 1,
            "both creates succeeded; the second overwrote the first agent's key"
        );
        assert!(
            outcomes
                .iter()
                .any(|r| matches!(r, Err(KeyStoreError::AlreadyExists { .. }))),
            "the losing create did not report AlreadyExists"
        );

        let record = store.agent_wallet(&a).expect("read").expect("present");
        assert_eq!(record.generation, 0);
        assert_eq!(
            store.load_agent_key(&a).expect("load").address(),
            record.address
        );
    }

    #[test]
    fn the_record_serializes_deterministically() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .rotate_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0 + 1)
            .expect("rotate");
        let record = store.agent_wallet(&a).expect("read").expect("present");
        let once = serde_json::to_string(&record).expect("encode");
        let twice = serde_json::to_string(&record).expect("encode");
        assert_eq!(once, twice);
        assert!(
            once.starts_with(r#"{"generation":1,"address":"0x"#),
            "field order changed: {once}"
        );
        let round_tripped: AgentWallet = serde_json::from_str(&once).expect("decode");
        assert_eq!(round_tripped, record);
    }

    // -- expiry (docs/decisions.md D-b) ------------------------------------

    #[test]
    fn expiry_states_follow_the_ninety_and_fourteen_day_window() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let record = store
            .create_agent_key(&a, secret(KEY_A), default_valid_until_ms(T0), T0)
            .expect("create");

        assert_eq!(
            record.expiry(T0),
            ExpiryState::Valid {
                remaining_ms: 90 * DAY_MS
            }
        );

        // One millisecond before the warning window opens.
        let just_before = T0 + 76 * DAY_MS - 1;
        assert!(matches!(
            record.expiry(just_before),
            ExpiryState::Valid { .. }
        ));

        // Exactly 14 days out is already a warning.
        assert_eq!(
            record.expiry(T0 + 76 * DAY_MS),
            ExpiryState::Expiring {
                remaining_ms: AGENT_EXPIRY_WARNING_MS
            }
        );

        // The instant of expiry is expired, not expiring.
        assert_eq!(
            record.expiry(T0 + 90 * DAY_MS),
            ExpiryState::Expired { elapsed_ms: 0 }
        );
        assert_eq!(
            record.expiry(T0 + 91 * DAY_MS),
            ExpiryState::Expired { elapsed_ms: DAY_MS }
        );
    }

    #[test]
    fn a_backwards_clock_does_not_underflow() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let record = store
            .create_agent_key(&agent("alpha"), secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        // A clock behind `approved_at_ms` must report the whole window as
        // remaining, not wrap into a huge `elapsed_ms` under `Expired`.
        assert_eq!(
            record.expiry(0),
            ExpiryState::Valid {
                remaining_ms: T0 + DAY_MS
            }
        );
    }

    #[test]
    fn default_valid_until_saturates() {
        assert_eq!(default_valid_until_ms(u64::MAX), u64::MAX);
        assert_eq!(default_valid_until_ms(T0), T0 + AGENT_APPROVAL_TTL_MS);
    }

    // -- the guardrail HMAC key (docs/spec.md item 3) ----------------------

    #[test]
    fn hmac_signs_and_verifies() {
        let key = HmacKey::from_bytes([7u8; HMAC_KEY_LEN]);
        let message = br#"{"max_order_usd":"25"}"#;
        let tag = key.sign(message).expect("sign");
        assert!(key.verify(message, &tag));
    }

    #[test]
    fn hmac_rejects_tampering() {
        let key = HmacKey::from_bytes([7u8; HMAC_KEY_LEN]);
        let tag = key.sign(br#"{"max_order_usd":"25"}"#).expect("sign");

        // A raised limit under the old tag.
        assert!(!key.verify(br#"{"max_order_usd":"25000"}"#, &tag));
        // A flipped tag bit.
        let mut flipped = tag;
        flipped[0] ^= 1;
        assert!(!key.verify(br#"{"max_order_usd":"25"}"#, &flipped));
        // A truncated tag.
        assert!(!key.verify(br#"{"max_order_usd":"25"}"#, &tag[..16]));
        // An empty tag.
        assert!(!key.verify(br#"{"max_order_usd":"25"}"#, &[]));
        // A different key.
        let other = HmacKey::from_bytes([8u8; HMAC_KEY_LEN]);
        assert!(!other.verify(br#"{"max_order_usd":"25"}"#, &tag));
    }

    #[test]
    fn hmac_key_round_trips_through_the_store() {
        let store = MemoryKeyStore::new(Network::Testnet);
        assert!(store.load_hmac_key().expect("load").is_none());

        let key = HmacKey::from_bytes([3u8; HMAC_KEY_LEN]);
        store.store_hmac_key(&key).expect("store");

        let loaded = store.load_hmac_key().expect("load").expect("present");
        let tag = key.sign(b"config").expect("sign");
        assert!(loaded.verify(b"config", &tag));

        assert_eq!(
            store.entry_names(),
            vec![format!("{SERVICE_TESTNET}:guardrail-hmac")]
        );
    }

    #[test]
    fn ensure_hmac_key_generates_once_and_then_returns_the_same_key() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let first = match store.ensure_hmac_key() {
            Ok(key) => key,
            // Non-unix has no RNG dependency yet; see HmacKey::generate.
            Err(KeyStoreError::EntropyUnavailable { .. }) if !cfg!(unix) => return,
            Err(other) => panic!("ensure_hmac_key: {other}"),
        };
        let tag = first.sign(b"config").expect("sign");
        let second = store.ensure_hmac_key().expect("ensure");
        assert!(second.verify(b"config", &tag));
    }

    #[cfg(unix)]
    #[test]
    fn generated_keys_are_not_constant() {
        // Not a randomness test — it catches a source that returns zeros or a
        // fixed buffer, which is the failure mode that would silently ship.
        let a = HmacKey::generate().expect("generate");
        let b = HmacKey::generate().expect("generate");
        assert_ne!(a.0, [0u8; HMAC_KEY_LEN]);
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn hmac_keys_of_the_wrong_size_are_typed_corruption() {
        let store = MemoryKeyStore::new(Network::Testnet);
        store
            .write(&EntryName::guardrail_hmac(Network::Testnet), "abcd")
            .expect("write");
        assert!(matches!(
            store.load_hmac_key(),
            Err(KeyStoreError::Corrupt { .. })
        ));
    }

    // -- secrets in memory -------------------------------------------------

    #[test]
    fn secrets_do_not_print_themselves() {
        let s = SecretText::new(KEY_A.to_owned());
        let rendered = format!("{s:?}");
        assert!(!rendered.contains(KEY_A), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");

        let key = HmacKey::from_bytes([9u8; HMAC_KEY_LEN]);
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("09090909"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn errors_never_debug_print_what_the_backend_read() {
        // `keyring::Error::BadEncoding` carries the raw credential blob the
        // store just read and derives `Debug`. A derived `Debug` on
        // `KeyStoreError` would carry it into `tracing::error!(?e)`, an
        // `expect` panic payload and a Tauri command's error result.
        let err = KeyStoreError::Backend(keyring::Error::BadEncoding(KEY_A.as_bytes().to_vec()));
        let rendered = format!("{err:?}");
        let first_byte = KEY_A.as_bytes()[0].to_string();
        assert!(!rendered.contains(&first_byte), "{rendered}");
        assert!(!rendered.contains("BadEncoding"), "{rendered}");
        assert_eq!(rendered, format!("{err}"));
    }

    #[test]
    fn non_utf8_secrets_are_refused_rather_than_guessed_at() {
        let mut bytes = KEY_A.to_owned().into_bytes();
        bytes[0] = 0xff;
        // Safety-free construction: SecretText is byte-backed, so an invalid
        // sequence is representable and must be rejected on read.
        let s = SecretText(bytes);
        assert!(matches!(s.as_str(), Err(KeyStoreError::Corrupt { .. })));
    }

    #[test]
    fn a_delete_racing_a_rotation_leaves_no_key_behind() {
        // The rotation is inside AGENT_WALLET_LOCK with a 60 ms read; the
        // delete starts 30 ms in. Without the lock on the delete side it reads
        // the pre-rotation record, deletes what that names, and reports success
        // while the key the rotation minted is still stored.
        let store = MemoryKeyStore::with_read_delay(Network::Testnet, Duration::from_millis(60));
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");

        thread::scope(|scope| {
            let (s1, a1) = (&store, &a);
            scope.spawn(move || s1.rotate_agent_key(a1, secret(KEY_B), T0 + DAY_MS, T0 + 1));
            let (s2, a2) = (&store, &a);
            scope.spawn(move || {
                thread::sleep(Duration::from_millis(30));
                s2.delete_agent(a2)
            });
        });

        let left = store.entry_names();
        assert!(
            !left.iter().any(|n| n.contains("agent-key/")),
            "a private key survived a delete that returned Ok: {left:?}"
        );
    }

    #[test]
    fn a_delete_removes_the_key_a_crashed_rotation_orphaned() {
        // `write_wallet` writes the key before the record, so a crash between
        // the two leaves generation + 1 with no record pointing at it.
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .write(
                &EntryName::agent_key(Network::Testnet, &a, 1).expect("valid id"),
                KEY_B,
            )
            .expect("write");

        store.delete_agent(&a).expect("delete");
        let left = store.entry_names();
        assert!(
            !left.iter().any(|n| n.contains("agent-key/")),
            "the orphaned key survived the delete: {left:?}"
        );
    }

    // -- expiry is not a load gate ----------------------------------------

    #[test]
    fn an_expired_wallet_still_loads() {
        // `docs/decisions.md` D-b's remedy is to cancel the agent's resting
        // orders, which needs its key. Expiry is the guardrail engine's
        // predicate, not this store's.
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let record = store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        assert!(matches!(
            record.expiry(T0 + 400 * DAY_MS),
            ExpiryState::Expired { .. }
        ));
        assert_eq!(
            store
                .load_agent_key(&a)
                .expect("an expired wallet must still load")
                .address(),
            record.address
        );
    }

    #[test]
    fn a_retired_address_cannot_return_under_a_second_agent_id() {
        // `docs/specs/onboarding.md` §7.3 retires the address, not the pairing.
        // A mark scoped to the agent id would let the same key — the same
        // pruned nonce set — be installed one container over.
        let store = MemoryKeyStore::new(Network::Testnet);
        let alpha = agent("alpha");
        let first = store
            .create_agent_key(&alpha, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .rotate_agent_key(&alpha, secret(KEY_B), T0 + DAY_MS, T0 + 1)
            .expect("rotate");

        for (id, key) in [("beta", KEY_A), ("gamma", KEY_B)] {
            assert!(
                matches!(
                    store.create_agent_key(&agent(id), secret(key), T0 + DAY_MS, T0 + 2),
                    Err(KeyStoreError::AddressReused { .. })
                ),
                "an address alpha retired came back as {id}"
            );
        }
        // Deleting alpha does not release them either.
        store.delete_agent(&alpha).expect("delete");
        assert!(matches!(
            store.create_agent_key(&agent("beta"), secret(KEY_A), T0 + DAY_MS, T0 + 3),
            Err(KeyStoreError::AddressReused { address }) if address == first.address
        ));
        // A key no agent has installed is still accepted.
        store
            .create_agent_key(&agent("beta"), secret(KEY_C), T0 + DAY_MS, T0 + 4)
            .expect("an unused address must still be installable");
    }

    // -- the real keychain -------------------------------------------------

    /// Exercises the platform credential store end to end.
    ///
    /// `#[ignore]` by default: CI has no keychain, and on a developer's macOS
    /// machine this writes and deletes real Keychain items. Run it with
    /// `cargo test -p oppen-core keys::tests::keychain -- --ignored`.
    #[test]
    #[ignore = "touches the real OS credential store"]
    fn keychain_round_trips_an_agent_wallet() {
        let store = KeychainKeyStore::new(Network::Testnet);
        // A fresh id per run, and the address marks are cleared at the end:
        // `delete_agent` keeps them on purpose, and they are not scoped to the
        // agent id, so a run that left them behind would (correctly) refuse
        // KEY_A and KEY_B for every later run.
        let a = agent(&format!("oppen-selftest.{T0}"));
        store.delete_agent(&a).expect("pre-clean");

        let record = store
            .create_agent_key(&a, secret(KEY_A), default_valid_until_ms(T0), T0)
            .expect("create");
        let key = store.load_agent_key(&a).expect("load");
        assert_eq!(key.address(), record.address);

        let rotated = store
            .rotate_agent_key(&a, secret(KEY_B), default_valid_until_ms(T0), T0)
            .expect("rotate");
        assert_eq!(rotated.generation, 1);
        assert_eq!(
            store.load_agent_key(&a).expect("load").address(),
            rotated.address
        );
        assert!(matches!(
            store.rotate_agent_key(&a, secret(KEY_A), default_valid_until_ms(T0), T0),
            Err(KeyStoreError::AddressReused { .. })
        ));

        store.delete_agent(&a).expect("delete");
        assert!(store.agent_wallet(&a).expect("read").is_none());
        assert!(matches!(
            store.load_agent_key(&a),
            Err(KeyStoreError::Missing { .. })
        ));
        // The address marks outlive the delete; clear them so the run leaves
        // nothing behind.
        for address in [record.address, rotated.address] {
            store
                .remove(&EntryName::agent_address(Network::Testnet, &address))
                .expect("clean up the address mark");
        }
    }
}
