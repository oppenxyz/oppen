# Paused Policy Setup

ES23 implementation on `feat/paused-policy-setup`.
Traces to spec items 4, 24, 26 and 32. This slice ends with a verified, globally
paused policy, not activation or a completed onboarding/live gate.

## Authority And Preservation

Require TESTNET, an existing anchored ledger, an existing HMAC key and the exact
authenticated agent/account registry route. Do not create or replace credentials,
grant registry authority, create a pilot, pair clients, start execution or issue
venue requests. Missing prerequisites remain explicit failures.

The frontend submits selected edits, never a complete policy replacement. Its
existing inspection type omits some Rust configuration fields and must not be
round-tripped into authority. Rust retains the complete before/proposed snapshots
and changes only explicitly reviewed fields on the selected agent. Preserve
other agents, account limits, unedited risk/freshness settings and existing kill
engagements, including their reasons and timestamps.

An existing authenticated policy must already be globally paused; this is not a
replacement for the runtime halt workflow. Initialization from legacy evidence
requires strict decoding of the retained review. Reject missing fields, unknown
data/schema or malformed values rather than dropping them or filling defaults.
An absent legacy file requires explicit acknowledgment; absence is not the same
as a present malformed file. Add a global pause only when absent in the reviewed
legacy source, preserving all other stops. The operator must explicitly confirm
that other policy writers have stopped. That assertion is not process discovery
or a substitute for the journal's fresh-source verification and CAS.

New-agent proposals use $15 order cap, $25 position and gross open exposure caps,
1x leverage, approval required and an empty symbol allowlist. They remain drafts
until explicit review and save. Existing values must be displayed before any
selected change; unrelated fields retain their exact values. Setup validates the
supervised bounds in Rust and never infers symbols or permission from a client.

## Review And Persistence

An opaque native review ID identifies the retained source/revision, exact route,
complete proposal and stable operation timestamp. The returned review exposes
identity, source provenance, before/after values and retained stops. Commit takes
only that ID: frontend timestamps, replacement snapshots or changed edits cannot
be smuggled into a previously confirmed operation.

Use `PolicyJournal::initialize_for_route` with its fresh legacy fingerprint check,
or `replace_for_route` with the reviewed revision. Both compare the authenticated
route inside the same write transaction. Never automatically rebase after a conflict.
A commit/anchor-publication error is an uncertain durable outcome, not proof of
no change. Keep the exact operation for an explicit idempotent retry and report
the verified committed revision only after publication succeeds. Saving cannot
acknowledge engine admission, release any stop or reset pilot accounting.

A caught persistence panic or lost worker result is recovery-required uncertainty,
not an ordinary retryable publication error. Retain identity, proposal and
diagnostics; refuse retry, discard and new admission. Controlled restart and
reconciliation are required, but restart alone does not prove publication. Never
clear a poisoned ledger mutex to make the operation appear retryable.

## Runtime Ownership

One native-owned setup slot reserves admission before authority I/O. It excludes
MCP startup and network/account context replacement while review or admitted
work remains owned. Blocking reads and writes run outside control locks. A
dropped IPC observer or view change does not abandon the worker or its result.

Cached status distinguishes review-in-progress, ready for review, persisting,
saved, failed, uncertain and stopped. Late replies cannot regress newer status.
Shutdown closes setup admission, invalidates idle reviews and joins admitted
writes before stopped/update completion. A canceled waiter cannot detach the
write. No new policy engine or independent event store is introduced.

## Console And Evidence

Use Settings / Permissions & limits with entry points from Setup and Agents.
Provide conventional labeled controls, a field-by-field review and explicit
paused save. Show all retained settings/stops without a raw JSON editor. Treat
source values and reasons as inert text. Keep unknown/stale outcomes visible;
neither a configured form nor a saved policy proves trading activation.

Acceptance needs actual synthetic journal persistence/reopen, unrelated-policy
and stop preservation, strict retained legacy decoding and changed-source
refusal, concurrent revision/route change refusal, missing-key no-write,
MCP/context exclusion, dropped-observer ownership, shutdown during a blocked
write, publication failure/retry, frontend remount/late-response handling and
rendered layout checks. Do not enable ignored venue tests or touch the dedicated
account to exercise this workflow. Exact-head CI and separate review remain PR
gates; account confirmation and publication approval remain owner gates.

## Verification

The frontend suite passes 152 tests and the production web build passes. A
regression demonstrated that polling cleared the review checkbox before the
fix; primitive watch sources now preserve confirmation for the same review ID
and phase, while a new ID or phase clears it.

Synthetic browser checks exercised explicit same-review retry, the saved receipt,
clearing a completed review and preparing a new empty-allowlist draft. The new
review requires fresh confirmation. Screenshots covered the normal wide viewport
and the supported 1280x800 desktop minimum, with no document horizontal overflow
at that minimum and wrapped long uncertainty diagnostics. The inherited console
minimum is 1280px; this does not establish mobile support. The fixture has no
native commands or account activity. Its browser tab and server were closed.
The recovery-required fixture also preserves its diagnostic and complete review,
with no retry, discard or editing action; its 1280x800 screenshot was checked.

Sixteen focused native setup tests, six existing-only opener tests and the
disconnect/reconcile regression pass. Independent review identified and then
rechecked fixes for uncertain-discard and worker-panic recovery. The opener
refuses absent/malformed anchors, unsupported schema versions, wrong networks,
corrupt history and conflicting alias anchors; it does not publish the allowed
one-row crash window merely by opening. Coordination/WAL support files are not
authority creation. The full workspace passes 977 Rust tests with 15 live-gated
tests ignored; formatting and workspace all-target clippy with warnings denied
pass. Separate automated review of the frozen working diff found no remaining
blockers after the two recovery fixes; it is not human approval. Exact-head
independent review and CI remain PR gates. These
results do not authorize publication or establish a live onboarding gate.
