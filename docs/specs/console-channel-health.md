# Console Channel Health

Status: locally implemented and independently reviewed at `4b23774`; remote CI,
installed and live acceptance remain pending. The subsequent
[selected-market incarnation contract](selected-market-incarnation.md) revises
pool ownership and account-observation semantics without relaxing this health
projection's freshness or diagnostic requirements.
Trace: spec 30, 31, 34; owner-requested TRADE subscription audit.
Baseline: quote correction `55657ad`, chart correction `4742184`.

## Evidence

The production shell currently promotes all market health on any context/BBO/L2
payload. Context timestamps are host receipt time, while BBO/L2 use venue time.
Reconnect, quarantine and venue-error messages become `connected: true` and then
global OK. Before the first message, its age timer leaves that OK unchanged.
A diagnostic replay reproduced these paths in three tests/nine assertions;
passing diagnostic assertions demonstrate defects, not correct behavior.

`WsPool::health()` already provides subscription-specific connection,
acknowledgment, quarantine, host receipt time and age budget. These timestamps
precede consumer application. Reconnect clears acknowledgments but retains old
receipt times. Missing subscriptions do not appear in that list. Pool-local
connection IDs are not global identities or unique reconnect incarnations.

## Boundary

Read existing pool owners; do not add sockets, subscriptions or a second pool
health implementation. Expose five expected public channels: context, BBO, depth,
trades and candles. Include expected-but-absent rows explicitly. At `4b23774`, the
console pool owned the first three and the selected chart pool owned the last
two. The selected-market incarnation follow-up moves all five to the existing
selected pool, with owner `selected`; it preserves the separate account consumer.
Its explicit desktop account-observation change is defined in that follow-up
contract. Reconciliation and execution health remain outside both changes.

Keep four facts distinct:

- Transport connected, subscription acknowledged and quarantine state.
- Last socket-level observation and whether its existing age budget is exceeded.
- Consumer failure and known malformed/lost-message diagnostics.
- Validity and age of the quote/bar actually displayed by the UI.

No fact establishes signing eligibility, reconciliation, contiguous observation
coverage or an executable quote. Never use account ingress pending counts as a
public market queue-depth measurement. Preserve the original account supervisor,
signing guards and thresholds, cumulative pilot state and operator authority.

## Native Projection

Use the existing runtime status poll. A bounded public-market health snapshot
contains the exact accepted selection binding, a string revision, native
`observed_at_ms`, five ordered channel rows and bounded diagnostics. Read current
registry state under retained owners; do not relabel queued events as a snapshot.
If owner locks are unavailable or selection changes while sampling, return no new
health observation. A poll response must not invent healthy default rows.
Add `WsPool::try_health()` using `try_lock` and the same registry projection as
`health()`. Busy or poisoned locks return no observation, including chart-pool
sampling; do not replace contention with an empty/healthy result. Keep the
existing blocking accessor and execution callers unchanged. No async wait or
blocking registry acquisition belongs in runtime health sampling.

Each row contains owner kind, channel, nullable pool-local connection ID,
`subscribed`, `connected`, `acked`, `quarantined`, nullable
`last_received_at_ms`, `age_ms`, `threshold_ms`, and `age_budget_exceeded`.
Connection identity is scoped by network, feed generation and pool owner;
selected-pool identity additionally includes its immutable selection incarnation.
The projection is read-only and carries no account address.

Native revisions advance monotonically for emitted health observations, not by
wall-clock comparison. Snapshot time, channel receipt time and venue quote time
remain different fields. Backward host time is explicit uncertainty, not zero
age or evidence of freshness. Retain the last good snapshot as historical if the
runtime poll fails, stalls, loses its owner or becomes superseded.
Only accepting an exact-binding, increasing-revision health snapshot advances
the health observation deadline. At five seconds without one, retained rows are
historical/unavailable even if runtime polls succeed every second with absent,
rejected or superseded health. Track this deadline with monotonic UI elapsed
time; wall-clock rollback cannot extend it. This is status-observation freshness,
not a new venue feed or signing-age budget. Stop/replacement invalidates current
claims immediately; receiving a stale snapshot cannot restore them.

The existing budgets remain BBO 2 seconds, context 5 seconds and depth 15 seconds;
trades/candles have no silence budget. Display budget exhaustion independently
of connection state. In particular, an unchanged BBO can be connected and
acknowledged while exceeding the existing reference-age limit. This is not a
claim that the venue missed a periodic update, nor a reason to loosen a guard.

The original reused console subscriptions did not gain A-B-A wire isolation from
the projection's selection label. The selected-market incarnation follow-up
addresses that boundary at the producer, not by relabeling health observations.
This snapshot still describes the current registry, not the origin of every
buffered venue frame. Do not claim that a new ACK refreshed old prices.

## Diagnostics And UI

Record typed disconnect, reconnect, quarantine, parse-loss and consumer-failure
facts at their responsible native owner. Keep unknown venue text display-only
and connection-scoped unless the typed event identifies a channel; do not infer
attribution by parsing error prose. Bound retained diagnostics and their text.
Retain at most 16 recent scoped diagnostics of at most 512 characters each, with
an omission count if older entries are displaced. Per-channel last-loss facts
and terminal owner failure remain separate and cannot be evicted into a false
healthy claim by unrelated warnings.
Preserve last-loss information independently of subsequent successful delivery;
new data may restore observation, never erase evidence of a gap or establish
complete backfill. Terminal consumer failure remains sticky for its owner.

Frontend acceptance checks the full binding and increasing revision. Remove
payload-driven promotion of shared market health, including `feedStatus(true)`
as a readiness signal. Unrelated context, REST, chart clocks, status replies and
account polls cannot freshen another channel. Existing quote ordering and chart
projection reducers remain the owners of displayed data; health cannot mutate
their values or observation timestamps.

Show compact channel rows with factual labels: not subscribed, disconnected,
awaiting acknowledgment, quarantined, no observation, age budget exceeded or
acknowledged. Keep transport and data-age summaries separate from quote validity.
The currently displayed touch supplies two-sided/unavailable/invalid information;
do not create another native quote-merging implementation. Detailed diagnostics
can expand without hiding the chart or requiring explanatory prose in the UI.
UI timers age observations only; they cannot restore native health. A stalled
runtime poll must lose current-status claims even if old rows were acknowledged.

## Acceptance

- Reconnect before ACK/first observation, missing expected subscription,
  quarantine and malformed frames cannot become readiness through context ticks.
- BBO age-budget exhaustion on a connected quiet feed is not a disconnect;
  depth/context budget expiry stays independent of other channel activity.
- Venue/host skew, host rollback and polling failure cannot synthesize freshness.
- Held registry locks cannot block runtime status or shutdown; repeated successful
  null-health polls cross the independent five-second health deadline.
- Stale poll replies, network/generation changes, selection/interval A-B-A and
  terminal owner failures preserve exact ownership fences.
- Real loopback subscription ACK/drop/reconnect/quarantine/parse-loss tests
  exercise native projection, not only handcrafted UI rows.
- Health observations do not change quotes, bars, account health, cumulative
  budgets, execution gates or retained startup/shutdown ownership.
- Browser checks cover compact channel states beside retained quotes/charts,
  desktop layout and escaped diagnostics. Independent review and combined local
  gates precede any commit acceptance. Installed/public-feed proof stays separate.

## Subsequent Subscription Work

The [official subscription reference](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)
was checked on 2026-09-08. Aggregate-market candidates include `allMids` and
`allDexsAssetCtxs`; `fastAssetCtxs` needs bounded decompression and delta handling.
Account candidates include `openOrders`, `clearinghouseState`, `spotState`,
`userFundings`, `userNonFundingLedgerUpdates` and `userEvents`.

These are candidates, not newly enabled subscriptions. Adopt aggregate market
updates in a separate typed adapter/rail change. Account stream adoption requires
independent durable-accounting review, duplicate/snapshot handling and explicit
recovery evidence; new streams must not create a second event ledger. Do not
equate cancellation with position closure or replace required REST reconciliation
with optimistic stream state. No new provider, key, wallet action or permission
is authorized by this inventory.
