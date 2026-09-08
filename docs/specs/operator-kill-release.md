# Reviewed TESTNET Kill Release

ES38 follows the locally implemented activation boundary (ES37). Planned, not
implemented or authorized for a real account. Trace: spec 24-29 and 32, the
operator-only mutation invariant, and the supervised testnet delivery goal.

## Outcome And Non-Goals

An operator can review one current kill scope, explicitly confirm its release,
and observe a durable correlated result. Every release leaves policy
acknowledgment absent. Fresh activation review and confirmation remain mandatory
before orders; release is not order eligibility, venue readiness or acceptance.

Initial pilot consent, legacy adoption, account provisioning, key changes,
budget changes, position closure, order cancellation and new agent permissions
are outside this action. Missing/unknown authenticated pilot accounting and
permanent pilot stops refuse release. Never replace consent, reset cumulative
usage or erase a pilot stop to make release possible.

## Reviewed Authority

Use the existing runtime engine, ledger, policy authority and registry. No
second engine or parallel event store. Review is an opaque engine/owner-bound
object; frontend JSON is display data, not authorization evidence.

Bind and show TESTNET, selected scope, persisted and local emergency engagement
evidence, policy revision, local stop generation, authenticated routes/pilots,
and the full affected roster. Re-engagement can retain the same timestamp and
reason while changing stop generation; comparing engagement fields is not
sufficient. Review expires after 60 seconds and clock rollback refuses.

For global release, show and bind the sorted full affected membership, not only
the selected account. Missing, ambiguous or changed membership/authority refuses.
Membership is the union of authenticated policy agent membership and active
registry grants, cross-validated in the same verified snapshot. Policy entries
with missing/retired routes and active grants without policy are unresolved
members, not silently omitted. Empty membership refuses. Every affected route
must have known authenticated pilot accounting without a permanent stop; one
unknown or permanently halted affected pilot refuses the entire global release.
Current pairings and the selected runtime are not authoritative membership.
No additional binding may be released outside the displayed, confirmed scope.
Per-agent kills survive global release. Agent release never removes a global
or another agent's kill. The UI distinguishes these remaining stops.

## Mutation Boundary

Do not implement a native precheck followed by unbound `operator_release_kill`.
The core must compare the exact reviewed evidence under mutation serialization
and verified ledger coordination before constructing the one-scope removal.
No stale full-policy rebase is allowed.

Reuse the existing authenticated policy transition and transaction machinery.
Verify checkpoint, policy, routes, affected membership and pilot state under
the write transaction. Persist correlated release evidence with the transition
and retain ownership through anchor publication. No engine-state lock spans
disk I/O and no synchronous guard spans async I/O.

Recheck local stop generation after publication. A concurrent stop wins; never
clear its emergency overlay. If the policy removal may already be durable,
report an uncertain outcome rather than claiming nothing changed. Keep order
admission inhibited, retain the actual task and require verified reconciliation.
No automatic retry, compensating release, acknowledgment or budget reset.

Assign a restart-safe operation identity before confirmation and bind it into
the durable request/transition evidence. Read-only reconciliation can report
committed, not committed, or still unknown without issuing another release
write. A committed classification requires the exact correlated authenticated
transition and verified chain/checkpoint, not a matching current kill boolean.
Absence can mean not committed only after actual work is terminal or exclusive
runtime ownership proves the previous owner cannot still commit. Pending work,
unverifiable history or ambiguous publication remains unknown. A later stop is
reported separately and prevents an old receipt from re-arming over that stop.

## Native And UI Ownership

Operator-only Tauri commands expose status, review, confirm and discard. The
retained controller uses owner/operation/review correlation and actual drain,
including dropped IPC observers, timeout, panic, shutdown and replacement.
Fresh status resolves only the matching operation; old receipts cannot resolve
a newer unknown command. No agent MCP tool exposes release.

HALT currently has a lifetime latch. Release must not leave future HALT presses
as successful no-ops. Re-arm only through a verified release outcome and proper
prior-worker lifecycle handling. Fence worker callbacks, observer sweep baselines
and frontend polls by the relevant HALT/release generation. Preserve historical
HALT/cancellation receipts separately from the current effective stop state.

A HALT during release invalidates that release and remains actionable. Stale
release completion cannot clear a new HALT. Explicit release permits a new
activation review; it does not silently clear frontend uncertainty or remove
later stops. Duplicate/retried commands cannot cause a second mutation.
Specifically, a HALT press during release must immediately advance the local
stop/release fence and inhibit orders even if the old HALT request is latched
or its worker is still retained. The ordinary already-requested fast path must
not swallow that press. Durable work can remain serialized and owned; local
inhibition cannot wait for prior release completion or worker drain.

## Acceptance

1. Exact agent/global scope removal, untouched unrelated policy and remaining
   stops, absent acknowledgment, unchanged original consent/budgets and intact
   ledger across physical restart.
2. Repeated engagement with identical fields, policy/route/pilot/membership
   changes, expiry and clock rollback all invalidate old review.
3. Real HALT during publication retains the stop and produces the appropriate
   refused/uncertain outcome. Audit/anchor failures never imply successful
   release or restore admission. Read-only outcome reconciliation never writes
   another release and cannot classify absent-but-running work as not committed.
4. HALT -> release -> HALT works through the same native owner. Delayed old
   callbacks and polls cannot overwrite the second HALT. Lost replies,
   cancellation, panic and shutdown retain actual ownership until drain.
5. Clear review, explicit scope confirmation, remaining-stop evidence, receipt,
   stale/refused/uncertain states and repeat-HALT behavior in the actual UI.
   Synthetic tests verify zero signatures/exchange POSTs for release itself.
6. Focused and integrated checks, independent exact-head review and green CI.
   Installed-artifact and explicitly authorized real-testnet gates remain
   separate; this specification authorizes no account activity or publication.
