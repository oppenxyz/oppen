# Selected market transport incarnation

Trace: spec 9, 11, 31, 34; AGENTS invariants 1, 3, 7. This extends
`console-channel-health.md` and `live-chart-observations.md`. It does not expand
subscriptions, enable execution, replace a ledger or authorize live activity.
Status: implemented locally; resumed workspace verification passed 1,298 Rust
tests (including 132 native tests), with 15 live/environment tests ignored, plus
246 frontend tests, strict workspace lint, formatting, build and QA types.
Cached/offline dependency checks and three release-script tests also passed.
Earlier mock browser checks and independent design/implementation source reviews
passed. Exact-head review, remote CI, installed and live acceptance remain pending.

## Evidence and failure boundary

At `4b23774383cfd3a36a714755ad970ebd651d8ede`, context/BBO/depth share the
long-lived console pool. Same-account symbol changes reuse its generation.
`createMarketFeed` checks symbol and feed generation but only chart projections
carry a selection identity. Returning BTC -> ETH -> BTC resets quote ordering;
a delayed first-BTC quote/context can bootstrap the later BTC selection.
A local production-controller replay reproduced both acceptances (one test,
three assertions). That replay asserts the defect, not a passing acceptance gate.

The venue's [subscription protocol](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)
does not supply our local selection ID. Adding the current ID at dequeue time
would relabel buffered old frames. Unsubscribe/ACK is not a proven drain barrier.
The existing fresh chart socket already captures an immutable producer binding.

## Ownership transition

Reuse the existing per-selection transport for all five selected public streams:
context, BBO, depth, trades and candles. Do not add a third pool or duplicate the
public subscriptions on the account pool. Keep one selected socket, with existing
channel configurations and age budgets. Rename misleading transport identifiers
only where required by this ownership change; chart reducer/history types can
retain their chart-specific names and APIs.

The account ConsoleFeed retains its account subscriptions, ledger application,
consumer lifetime and network/account generation. Same-account market selection
must not recreate it, clear its failure, cancel its work or reopen its ledger.
The MCP FeedPump, ingress admission, reconciliation and pre-sign session remain
untouched. No public event is forwarded into that signing session.

At selection admission retire the previous public owner immediately. Await its
actual producers and consumer drain before starting the next selected socket.
Every selected event captures the original full binding at production: network,
feed generation, selection ID, symbol and interval. An old closure never reads
the mutable current selection. Recheck terminal state and request ownership after
waits. Partial start and failed emission remain owned until real drain; a failed
selected transport is recoverable without terminating account supervision.

## Delivery and failure scope

The event DTO must distinguish selected-public events from account-owner status
explicitly. Selected updates require their producer's selection identity in
addition to network/generation. Reject absent, foreign or retired selected scope
before applying values, quote invalidation, chart retention or failure latches.
Preserve symbol and interval validation as additional checks, not substitutes.
Account-owner status must not carry public payloads or inherit a mutable selected
ID. Its genuine terminal failure remains sticky for that account-feed generation
across symbol/interval changes. Selected failures latch only their own selection.
No compatibility fallback may accept an unscoped quote as current.

Use a discriminated event envelope: selected scope contains `binding:
ChartBinding`, its public update and optional failure; account scope contains
`binding: FeedBinding`, a status-only update and optional failure. Account scope
cannot carry a quote/context/chart payload. Account status never runs through
the selected quote/chart reducer, including during a pending selected watch.
Cache genuine bound account terminal failure for missed events/dead consumers
through runtime status; do not infer account ownership from generic error prose.
Rename the public-producer runtime diagnostic to `selected_failure`; keep its
full binding and sticky owner semantics, now applicable to all five channels.

Frontend acknowledgment binds the complete selected identity already returned by
watch_market. A pending/rejected/superseded watch accepts no selected payloads.
Existing quote timestamp ordering, null-side handling, REST request fences and
chart provenance remain intact. Old values may be retained as historical, never
freshened by rejected data or status. This internal DTO ships as one native/web
artifact; mixed old/new payloads fail closed, not by silently reusing generation.

Health ownership follows the selected producer for all five rows. Use a truthful
selected owner label, not five rows attributed to an account pool that no longer
subscribes them. Keep exact-binding and final-publication checks, nonblocking
sampling, bounded diagnostics, persistent scoped losses and the independent
five-second monotonic UI deadline. A retired selected owner's loss/failure cannot
poison a newer owner; account failure remains available through runtime status.

## Desktop account observation clock

Moving public streams changes the old desktop account freshness input. Do not
claim it is behavior-preserving or continue labeling market/reconnect activity
as successful account observation. Retain a native last-applied account-stream
receipt timestamp owned by ConsoleFeed, separately from core FeedSession state.
Advance it only after successful application of matching-account UserFills or
OrderUpdates, including valid empty snapshots. Failed/blocked application,
foreign accounts, public data, reconnect, ACK, REST and chart clocks do not
advance it. Preserve it on disconnect and selected-owner replacement; reset only
with the actual account/network owner. Quiet account streams can age despite a
connected transport; this timestamp is not reconciliation or signing readiness.

The existing desktop account projection may consume this timestamp, but its UI
must name observation age rather than claim socket health. Retain the last
timestamp internally on host rollback; do not supply a future timestamp that the
existing saturating age calculation would present as newly fresh. Missing or
uncertain observation is not zero age. Do not change shared core/MCP clocks or
relax any execution freshness predicate to accommodate quiet streams.
The getter must match both network and actual configured account. Capture the
observation before sampling the `as_of_ms` passed to `assemble`, and validate
against that same time. Foreign-account frames must be refused before ledger
application, not merely excluded from the timestamp. AccountNotice and the
status-bar account label must follow these observation-only semantics.

## Preservation and rollback

No persistent schema, policy, key or budget migration. Existing hash-chained
events and gaps remain untouched, including earlier public-feed gap records;
do not erase them because the new console pool no longer creates those records.
Rolling back the artifact restores the old producer layout without rewriting
state, but also restores the documented selection race. Never call that a safe
live acceptance result. No updater/release migration is part of this work.

## Acceptance

1. Production frontend replay rejects delayed first-BTC quote, context, depth,
   status and terminal failure after BTC -> ETH -> BTC; controls accept the final
   owner's updates. Also cover interval round trips, missing scope, pending
   watches, stale acknowledgments, network/generation replacement and stop.
2. Actual loopback selected sockets prove held old frames retain their original
   binding, including after retirement admission; old producers fully drain
   before new start. Exactly five public subscriptions, none on account pool.
3. Preserve ledger/account owner and true terminal account failure across public
   selection changes. Prove successful matching application advances the desktop
   observation clock; blocked/failed application and all non-account events do
   not. A physical ledger reopen retains existing rows and gaps unchanged.
4. All five health rows follow selected ownership, with retained diagnostics,
   busy-sample behavior, independent receipt ages and expired-status semantics.
5. Focused regressions, full Rust/frontend suites, strict lint, build/types and
   separate aggregate/exact-head review. Browser replay and inspected desktop
   screenshots exercise real frontend consumers with synthetic inputs.
6. Native-to-UI installed/public testnet proof remains a separate gate. Do not
   count synthetic fixtures as authorized account activity or remote CI.
