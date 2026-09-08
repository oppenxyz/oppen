# Own-order cancellation provenance

ES31 design investigation, after ES30 and before supervised activation.
Implements the D1 requirement that agents cancel only orders they opened.
This document is an implementation contract under review, not shipped behavior.

## Evidence Boundary

The current `SubmissionJournal::begin` reserves an order before signing.
Its `Started` and `Resolved` payloads are hash-chained but not authenticated
ownership evidence. The authenticated constructor supplies pilot admission;
it does not authenticate these payloads. Existing `Observed { oid, status }`
records cannot establish cancellation authority. Preserve their accounting
meaning and historical bytes without upgrading them to ownership claims.

Keep four states distinct: reserved, signed, accepted, and currently resting.
Account binding, CLOID prefixes, operator approval, a reservation, or a valid
wallet signature alone cannot prove passage through Oppen's guarded submission.

## Required Implementation

- Extend the existing submission journal and ledger, not a second order store.
  Authenticate new provenance envelopes with a separate HMAC domain and a
  reader barrier. Historical evidence remains unknown for ownership.
- The existing guarded signer must privately produce evidence binding the
  ledger/network, account, agent, exact registry route and signer generation,
  reservation and intent receipts, full signed action, nonce and expiry.
  Publish durable evidence before exposing a request eligible for transport.
  Publication failure must not POST. Do not add an unchecked signing path.
- Bind venue OIDs separately to the authenticated signed evidence through
  validated response or reconciliation observations. These are authenticated
  local observations, not venue-signed ownership certificates. Reject conflicting
  links. Acceptance evidence must not prematurely release exposure reservations.
- Ordinary `cancel` and `cancel_all`, with approval on or off, require provenance
  for every frozen target. Recheck at proposal creation, native preparation,
  confirmation and final signing. Pin evidence links in the review commitment.
  Unknown, missing, redacted or contradictory evidence refuses the whole request;
  never silently narrow cancel-all or transfer ownership after route regrant.
- Runtime HALT/pause cleanup remains a separate registry-authorized path. No
  agent argument or reason string selects it. Operator review cannot adopt a
  manual order into agent ownership.
- Dropped transport and restart retain reservations and reconcile without
  resending. Missing or unknown venue status grants no ownership and does not
  release exposure. Legitimate interrupted submissions need a usable recovery
  path, not a permanent blanket cancellation refusal.

## Implementation Status

ES31a validates direct exchange response kind and cardinality. ES31b's working
branch adds V13 authenticated `submission_signed` and `submission_accepted`
envelopes in the existing ledger. An opaque capability binds guarded signing to
one engine and reservation; uncertain signed publication withholds transport.
The ledger stores a domain-separated canonical request digest, not a replayable
signature. A validated direct response links its OID to that signed receipt;
accepted publication uncertainty remains non-retryable and retains liability.

Production MCP reservation and submission run on bounded retained blocking
workers, retaining account guards and actual session/operator authority. Account
locking precedes submission-slot acquisition. Final observational authorization
checks run after ledger waits, before signing and initial HTTP dispatch admission.
These local checks do not prove socket delivery or venue acceptance. Evidence
replay reuses one verified chain walk, rather than replaying for each signed row.

ES31c adds discretionary cancellation enforcement across proposal, retained
review, signing and consuming dispatch. Each target must match authenticated
signed/direct-accepted evidence for the exact historical route. The V14 proposal,
review and clearance pin receipt links; historical absent fields preserve their
canonical bytes but grant no ownership. Limit TIF and protective metadata must
match the signed action. Partial fills may decrease remaining size without
changing the frozen identity or action; increases and changed attributes refuse.
The operator review displays observed TIF without inferring historical defaults.

Cancellation uses the retained account worker and the core dispatch check;
legacy raw-request signing methods refuse discretionary cancellation. Runtime
cleanup remains separate. Legacy MCP sessions are explicitly closed after HTTP
shutdown, and shutdown waits for actual handler destruction and execution drain.
The regression first reproduced retained ledger ownership and then passed with
the session-lifetime fix. The full workspace passed 1,139 Rust tests with 15
live/keychain-gated tests ignored; 175 frontend tests, build, QA typecheck, lint
and dependency checks passed. Separate automated working-diff review found no
blocking issues. Exact-head review and green CI remain required before merging.

Ownership recovery after a lost response remains open. Signed-only evidence is
not acceptance; no historical adoption, reconstructed transport capability,
re-signing or resend is provided. Synthetic tests and automated review are not
live acceptance evidence.

## Decision Status

1. Implemented locally in ES31b: opaque signer/journal handoff and lock ordering. Publish after
   crypto without recursively acquiring a signing read permit, and prevent
   request exposure on uncertain publication. Reuse the single signer.
2. Implemented locally in ES31a/b: direct response shapes establishing OID linkage. The
   documented resting response supplies an OID, not a CLOID; correlate it to the
   exact submitted action and validate response cardinality and type.
3. Establish sufficient restart-reconciliation evidence. A query by CLOID and
   matching account alone is insufficient. Validate immutable order attributes
   and reject missing identity, conflicting OIDs and ambiguous external orders.
   Do not assume venue-enforced lifetime CLOID uniqueness. If the existing API
   cannot distinguish a signed-but-unsubmitted request from an external order
   with identical identifiers, document and resolve that boundary before claiming
   recovered ownership. Expand typed venue evidence only where required.
4. ES31c implements immutable identity comparisons for direct-accepted orders
   using frontend-order observations, including TIF and protective metadata;
   remaining size can decrease, original order identity cannot. Recovery still
   requires sufficient authenticated execution association, not just an expanded
   order-status response model.

Protocol references inspected 2026-09-08: [exchange endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint)
and [order-status info endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint).
These document request/response shapes, not an Oppen ownership guarantee.

### Recovery Investigation

The official [node documentation](https://github.com/hyperliquid-dex/node)
offers `--replica-cmds-style actions-and-responses`. However, its linked
[L1 schemas](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/nodes/l1-data-schemas)
do not specify the signed-action/response association needed here (inspected
2026-09-08). This is a candidate evidence source, not a verified recovery API.
Validate a representative association and its acquisition trust boundary before
building a verifier. A dispatch marker or matching timestamp cannot replace
that evidence. Running a node or subscribing to a provider is not authorized by
this investigation and is not an implicit desktop installation requirement.

## Acceptance Evidence

- Owned orders cancel through both approval modes and native review; manual,
  historical-unknown and cross-agent/account/route targets do not sign or POST.
- Partial fills preserve valid identity; changed protective attributes, duplicate
  OIDs, mismatched CLOIDs and conflicting ownership observations refuse.
- Evidence publication failure prevents POST. Lost response and process restart
  recover a genuinely submitted order without resend or budget reset.
- Missing/redacted/invalid-MAC evidence after review refuses at final signing.
  Use defensive synthetic fixtures, not credential extraction or exploit tooling.
- Existing reservation, cumulative-budget, approval TTL, revocation and shutdown
  drain regressions remain green. HALT cleanup works independently of provenance.
- Reopen/migration tests preserve old bytes and pending liabilities. Stop old
  writers before upgrade; rollback uses a compatible reader without lowering
  schema version or restoring older accounting state.

Synthetic tests do not satisfy dedicated-account or live recovery gates. Exact
head CI and separate review are still required; this work authorizes no account
activity, key reads, publication, funding or consent/budget reset.
