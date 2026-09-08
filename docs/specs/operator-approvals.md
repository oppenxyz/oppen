# Operator Approvals

Implementation planning for spec items 18, 19, 28 and 32. Existing core approval
methods are not a completed operator workflow. All stages below remain safety
gates before exposing a usable approval action or claiming live acceptance.

## Current Evidence

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
