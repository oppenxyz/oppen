# oppen MCP contract

Version: `0` (pre-release, unstable). The contract is versioned from the first public release; the envelope carries `contract_version`.

This document is the normative reference for every tool the gateway exposes. `docs/spec.md` section C is the specification it implements; where the two disagree, the spec wins and this file is wrong.

## Principles

- Deterministic JSON: stable key order, units in field names, pre-rounded values, timestamps no finer than the field needs.
- Every failure is typed. Retryability is part of the type.
- `reason` is required on every action and is treated as untrusted text.
- One cursor: `get_events(since)` over the ledger rowid. Cursor too old → `resync_required`.

## Tools (v1)

`get_state` · `get_meta` · `get_events` · `get_features` · `preflight` · `place` · `cancel` · `cancel_all` · `close_position` · `get_order_status` · `remember` · `recall` · `set_alert` · `get_alerts` · `cancel_alert` · `get_execution_report`

Every tool in that list is implemented.

## Pairing

A token is not an anonymous key to the gateway: it names **one agent bound to one venue account** (D1 as revised, `docs/decisions.md` C9). The operator creates the pairing — naming the agent, binding its container, assigning its guardrails — and the token is minted from that. An agent oppen has not been told about has no credential to present, which is what default-deny means here.

Every tool call resolves its identity from the token presented (C10), so one gateway serves every paired agent and each acts as itself: its own guardrails, its own container, its own events. Revoking a pairing closes its live sessions and leaves every other pairing untouched.

Pairing identity and revocation persist as authenticated operator records in the
network's existing audit chain (ES16). IDs carry network and issuance sequence;
only a SHA3-256 digest of the random bearer is retained. A bearer is revealed
only after issuance commits and its anchor is published. Revoked bindings remain
available for resting-order cancellation after restart. Pairing lifecycle rows
are operator-only and are not included in an agent's scoped event view.

One live owner holds the durable token cache. Ownership remains held by actual
tool tasks as well as HTTP sessions, and a network-mismatched store cannot serve
the gateway. Missing task authority fails closed. A revocation interrupts
asynchronous tool work but cannot retract already-admitted signing or exchange
work; an interrupted submission must be reconciled rather than blindly retried.

If a pairing mutation fails or panics, every session in that store is closed and
new authentication is denied until verified reopen. This is broader than a
successful single-pairing revoke. Failed durable publication is reported as an
error, not a promise that revocation survived a crash. Missing authentication
keys, invalid MACs, corrupt or redacted required records never fall back to an
empty credential store. Key provisioning, registry validation and pilot
activation remain separate operator steps.

Authentication fails closed while a durable pairing mutation holds the store
lock. Cancellation supervision retries on its next interval rather than
blocking the server's async workers or reporting delivery during contention.
Operator commands must run synchronous pairing persistence off async workers
and await its result; abandoning a waiter does not roll back a durable mutation.

## Signing Authority

A valid pairing does not itself authorize signing. The operator must separately
grant the agent's account route in the authenticated registry (ES17). Execution
requires agreement between the pairing's network/account, the verified route
and the exposure snapshot; orders additionally require a matching durable
reservation. Missing, retired, tampered or mismatched authority fails closed.
Engine route refusals use `guardrail_reject`; pairing/container mismatches may
return `unavailable`, and network mismatch may fail during authentication.
Creating a policy or importing legacy account metadata does not grant a route.

Every clearance carries the complete route and its grant revision. Immediately
before signing, Rust verifies that revision again and compares the actual loaded
key and wallet generation with the grant. The shared ledger guard and database
snapshot remain held through cryptographic signing. Cancellation and dead-man
cleanup also require route authority; retirement is not a cancel-only grant.
Cancel before retirement or use a separately approved operator recovery path.

Route records express local operator authorization, not proof of venue approval
or trading readiness. Account confirmation, wallet approval, authenticated policy
and pilot activation remain separate gates. Existing unresolved submissions keep
their historical payloads and reservations; migration never fabricates route
evidence to release them. Schema V6 requires a compatible reader on rollback.

Decision replay runs on a bounded worker that retains execution tracking and
pairing ownership until completion. Canceling or timing out its waiter cannot
resume that decision into signing, but does not undo evaluation audit, rate or
approval effects. HTTP shutdown remains responsive while a stalled decision
drains; the runtime is not fully stopped and cannot hand off ownership until
that drain completes. Existing synchronous signing is not made interruptible by
this mechanism. An interrupted submission still requires reconciliation.

## The result envelope

Every acting tool answers with one JSON object. `contract_version` first, then `status`, then the fields that status implies.

| `status` | Meaning | Fields |
|---|---|---|
| `resting` | On the book until it fills or is cancelled | `oid`, `cloid` |
| `filled` | Crossed on arrival; `avg_px` is the venue's own | `oid`, `cloid`, `filled_sz`, `avg_px` |
| `canceled` | A cancel or cancel-all landed | `requested`, `canceled`, `failed[]` |
| `pending_approval` | Item 28: nothing signed, a human decides | `approval_id`, `symbol`, `notional_usd`, `expires_at_ms` |
| `rejected` | A predicate refused before signing | `retryable`, `code`, and the code's own fields |

A rejection is a **successful** tool call, not a protocol error. It is the expected outcome of asking for something outside a limit, and returning an error there invites the blind retry that is exactly the wrong response.

```json
{"contract_version":0,"status":"resting","oid":42,"cloid":"0xaa"}
{"contract_version":0,"status":"filled","oid":43,"cloid":null,"filled_sz":"0.25","avg_px":"63999.5"}
{"contract_version":0,"status":"canceled","requested":3,"canceled":2,"failed":[{"oid":2,"cloid":"0xcc","venue_message":"Order was never placed, already canceled, or filled."}]}
{"contract_version":0,"status":"rejected","retryable":false,"code":"venue_reject","subtype":{"venue_rule":"min_notional","notional_usd":"4","minimum_usd":"10"}}
```

Decimals are strings, always. A price written as a JSON number is a price a different platform may round differently.

## Rejection codes

Carried on `status: "rejected"`. Nothing was signed and nothing reached the venue.

| `code` | `retryable` | Means | Carries |
|---|---|---|---|
| `guardrail_reject` | `false` | A guardrail predicate breached, or the engine could not establish that one was not | `refusal`: the predicate, the observed value and the limit |
| `venue_reject` | `false` | A rule the venue would have enforced, caught before a nonce was spent | `subtype`: `min_notional`, `price_decimals`, `size_decimals`, `delisted`, … |
| `rate_limited` | `true` | oppen's own budget, spent before the venue's (item 10) | `retry_after_ms`, `refusal` |
| `trading_paused` | `false` | The kill switch is engaged (item 26) | `refusal`, naming scope and reason |

`guardrail_reject` covers the fail-closed refusals too — a stale feed, a missing reference price, an unreconciled account. Those are **not** retryable even though the condition may pass on its own: telling an agent to retry into a degraded feed is how a quiet outage becomes a retry storm.

The optional operator-set `risk.max_open_exposure_usd` caps gross marked positions
plus opening resting orders across the container, including a proposed opening
order. Opposite orders do not net. Its `open_exposure` refusal carries
`observed_usd` and `limit_usd`; genuine reduce-only reductions retain their unwind
exemption. This is separate from per-symbol `max_position_usd`, leverage, and
cumulative execution or loss budgets.

An operator-authorized supervised pilot also enforces non-resetting cumulative
budgets before submission and inside the signer. A `pilot_budget` refusal carries
`metric`, `observed_usd` and `limit_usd`; metrics distinguish an order's notional,
executed turnover, executed-plus-reserved commitment, and realized loss. These
refusals are normal non-retryable `guardrail_reject` replies, including for closes.
Missing or contradictory budget evidence is `pilot_budget_unavailable` within an
`unevaluable` refusal. A fill awaiting its authoritative order linkage blocks new
orders until reconciliation proves that link; it does not erase consumption.
No MCP method authorizes, renews or resets a pilot.

The runtime retries cancellation of resting orders for a verified permanent
pilot stop or unavailable accounting, using the same bounded sweep as an
operator pause. Transient reconciliation alone does not trigger cancellation.
Cancellation cannot flatten positions or undo fills that race the stop. If
required authority history cannot be verified, pilot cancellation reports an
error instead of inventing a binding; a separate operator pause remains
available. The desktop's local stop status is not a cancellation receipt.

## Error codes

Protocol errors, carrying the taxonomy in the JSON-RPC error's `data` — `{contract_version, code, retryable, detail, cloid}` — rather than only in the message.

| `code` | `retryable` | Means |
|---|---|---|
| `venue_error` | `429` and `5xx` only | The venue saw the request and refused it. A nonce was spent. `detail` is the venue's own words, **display-only** — never branch on it |
| `timeout_unknown_outcome` | never | The request left the process and no answer came back. The order may be live |
| `unavailable` | `true` | oppen could not read what it needed. Nothing was signed |
| `invalid_params` | `false` | The caller's own input, refused as sent |

`timeout_unknown_outcome` is never retryable, and that is the whole content of the rule. The only safe move is `get_order_status` by the `cloid` the failed call returned — never a resend, which is how an agent doubles a position. `place` mints a cloid when the caller supplies none precisely so this move always exists.

Not yet constructible, so not yet in the taxonomy: `auth_expired`. The door refuses an unpaired or revoked agent before any tool runs, and agent-wallet expiry is not detected yet (roadmap P2, "wallet expiry warnings").

## Tools

### `get_meta(symbols?)`

Per-symbol trading rules. Read-only. Returns an array sorted by `asset_id`: `symbol`, `asset_id`, `size_decimals`, `price_decimals`, `max_leverage`, `min_notional_usd`, `funding_interval_hours`, `only_isolated`, `is_delisted`.

### `get_state()`

The account now: `contract_version`, `network`, `address`, `as_of_ms`, `feed_age_ms`, `feed`, `balances`, `margin_runway_h`, `loss_budget`, `positions`, `orders`. If `feed` is not `live` the data is stale and execution fails closed.

**Risk in σ-units.** Each position carries `liq_distance_frac` and `liq_distance_sigma` — the same distance as a fraction of the mark and in daily standard deviations. The second is the one to compare across symbols: a 4% gap is a different fact on BTC than on a coin that moves 20% a day. `carry_usd_per_day` is signed funding on that position over a day, **negative when the position pays**.

`margin_runway_h` is account-level, not per position: free margin is shared, so a per-position runway would divide the same margin among all of them and overstate each. It is absent when the book receives funding on net — there is no runway to run out of.

**The loss budget, before it fires.** `loss_budget` is one row per configured budget — `scope` (`agent` or `account`), `kind` (`daily` or `drawdown`), `consumed_usd`, `limit_usd`, `utilization_pct`, `remaining_usd`, `tripped`. It is the circuit breaker's own predicate read as a dial rather than as the step function it is on the order path, so you can slow down at 80% instead of finding the limit by hitting it. `remaining_usd` is the actionable number: dollars of loss left, and **zero is the trip** — a budget is exhausted at its limit, not past it.

Read the `account` rows. That budget is shared with every other container under the same operator, and a breach there stops **you**, not just the agent that spent it — so you can sit at 20% of your own daily budget and be one bad hour from a kill nothing you have been refused would have mentioned.

Absent rows mean the budget is not configured. `utilization_pct` is absent only when the limit is zero, where a ratio has no meaning; `tripped` answers for that case. On a profitable day `utilization_pct` is `0` while `consumed_usd` goes negative — that is the dollars you are up, and it does not buy extra budget: `remaining_usd` never exceeds `limit_usd`.

Any of these is absent when its input is: no liquidation price, no σ yet, or a symbol the venue has stopped quoting (which also cannot be traded — see above).

**An asset the venue has stopped quoting has no price here, and cannot be traded.** `markPx` keeps being published for a dead market — it is the last print, frozen — so oppen reads the venue's asset contexts and treats a null `midPx` as "not quoted", which is what it means. Those assets carry no liquidation distance and every order on them is refused for a missing reference price rather than sized against a number that can be an order of magnitude stale. This is 24% of the main dex, so it is the ordinary case and not an edge one.

**Two moments where that is expected rather than broken.** oppen walks a catch-up window at startup and again after every socket drop, and until that window returns the account is unreconciled and every order is refused — the window may hold fills nothing has seen, and sizing against a position oppen has mis-stated is the failure this exists to prevent. Both are seconds, not minutes. Poll `get_state` rather than retrying the order: the refusal names the condition, and it clears on its own.

### `set_alert(condition)`

Ask to be woken when a condition holds, instead of holding a session open and polling. Takes `{kind, symbol?, direction?, px?, hour_to_date_bps?}` and returns `{contract_version, alert_id, condition, armed_at_ms}`.

| `kind` | Fires when | Needs |
|---|---|---|
| `price_cross` | the venue's mark for `symbol` reaches `px` | `symbol`, `direction`, `px` |
| `fill` | a fill prints on this account | `symbol` optional; absent means any |
| `funding_rate` | the hour-to-date funding rate reaches `hour_to_date_bps` | `symbol`, `direction`, `hour_to_date_bps` |

`direction` is `above` or `below`, and both are inclusive at the level. `px` and `hour_to_date_bps` are decimal strings. **`hour_to_date_bps` is the rate for one hour in basis points, never an annualised APR.**

**It fires once.** A crossing that fired does not fire again on the next tick still past the level; re-arm it if you want it again. The firing arrives as an `alert` event in `get_events`, carrying both the condition you set and the value observed — so an agent reading it later does not have to re-read a feed that has moved.

**A wakeup is an event, not a push.** oppen cannot start your turn: nothing in the transport can. What this replaces is the session held open polling `get_state`, not the act of reading.

**Not available yet:** liquidation distance and feature thresholds, the other two conditions spec item 22 names. The first needs a position poll on a cadence nobody has chosen, the second needs `get_features`. They are absent rather than approximated, because an alert that cannot fire is worse than one that was refused — you would stop watching and wait for a wakeup nothing will send.

An alert on an asset the venue has stopped quoting never fires, for the reason such an asset cannot be traded (see `get_state` above).

### `get_features(symbol)`

Deterministic market features. Returns `{contract_version, symbol, as_of_ms, book, funding, vol}`.

**`book`** — `spread_bps`, `book_imbalance` (touch notional, positive means more resting on the bid), `micro_tilt_bps`, `bid_reach_bps`, `ask_reach_bps`, and `depth`: one entry per band in 10/25/50 bp, each `{band_bps, bid_usd, ask_usd, covers_band}`.

> **Read `covers_band` before using a depth figure.** The venue's default ladder does not reach these bands on most symbols — measured at a median of 13.18 bp across the top 50, and **2.40 bp on BTC**. When `covers_band` is false the figure is that side's *whole book*: a floor on the real depth, not the band's contents. `bid_reach_bps` and `ask_reach_bps` say how far the ladder actually went. On testnet the thin book does span the bands, so a test there will not show you this.

`micro_tilt_bps` is the Stoikov microprice's distance from the mid, sourced from the `bbo` channel and never from the depth ladder, which pushes too slowly to define it. It may be `null` on the first call for a symbol while its quote feed warms — call again. A one-sided quote yields `null` rather than zero.

**`funding`** — `hour_to_date_bps` is what has accrued **so far this hour**, not the hour's finished rate; `apr_pct` compounds it hourly, so it **reads low early in the hour** and `next_funding_s` tells you how far through you are. `predicted_apr_pct` is the venue's own forward number, annualised the same way, and `null` when the venue does not carry one. `next_funding_s` is derived from the clock: the venue's own `nextFundingTime` points into the past. `basis_bps` is the mark's distance from the oracle.

**`vol`** — `rv_1h_bps` (EWMA-Parkinson over the last hour of 1 m bars, as an hour's σ), `rv_24h_bps` (over the last day of 1 h bars, as a day's σ), and `vol_ratio` — the hour against what the day implies for one hour, so **1.0 means the last hour moved like a normal hour of this day**. `bars_1h` and `bars_24h` carry the sample behind each: a σ from four bars is a different claim from one over sixty. Any of the three is `null` when no bar carried a usable range.

A pack whose venue read failed comes back with its fields `null` rather than failing the call — the packs that did arrive are still true.

### `get_alerts()`

Everything you have armed or that has fired, newest first: `{contract_version, alerts[]}`, each `{alert_id, condition, armed_at_ms, fired_at_ms}`. A null `fired_at_ms` is still watching.

You are amnesiac across sessions, so this is how you find out what a previous you asked to be woken for, and where the `alert_id` for `cancel_alert` comes from. Only your own alerts; another agent's are not returned and their ids are not discoverable.

### `cancel_alert(alert_id)`

Stop watching for a condition you armed. Returns `{contract_version, alert_id, cancelled}`.

`cancelled: false` means the alert is not watching on your behalf — an id you do not own, one that never existed, or one that has already fired. **These are deliberately indistinguishable**, so nothing can probe for another agent's alert ids. It is not an error: you wanted the alert not to be watching, and it is not. Use `get_alerts` when you need to know which case you are in.

Cancelling frees a slot against the per-agent cap and releases the market feed the alert was holding. A cancelled alert is gone rather than kept as history — nothing records an arming in the first place, only a firing.

### `get_events(since_cursor?, limit?)`

The durable record. Returns `{contract_version, events[], next_cursor, resync_required, head_seq}`; pass `next_cursor` back to continue. `limit` is clamped to the ledger's page cap (1000) rather than refused.

Each event carries `seq`, `ts_ms`, `kind`, `agent_id`, `payload`, `payload_hash`, `prev_hash`, `hash`, and the snapshot reference where one exists. `kind` is item 18's taxonomy: `order_intent`, `agent_decision`, `refusal`, `fill`, `order_state_change`, `submission_started`, `submission_resolved`, `pilot_authorized`, `pilot_halted`, `operator_action`, `approval_decision`, `kill_switch_changed`, `guardrail_trip`, `ws_disconnected`, `ws_reconnected`, `alert`, `agent_wallet_expiry_warning`, `payload_redacted`. A pilot stop caused by a new fill is embedded in that fill's `pilot_stop` payload; independent evidence faults use `pilot_halted`.

**`resync_required: true` means the cursor cannot be served** — it is older than what the ledger retains, or ahead of the head, which is what a mainnet cursor presented to a testnet file looks like (R4). Discard local state and re-read from `get_state`. Never treat it as an empty gap: item 18 makes this explicit precisely so a hole is never silent.

**Scope.** An agent reads its own events plus the account-wide ones no agent owns — the kill switch, feed drops, alerts. Another agent's intents and `reason` strings are not returned (`docs/decisions.md` C6). `next_cursor` still advances past rows that were filtered out, so a page can be empty without the cursor stalling; compare it against `head_seq` to know how far behind you are.

Operator surfaces read the whole chain unscoped — the activity stream and the audit export are the human's view.

### `preflight(symbol, is_buy, size, limit_px, reduce_only?)`

What the order would do, without doing it. Costs **no order-rate token**, draws no request against the account budget, mints no approval proposal and writes no ledger row — item 20 says "without executing", and answering must not be a thing that happened.

No `reason`. Item 19 requires one on every *action*; this is a question, and requiring a justification for asking only trains an agent to write a placeholder.

Returns `{contract_version, symbol, is_buy, notional_usd, guardrail, book, book_as_of_ms, margin, post_fill, feed}`:

- **`guardrail`** — `{would_clear, utilization, refusal}` from the engine's own predicates, run in the same order against the same state. Not a restatement: a second copy of the guardrails is the thing invariant 1 exists to prevent. `refusal` names the predicate, the observed value and the limit, exactly as `place` would. On `approval_required` the `approval_id` is empty — no proposal was minted, because nothing was asked for.
- **`book`** — a live walk for this exact size: `top_px`, `avg_px`, `filled_sz`, `exhausts_book`, `slip_bps`, and `max_size_usd_within_bps` for 5/10/25 bps. Slippage is measured from the touch, not the mid, so it is the cost this order causes rather than the spread it pays. The band limits are on the *average* fill, so a level past the band still contributes the part that fits.
- **`margin`** — `initial_margin_usd` at the asset's own maximum leverage (the least the venue could ask), against `withdrawable_usd`. The operator's leverage cap is lower or equal and is carried by the guardrail verdict.
- **`post_fill`** — equity and margin used after this order fills.

**A clear verdict is not a promise.** The book is a snapshot the venue has already moved past, nothing reserves depth, and the rate token this did not spend may be gone by the time the order is sent. `exhausts_book: true` means the resting depth could not cover the size at all.

**The vol-scaled cap, if your operator set one.** When `max_risk_usd` is configured, the position is bounded by `max_risk_usd / (2 × σ_day × vol_scale)` as well as by the fixed `max_position_usd` — the tighter of the two binds, and `utilization.vol_scaled_position_pct` says where you sit against it. It is absent when no such cap is set, which is the default.

The point is that one budget means different sizes on different markets: $5 of risk buys $125 of position on something that moves 2% a day and $62.50 on something that moves 4%. So a refusal on a volatile symbol is not the venue being arbitrary — trading something calmer, or asking your operator to raise `max_risk_usd`, are different remedies and the refusal names which knob it was. `vol_scaled_position_notional` and `position_notional` are separate refusals for exactly that reason.

**`vol_scale` is what the last hour is doing, and it only ever tightens.** `σ_day` is measured over twenty-four hourly bars, so a market that started moving an hour ago has barely shifted it — one bar in twenty-four — and a cap derived from it alone stays wide for most of a day after the market changed. `vol_scale` is `max(1, vol_ratio)`, the same realised-volatility measurement taken over the last hour against what the day implies for one hour. A quiet hour does **not** widen your cap: a sixty-bar sample is not evidence to loosen a guardrail with, so the scale floors at one, and so does an hour nobody could measure.

The refusal reports `sigma_day_pct` and `vol_scale` separately rather than multiplied together, because they call for different responses. A coin with 6% daily volatility is an asset fact and will still be true tomorrow; a coin with 2% daily volatility moving at `vol_scale: 3` is a market that is busy right now and will likely not be in an hour. Waiting is a remedy for one and not the other.

**This cap moves, and trimming into it is always allowed.** Volatility doubles and the cap halves, so a position that was inside it can be over it with you having done nothing. Any order that leaves you holding *less* than before clears regardless — you never have to exit all at once to get back under. Adding to the position is what the cap refuses.

If the volatility cannot be measured, an order on that symbol is **refused**, not sized against a guess: `missing_volatility` names the symbol. A configured cap that cannot be computed is a cap that is not enforced. That applies to `σ_day`, which is the cap's denominator; an unmeasurable `vol_ratio` is not a refusal, because the cap still computes and only its tightening is lost — you get `vol_scale: 1` and the size the day alone allows.

**Not included: estimated fees.** Item 20 names them; oppen has no fee schedule yet, and guessing a tier would be worse than omitting one. It needs a `userFees` read audited against the live API, which is its own change.

### `get_execution_report(hours?)`

What your execution actually cost, from your own chained fills. Default window 24 hours.

Returns `{contract_version, from_ms, to_ms, scored_fills, unscored_fills, slippage, all, costs, by_symbol}`.

**`slip_bps` is measured against the arrival mid** — the price the guardrails measured your order against at the instant the decision was taken, before anything was sent. Positive is cost; **negative is price improvement and is reported as such**, not floored at zero. A mean that floored would be biased upward by every fill that went well.

**Every statistic carries its `n`, and every one carries a baseline.** The baseline is the maker/taker split: `slippage.taker` against `slippage.maker`, per symbol and overall. That is what crossing the spread cost you, measured against what resting earned — the comparison you can act on. A side is absent when nothing landed on it, which is itself the finding if you only ever cross.

`p90_bps` is nearest-rank, so every percentile is a slippage that really happened and can be found in the ledger beside the report.

**Read `unscored_fills` before believing the rest.** A fill with no arrival mid cannot be scored — an external trade, a manual ticket, or an order placed before arrival mids were stamped. The count is published rather than dropped, because a clean mean over half your trading would be worse than no report.

**`costs` keeps price and fees apart** — `closed_pnl_usd`, `fees_usd`, `fee_bps_of_notional` — because they are fixed by different decisions and a net number hides which one is the problem.

**Not included: funding.** The decomposition spec F asks for is price / funding / fees, and funding is the missing third. It needs a `userFunding` read that does not exist in the client yet and would have to be audited against the live API before being trusted — the same treatment the fee estimate above gets. An `n` of four is not evidence: the report tells you the sample size precisely so you do not have to guess whether it is.

### `remember(key, value)` · `recall(key?)`

The per-agent scratchpad. Agents are amnesiac across sessions; this is what survives.

`remember` replaces whatever was under `key` — the journal holds what is **true now**, and `get_events` holds what happened. `recall` returns `{contract_version, notes[]}` with one note or all of them, newest first; a key never written comes back as an empty list rather than an error, so a caller reads one field either way.

Notes are **yours**: keyed by the agent your token names, like everything else, and no tool returns another agent's. They live in a separate per-network file, not in the ledger — the ledger is append-only and hash-chained, and a rewritable table does not belong in the file whose point is that nothing is rewritten. Per-network because a note reasoned from testnet prices is not a mainnet note.

Bounded: 8 KiB a note, 256 bytes a key, 1000 notes an agent. A write that costs no rate token and is kept forever is unlimited free disk otherwise — the same hole the `reason` length bound closes. A full journal still accepts corrections to keys it already holds; being full must not also mean being stuck.

### `place(symbol, is_buy, size, reason, order_type…, reduce_only?, cloid?)`

`reason` is required (item 19) and is untrusted text (item 30). `cloid` is minted when absent and comes back on every result.

`order_type` selects one of item 12's three single-order forms, and carries its own fields — a stop with no trigger price, or a market order with a limit price, is not spellable rather than refused one case at a time:

| `order_type` | Fields | Behaviour |
|---|---|---|
| `limit` | `limit_px`, `tif?` | Rests at `limit_px`. `tif` is `gtc` (default), `ioc` or `alo` |
| `market` | — | Crosses now, as an IOC priced from the mid at your configured max slippage |
| `stop_market` | `trigger_px`, `tpsl?` | Rests off-book until `trigger_px`, then crosses. `tpsl` is `sl` (default) or `tp` |

**Hyperliquid has no market order type.** `market` and `stop_market` are IOCs priced through the book, and the bound is the **operator's** `max_slippage_bps`, not yours (item 24, D3). Prices are rounded toward the reference, so pricing at that bound cannot be refused for exceeding it (C5).

A `stop_market` is priced from its **trigger**, not today's mid — that is where the book will be when it fills, and it is the reference the guardrail engine measures the slippage cap against.

`tif` defaults to `gtc`. An order that silently became `ioc` would be cancelled instead of working, which is the expensive direction to guess wrong in.

The guardrail check runs immediately before signing, in Rust, on the single path to the signer. There is no branch around it.

**Not yet: attached TP/SL and batched actions.** Item 12 names `positionTpsl` and batching; both send several orders in one action, and the guardrail engine clears one intent at a time. A batch that partially clears must not partially send, and deciding what that means is its own change.

### `cancel(oid? | cloid?, reason)`

One resting order. Supply `oid` or `cloid`. Cancels are risk-reducing: they clear while the kill switch is engaged and they cost no order-rate token. An order that is already gone is reported as `canceled` with that cancel in `failed[]`, not as an error — the caller wanted it gone and it is gone.

### `cancel_all(symbol?, reason)`

Every resting order, or every one on a symbol. Partial success is normal, so `failed[]` itemises what the venue would not take, paired positionally with what was sent. Nothing resting answers `canceled` with `requested: 0`.

### `close_position(symbol, reason)`

Flatten the open position on one symbol with a **reduce-only IOC** order, sized at the position's own magnitude and priced at the agent's configured `max_slippage_bps`.

It cannot open or flip a position. The size is the position's, not the caller's, and the order is reduce-only — so a fill that would cross through flat is refused by the venue rather than reversed. Closing a long sells; closing a short buys.

The slippage bound is the **operator's**, read from the guardrail engine rather than taken from the caller: item 24 and D3 make slippage operator-set, and an agent that could widen it to close could widen it to open. The price is rounded *toward the mid*, so pricing at the limit cannot be refused for exceeding that limit by a rounding step nobody chose.

Nothing open answers `canceled` with `requested: 0` — the caller wanted the symbol flat and it is flat. No mid for the symbol answers `unavailable`: there is no price to send, and a close is never sent at a guessed one.

A close is not privileged. It passes the same gate as `place`, and is refused when the account is unreconciled or the feed is stale exactly as an opening order is.

### `get_order_status(oid? | cloid?)`

The venue's own status for one order. `{contract_version, known, ...}`; `known: false` means the venue never saw it, which after a `timeout_unknown_outcome` is the answer that makes it safe to place again. Anything else means it did see it.
