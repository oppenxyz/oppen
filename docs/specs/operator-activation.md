# Supervised Testnet Operator Activation

Next safety gate after ES36; planned, not implemented or authorized. Traces to
spec items 9, 15, 24-29 and 34. Broad agent-client onboarding follows this gate.

## Scope

Implement native review, explicit confirmation and an observable result on the
existing supervised testnet runtime. Reuse its engine, ledger, feed, pairings
and retained controller ownership; do not construct a second engine. Starting
supervision remains order-inhibited and may enforce existing cleanup. A
connection test is read-only, not supervision startup or activation.

Require existing authenticated pilot consent. Initial consent, legacy adoption,
provisioning and key replacement remain separate operator ceremonies. Initial
consent needs confirmed account identity and exclusive use plus a reconciled
flat venue baseline; local ledger checks cannot prove those conditions.
Restart activation preserves the original baseline, usage, reservations and
stops, including nonzero usage. Never require a replacement pilot on restart.

Kill release is a separate explicit action identifying its scope and current
stop evidence. Activation cannot silently release a global or agent kill.
Permanent pilot stops and exhausted budgets cannot be released here. A
separately completed kill release invalidates prior activation review.

## Review Contract

Gather verified authority and fresh account evidence while admission remains
inhibited. Return a display projection and an opaque runtime-owned review ID,
not frontend-provided reconciliation claims or deserializable authority. Keep
one current review per runtime owner; replacement and restart invalidate it.

Identify and display:

- Testnet, agent, full account address, authenticated route and signer binding,
  a matching non-revoked pairing, and current venue wallet-approval evidence.
  Local metadata proves neither venue approval nor user identity confirmation.
  Review must not request an owner signature.
- Verified policy revision, approval mode, effective caps and kill/stop evidence.
  Preserve $15/order, $25 gross open exposure including resting opening orders,
  $150 cumulative executed notional, $5 realized loss including fees, and 1x
  maximum leverage. Stricter policy remains effective.
- Authenticated pilot identity and original baseline, known cumulative usage,
  reservations, remaining capacity and absence of permanent halt. Unknown
  accounting is not zero or usable remaining capacity.
- Fresh positions, opening commitments, fills and reconciled submission
  liabilities for the exact account; actual leverage/margin observations;
  completed reconciliation with healthy account ingress and no unapplied work.
  Unknown outcomes, incomplete history or missing valuation block review.

## Confirmation Boundary

Revalidate reviewed identity, authority, policy revision, stop generation, pilot
accounting and venue/feed evidence. Capture the feed stamp before gathering
account observations. Cached status booleans or unchanged policy revisions are
not evidence that the account snapshot remains valid. Define an explicit
freshness bound using execution's existing clock/validity rules, checked after
waits rather than only at request receipt.

Close the check-to-acknowledgment race under existing core/ledger/feed
coordination. A native precheck followed by unbound
`operator_acknowledge_policy` is insufficient: that API leaves venue validation
to its caller. Reuse or narrowly extend the responsible core boundary so changed
authority, ingress, account evidence or stops cannot enable stale confirmation.
Never hold synchronous guards across async I/O. Final per-order signing and
dispatch checks remain mandatory; activation is not reusable order clearance.

## Ownership And Outcomes

Use the retained controller pattern: one confirmation at a time, closed
admission before shutdown/context replacement, and actual work drained despite
IPC cancellation. Duplicate requests cannot perform a second mutation. Audit
through the existing ledger before success. Failed or uncertain persistence
stays inhibited; retry/restart cannot infer success from an attempted write or
silently supply fresh consent.

Expose reviewing, review-ready, confirming, acknowledged, stale/refused and
uncertain outcomes with typed reasons. Show identity and remaining budgets
before explicit confirmation. Separate policy acknowledgment from current order
eligibility, feed readiness and venue acceptance. Later stops, expiry, revocation
and policy changes remain visible and effective; do not replace hardcoded
inhibition status with a success latch.

## Acceptance And Delivery

1. Exercise native review/confirmation on the actual owned engine with synthetic
   authority and loopback venue evidence. A permitted MCP order traverses guarded
   signing and delivery only after explicit confirmation.
2. Refuse missing/changed authority, revoked pairing, wrong account/network,
   unknown or halted accounting, exhausted budgets, unresolved liabilities,
   unsuitable venue state, stale review and unverified wallet approval. Use
   legitimate synthetic fixtures, never real wallet/keychain access.
3. Force policy, route, kill, ingress and fill changes during waits. Refusal
   leaves admission inhibited with no extra signature/order POST. Later
   reconciliation cannot revive an old review.
4. Cover audit failure, lost IPC observers, duplicate confirmation, panic,
   shutdown and physical restart. Preserve original consent and cumulative
   accounting; restart requires fresh review and explicit acknowledgment.
5. Verify UI success/error/stale/unknown states and narrow-window layout, then
   obtain independent exact-head review and green CI. Synthetic tests cannot
   prove live wallet validity, account exclusivity or venue acceptance.

Implement the core confirmation contract and retained native owner, then wire
and exercise the complete UI flow before marking activation done. Select and
verify actual read-only venue approval evidence during implementation; missing
evidence means refusal, not a local-metadata fallback. Account confirmation and
publication remain separate user approval gates. No mock completes a live gate.
