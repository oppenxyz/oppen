# Live Chart Observations

Status: implemented and verified locally; independent automated working-tree
review found no remaining blockers. Remote CI, installed-artifact and public-feed
acceptance remain pending. This is not live trading approval.
Trace: spec 30, 31, 34; charts section 2.4; owner-requested TRADE feed audit.
This follows the local quote correction `55657ad` and changes no execution,
accounting, wallet, permission or trading authority.

## Required Boundary

One retained Rust chart owner binds network, native feed generation, symbol,
interval and a distinct selection incarnation. Symbol equality alone does not
prove ownership across A-B-A selection. Emit ordered projection revisions so
the frontend rejects late owners and older responses without rebuilding bars.
TypeScript remains responsible for parsing display numbers, retaining current
projections and rendering, not a second trade-to-candle implementation.

Reuse the core interval parser, checked bar types, venue partition validation
and appropriate aggregation primitives. `LocalAggregator` intentionally discards
the first partial bucket and can synthesize covered empty buckets; neither rule
may be silently changed to support immediate chart bootstrap. No unobserved
interval may be drawn as known zero volume merely because context/BBO traffic
continues. Keep native interval UI scope unchanged.

## Observations And Ordering

Preserve individual print identity, timestamp, price and size until Rust has
assigned each print to its own bucket. Deduplicate before accumulation and order
open/close by available event ordering, not arrival time alone. Define equal-time
conflicts explicitly; a trade identifier is not automatically chronology.
Bound retained history, pending observations and dedup state. Disclose loss of
coverage rather than silently counting replayed prints after eviction.

Native candle bucket start/end are not update timestamps or print-inclusion
watermarks. Keep venue candle observations and locally observed print aggregates
distinct. Never add local print volume to venue cumulative volume without proof
of non-overlap. A local partial observation remains partial after bucket rollover;
display its provenance and distinguish it from venue-reported volume.

Older candles may reconcile their historical bucket but must not move the
forming bucket backward or duplicate closed bars. Define same-bucket conflict
handling without inventing an exchange revision. Interruption invalidates
coverage before subsequent prints; a reconnect is not proof of complete history.

## REST And UI

Merge REST history under the retained Rust owner and request/observation fences.
Late success or failure cannot erase newer live observations or affect a later
selection incarnation. Live projections must bootstrap independently of failed
REST history reads, with truthful interval and price-precision metadata.

Keep a valid retained/live chart visible alongside history-read errors. Surface
chart-specific observation age, source and partial/unverified volume in visible
status and accessible summary. Shared market traffic cannot refresh chart age.
Do not introduce arbitrary intervals, Quantoppen, account subscriptions or a new
execution reference-price source in this remediation.

## Acceptance

- Deterministic cross-bucket batches, duplicate and out-of-order prints, equal-time
  conflicts, late candles, and overlapping native/local observations.
- Live bootstrap after REST failure; concurrent REST success/error, selection
  and interval A-B-A, disconnect/reconnect and bounded retention.
- Native loopback transport-to-projection checks plus frontend stale-owner and
  rendering checks. Existing renderer golden tests remain unchanged in meaning.
- Independent review, exact-head checks and the relevant combined test gates.
  Public-feed and installed-artifact verification remain separate; no fixture
  proves a live trading gate.

## Projection Contract

The native envelope binds network and feed generation; its chart projection
additionally carries `selection_id`, monotonically increasing `revision`, symbol,
canonical interval, `interval_ms`, nullable `price_decimals`, `closed`, `forming`,
`latest_trade`, `history_error`, `observation_error`,
`last_observation_received_at_ms` and `tape_status`.
Identifiers and revisions cross IPC as strings. Decimal OHLCV values remain
strings. Missing precision uses an explicit display fallback, not invented asset
metadata. Tape status is observing, interrupted or capacity-exceeded; observing
does not mean complete.

Each observed bar includes bucket `time_ms`, OHLCV, source (`venue` or
`observed_trades`), `partial`, `open_close_ambiguous` and host `received_at_ms`.
Closed means an elapsed bucket, not complete observation coverage. Every tape bar
remains partial. The latest-trade marker includes event `time_ms`, decimal price
and `price_ambiguous`; it is not asserted newer than a venue candle close.

Keep venue and tape bars separately internally. Prefer the venue bar when one
exists, otherwise project the partial tape bar. Never combine their OHLCV. Tape
prints move the separate latest-trade marker even while a venue bar is displayed.
This preserves visible live price observations without fabricating candle volume.

Choose tape open/close by minimum/maximum event time. Distinct endpoint prices
at equal timestamps use a deterministic identity tie-break and set ambiguity;
identity is not proof of chronology. Venue WS observations use local arrival
precedence, explicitly not exchange revision ordering. Identical repeats need
not rebuild bars but may advance receipt age. Neither candle end nor trade count
establishes a cross-channel inclusion watermark.

Classification of elapsed buckets uses a monotonic local watermark. Host-clock
rollback cannot reopen or hide previously elapsed bars; expose clock uncertainty
until the clock catches up. A clock-only projection changes classification, not
the last observation receipt or shared market health. Chart/history projections
must never synthesize a general market tick.

For display trade ingestion only, allow up to five seconds of forward venue/host clock
disagreement with explicit uncertainty and an unverified marker. Larger forward
disagreement is refused recoverably without advancing retention or permanently
freezing the tape. This bound is not an exchange timing guarantee or execution
freshness tolerance. It changes no signing guard.

## Bounded Ownership

Use the existing chart-history bar bound and at most 65,536 retained trade
identities per selection. Identity includes coin, venue time and trade ID.
Identical identity/payload repeats never accumulate again. Conflicting payloads
invalidate tape coverage and freeze its aggregation for that selection; do not
choose one silently. Preserve a distinct invalid-observation status when needed
to explain that refusal rather than mislabelling it a capacity failure.

Evict identities only with permanently inadmissible old buckets, retain a
monotonic admission floor, and reject later prints below that floor. No LRU
eviction that makes replay countable again. Reaching capacity latches
capacity-exceeded and freezes tape aggregation and its marker for that selection.
Retain the marker explicitly unverified/ambiguous; venue bars may continue. A new
selection starts partial, never implicitly complete.
The floor also rejects old candle and REST bucket reinsertion. Conflict and
capacity states stay latched; marker updates cannot clear them or erase known
equal-time price ambiguity.

The retained native owner selects before subscription/history work, returning
the selection binding. A history ticket captures unforgeable local owner and
request incarnations under the owner's short lock; all REST awaits occur outside
locks. Only the current selection's latest history request may finish. WS-owned
buckets are protected regardless of whether they arrived before or after the
request began, rather than inventing an ordering from REST completion time. Fill
missing venue history but preserve WS-owned bucket observations, independent
tape aggregates and marker. A matching error updates only history status.

IPC replies and events return the same ordered projection. Frontend acceptance
requires the acknowledged selection and an increasing revision, regardless of
reply/event delivery order. A distinct selection ID alone cannot identify a
delayed A-B-A wire frame on a reused venue subscription: native implementation
must bind the actual receiver/subscription incarnation or explicitly reject that
ownership guarantee before implementation review. Relabelling queued old frames
with the current selection is not a fence.
Use a fresh chart-only transport/receiver incarnation per selection, captured
by its producer. Retire and drain the previous chart owner before starting its
replacement, with joins retained through cancelled IPC waits. Keep account
supervision and its receiver unchanged. The implementation review must prove a
bounded transport lifecycle, including bytes decoded after selection changes.

One retained `ChartTransport` inside `ConsoleFeed` owns a chart-only `WsPool`
capped at one connection, receiver-consumer join and immutable chart binding.
Remove trades/candle subscriptions from the account/context/book pool. Runtime's
existing retained watch worker performs retirement, producer drain, pool drop,
consumer join, desired-selection recheck, then replacement. Retain handles while
awaiting; superseded starts remain owned until drained. Shutdown waits for the
same worker and transport. Invalidate accepted chart ownership at synchronous
selection admission; do not change account feed identity on chart selection.

`ChartBinding` contains network, native feed generation, selection ID, symbol and
canonical interval. `watch_market` returns the existing feed binding plus a chart
binding; keep shared execution/runtime `FeedBinding` semantics unchanged (use a
watch-specific wrapper if needed). `chart_series` accepts the exact chart binding
and returns its revisioned projection. Retired-owner history completions are
superseded and cannot publish into the replacement. The actual native owner
resolves that binding before beginning history, rather than trusting caller
symbol equality. The independent chart socket buys wire-incarnation isolation;
it is bounded and read-only, not an additional account or execution feed.

The existing runtime poll carries exact-binding chart-consumer failures to idle
views. A matching failure is sticky: later projections and null polls cannot
restore a live label, and an old binding's failure cannot poison its replacement.
Core transport diagnostics preserve earlier observation and clock warnings,
freeze tape/marker without advancing freshness, and do not churn revisions when
the same failure is reported again.

Once chart producers and consumer have actually joined, chart-only failure is a
recoverable chart refusal, not termination of unchanged account supervision.
Surface the diagnostic and allow a later selection retry. Pending shutdown takes
precedence over that recoverable result; no late chart error may reset stopping
to running or discard retained drain ownership.

W2 is amended to replace optimistic venue/tape blending with this source-separated
observation policy. No dependency was added.

## Local Evidence

The combined Rust workspace passed 1,282 tests with zero failures and 15 ignored
tests. This includes 15 core chart regressions and all 119 native library tests.
Real loopback frames exercise cross-selection retirement, retained consumer
drain, quiet clock rollover, emission failure and consumer panic. A deterministic
shutdown-race test failed with Running while account drain was held, then passed
after terminal state was given precedence over recoverable chart failure.

All 233 frontend tests passed, including unchanged renderer golden frames,
binding/revision fences, independent market health and sticky consumer failure.
Production build, QA typecheck, strict all-target workspace Clippy and formatting
passed. Mock-only browser replay proved REST
failure bootstrap, separate marker/volume, retained errors, A-B-A rejection,
independent age and escaped consumer diagnostics. Screenshots were inspected at
1440px and the supported 1280px minimum, including an 800px-high failure state.
These checks use synthetic data; they do not prove native-to-installed-app venue
delivery or any account activity. No new dependency or execution path was added.
