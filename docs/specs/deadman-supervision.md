# Durable supervised dead-man arming

Trace: spec 7, 9, 24–27, 29, 32, 34; decision O1; recovery gate #43.
Status: proposed implementation contract. The existing pure intent/clearance
helper is not a scheduler. No implementation or live protection is claimed.

## Scope and owners

Implement per-account arm/refresh supervision through the existing MCP runtime,
account execution queue, core signer, authenticated ledger and native status
projection. Do not add another signer, event store, timer service or MCP tool.
Retain the existing supervised testnet boundary and pilot budgets. The current
official quota counts scheduled firings, not routine refreshes; the corrected
[signing reference](../hl-signing.md#10-schedulecancel--the-dead-mans-switch-and-its-daily-budget)
records source and live acceptance limits.

The existing `server.rs` supervision loop owns the work and shutdown/drain. Its
protective-cleanup tracker is a lifecycle owner, not session signing authority.
Use an explicit account/route-bound protective operation at the core boundary;
do not mislabel scheduling as a paired agent's discretionary order. Authorization
must remain valid after account-queue waits, key loading, signing and before POST.
Unpairing or owner replacement cannot detach already admitted work or grant a
new account access to an old account's schedule state.

Initial arming requires a distinct native operator review and confirmation of
the exact account-wide cancel effect, authenticated registry route, existing
pilot/policy, coverage limits and retained-schedule behavior on quit. Persist
that confirmed authority through the existing authenticated ledger before it
can admit a schedule operation. Initial pilot consent, pairing, exposure or
starting supervision alone is not dead-man consent. An agent cannot invoke this
operator action. Live use still requires the owner's account/activity approval.

Bind the grant to the current runtime owner as well as network/account/route and
original pilot. It authorizes only protective arm/refresh, not disarm, trading
or policy changes. Ordinary execution pause or pairing revocation does not
implicitly revoke that explicitly retained protective authority; current core
registry/key/route checks still apply before every signature and dispatch.
Actual route/key revocation refuses new operations. Runtime stop/replacement
closes this grant's admission; a new runtime requires fresh operator review,
while prior accepted/unknown schedules remain durable and visible. No automatic
initial arming or silent protection re-enrollment occurs on startup.

Use reconciled account positions and resting orders to establish a need for
coverage, rather than agent liveness alone. Unknown exposure is not flatness.
A protective attempt for the existing authenticated account may still be needed
while new order admission is paused. It must not restore activation, release a
stop, reset accounting or claim reconciliation. Missing authenticated route/key
or unavailable policy is a visible refusal, not an alternate signing path.

## Durable operation boundary

Extend the concrete authenticated-event/anchor machinery already used for policy
and registry authority. The order submission journal requires an order/CLOID and
must not be reused by inventing a synthetic order. Every schedule operation is
part of the same append-only event ledger and is authenticated for its network,
account, route, ledger instance and predecessor sequence.

Before signing, durably record the intended account-bound deadline and owned
operation identity. Record the signed request's exact correlation before wire
submission, then accepted, definitive rejection or unknown delivery outcome.
Reuse existing nonce allocation and retained account serialization. Failed
persistence/publication prevents a new POST; loss of an observer does not release
the account queue or forget an operation. A lost response or failed outcome
publication remains unknown across physical reopen.

An accepted response must correspond to that exact operation and requested
deadline. Never turn a generic transport success, unrelated response or queued
request into confirmed coverage. Late completions update their original operation
only and cannot overwrite a newer owner/deadline. Preserve rejected/unknown
refreshes alongside the prior accepted schedule: a failed refresh does not prove
the prior protection disappeared, and an unknown refresh does not prove it moved.

Existing protective cancellation fallback does not provide this durable schedule
receipt boundary. Extend the owned submission path rather than assuming the
current untracked fallback is sufficient. Final authorization and dispatch checks
remain in the single core signing path; a TypeScript guard is not authority.

## Time, quota and truthful coverage

Bound refresh traffic with the existing supervisor timer and skip missed ticks;
do not replay a burst of old timer ticks after sleep. Re-evaluate deadline lead
time after waits and key loading. A locally valid requested time is not proof
the venue accepted its minimum lead. A rejected request is reported as rejected,
without promising a replacement schedule.

Keep three facts separate: the latest accepted future schedule, potentially
effective schedules from unknown operations, and evidence of past firings.
A host deadline elapsing or orders disappearing does not prove a venue firing.
Track a possibly fired schedule once, with its original operation identity;
repeated status polls and restart must not double-count it. Routine refreshes
must not consume the firing count. Multiple unknown replacements must not be
collapsed into a conveniently known schedule or quota.

Replay supersession only when ordering and timing are established. For example,
after A's acceptance is durable, an accepted refresh B whose signed, venue-enforced
`expiresAfter` is strictly before A's deadline establishes replacement before A
could fire. Preserve A's history but do not later count its old deadline as a
possible firing. A host receipt timestamp alone does not establish this proof
under clock uncertainty. If B may have applied after A's deadline, retain the
possibility of A firing. Unknown A followed by accepted B is not sufficient to
discard A: the earlier request may still have reached the venue out of order.
Until its effect/order is established, retain ambiguous alternatives and report
uncertain coverage/quota rather than a sum of supposedly confirmed firings.

Use UTC venue-day semantics without erasing unresolved operations at midnight.
Clock rollback/uncertainty cannot create new capacity or freshen old coverage.
Expose remaining capacity as unknown when the available evidence cannot establish
the account's full daily count, including outside activity. Do not publish a
precise remaining venue quota by subtracting local requests from ten. Exhausted
or unknown evidence is visible; retries must not be justified by a fabricated
counter reset. Operator-exclusive-use statements do not replace venue evidence
for an already elapsed ambiguous operation.

The operator projection identifies account/network, accepted deadline and receipt
time, refresh pending/refused/unknown state, potentially elapsed schedules, and
quota evidence/uncertainty. Display accepted future coverage distinctly from
currently unverified protection and from execution readiness. Venue prose remains
inert display text; control flow uses typed errors. A dead-man cancels resting
orders and does not close positions.

## Disarm, stop and compatibility

Automatic disarm on agent inactivity is not included. Disabling protection needs
separate explicit operator authority under `operator-approvals.md`; no agent tool
may disarm. Shutdown/context replacement stops new admissions and drains actual
owned work, preserving unresolved schedule evidence. It must not clear a venue
schedule merely because a local task ended. The operator must see that a retained
schedule can later cancel orders on its account.

Before the full spec-27 gate can close, separately deliver reviewed disarm and
quit/cancel semantics with account-wide effects visible. Arming/refresh is a
coherent first slice, not completion of every dead-man requirement.

Preserve all prior ledger bytes, anchors, budget evidence and pending liabilities.
An upgrade with no historical schedule evidence reports that absence/uncertainty;
it does not seed confirmed zero usage. Define compatibility with older writers
before shipping new authenticated event kinds. Rollback cannot restore an old
accounting snapshot or silently discard schedule events.

## Acceptance for the first slice

1. Actual loopback supervisor-to-exchange tests prove arm and refresh through
   the existing signer/queue and exact response correlation, with no extra pool
   or independent daemon. No operator grant, stale review, mismatched account or
   replaced runtime admits initial signing. Multi-account cases preserve
   independent state; pairing revocation preserves only the separately granted
   protective scope and never permits trading or disarm.
2. Delay queue acquisition, key loading and POST; retire/revoke authority, stop
   the runtime, or advance the clock across each wait. No stale deadline or
   unauthorized dispatch escapes the final checks. Guardrail no-bypass property
   and signing vectors remain green.
3. Drop replies, cancel observers, panic workers and fail intent/signed/outcome
   publication. Retained owners drain; physical reopen preserves uncertainty,
   original budgets and chained history. No blind retry or false acceptance.
4. Cross midnight and roll clocks backward with accepted and unknown schedules;
   never double-count possible firing, decrement on refresh, or invent quota.
   Unknown exposure does not trigger disarm; startup cannot reset history.
   Prove accepted A followed by timely accepted B supersedes A without charging
   both deadlines; unknown A followed by accepted B retains unresolved ordering.
   Missing/expired request expiry or uncertain clock evidence cannot establish
   timely replacement. Keep `expiresAfter` signing vectors and venue acceptance
   explicit; a test fixture accepting an invalid expiry is not protocol proof.
5. Render accepted, pending, rejected, expired and uncertain account-scoped
   states through the actual native/UI status consumers. No stale poll can
   overwrite a newer operation. Inert diagnostic and layout checks remain green.
6. Independent design/implementation review, exact-head CI and installed proof
   precede completion. Explicitly authorized venue scheduling/firing evidence
   is still required for the live gate; synthetic data cannot supply it.
