# Operator Approvals

Implementation planning for spec items 18, 19, 28 and 32. Existing core approval
methods are not a completed operator workflow. All stages below remain safety
gates before exposing a usable approval action or claiming live acceptance.

## Baseline Before ES24/ES25

The core queue retains an ID, agent, normalized order intent and TTL in memory.
Approval consumes a proposal atomically and re-evaluates it, but does not submit
it. Expiry silently removes entries; rejection removes before attempting an
audit write and can report success despite audit failure. Restart does not
reconstruct pending proposals. These behaviors do not establish a durable
proposal-to-decision-to-submission history.

The gateway already owns account serialization, fresh exposure/context reads,
durable submission begin-before-sign, pilot enforcement, guarded signing and
unknown-outcome reconciliation. Desktop MCP ownership already retains the real
gateway and engine. Reuse these owners; do not build another execution path or
grant approval through an agent-reachable MCP tool.

## Delivery Order

1. **Exact proposal identity (ES24).** Bind the original authenticated route
   inside the retained proposal. Compare within approval evaluation against the
   route used for the clearance, not in a separate preliminary lookup. Refuse
   account reassignment, retirement/regrant and signer rotation. Keep final-sign
   route verification, all fresh guards and one-shot consumption. This change
   alone is not a durable queue or UI approval workflow.
2. **Durable lifecycle.** Use the existing append-only ledger, with authenticated
   authority for operator decisions and explicit proposal linkage. Preserve
   original request, route, lifetime, disposition and submission identity.
   Surface recorded/uncertain/refused outcomes truthfully. Cover approved,
   rejected and expired events. Define restart reconstruction or explicit
   invalidation from durable evidence; never silently restore executable consent,
   reset a TTL, collide IDs, or remint a lost request. Legacy unbound proposals
   cannot become trusted approvals by migration defaults.
3. **Pricing review and execution.** Preserve enough of the original request to
   distinguish a market request from its normalized IOC order. Show original
   reference, current reference, candidate price/notional, drift and expiry.
   Explicit limit/trigger constraints must not be changed by normalization or
   repricing. Native review retains what was shown and defines a bounded,
   rechecked confirmation; material changes require a new operator review.
   Submit only through the existing gateway reservation and reconciliation
   machinery. An approval is not permission to bypass a budget or stale feed.
4. **Native ownership and console.** Expose scoped operator-only queue/status,
   review and decision commands on the actual runtime owner. Match runtime
   generation, account, agent, route and proposal identity. Retain admitted
   decisions/submissions across dropped IPC and view changes; quit/update must
   close admission and drain actual work. Show pending, expired, rejected,
   refused, submitted and uncertain outcomes independently of venue fills.
   Expose only the requesting agent's pending evidence in MCP `get_state`.
5. **Integrated acceptance.** Exercise real gateway dispatch, durable journals,
   synthetic signing and loopback transport before the separately authorized
   supervised testnet run. The latter remains unperformed and identity-gated.

## Required Proof

The native pricing implementation must preserve these ownership boundaries:

- Retain one opaque, non-cloneable/non-deserializable review in an operator-only
  controller owned by `OwnedMcp`. IPC confirms only its ID; never accept edited
  candidate fields or a client-supplied approval flag. Bind the owner generation,
  exact route/policy, original proposal, displayed rounded action and deadline.
- Authenticate the reviewed candidate commitment in the one-shot claimed event,
  then validate the actual approval audit receipt against that commitment. An
  actual receipt alone proves what was cleared, not what the operator reviewed.
  Preserve older claim bytes and never replay an executable review or clearance.
- Keep explicit limits/TIF and stop trigger/TPSL fixed. Reprice market requests
  only from retained original semantics. A changed position-close size/direction
  requires another review, not silent resizing or reversal. Unknown historical
  source cannot become a market request by inference.
- Confirmation uses the existing account reservation, fresh context, core
  decision, durable submission begin, signer and reconciliation path. Pin the
  displayed candidate; a material change requires a new review, never a silent
  second repricing. Recheck the effective approval deadline at final signing.
- Obtain tracked operator work from the actual server admission/drain owner,
  not a detached gateway clone. Close admission synchronously on stop; retain
  admitted work through lost IPC and dropped drain waiters until it really ends.
- Pin a live pairing lease for review/confirmation instead of treating a cached
  binding as live authority. Proposals currently do not record originating token
  identity; do not claim otherwise. Revocation must prevent new execution and
  preserve uncertain/submitted outcomes for reconciliation, not assert not-sent.

These are implementation requirements, not an implemented native approval flow.

Implementation trace after ES28:

- `Gateway::reserve_reconciled`, `evaluation_context` and `submit` own the
  existing reservation, fresh account reads and submission path. Native
  confirmation must reuse them, not construct a second exchange/signing path.
- `Clearance` is not the full executable action: order type/TIF, grouping and
  builder are also part of `Action`. Retain and authenticate the full rounded
  action, and link its commitment to the actual intent audit receipt. A receipt
  hash or clearance-only comparison cannot prove displayed-action consent.
- Review uses the freshly verified policy shown to the operator; confirmation
  must match that same revision/hash, not silently adopt a subsequent policy.
  The original proposal route remains fixed. Preparation consumes no proposal
  and grants no signable clearance; confirmation claims exactly once.
- The existing five-second `Gateway::decision` timeout retains its blocking
  worker but returns to its caller. Native confirmation must retain the account
  reservation through actual completion, including a timed-out or dropped IPC
  waiter. Merely calling this wrapper from a detached gateway clone is not enough.
- `ShutdownHandler`/`BoundServer` own actual server execution admission and
  drain. Obtain native work from that owner and retain a live `SessionAuthority`
  lease. `supports_binding` deliberately includes revoked records and cannot
  grant execution. No originating pairing-token identity is currently stored
  in proposals, so a live same-binding lease is not proof of originating token.

These traced gaps are still implementation work. They do not authorize native
approval execution, key reads or live activation.

- Approve/approve and approve/reject races produce one disposition, never two
  executable clearances; restart and same-time ID generation cannot revive one.
- Reassignment and retirement/regrant between mint, review, decision and final
  signing cannot redirect the request to another account or signer.
- Policy changes, kill switches, expiry, missing consent, exhausted cumulative
  pilot limits, stale exposure and account reservations remain enforced.
- Audit/anchor failure before or after commit preserves uncertainty; UI success
  requires evidence. A transport timeout never creates another order, and
  query-by-cloid remains the recovery path for unknown submission outcomes.
- Event replay, stale frontend replies and direct IPC cannot manufacture
  approval, replace its intent, release stops or unlock another runtime.
- Every agent action in item 28 needs an explicit disposition design. Existing
  safety-driven cancellation/cleanup must remain executable without waiting on
  human approval; it is not an agent bypass. Resolve the ordinary agent-cancel
  queue behavior before claiming full item-28 coverage. In particular, disabling
  dead-man protection is not risk-reducing merely because it shares an API with
  protective cancellation; its authorization needs separate treatment.

No activation, consent adoption, registry grant, pairing, key access, funding or
publication is implied by this plan. Compatible and verified MCP clients must
share these same server-side safeguards.

## Durable Lifecycle Evidence

ES25 installs authenticated proposal/claim/disposition authority in both
production engine constructors. Approval persistence runs outside engine-state
locks. A published claim is one-shot; restarting or republishing evidence cannot
recreate its execution permit. Approved dispositions reference actual intent
receipts, not a guessed latest row. Queue/rejection failures are typed errors.

Before each approval append, verify the existing head under coordination and
publish any tolerated one-row lag; stop if that publication fails. A mint error
does not trigger a second best-effort refusal append. This bounds the approval
writer's own failure path, not every independent writer in the system. During
sustained anchor failure, preserving additional fills may exceed the verification
window: authority must fail closed and require reconciliation, not discard fills
or blindly adopt a new head. No automatic global recovery or unconditional
lossless-ingestion claim follows from this change.

V9 excludes older readers without rewriting prior events. Operator authority
records are withheld from raw agent event views; scoped pending/status and
client-visible decision projections remain integration work, not a completed
item-28 surface. Local verification passes 34 focused approval tests and the full
workspace (1,008 Rust tests passed, 15 live-gated ignored), plus formatting,
all-target Clippy, 152 frontend tests and the desktop frontend build. Fixtures
exercise production constructors without live account activity or signing keys.
PR #65 at `125e899` has separate exact-head automated review with no blocking
findings. CI run `34186908204` did not start: GitHub reports account payment or
spending-limit restrictions. Local tests are not a substitute for green CI.
Native approval execution/UI and original-request repricing remain integration
work.

## Original Request Evidence

ES26 preserves typed market, explicit limit/IOC, stop-market and position-close
origins alongside the normalized order. The actual normalization quote/time and
slippage are retained, as are explicit limit/trigger constraints and signed close
size. Core refuses mismatched evidence before minting; evidence never supplies
approval authority or changes the existing signing/reservation path.

Historical absent evidence stays absent and means original request unknown.
V10 prevents older writers from discarding the new semantics; prior event bytes,
MACs, claims and TTLs are not rewritten. The ES25 stopped-writer and preservation
rules also apply to V10. No actual operator ledger is migrated during development.

Deduplication ignores refreshed quote observations only when the original request
kind and every normalized field still match. Return the retained evidence and
expiry unchanged. A changed normalized market price still conflicts, as before;
this does not introduce automatic repricing or extend consent. A future native
review must retain and recheck a displayed candidate before gateway submission.
Core normalization checks do not establish end-to-end overflow safety for the
existing gateway pricing helpers. Local verification passes 1,020 Rust workspace
tests (15 live-gated ignored), including actual MCP-to-journal retry with no
signing-key read or submission, source mismatch refusal, variant reopen, source
identity conflicts and historical-unknown byte preservation. Formatting,
all-target Clippy, 152 frontend tests and typecheck/build pass. Independent review
and exact-head CI remain gates; no live acceptance or native approval UI is claimed.

Upgrade requires stopping older writers first: an open-time version check cannot
evict a process that already holds an older connection. Keep the ledger and its
anchor together. Recovery/rollback must use V9-capable code preserving every
committed event; never lower `user_version`, discard new decisions or restore an
old budget snapshot to make an older binary run. A verified backup is evidence,
not authorization to erase later execution or reset cumulative limits. No local
operator ledger is migrated by the synthetic development checks.

## Signing Deadline Evidence

ES27 on `fix/approval-signing-deadline` reproduced a synthetic signature at the
proposal's exclusive expiry after approval one millisecond earlier. The private
deadline now survives consumption and is checked after key loading and durable
authority/state locks. It does not introduce venue-side expiry or cancel already
sent orders. A refused signature leaves the approval consumed.

Verification passes 43 focused approval tests and the full workspace (1,024 Rust
passed, 15 live-gated ignored), formatting, all-target Clippy, 152 frontend tests
and typecheck/build. Production-constructor fixtures reopen pending approval,
approve at TTL-1, reserve through the real submission journal, then cross expiry
during key-loading or ledger-lock waits. Opening and reduce-only orders refuse
with the final observed audit timestamp, and consumed state survives reopen.
Separate automated diff review found no blockers; exact-head review and CI remain
required. No live signing, account activity, budget use or ledger migration.

## Route-Binding Evidence

On `fix/approval-route-binding`, the replacement-account regression produced a
clearance for the replacement account/signer before the fix. Both new tests pass
afterward, and the focused approval selection passes seven tests. The cases cover
account, binding revision, signer, generation, validity-window and vault changes,
plus one-shot refusal and unchanged-route exposure/kill checks. Separate automated
working-diff review found no blockers. These tests use synthetic route replacement;
they do not establish authenticated journal retirement/regrant end to end. The
full workspace passes 981 tests with 15 live-gated tests ignored, including the
guardrail property and one-proposal concurrency regressions. Exact committed-head
review and CI remain verification gates. Formatting, diff checks and workspace
all-target clippy with warnings denied pass. No frontend code changed in this
slice; frontend validation will also run in CI.
