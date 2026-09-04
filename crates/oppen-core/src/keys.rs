//! OS keychain storage for the two secrets oppen holds: agent wallet private
//! keys and the guardrail-config HMAC key.
//!
//! `docs/spec.md` item 2 puts keys in the OS keychain; item 3 HMAC-checks the
//! guardrail configuration with a key from that same keychain. This module
//! owns the storage and the HMAC primitive. It does **not** own the guardrail
//! configuration, which lives in [`crate::guardrail`], and it does not own key
//! generation for agent wallets, which is `oppen-hl`'s job — the key material
//! arrives here already generated and leaves here only as an
//! [`oppen_hl::AgentKey`].
//!
//! # What this protects against, and what it does not
//!
//! `docs/threat-model.md` is the normative text and this module must not
//! promise more than it does:
//!
//! - On **Windows** (Credential Manager) and **Linux** (Secret Service), any
//!   process running as the same OS user can read these secrets. An agent with
//!   shell access can read a stored agent key and sign orders directly against
//!   Hyperliquid, bypassing every guardrail.
//! - On **macOS**, the Keychain prompts per application. That is a real barrier
//!   against casual access, not against a determined process holding your
//!   user's privileges.
//! - The keychain is therefore *containment*, not a boundary. The only
//!   containment property that survives a compromised machine is the venue's:
//!   an agent (API) wallet cannot withdraw.
//!
//! Zeroizing a loaded key on drop narrows a window in this process's address
//! space. It does not make the keychain a boundary, and nothing in this file
//! should be read as claiming it does.
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
//!   generation and refuses an address the record has already seen.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{Ordering, compiler_fence};

use hmac::{Hmac, KeyInit, Mac};
use oppen_hl::{Address, AgentKey, Network};
use serde::{Deserialize, Serialize};
use sha3::Sha3_256;

use crate::guardrail::AgentId;

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

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
pub const SERVICE_TESTNET: &str = "xyz.oppen.testnet";

/// Keychain service holding every mainnet secret. Frozen, for the same reason
/// as [`SERVICE_TESTNET`].
///
/// The network is in the *service* rather than in the entry name because
/// `docs/decisions.md` R4 calls a mainnet number that is actually a testnet
/// number the worst bug this product can ship. A per-network service makes the
/// collision impossible to write, and makes the split visible in Keychain
/// Access and Credential Manager where an operator can audit it.
pub const SERVICE_MAINNET: &str = "xyz.oppen.mainnet";

/// Entry-name prefix for one generation of one agent's wallet key.
const PREFIX_AGENT_KEY: &str = "agent-key";
/// Entry-name prefix for an agent's non-secret wallet record.
const PREFIX_AGENT_RECORD: &str = "agent-record";
/// Entry name of the guardrail-config HMAC key (`docs/spec.md` item 3).
const ENTRY_GUARDRAIL_HMAC: &str = "guardrail-hmac";

/// Separator inside an entry name. Also the one byte an [`AgentId`] may not
/// contain, which is what keeps `agent-key/<id>/<generation>` unambiguous.
const ENTRY_SEPARATOR: char = '/';

/// Longest [`AgentId`] accepted into an entry name.
///
/// `AgentId::new` takes any `String`, so the bound has to be applied here
/// rather than assumed. 64 bytes is longer than any identifier the pairing
/// flow mints and short enough that the resulting entry name stays well inside
/// every platform's target-name limit.
const MAX_AGENT_ID_BYTES: usize = 64;

// ---------------------------------------------------------------------------
// Expiry (docs/decisions.md D-b)
// ---------------------------------------------------------------------------

/// How long an `approveAgent` approval oppen requests is valid for: 90 days
/// (`docs/decisions.md` D-b). Long enough not to be a chore, short enough that
/// an abandoned deployment stops trading within a quarter.
pub const AGENT_APPROVAL_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1_000;

/// How long before `valid_until` the console and `get_state` start warning:
/// 14 days (`docs/decisions.md` D-b).
pub const AGENT_EXPIRY_WARNING_MS: u64 = 14 * 24 * 60 * 60 * 1_000;

/// `valid_until` oppen should request for an approval signed at `now_ms`.
///
/// Saturating rather than wrapping: a clock far enough in the future to
/// overflow should produce a permanently-valid approval, never a
/// silently-expired one.
pub fn default_valid_until_ms(now_ms: u64) -> u64 {
    now_ms.saturating_add(AGENT_APPROVAL_TTL_MS)
}

/// Most retired agent addresses kept in a wallet record.
///
/// The record is one keychain entry, and the Windows Credential Manager caps a
/// credential blob at 2,560 bytes (`CRED_MAX_CREDENTIAL_BLOB_SIZE`). A retired
/// entry serializes to roughly 100 bytes, so sixteen of them plus the record's
/// own fields stays under half that limit. At one rotation per 90-day approval
/// that is four years of history, which is long enough to make accidental
/// reuse of a pruned address a non-event, and the alternative — an unbounded
/// list — is a record that silently stops being writable on Windows.
pub const MAX_RETIRED_ADDRESSES: usize = 16;

// ---------------------------------------------------------------------------
// Secrets in memory
// ---------------------------------------------------------------------------

/// Secret text held only as long as it is needed, overwritten on drop.
///
/// This is the hand-off type between the keychain and
/// [`oppen_hl::AgentKey::from_hex`], which zeroizes its own decode buffer. The
/// review item it closes is that the *hex* must not linger either: the
/// keychain hands back an ordinary `String`, and without a wrapper that
/// allocation is freed with the private key still in it.
///
/// **This is a stand-in for `zeroize::Zeroizing<String>` and is weaker.**
/// `oppen-core` does not declare the `zeroize` crate (see this module's
/// follow-ups), so the overwrite here is a plain loop plus a compiler fence
/// rather than `zeroize`'s volatile writes; a sufficiently aggressive
/// optimizer is permitted to elide it. It also cannot reach the copy
/// `keyring` made inside its own decode path. Swap it for `Zeroizing<String>`
/// the moment the dependency exists.
pub struct SecretText(Vec<u8>);

impl SecretText {
    /// Takes ownership of `text`. The `String`'s allocation is moved, not
    /// copied, so no second buffer holding the secret is created here.
    pub fn new(text: String) -> Self {
        SecretText(text.into_bytes())
    }

    /// Borrows the secret as `&str` for the one call that needs it.
    ///
    /// Fails rather than lossily converting: a keychain entry that is not
    /// UTF-8 is a corrupt entry, and guessing at a private key is worse than
    /// refusing to load it.
    pub fn as_str(&self) -> Result<&str, KeyStoreError> {
        std::str::from_utf8(&self.0).map_err(|_| KeyStoreError::Corrupt {
            detail: "secret is not valid UTF-8".to_owned(),
        })
    }

    /// Length in bytes. Exposed because callers validate hex length before
    /// deciding whether an entry is plausible; the *content* stays private.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the stored secret is empty, which for every entry this module
    /// writes means a corrupt entry.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every way a key operation fails, typed so no caller has to parse a message
/// (`AGENTS.md` invariant 8).
#[derive(Debug, thiserror::Error)]
pub enum KeyStoreError {
    /// The platform credential store failed or is unavailable. On Linux this
    /// is most often a locked or absent Secret Service.
    #[error("keychain backend: {0}")]
    Backend(#[from] keyring::Error),

    /// The in-memory store's lock was poisoned by a panic in another thread.
    #[error("key store lock poisoned")]
    Poisoned,

    /// Nothing is stored under this entry.
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

    /// A rotation tried to install an address the record has already used.
    ///
    /// Hyperliquid prunes an agent when it is replaced and the nonce state
    /// goes with it, so a reused address can be replayed against. This is
    /// refused rather than warned about.
    #[error("agent address {address} was already used by this agent; addresses are never reused")]
    AddressReused { address: Address },

    /// The generation counter cannot advance further. Unreachable in practice
    /// — it is `u32` and a generation is one 90-day approval — but it is not a
    /// place to panic.
    #[error("agent {agent} has exhausted its rotation counter")]
    RotationOverflow { agent: String },

    /// No OS entropy source is reachable, so no key was generated.
    ///
    /// Fails closed on purpose: a guardrail HMAC key from a weak source is
    /// worse than no key, because it looks like protection.
    #[error("no OS entropy source available: {detail}")]
    EntropyUnavailable { detail: String },

    /// Serializing or deserializing a wallet record failed.
    #[error("wallet record encoding: {0}")]
    Encoding(#[from] serde_json::Error),

    /// The HMAC key length was rejected by the MAC construction. Structurally
    /// unreachable for a 32-byte key; typed so the primitive never panics.
    #[error("hmac key rejected by the mac construction")]
    BadMacKey,
}

// ---------------------------------------------------------------------------
// Entry names
// ---------------------------------------------------------------------------

/// A fully-qualified keychain entry: the per-network service plus the account
/// name inside it.
///
/// Constructed only through the associated functions below, so every entry
/// this crate touches is network-qualified by construction and no caller can
/// assemble a name that crosses networks (`docs/decisions.md` R4).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryName {
    service: &'static str,
    account: String,
}

impl EntryName {
    /// The keychain service, which carries the network.
    pub fn service(&self) -> &'static str {
        self.service
    }

    /// The account (entry) name inside the service.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// One generation of one agent's wallet key.
    ///
    /// The generation is part of the name because `docs/decisions.md` D-b
    /// forbids reusing an agent address across a rotation: a rotation writes a
    /// name that has never existed, so it cannot overwrite the key it
    /// replaces.
    pub fn agent_key(
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

    /// An agent's wallet record: which generation is current, its address, its
    /// approval window and the addresses it has retired. Not secret, but it
    /// belongs next to the key it describes so that deleting an agent deletes
    /// both.
    pub fn agent_record(network: Network, agent: &AgentId) -> Result<Self, KeyStoreError> {
        let id = checked_agent_id(agent)?;
        Ok(EntryName {
            service: service_of(network),
            account: format!("{PREFIX_AGENT_RECORD}{ENTRY_SEPARATOR}{id}"),
        })
    }

    /// The guardrail-config HMAC key (`docs/spec.md` item 3). One per network,
    /// because the guardrail configuration is per network too.
    pub fn guardrail_hmac(network: Network) -> Self {
        EntryName {
            service: service_of(network),
            account: ENTRY_GUARDRAIL_HMAC.to_owned(),
        }
    }
}

impl std::fmt::Display for EntryName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.service, self.account)
    }
}

/// The keychain service for `network` (`docs/decisions.md` R4).
pub fn service_of(network: Network) -> &'static str {
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
fn checked_agent_id(agent: &AgentId) -> Result<&str, KeyStoreError> {
    let id = agent.as_str();
    if id.is_empty() {
        return Err(KeyStoreError::InvalidAgentId {
            id: id.to_owned(),
            reason: "empty",
        });
    }
    if id.len() > MAX_AGENT_ID_BYTES {
        return Err(KeyStoreError::InvalidAgentId {
            id: id.to_owned(),
            reason: "longer than 64 bytes",
        });
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(KeyStoreError::InvalidAgentId {
            id: id.to_owned(),
            reason: "characters outside [A-Za-z0-9._-]",
        });
    }
    Ok(id)
}

// ---------------------------------------------------------------------------
// Wallet records
// ---------------------------------------------------------------------------

/// An agent address this agent has stopped using.
///
/// Kept so a rotation can refuse to reinstall it. Hyperliquid prunes a
/// replaced agent and its nonce state, so re-approving an old address hands an
/// attacker a signer whose nonce window has been reset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredAgentWallet {
    /// Generation this address served.
    pub generation: u32,
    /// The retired agent address.
    pub address: Address,
    /// When the rotation that retired it happened, in ms since the epoch.
    pub retired_at_ms: u64,
}

/// The non-secret record describing an agent's current wallet.
///
/// Serialized in declaration order into one keychain entry, so the stored
/// bytes are byte-identical for equal values (`AGENTS.md` invariant 6). It is
/// what lets the console and `get_state` answer "how long has this agent got"
/// without ever touching the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWallet {
    /// Which agent this wallet belongs to.
    pub agent: AgentId,
    /// Rotation counter. `0` is the first wallet; each rotation adds one and
    /// names a keychain entry that has never existed before.
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
    /// Addresses this agent has retired, oldest first, capped at
    /// [`MAX_RETIRED_ADDRESSES`].
    pub retired: Vec<RetiredAgentWallet>,
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
    Valid {
        /// Milliseconds until `valid_until`.
        remaining_ms: u64,
    },
    /// Inside D-b's 14-day warning window. The operator should be told.
    Expiring {
        /// Milliseconds until `valid_until`.
        remaining_ms: u64,
    },
    /// `valid_until` has passed. D-b: signing fails, oppen halts the agent and
    /// cancels resting orders, and **leaves positions open** — force-closing
    /// on a calendar event is a destructive action triggered by a clock.
    Expired {
        /// Milliseconds since `valid_until`.
        elapsed_ms: u64,
    },
}

impl AgentWallet {
    /// Milliseconds left on the approval, `0` once it has passed.
    ///
    /// Saturating both ways so a clock that jumped backwards produces a large
    /// remaining time rather than an underflow.
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.valid_until_ms.saturating_sub(now_ms)
    }

    /// Where this approval sits in D-b's window at `now_ms`.
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

    /// Whether `address` has ever been this agent's, current or retired.
    ///
    /// The retired list is capped, so a `false` from this means "not in the
    /// last [`MAX_RETIRED_ADDRESSES`] rotations", not "never". Documented
    /// rather than papered over: the addresses come from freshly generated
    /// keys, so a collision past the cap requires deliberately importing an
    /// old key.
    pub fn has_used(&self, address: &Address) -> bool {
        self.address == *address || self.retired.iter().any(|r| r.address == *address)
    }
}

// ---------------------------------------------------------------------------
// The guardrail HMAC key
// ---------------------------------------------------------------------------

/// Length of the guardrail HMAC key and of the tags it produces, in bytes.
pub const HMAC_KEY_LEN: usize = 32;
/// Length of an HMAC-SHA3-256 tag, in bytes.
pub const HMAC_TAG_LEN: usize = 32;

/// The key behind `docs/spec.md` item 3: the guardrail configuration is
/// HMAC-checked so that editing the SQLite file cannot silently raise a limit.
///
/// `docs/threat-model.md` is explicit that this makes tampering **detectable,
/// not preventable** — the same-user process that can edit the database can
/// also read this key out of the keychain. What it buys is that a limit
/// changed outside oppen does not pass unnoticed.
///
/// HMAC-SHA3-256 because `sha3` is already the workspace's hash — the ledger
/// chain and the L1 action hash both use Keccak — and adding a second hash
/// family for one MAC is a dependency with no argument behind it.
pub struct HmacKey([u8; HMAC_KEY_LEN]);

impl HmacKey {
    /// Wraps caller-supplied key bytes.
    ///
    /// Public so a front end that *does* have an OS RNG can generate the key
    /// itself on a platform where [`HmacKey::generate`] cannot (see its docs).
    pub fn from_bytes(bytes: [u8; HMAC_KEY_LEN]) -> Self {
        HmacKey(bytes)
    }

    /// Generates a key from the OS CSPRNG.
    ///
    /// On unix this reads `/dev/urandom`, which is the kernel CSPRNG on both
    /// macOS and Linux and does not block. **On other platforms — Windows
    /// included — this returns [`KeyStoreError::EntropyUnavailable`]**, because
    /// `oppen-core` does not declare a `getrandom`-style dependency and there
    /// is no `std` API for OS entropy. That is a real gap, not a design
    /// choice; it fails closed so a weak key is unrepresentable, and the fix is
    /// one dependency line. Until then, a Windows front end must call
    /// [`HmacKey::from_bytes`] with entropy it obtained itself and store it via
    /// [`KeyStore::store_hmac_key`].
    pub fn generate() -> Result<Self, KeyStoreError> {
        let mut bytes = [0u8; HMAC_KEY_LEN];
        os_entropy(&mut bytes)?;
        Ok(HmacKey(bytes))
    }

    /// Tags `message`. The guardrail module decides *what* is signed; this is
    /// only the primitive.
    ///
    /// Returns a `Result` rather than panicking on a key length the MAC could
    /// reject. HMAC accepts any key length, so the error branch is structurally
    /// dead, but a dead branch is cheaper than a `expect` on a signing path.
    pub fn sign(&self, message: &[u8]) -> Result<[u8; HMAC_TAG_LEN], KeyStoreError> {
        let mut mac =
            Hmac::<Sha3_256>::new_from_slice(&self.0).map_err(|_| KeyStoreError::BadMacKey)?;
        mac.update(message);
        Ok(mac.finalize().into_bytes().into())
    }

    /// Constant-time check that `tag` is this key's tag over `message`.
    ///
    /// Returns `false` for a wrong tag, a wrong length and for any internal
    /// failure: a verifier that could not run has not verified anything, and
    /// on this path "unknown" has to read as "no".
    pub fn verify(&self, message: &[u8], tag: &[u8]) -> bool {
        let Ok(mut mac) = Hmac::<Sha3_256>::new_from_slice(&self.0) else {
            return false;
        };
        mac.update(message);
        mac.verify_slice(tag).is_ok()
    }

    /// Storage form: 64 lowercase hex characters, wrapped so the encoded copy
    /// is overwritten when it goes out of scope.
    fn to_secret_hex(&self) -> SecretText {
        SecretText::new(hex::encode(self.0))
    }

    /// Parses the storage form written by [`HmacKey::to_secret_hex`].
    fn from_secret_hex(secret: &SecretText) -> Result<Self, KeyStoreError> {
        let text = secret.as_str()?;
        let mut bytes = [0u8; HMAC_KEY_LEN];
        hex::decode_to_slice(text, &mut bytes).map_err(|_| KeyStoreError::Corrupt {
            detail: "hmac key entry is not 32 bytes of hex".to_owned(),
        })?;
        Ok(HmacKey(bytes))
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

/// Fills `dst` from the OS CSPRNG. See [`HmacKey::generate`] for the platform
/// gap this leaves open.
#[cfg(unix)]
fn os_entropy(dst: &mut [u8]) -> Result<(), KeyStoreError> {
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
/// entropy. See [`HmacKey::generate`].
#[cfg(not(unix))]
fn os_entropy(_dst: &mut [u8]) -> Result<(), KeyStoreError> {
    Err(KeyStoreError::EntropyUnavailable {
        detail: "oppen-core has no OS RNG dependency on this platform; \
                 supply the key with HmacKey::from_bytes"
            .to_owned(),
    })
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// Read/write access to one network's secrets.
///
/// The three required methods are raw entry access; everything an operator or
/// the guardrail engine actually calls is a provided method built on them, so
/// the agent-wallet rules (one record per agent, rotation mints a new
/// generation, an address is never reinstalled) are implemented once and hold
/// for the in-memory store used by tests exactly as they do for the keychain.
///
/// `Send + Sync` because `docs/decisions.md` R1 has the core running headless
/// with the console as a client: a store is shared across the async runtime's
/// worker threads.
pub trait KeyStore: Send + Sync {
    /// Which network's secrets this store holds. Every entry name is built
    /// from it (`docs/decisions.md` R4).
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

    /// The agent's wallet record, or `None` if it has no wallet.
    fn agent_wallet(&self, agent: &AgentId) -> Result<Option<AgentWallet>, KeyStoreError> {
        let entry = EntryName::agent_record(self.network(), agent)?;
        let Some(stored) = self.read(&entry)? else {
            return Ok(None);
        };
        let record: AgentWallet = serde_json::from_str(stored.as_str()?)?;
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
    /// The key entry is written before the record. A crash between the two
    /// leaves a key no record points at, which is unusable and which a retry
    /// overwrites; the reverse order would leave a record pointing at nothing,
    /// which wedges the agent permanently.
    fn create_agent_key(
        &self,
        agent: &AgentId,
        key_hex: SecretText,
        valid_until_ms: u64,
        now_ms: u64,
    ) -> Result<AgentWallet, KeyStoreError> {
        if self.agent_wallet(agent)?.is_some() {
            return Err(KeyStoreError::AlreadyExists {
                agent: agent.as_str().to_owned(),
            });
        }
        let normalized = normalize_key_hex(&key_hex)?;
        let address = address_of_key(&normalized)?;
        let record = AgentWallet {
            agent: agent.clone(),
            generation: 0,
            address,
            approved_at_ms: now_ms,
            valid_until_ms,
            retired: Vec::new(),
        };
        self.write(
            &EntryName::agent_key(self.network(), agent, 0)?,
            normalized.as_str()?,
        )?;
        self.write(
            &EntryName::agent_record(self.network(), agent)?,
            &serde_json::to_string(&record)?,
        )?;
        Ok(record)
    }

    /// Installs a new agent wallet at generation `n + 1`.
    ///
    /// This is the API shape `docs/decisions.md` D-b asks for: a rotation
    /// *mints* an entry rather than overwriting one. The previous generation's
    /// key stays in the keychain so the retiring agent can still cancel its own
    /// resting orders; [`KeyStore::forget_retired_keys`] removes it once that
    /// is done, and the retired *addresses* survive that deletion.
    ///
    /// Refuses an address this agent has used before. Hyperliquid prunes a
    /// replaced agent along with its nonce state, so reinstalling an old
    /// address hands out a signer whose replay window has been reset.
    fn rotate_agent_key(
        &self,
        agent: &AgentId,
        key_hex: SecretText,
        valid_until_ms: u64,
        now_ms: u64,
    ) -> Result<AgentWallet, KeyStoreError> {
        let current = self
            .agent_wallet(agent)?
            .ok_or_else(|| KeyStoreError::Missing {
                entry: format!("{PREFIX_AGENT_RECORD}{ENTRY_SEPARATOR}{agent}"),
            })?;
        let normalized = normalize_key_hex(&key_hex)?;
        let address = address_of_key(&normalized)?;
        if current.has_used(&address) {
            return Err(KeyStoreError::AddressReused { address });
        }
        let generation =
            current
                .generation
                .checked_add(1)
                .ok_or_else(|| KeyStoreError::RotationOverflow {
                    agent: agent.as_str().to_owned(),
                })?;

        let mut retired = current.retired;
        retired.push(RetiredAgentWallet {
            generation: current.generation,
            address: current.address,
            retired_at_ms: now_ms,
        });
        if retired.len() > MAX_RETIRED_ADDRESSES {
            let drop_count = retired.len() - MAX_RETIRED_ADDRESSES;
            retired.drain(..drop_count);
        }

        let record = AgentWallet {
            agent: agent.clone(),
            generation,
            address,
            approved_at_ms: now_ms,
            valid_until_ms,
            retired,
        };
        self.write(
            &EntryName::agent_key(self.network(), agent, generation)?,
            normalized.as_str()?,
        )?;
        self.write(
            &EntryName::agent_record(self.network(), agent)?,
            &serde_json::to_string(&record)?,
        )?;
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
    fn load_agent_key(&self, agent: &AgentId) -> Result<AgentKey, KeyStoreError> {
        let record = self
            .agent_wallet(agent)?
            .ok_or_else(|| KeyStoreError::Missing {
                entry: format!("{PREFIX_AGENT_RECORD}{ENTRY_SEPARATOR}{agent}"),
            })?;
        let entry = EntryName::agent_key(self.network(), agent, record.generation)?;
        let stored = self.read(&entry)?.ok_or_else(|| KeyStoreError::Missing {
            entry: entry.account().to_owned(),
        })?;
        AgentKey::from_hex(stored.as_str()?).map_err(|_| KeyStoreError::InvalidKey)
    }

    /// Deletes the key material of every generation before the current one,
    /// keeping the record and therefore the retired-address list.
    ///
    /// The point of the split: forgetting a retired *key* is safe housekeeping,
    /// while forgetting a retired *address* would let a later rotation
    /// reinstall it.
    fn forget_retired_keys(&self, agent: &AgentId) -> Result<(), KeyStoreError> {
        let Some(record) = self.agent_wallet(agent)? else {
            return Ok(());
        };
        for generation in 0..record.generation {
            self.remove(&EntryName::agent_key(self.network(), agent, generation)?)?;
        }
        Ok(())
    }

    /// Removes an agent entirely: every generation's key and the record.
    ///
    /// This does drop the retired-address list, so a later agent under the same
    /// id could in principle be given an old address again. Deliberate and
    /// narrow: addresses come from freshly generated keys, so reaching that
    /// state means deliberately importing a retired key into a re-created
    /// agent. Rotation, which is the path that runs unattended, keeps the list.
    fn delete_agent(&self, agent: &AgentId) -> Result<(), KeyStoreError> {
        if let Some(record) = self.agent_wallet(agent)? {
            for generation in 0..=record.generation {
                self.remove(&EntryName::agent_key(self.network(), agent, generation)?)?;
            }
        }
        self.remove(&EntryName::agent_record(self.network(), agent)?)
    }

    /// The guardrail HMAC key, or `None` on a machine that has never run oppen
    /// on this network.
    fn load_hmac_key(&self) -> Result<Option<HmacKey>, KeyStoreError> {
        let entry = EntryName::guardrail_hmac(self.network());
        match self.read(&entry)? {
            None => Ok(None),
            Some(stored) => Ok(Some(HmacKey::from_secret_hex(&stored)?)),
        }
    }

    /// Stores `key`, replacing any existing one.
    ///
    /// Replacing invalidates every guardrail-config tag written under the old
    /// key, which then reads as tampering. That is the correct alarm — the
    /// configuration really is no longer the one that was authenticated — so
    /// the caller must re-tag the configuration in the same operation.
    fn store_hmac_key(&self, key: &HmacKey) -> Result<(), KeyStoreError> {
        let entry = EntryName::guardrail_hmac(self.network());
        let encoded = key.to_secret_hex();
        self.write(&entry, encoded.as_str()?)
    }

    /// Loads the guardrail HMAC key, generating and storing one on first run
    /// (`docs/spec.md` item 3).
    ///
    /// Not atomic against a second oppen process racing it on the same machine:
    /// both would generate, the later write would win, and configuration tagged
    /// by the loser would then fail verification. That surfaces as a tamper
    /// alarm rather than as a silent bypass, which is the failure direction
    /// this subsystem is supposed to have, and a single-instance desktop app
    /// does not reach it.
    fn ensure_hmac_key(&self) -> Result<HmacKey, KeyStoreError> {
        if let Some(existing) = self.load_hmac_key()? {
            return Ok(existing);
        }
        let key = HmacKey::generate()?;
        self.store_hmac_key(&key)?;
        Ok(key)
    }

    /// Removes the guardrail HMAC key. Every existing configuration tag becomes
    /// unverifiable, so this is an operator action, not a cleanup step.
    fn delete_hmac_key(&self) -> Result<(), KeyStoreError> {
        self.remove(&EntryName::guardrail_hmac(self.network()))
    }
}

/// Canonicalises a private key hex string to bare lowercase 64 characters.
///
/// Stored canonically so that the same key written by the onboarding flow and
/// by an import produce identical entries, and so a later reader never has to
/// guess whether a `0x` prefix is present.
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

// ---------------------------------------------------------------------------
// The keychain-backed store
// ---------------------------------------------------------------------------

/// The real store: Keychain on macOS, Credential Manager on Windows, Secret
/// Service on Linux, via the `keyring` crate.
///
/// See this module's header for what that does and does not protect. In short:
/// on Windows and Linux any process running as the same OS user can read these
/// entries, and on macOS the prompt is per application.
#[derive(Debug, Clone, Copy)]
pub struct KeychainKeyStore {
    network: Network,
}

impl KeychainKeyStore {
    /// A store over `network`'s keychain service.
    ///
    /// The network is fixed at construction rather than passed per call so that
    /// no call site can pick the wrong one (`docs/decisions.md` R4).
    pub fn new(network: Network) -> Self {
        KeychainKeyStore { network }
    }

    /// Opens the platform entry behind `name`.
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

// ---------------------------------------------------------------------------
// The in-memory store
// ---------------------------------------------------------------------------

/// A process-local store for tests and for headless runs on a machine with no
/// credential store.
///
/// CI has no keychain, and a test suite that skipped itself there would leave
/// the agent-wallet rules — one record per agent, rotation mints a generation,
/// an address is never reinstalled — untested on the only machine that gates a
/// merge. Those rules are provided methods on [`KeyStore`], so exercising them
/// here exercises the same code the keychain store runs.
///
/// `BTreeMap`, not `HashMap`: `AGENTS.md` invariant 6 wants deterministic
/// iteration anywhere state is enumerated, and it makes a test that dumps the
/// store's contents stable.
///
/// **Not a substitute for the keychain in a shipped build.** It holds secrets
/// in process memory for the process's whole life and writes nothing to disk.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
    network: Network,
    entries: Mutex<BTreeMap<(&'static str, String), String>>,
}

impl MemoryKeyStore {
    /// An empty store over `network`.
    pub fn new(network: Network) -> Self {
        MemoryKeyStore {
            network,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Entry names currently populated, in deterministic order. For tests that
    /// assert a rotation minted rather than replaced.
    pub fn entry_names(&self) -> Result<Vec<String>, KeyStoreError> {
        let map = self.entries.lock().map_err(|_| KeyStoreError::Poisoned)?;
        Ok(map
            .keys()
            .map(|(service, account)| format!("{service}:{account}"))
            .collect())
    }
}

impl KeyStore for MemoryKeyStore {
    fn network(&self) -> Network {
        self.network
    }

    fn write(&self, entry: &EntryName, secret: &str) -> Result<(), KeyStoreError> {
        let mut map = self.entries.lock().map_err(|_| KeyStoreError::Poisoned)?;
        map.insert(
            (entry.service(), entry.account().to_owned()),
            secret.to_owned(),
        );
        Ok(())
    }

    fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
        let map = self.entries.lock().map_err(|_| KeyStoreError::Poisoned)?;
        Ok(map
            .get(&(entry.service(), entry.account().to_owned()))
            .map(|s| SecretText::new(s.clone())))
    }

    fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError> {
        let mut map = self.entries.lock().map_err(|_| KeyStoreError::Poisoned)?;
        map.remove(&(entry.service(), entry.account().to_owned()));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    /// failure here is the whole assertion.
    #[test]
    fn error_is_send_and_sync() {
        fn require<T: Send + Sync + 'static>() {}
        require::<KeyStoreError>();
        require::<KeychainKeyStore>();
        require::<MemoryKeyStore>();
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
        assert!(record.retired.is_empty());
        assert_eq!(record.valid_until_ms, T0 + AGENT_APPROVAL_TTL_MS);

        let key = store.load_agent_key(&a).expect("load");
        assert_eq!(key.address(), record.address);
        assert_eq!(
            store.agent_wallet(&a).expect("read").as_ref(),
            Some(&record)
        );
    }

    #[test]
    fn a_prefixed_uppercase_key_normalizes_to_the_same_entry() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let plain = store
            .create_agent_key(&agent("plain"), secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        let prefixed = store
            .create_agent_key(
                &agent("prefixed"),
                secret(&format!("0x{}", KEY_A.to_ascii_uppercase())),
                T0 + DAY_MS,
                T0,
            )
            .expect("create");
        assert_eq!(plain.address, prefixed.address);

        let names = store.entry_names().expect("names");
        assert!(names.contains(&format!("{SERVICE_TESTNET}:agent-key/plain/0")));
        assert!(names.contains(&format!("{SERVICE_TESTNET}:agent-key/prefixed/0")));
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
        assert!(store.entry_names().expect("names").is_empty());
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
        assert_eq!(second.retired.len(), 1);
        assert_eq!(second.retired[0].generation, 0);
        assert_eq!(second.retired[0].address, first.address);
        assert_eq!(second.retired[0].retired_at_ms, T0 + DAY_MS);

        // Generation 0's key was not overwritten: both entries exist.
        let names = store.entry_names().expect("names");
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
    fn the_retired_list_is_capped_and_keeps_the_newest() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        // Distinct keys: vary the leading byte across the scalar's range.
        let key_at = |i: u32| format!("{:02x}{}", (i % 200) + 1, &KEY_A[2..]);

        store
            .create_agent_key(&a, secret(&key_at(0)), T0 + DAY_MS, T0)
            .expect("create");
        let rotations = MAX_RETIRED_ADDRESSES as u32 + 4;
        for i in 1..=rotations {
            store
                .rotate_agent_key(&a, secret(&key_at(i)), T0 + DAY_MS, T0 + u64::from(i))
                .expect("rotate");
        }

        let record = store.agent_wallet(&a).expect("read").expect("present");
        assert_eq!(record.generation, rotations);
        assert_eq!(record.retired.len(), MAX_RETIRED_ADDRESSES);
        // Oldest first, and the oldest kept is the one that leaves the window.
        let generations: Vec<u32> = record.retired.iter().map(|r| r.generation).collect();
        let expected: Vec<u32> = (rotations - MAX_RETIRED_ADDRESSES as u32..rotations).collect();
        assert_eq!(generations, expected);
    }

    #[test]
    fn forget_retired_keys_drops_key_material_but_not_addresses() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let a = agent("alpha");
        let first = store
            .create_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        store
            .rotate_agent_key(&a, secret(KEY_B), T0 + DAY_MS, T0)
            .expect("rotate");
        store.forget_retired_keys(&a).expect("forget");

        let names = store.entry_names().expect("names");
        assert!(!names.contains(&format!("{SERVICE_TESTNET}:agent-key/alpha/0")));
        assert!(names.contains(&format!("{SERVICE_TESTNET}:agent-key/alpha/1")));

        // The retired address is still blocked.
        let record = store.agent_wallet(&a).expect("read").expect("present");
        assert!(record.has_used(&first.address));
        assert!(matches!(
            store.rotate_agent_key(&a, secret(KEY_A), T0 + DAY_MS, T0),
            Err(KeyStoreError::AddressReused { .. })
        ));
        // The current key still loads.
        assert!(store.load_agent_key(&a).is_ok());
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
        assert_eq!(
            store.entry_names().expect("names"),
            vec![
                format!("{SERVICE_TESTNET}:agent-key/beta/0"),
                format!("{SERVICE_TESTNET}:agent-record/beta"),
            ]
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
            once.starts_with(r#"{"agent":"alpha","generation":1,"address":"0x"#),
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
        assert_eq!(record.remaining_ms(T0), 90 * DAY_MS);

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
        assert_eq!(record.remaining_ms(T0 + 91 * DAY_MS), 0);
    }

    #[test]
    fn a_backwards_clock_does_not_underflow() {
        let store = MemoryKeyStore::new(Network::Testnet);
        let record = store
            .create_agent_key(&agent("alpha"), secret(KEY_A), T0 + DAY_MS, T0)
            .expect("create");
        assert!(matches!(record.expiry(0), ExpiryState::Valid { .. }));
        assert_eq!(record.remaining_ms(0), T0 + DAY_MS);
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
        assert_eq!(tag.len(), HMAC_TAG_LEN);
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
    fn hmac_matches_the_published_test_shape() {
        // Two distinct messages under one key must not collide, and the tag is
        // deterministic across calls — the two properties the config check
        // relies on.
        let key = HmacKey::from_bytes([1u8; HMAC_KEY_LEN]);
        let a = key.sign(b"a").expect("sign");
        let b = key.sign(b"b").expect("sign");
        assert_ne!(a, b);
        assert_eq!(a, key.sign(b"a").expect("sign"));
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
            store.entry_names().expect("names"),
            vec![format!("{SERVICE_TESTNET}:guardrail-hmac")]
        );

        store.delete_hmac_key().expect("delete");
        assert!(store.load_hmac_key().expect("load").is_none());
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
        assert_eq!(s.len(), 64);
        assert!(!s.is_empty());

        let key = HmacKey::from_bytes([9u8; HMAC_KEY_LEN]);
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("09090909"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
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
        let a = agent("oppen-selftest");
        // Leave nothing behind from an interrupted earlier run.
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
    }

    /// Same, for the guardrail HMAC key.
    #[test]
    #[ignore = "touches the real OS credential store"]
    fn keychain_round_trips_the_hmac_key() {
        // Uses the real entry name, so it is destructive to a local install's
        // guardrail key. Deliberately not run by default.
        let store = KeychainKeyStore::new(Network::Testnet);
        let existing = store.load_hmac_key().expect("load");
        if existing.is_some() {
            panic!("refusing to overwrite an existing guardrail-hmac entry");
        }
        let key = store.ensure_hmac_key().expect("ensure");
        let tag = key.sign(b"config").expect("sign");
        let again = store.ensure_hmac_key().expect("ensure");
        assert!(again.verify(b"config", &tag));
        store.delete_hmac_key().expect("delete");
        assert!(store.load_hmac_key().expect("load").is_none());
    }
}
