# Pilot Consent Authority Audit

Safety follow-up, 2026-09-07. Implementation is pending. This is not an
activation approval or a completed live gate. Traces to spec items 3, 24-26
and 29, and the supervised-testnet requirement for explicit consent and
non-resettable cumulative budgets.

## Finding

At `4b3f8a6`, `PilotJournal::new(Arc<Ledger>)` and `authorize` require no
authenticated registry capability or HMAC. Replay checks the hash chain,
fixed limits, baseline and identity, but does not authenticate operator consent.
Generic ledger append rejects pilot events; that API restriction does not
protect against a direct database writer. The anchor deliberately permits one
unwitnessed tail row to recover a crash between commit and anchor publication.
A correctly constructed unsigned consent row is therefore not distinguished
cryptographically from an operator-created one.

Evidence is source inspection, independently corroborated by two automated
investigators. A requested synthetic reproduction was blocked by tooling and
did not run; no end-to-end exploit or live-account test is claimed.

This is not an unconditional signing bypass. Registry and policy authentication,
engine acknowledgment, durable kills, route checks, actual signer possession
and other guardrails still apply. Desktop startup remains inhibited. Duplicate
authorization does not reset a pilot: replay rejects it. A consent MAC would
not authenticate venue-origin fills or protect against an attacker who also
obtains the authority key.

Relevant boundaries:

- `crates/oppen-core/src/ledger/pilot.rs`: authorization, replay, admission and
  final-sign pilot checks. No applicable pilot currently means no pilot-specific
  restriction, not a refusal.
- `crates/oppen-core/src/ledger/mod.rs`: final signing authenticates registry
  and policy before calling the keyless pilot check; local pilot status uses
  the same hash-only replay.
- `crates/oppen-core/src/ledger/verify.rs`: one-row anchor lag tolerance.
- `apps/desktop/src-tauri/src/mcp_runtime.rs`: existing pilot evidence is a
  startup prerequisite, separate from acknowledgment and venue readiness.

## Required Repair

Authenticate operator consent through the same registry-backed ledger/key
authority already used by policy and final signing. No new keychain reads in
feed processing, no independent caller-supplied ledger/key pair, and no automatic
key creation or replacement. Bind the canonical authorization envelope to its
version, kind, network, row linkage/time, validated agent/account, authenticated
registry route, original baseline and fixed limits. Reject malformed, duplicate,
unknown or missing required fields rather than defaulting authority.

Both atomic reservation admission and final signing must verify consent from
the same ledger snapshot used for their decision. Supervised-alpha execution
must require applicable authenticated consent: absent, deleted, wrong-account,
legacy or unverifiable consent cannot become an optional-pilot bypass. Before
implementation, settle how this requirement is represented and enforced across
all production constructors, without relying on a frontend flag or startup-only
check. Keep registry-verified cancellation available when consent is unusable.

Separate accounting inspection from authenticated authority. Keyless local
inspection must state that consent authentication is unverified. Continue
recording fills and deny-only stop evidence even when consent authentication
fails; do not discard reconciliation evidence because an authorization cannot
be trusted. Never render unavailable accounting as zero usage.

## Migration Requirements

- Add an old-reader barrier before new authority becomes usable. Stop old
  writers; rollback requires a compatible reader, not deleting history or
  falling back to unsigned consent.
- Preserve legacy rows and their original baseline, limits, reservations,
  executed totals, realized PnL including fees, and permanent stops. Opening a
  ledger must not sign legacy consent or create a fresh budget.
- Explicit legacy review/adoption must reference the original authorization
  identity and reviewed history head. Recheck under ledger coordination;
  changed history requires renewed review. Invalid identities, missing history
  or unavailable accounting must refuse adoption, never silently remap or reset.
- Adoption authenticates the operator's explicit decision to trust reviewed
  history; it does not retroactively prove that historical consent was genuine.
- Handle commit/anchor failure as an uncertain durable outcome. Exact retry
  must verify and publish the committed head before success. Adoption must not
  release a kill, clear uncertainty, or acknowledge engine admission.

## Acceptance Gates

Synthetic regression evidence must cover forged one-row-lag consent rejection
at admission and final signing; wrong key/network/route; absent required consent;
legacy adoption with unchanged row hashes and cumulative usage across reopen;
pending and canceled-order liabilities; permanent exhausted-budget stops;
stale review; missing key without writes; uncertain commit retries; old-reader
refusal; and keyless inspection never enabling orders. Keep fill ingestion and
authenticated cleanup working under failed consent verification.

Venue readiness remains separate: dedicated-account identity confirmation,
exclusive account use, reconciled positions/orders/fills, the $25 gross exposure
cap and 1x leverage, and explicit operator activation. Authenticating a local
statement alone proves none of those venue conditions. Broad client onboarding
follows this safety work; client choice never changes these requirements.
