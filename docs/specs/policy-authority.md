# Authenticated Policy Authority

Implementation contract, 2026-09-07. Implemented on the current safety branch;
final supervision verification, independent review and CI remain pending.
This is not a merge or trading-activation claim.
Follows registry signing authority (ES17, PR #52). Traces to spec items 3,
24, 26 and 29 and the supervised-testnet failure/recovery requirements.

## Required Outcome

The complete agent-policy map, approval settings, account loss limits and kill
state become authenticated snapshots in the existing anchored ledger. Deleting
a setting must not substitute an unset limit or an unengaged kill switch.
Legacy configuration remains inspectable, never an execution-authority fallback.

Each policy event binds its schema, operation, network, sequence, previous hash,
timestamp, full snapshot and previous policy event identity. The policy event
sequence is the durable revision; unrelated events may leave gaps. Required
fields, nested unknown fields, duplicate keys, invalid IDs and invalid numeric
configuration must be checked explicitly rather than relying on permissive
serde defaults. Generic ledger append paths cannot create policy authority.

Orders, including reduce-only orders, carry that revision and verify it again
inside the same retained ledger snapshot as registry and pilot authority,
through signing. A cached policy revision cannot replace that verification.

## Migration Boundary

- Opening never initializes or authenticates unsigned settings automatically.
  Initialization requires an explicit complete replacement snapshot, an
  operator-reviewed legacy fingerprint and an engaged global pause.
- Fingerprint source presence and original rows, including unknown or missing
  data; do not fingerprint the old loader's default-filled result.
- Legacy policy currently lives in a separate `guardrails-{network}.db` file.
  A ledger transaction does not lock that file. Initialization opens a fresh
  read-only legacy transaction, compares its fingerprint with the reviewed
  fingerprint and retains that snapshot through the ledger commit. Bind the
  source/network, presence information and review provenance into initialization.
  This detects changes before the fresh snapshot, not subsequent WAL writes;
  it is consistent adoption evidence, not cross-file atomicity.
- Stop old writers before adoption. Preserve their data and existing pairing,
  registry, pilot and submission history. Add the next reader-version barrier;
  rollback requires a compatible reader, never unsigned fallback or history
  deletion. Missing keys never trigger key provisioning or replacement.
- The console must distinguish unverified historical inspection from verified
  runtime policy. Read-only inspection must not silently introduce keychain
  reads, initialize authority, or display legacy settings as the active policy.
  Report unverified legacy policy, unverified stored ledger policy, or
  runtime-verified policy explicitly, with revision/observation where available.
  Chain integrity alone does not prove policy authenticity.

## Concurrency Contract

Use one shared ledger/key authority for production registry, policy persistence
and final signing. Independent store/sink assembly must not permit mismatches.
Policy writes compare the expected durable revision and append a full snapshot
atomically; a stale writer cannot overwrite a newer policy or another kill scope.

Maintain ledger-before-engine-state lock ordering at final signing. Mutations
serialize locally, perform verified load/CAS without holding engine state, then
publish the committed projection. A delayed load/publication cannot regress the
cache. Buckets, proposals and live venue-rate budgets are not policy snapshots.

Periodic supervision verifies policy even when no agent calls tools. Once per
sweep is sufficient; changes after that observation belong to the next sweep.
Run the refresh on the bounded, tracked decision worker with ownership retained
until completion. A completed policy-verification failure inhibits orders and
continues registry-verified cleanup. Worker contention or timeout is incomplete
verification and must report retry, not freshness or absence of work. Preserve
known-stop processing and per-account failure reporting. Supervision never
acknowledges policy or enables orders, including after later valid replacement.

## Failure Contract

These conditions are required for implementation and verification:

- Unusable policy must inhibit orders without preventing construction of a
  restricted cleanup path. Cleanup still verifies registry, route and actual
  signer; it must not manufacture default policy or an order clearance.
  Construction, agent lookup, clearance and final signing must all preserve this
  independence. Cleanup agent identity comes from verified registry authority,
  not membership in an unavailable policy map.
- Every engine open/restart starts order-inhibited. Explicit operator
  reconciliation/acknowledgment must bind current policy revision and local
  stop generation. Actual desktop venue/baseline validation remains a separate
  activation gate; examples must not auto-acknowledge it.
  At final signing, verified policy revision, clearance revision and acknowledged
  revision must agree. Independent-handle policy changes require acknowledgment
  again; cache refresh cannot transfer it to the new revision.
- Loss trips queue cancellation and retain an emergency stop before persistence.
  Failed writes latch admission inhibition. Refresh cannot clear it; restart
  inhibition prevents a lost process-local overlay from silently resuming.
- A commit followed by anchor-publication failure has an uncertain durable
  outcome, not a promise that policy stayed unchanged. The initiating engine
  remains inhibited until explicit reconciliation. Exact retries must publish
  the verified head before acknowledging success; continued failure stays an
  error. Reopen does not itself authorize orders.
- Release acknowledges a specific stop generation. Successful persistence of an
  older release must not clear a newer concurrent trip. Policy snapshots and
  process-local emergency evidence must have distinct ownership.
  Compare and clear the captured generation atomically under engine state after
  publication. Acknowledgment cannot override durable kills, pilot stops or
  unavailable authority. Reconcile the actual committed revision, never merely
  the attempted mutation.

## Acceptance Evidence

Require focused tests for explicit paused initialization and changed review
evidence; missing/unknown/redacted/tampered snapshots; independent-handle CAS;
revision changes between evaluation and signing across restart; mutation waiting
through the actual signing permit without deadlock; failed engagement/release
and newer stop generations; unavailable-policy cleanup with valid registry;
uncertain publication and idempotent retry; old-reader refusal and preservation
of existing authority, budgets and unresolved submissions. These are local
proofs, not substitutes for the remaining supervised testnet gates.
