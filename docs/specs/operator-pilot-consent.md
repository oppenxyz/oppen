# Reviewed Initial TESTNET Pilot Consent

ES39 follows local ES37 activation and ES38 kill release. This is an
implementation contract, not account confirmation or authorization to trade.
Trace: spec 24-29 and 32, operator-only authority, and the supervised alpha goal.

## Outcome And Scope

Before MCP startup, the operator reviews the exact dedicated TESTNET identity,
existing registry route, paused policy, ledger checkpoint and fresh account
evidence, then explicitly confirms initial immutable pilot consent. The result
is a correlated authenticated receipt. Consent never starts MCP, activates
orders, releases kills, changes policy or submits a venue action.

Use the existing signed `PilotAuthorized` record, its original baseline and
fixed limits: $15/order, $150 cumulative executed notional and $5 realized loss
including fees. Authenticated policy must separately enforce $25 gross exposure
including resting opening orders and maximum 1x leverage. Display all five
limits without rounding away material values; this ceremony cannot edit them.

Existing authenticated consent is inspect-only. Existing legacy consent needs
its separate preservation/adoption path, never fresh authorization. Neither
account changes, application restart, later profit nor canceled orders renews
consent or resets accumulated usage or permanent stops.

## Baseline Preservation

The legacy trusted `authorize()` API checks outstanding liabilities but permits
fully filled prior submissions before a new baseline. That is insufficient for
this ceremony: currently flat and no pending liability do not establish zero
prior in-scope execution.

Refuse fresh consent when known in-scope execution or other accounting requires
preservation, including unmatched fills, ambiguous submissions and relevant
redacted/unverifiable evidence. Recheck this inside the authorization transaction.
Do not classify unresolved submissions as not sent, omit fills, adopt missing
anchors, create a clean ledger or erase history to enable consent. Previously
active accounts without adequate accounting require a separately reviewed
preservation path and completeness decision.

Existing reconciliation may journal fetched fills/gaps before review. That is
venue-read-only but not local-ledger-read-only. Reuse its ownership and verified
event machinery, not a second accounting store. Its first-run 30-day lookback
does not prove lifetime account history. Display the actual observation coverage
and require explicit dedicated-account/exclusive-use and baseline confirmation;
never label bounded observations as complete lifetime verification.
Require a separate explicit attestation that the account has never previously
been used for in-scope trading. Previously used or uncertain history cannot use
this zero-baseline ceremony and requires the preservation decision instead.
Known contradictory evidence overrides the attestation. Complete pagination
over the claimed observation interval is mandatory; a bounded lookback is not
permission to omit pages or suppress reconciliation findings within that window.

## Core Authority

Add an opaque, non-cloneable, non-deserializable owner-bound review to the
existing pilot journal. Bind TESTNET, full authorized route, authenticated paused
policy/revision, ledger head, fixed baseline timestamp, account evidence and
limits. Review expires after 60 seconds; account observations must independently
meet the existing policy's account-freshness bound at confirmation.

Confirmation accepts the retained review, not client-supplied authority fields.
Under verified ledger coordination and a write transaction, recheck the exact
head, route, policy, absence of any consent, baseline-preservation predicates,
fresh evidence and final native-owner admission. Resample time after waits and
authority replay, immediately before append. Clock rollback or changed evidence
refuses; do not silently rebase the baseline to the latest head.

Append the existing authenticated consent shape and retain ownership through
commit and anchor publication. Correlate outcomes by the exact signed route,
reviewed head and fixed baseline timestamp; a different consent is not success.
No new event store or data migration is required. Preserve old reader behavior.

Ambiguous commit/publication is uncertain. Read-only outcome reconciliation
verifies the exact authenticated row and published checkpoint without changing
anchors or retrying authorization. Absence alone is not proof of non-commit
while a retained worker may still commit. Existing `authorize()` publication
retry is a mutation and must not be disguised as a status read or automatically
invoked by this ceremony.

## Native And UI Ownership

Reuse the pre-MCP retained setup-operation lifecycle. Consent owns one Runtime
slot, exclusive with policy setup, MCP launch and network/account/context
replacement. Open only existing anchored authority and its HMAC; do not create
keys or load an agent signing key for consent. Keep the actual worker through
dropped IPC, observer timeout and shutdown drain.

Reuse concrete read-only account-gathering and reconciliation logic. Both review
and confirmation need fresh flat positions, no resting orders, exact account
identity and unchanged authority. A feed's `reconciled` flag is not proof that
pending submissions are settled. Venue reads and local commit are not atomic;
bound their age and preserve ledger/ingress fences, without claiming they prove
exclusive use. An untrusted agent must not reach consent commands.

The UI exposes review, explicit confirmation, discard before admission, current
status and read-only outcome recovery. Show complete evidence, observation
coverage, original limits and the no-reset commitment. Retain correlated unknown
outcomes across context/owner changes; a lost reply cannot trigger another write.
Known terminal native evidence may release an observer's busy state, but late
replies cannot clear newer work. Existing consent remains inspectable without
offering replacement. Successful consent still leaves policy paused and
activation unacknowledged.

## Acceptance

1. Exactly one authenticated consent with the displayed baseline and limits;
   physical restart preserves it without policy, kill or acknowledgment changes.
2. Existing/legacy consent, prior execution, unresolved or redacted accounting,
   changed head/route/policy, expiry after waits and clock rollback refuse without
   creating a new baseline. No budget or permanent-stop reset.
3. Fresh flat/no-order evidence at both stages; stale or incomplete observations
   and concurrent ledger/ingress changes refuse. No lifetime-completeness claim.
4. Lost IPC, competing confirmations, panic, shutdown and replacement preserve
   actual ownership and exact outcome identity. Reconciliation performs no write.
5. Real UI review/confirm/receipt/refusal/unknown flow works before MCP startup;
   synthetic checks prove zero signatures or exchange mutations.
6. Focused and workspace tests, independent exact-head review and green CI.
   Installed-artifact and live acceptance remain separate. The candidate account
   still needs explicit owner identity confirmation; no test fixture supplies it.
