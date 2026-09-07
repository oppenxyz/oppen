# oppen roadmap

Everything mapped so far, from the v1 MVP through the versions after it. Numbers in brackets are spec items in [docs/spec.md](docs/spec.md); D1–D8 are the settled architecture decisions there and are not re-opened here. Each phase has a gate: the phase is done when the gate passes on testnet, not on mocks.

Legend: `[x]` shipped on `main` or in an open PR · `[ ]` not started · **gate** = what proves the phase.

## Execution audit follow-up (2026-09-07)

The component checkboxes below are not production-readiness claims. The assembled
execution path failed review despite its passing unit tests. Follow this order
before expanding features:

1. **Baseline:** repositories moved to `oppenxyz` with private visibility and
   history preserved; local remotes updated. Corrected baseline `bb623d8` passes
   every CI job in draft PR #40; the original `main` still needs that fix merged.
2. **Guarded execution:** correct the production ledger adapter, independent
   buy/sell exposure, per-container submission serialization, signing-time policy
   checks, and cancellation delivery with retries. Local regressions now cover
   these repairs, including the real SQLite sink and test signer, clipped fill
   permutations, dropped requests, revocation, and cancellation failures. The
   complete fixture-exchange lifecycle and explicitly authorized testnet run
   remain open gates.
3. **Operator supervision:** desktop MCP lifecycle, pairing/revocation, real
   positions and orders, policy editing, approval decisions, and a working halt.
4. **Recovery:** durable unknown-submission reconciliation across restarts,
   reconnect/sleep/network races, feature freshness, config HMAC, and confirmed
   dead-man behavior. A timeout or missing order status is not permission to retry.
5. **Release gate:** measured first-run onboarding and a small supervised testnet
   pilot. No live-trading readiness claim until these gates pass.

Work in progress is not a completed phase. The P1 signed-order, live-disconnect,
and supervised-agent gates remain open.

Tracked gates: [execution #41](https://github.com/oppenxyz/oppen/issues/41),
[supervision #42](https://github.com/oppenxyz/oppen/issues/42),
[recovery #43](https://github.com/oppenxyz/oppen/issues/43).

Status as of 2026-09-07: P0 and the P1 code are on `main`; the P1 gate is waiting on a funded testnet agent wallet. P2, P3, P4 and P6 code has been landing since 2026-09-04 (PRs #9–#34). **The P2 and P3 boxes were audited against the code on 2026-09-07** and are now accurate: the ws pool, the hash-chained ledger, the event taxonomy, the keychain, the per-agent guardrails, the loss breaker, the kill switch and invariant 1's property test are ticked with the module that implements each. Four bullets are deliberately still open and say what landed and what did not — the rate-budget manager (no batching), the container registry (no venue or container-kind keying), the guardrail-config HMAC (the key exists, nothing uses it) and the dead-man's switch (no daily trigger budget). **Spec F's v1 quant layer is complete** (PRs #28–#32), and the vol-scaled cap's σ-staleness caveat is closed down to a five-minute cache residue (B1–B4). Six feature specs are written, two more were commissioned by the 2026-09-04 venue audit ([onboarding.md](docs/specs/onboarding.md), [venue-containers.md](docs/specs/venue-containers.md)), and one hundred and twenty-nine product decisions are recorded in [docs/decisions.md](docs/decisions.md).

**D1 was revised on 2026-09-04.** The unit of isolation is one venue *account* per agent — a sub-account where the venue grants one, a top-level account where it does not. Hyperliquid gates sub-accounts behind $100,000 of protocol-enforced traded volume, on testnet as well as mainnet, so v1 provisions one top-level account per agent. Where this roadmap used "sub-account" to mean the unit of isolation, it now says **container**; where it names the venue's own `subAccounts` endpoint, it still means a sub-account. Reasoning: [decisions.md](docs/decisions.md) V1–V6.

---

## v1 · MVP — Hyperliquid only, agents via MCP, human supervises

### P0 · Scaffold — done

- [x] Rust workspace: `oppen-hl`, `oppen-core`, `oppen-mcp`, Tauri 2 desktop app with the Vue 3 console shell [1]
- [x] CI: fmt, clippy `-D warnings`, tests, `cargo deny`, web build; actions pinned by SHA, `--ignore-scripts` [1]
- [x] Open core (Apache-2.0 `crates/*`, commercial `apps/desktop`) + CLA gate, `NOTICE`, `AGENTS.md`, `skills/oppen` stub [23, L1–L7]
- [x] Testnet-default `Network` type, every network constant selected by it [13, D4]
- [x] Design system in `docs/design/`, ASCII primitives and the shared 90 ms motion clock in the app shell [36]
- [x] Ink ladder raised so labels and rules clear WCAG AA
- **Gate:** app launches; CI must be re-established on the corrected baseline.

### P1 · Hyperliquid protocol crate — code merged, gate pending

- [x] L1 signing: msgpack action hash, phantom agent, k256 signer, 40 official SDK vectors, mutation-tested [6] — PR #1
- [x] `WireFloat` normalization so a trailing zero can never reach the hash [6] — PR #1
- [x] EIP-712 typed data for the WalletConnect ceremony: `approveAgent`, `approveBuilderFee` [D5] — PR #1
- [x] Exchange envelope + per-signer nonce allocator [7] — PR #1
- [x] Info client: meta, asset contexts, book with `nSigFigs`, candles, mids, predicted fundings, clearinghouse state, open orders, fills by time, order status by cloid, rate limit, sub-accounts [11] — PR #2
- [x] Exchange client and typed status parsing (resting / filled / error / success) [12] — PR #2
- [x] Universe + asset validation: 5 significant figures, `6 − szDecimals`, `szDecimals`, $10 minimum notional, slippage price [8] — PR #2; `slippage_price_bounded`, which rounds toward the mid so a price *at* a slippage limit cannot be refused *for* it — PR #17
- [x] `OrderSpec` → validated `OrderWire` [8, 12] — PR #2
- [x] `examples/testnet_order.rs`: place ALO, confirm by cloid, cancel by cloid, `expiresAfter` probe — PR #2
- [ ] Rate-budget manager: batch orders/cancels, reserve headroom for risk-reducing actions, throttle before the venue does [10] — **two of three landed** with the guardrail engine: `GlobalRateBudget` reserves headroom (`spend_global` refuses an order once the remainder would fall to the reserve, while a cancel still goes through) and throttles before the venue does. **Batching has not**, and is the same blocker as item 12's batched actions below
- [ ] Testnet answers to the open questions in `docs/hl-signing.md`: `expiresAfter` encoding, accepted `signatureChainId` values, agent-wallet no-withdraw scope
- **Gate:** signed testnet order via the CLI (needs a funded testnet agent key)

### P2 · WS pool, reconcile, event ledger

- [x] Shared socket pool per network, client pings, per-IP caps [9] — `oppen-hl::ws`: `WsPool` with `max_connections`, `MAX_SUBSCRIPTIONS_PER_IP`, client pings and black-holed-socket detection
- [x] Subscriptions: book, trades, candles, mark/oracle, user fills, order updates, funding context [9, 11] — all seven, as `l2Book`, `trades`, `candle`, `activeAssetCtx` (mark, oracle **and** funding in one frame), `userFills`, `orderUpdates`, plus `bbo` for the microprice (fair-value.md §14.4 correction 4)
- [x] Reconnect state machine: a drop un-reconciles, a reconnect asks for the gap, and only a returned reconcile clears the flag; live fills recorded on arrival and deduped by `tid` [9] — `oppen-core::feed`
- [x] Pumping a live `WsPool` into that session, and the backfill calls it asks for [9] — `oppen-core::feed::pump`, [decisions.md](docs/decisions.md) F1–F4. A drop now writes a `feed_gaps` row per subscription and a reconnect closes it, so `reconcile.rs` finally works a table something other than its own tests fills. Still driven by a fixture rather than a live socket in tests; the gate below wants a real 30 s outage
- [x] Append-only, hash-chained SQLite ledger: the one source for `get_events`, the activity stream and the audit export [D6, 29] — `oppen-core::ledger`, with `verify`, anchors, redaction tombstones and CSV/JSONL export
- [x] Event taxonomy: fills, order transitions, rejections, guardrail trips, approvals, kill-switch changes, wallet expiry warnings, WS state, alerts [18] — all nine, plus `OrderIntent`, `AgentDecision`, `OperatorAction` and `PayloadRedacted`
- [x] Keys in the OS keychain, read only from `oppen-hl`; keychain hand-off zeroizes the hex string [2] — `oppen-core::keys`: `KeychainKeyStore`, agent records, rotation, permanent address retirement, D-b expiry states, and `SecretText`/`HmacKey` zeroizing on drop
- [ ] Container registry: one venue account per agent — a **top-level Hyperliquid account** in v1, keyed by venue, address and container kind so V5's sub-account upgrade needs no migration of ledger rows — plus the `manual · external` bucket for outside fills [D1 as revised, decisions.md V1–V2, V5] — **the store and the bucket landed**, as `ledger::SubAccount` (address, owner, `recorded`, `provisioned_by_oppen`) and `reconcile`'s `external` / `manual` attribution. **The keying has not**: there is no `venue` or container-kind column, which is precisely the part that exists so V5's upgrade needs no migration — [venue-containers.md](docs/specs/venue-containers.md)
- **Gate:** zero fills lost across a 30 s disconnect — **held against a fixture, not a socket.** `no_fill_is_lost_across_a_disconnect` drives a real outage through the feed session and the ledger, but the venue is a test double; the gate wants a live 30 s drop

### P3 · Guardrails, kill switch, dead-man

- [x] Per-agent guardrails: symbol allowlist, max position, notional cap, order-rate cap, reduce-only mode, max slippage, leverage cap; leverage and margin mode operator-set [24, D3] — all eight on `AgentGuardrails`, with D-c's near-zero defaults, plus spec F's `max_risk_usd` vol-scaled cap (N1–N5), whose σ is corrected by the last hour's realised vol so a twenty-four-bar statistic cannot leave the cap wide through a regime change (B1–B4)
- [ ] Guardrail config HMAC-checked with a keychain key [3] — **the key exists and nothing uses it.** `keys::HmacKey` has `sign`/`verify`, zeroizes on drop and redacts its own `Debug`, but no call site outside its definition: the config store neither writes a tag nor checks one. The primitive is done; the wiring is the work
- [x] Loss circuit breaker: max daily loss / drawdown per agent and account-wide trips the kill switch [25] — `guardrail::breaker`, exhausted **at** the limit rather than past it, plus spec F's continuous gauge over the same predicate (L1–L4)
- [ ] Kill switch per agent and global: pauses new orders, cancels resting, persists across restart, typed `trading_paused` [26] — core pause predicates exist; runtime cancellation delivery/retry and restart behavior must pass the assembled gate before this is complete
- [ ] Dead-man's switch: `scheduleCancel` armed while any agent is active; quit dialog with cancel-all when positions are open [27] — **it is a daily budget, not a standing net**: minimum 5 s ahead, maximum 10 triggers per day resetting 00:00 UTC, so the arming policy is deliberate and the remaining count is shown in the risk console [decisions.md O1] — **the lead-time half landed** in `guardrail::deadman` (`DEAD_MAN_MIN_LEAD_MS`, a 60 s arm refreshed at 20 s remaining) and `clear_schedule_cancel` clears it. **The daily budget has not**: nothing counts triggers or resets at 00:00 UTC, so O1's central claim — that this is a budget and not a standing net — is the part still to build
- [x] Property test proving there is no signer path without a guardrail check [invariant 1] — `no_input_produces_a_signable_value_without_passing_every_predicate`, 20,000 fuzzed cases with a vacuity guard, each cleared case re-derived predicate by predicate in `verify_every_predicate`
- **Gate:** no signer path without a guardrail check — **met.** `no_input_produces_a_signable_value_without_passing_every_predicate` searches 20,000 fuzzed configurations and re-derives every predicate on each cleared case; `sign_cleared` takes a `Cleared` whose only constructor is the success branch of `decide`

### P4 · MCP gateway

- [x] Streamable HTTP on loopback only: `Origin`/`Host` validation, bearer on every request, constant-time compare, revocation closes live sessions [14, D2] — PR #11, #12. The on/off toggle is an operator surface and waits on P5
- [x] Default-deny pairing in the crates: a token binds one named agent to one container, and every tool resolves its identity from the token presented [15] — [decisions.md](docs/decisions.md) C9–C10
- [ ] The approve dialog itself, and assigning guardrails from it [15] — operator surface, waits on P5
- [x] `get_state`: versioned deterministic envelope, staleness flags, positions with liq distance, orders, balances [16] — PR #13. Time-since-last-action, funding, guardrail utilization, pending proposals, kill state and rate budget are not in the envelope yet
- [x] `get_meta` [17] — PR #12
- [x] `get_events(since_cursor)` with `resync_required` [18] — scoped to the calling agent, [decisions.md](docs/decisions.md) C6
- [x] `place`, `cancel`, `cancel_all`, `close_position` with required `reason`; synchronous result contract; `get_order_status(cloid|oid)` [19] — PR #14, #17
- [x] Typed error taxonomy with retryability: `guardrail_reject`, `venue_reject{…}`, `venue_error`, `rate_limited`, `timeout_unknown_outcome`, `trading_paused`, `pending_approval` [19] — PR #17, [decisions.md](docs/decisions.md) C1–C5. `auth_expired` is omitted until something constructs it: the door refuses an unpaired agent before a tool runs, and wallet expiry is a P2 item
- [x] `preflight(order)`: margin, live book walk, guardrail verdict, post-fill exposure, `max_size_usd_within_{5,10,25}bps` [20]
- [ ] `preflight` estimated fees [20] — needs a `userFees` read audited against the live API; a guessed fee tier is worse than an absent one
- [x] `remember` / `recall` journal [21]
- [x] `set_alert(condition)`, with `get_alerts` and `cancel_alert` [22] — `oppen-core::alert`, [decisions.md](docs/decisions.md) G1–G6. Price cross, fill and funding rate; liquidation distance and feature thresholds deferred with named blockers, and the OS-notification half is P5
- [x] Market = slippage-bounded IOC, limit GTC/IOC/ALO, stop-market, reduce-only, cloid on everything [12]
- [ ] Attached TP/SL with `positionTpsl`, and batched actions [12] — several orders in one action, and the engine clears one intent at a time; a batch that partially clears must not partially send
- [ ] Builder code attached by default via `OPPEN_BUILDER_ADDRESS`; missing approval prompts the ceremony, never drops the order path [5, D7]
- **Gate:** `claude mcp add` → paired → guarded testnet order. This is the demoable loop.

### P5 · Operator console

The gate was cut from "parity with the design" to the named list below on
2026-09-03 — see [decisions.md](docs/decisions.md) P1. Parity is a judgement, not
a test, and an ungated judgement resolves as schedule drift.

**In the gate:**

- [x] **Market data in the console** [30, 31] — [decisions.md](docs/decisions.md) Z1–Z4. `oppen-core::market` projects the rail and a per-symbol snapshot; TradeView's Markets, strip, Book and Features panels read them. A bookless asset is dimmed rather than hidden and a market with no mid keeps its row, because `allMids` answers for exactly those with a frozen print (H1). `micro_tilt_bps` is still absent, but no longer for the reason given here: the console now subscribes `bbo` (see the live feed line below), so the input exists and only the computation is unbuilt
- [ ] Activity stream with rejection explainability: guardrail versus venue versus auth, attempted versus limit, inline link to edit [31]
- [ ] Agents / Control: roster with real PnL, last-seen, idle-with-open-position alert, guardrail utilization; policy panel; approvals queue; risk console with exposure, rate budget, feed health, kill switches [32]
- [ ] Manual escape hatch drawer; manual actions land as `manual · external` [33]
- [x] **Staleness: per-feed status, last-tick timestamps, stale overlay** [34] — [decisions.md](docs/decisions.md) W1, W4. `account_state` reports the socket's own `last_tick_ms` instead of the constant `None` it carried since P5 began; the market feed's indicator is owned by the socket rather than by the account poll, because the two now have different clocks, and a one-second timer ages it to `stale` after five seconds of silence. **Execution failing closed during a disconnect is not in this line** — the console has no order path yet, and the gateway already fails closed from `FeedSession`
- [ ] Persistent MAINNET/TESTNET badge; boot sequence bound to real state [36, D4]
- [ ] Agent `reason` strings rendered as inert plain text, labelled agent-authored [30]
- [x] **ASCII candle renderer** promoted from the marketing site: real X and Y axes on nice numbers at the asset's own precision, `--up` / `--down` colour with the glyph as a redundant channel — [charts.md](docs/specs/charts.md) §2, [decisions.md](docs/decisions.md) E1–E4. `oppen-core::market::chart` splits the venue's rows into closed buckets and the one still forming; `CandleChart.vue` measures its grid and maps the renderer's ink codes to tokens. **Native intervals only** — `1m 5m 15m 1h 4h 1d`; the resampler exists in `oppen-core::candles` and nothing calls it yet, which is the v1.5 arbitrary-intervals line
- [x] **The console's own live feed** [31, 34] — [decisions.md](docs/decisions.md) W1–W6. Five channels for the selected symbol: `activeAssetCtx` for the strip, `bbo` for the spread, `l2Book` for the ladder, `trades` for the bar as it forms, and `candle` to reconcile that bar against the venue's own aggregation. **The tape drives the chart, not the candle channel** — measured on testnet BTC at eight candle frames a minute with a seventeen-second tail carrying none (W2). The rail keeps a 10 s poll because no channel answers for the whole universe at once (W3). Proved by `apps/desktop/src-tauri/tests/live_feed.rs`, which opens a real socket and asserts each channel delivered separately; it is not a CI gate because it needs the venue
- **Gate:** every surface above renders correctly, and the stale overlay appears on socket loss

**Deferred to v1.1:**

- [ ] Agent fill marks on the chart, with reasons shown in the stream rather than in the plot [31, decisions.md P4]
- [ ] Follow-agent toggle
- [ ] OS notifications by severity [35]
- [ ] Designed empty state for every panel [4]
- [ ] Full parity with `docs/design/`

### P6 · Quant features — features, not signals

- [x] `get_features(symbol)`: `spread_bps`, `depth_usd_{bid,ask}_{10,25,50}bps`, `book_imbalance`, `micro_tilt_bps`; funding pack (`funding_apr_pct`, predicted, `next_funding_s`, `basis_bps`); vol pack (EWMA-Parkinson `rv_1h_bps`, `rv_24h_bps`, `vol_ratio`) — `oppen-core::features`, [decisions.md](docs/decisions.md) J1–J4. Each depth band carries whether the venue's ladder actually reached it (§14.5)
- [x] Position risk in σ-units: `liq_distance_sigma`, `margin_runway_h`, `carry_usd_per_day` in every snapshot — [decisions.md](docs/decisions.md) K1–K4. Filled by `get_state`, never by the signing path; `margin_runway_h` is account-level because free margin is shared
- [x] Loss-budget utilization % as a continuous gauge before the breaker — [decisions.md](docs/decisions.md) L1–L4. The breaker's own predicate read as a dial, both budget kinds and both scopes, in `get_state`; the same pass gave `Utilization` the `drawdown_pct` the breaker had always fired on and the block had never reported
- [x] Vol-scaled notional cap guardrail option: `effective_cap = risk_budget / (2σ_day)` — [decisions.md](docs/decisions.md) N1–N5. `risk.max_risk_usd` in dollars, off by default and only ever tightening the fixed cap; σ rides on `MarketRef` so the engine stays a pure function of its inputs, and a cap that cannot be computed refuses
- [x] TCA foundation: `arrival_mid` on every order (it was already the clearance's `reference_px`), `slip_bps` per fill stamped against it, `get_execution_report` with `n=` and a maker/taker baseline on every stat — [decisions.md](docs/decisions.md) Q1–Q5. PnL decomposition is price and fees; **funding is not**, and needs a `userFunding` read audited against the live API
- **Gate:** cross-checked against hand computation

### P7 · Approval mode, skill, release

- [ ] Approval mode, built last: `pending_approval` with TTL, re-priced at approval time with drift shown, typed approved/rejected/expired events, visible in `get_state` [28]
- [x] **The console can see the keychain** [2, 4] — [decisions.md](docs/decisions.md) X1–X5. `KeyStore::reachable`, a read-only probe, behind one Tauri command: whether the store answers, never what is in it. Turns the tracker's first milestone into a real check and makes a locked keychain visible instead of surfacing as an unrelated failure three steps later
- [x] **Guided walkthrough and setup tracker** [4] — [decisions.md](docs/decisions.md) W1–W4. A spotlight pass over every screen that points at the live controls and names what each is for, plus a milestone tracker toward a first paired agent. The tracker reports a step it cannot check as **unverifiable** with the missing read named, never as merely pending, and counts only the checkable ones
- [ ] First-run onboarding: testnet default → one container + agent wallet per agent (`usdSend` to fund it, `approveAgent` to authorise the agent wallet, `approveBuilderFee` for the builder code — every signature in the user's own wallet, never a container key in the app) → pair first agent with the `claude mcp add` snippet and a connection test [4, D5, decisions.md V2, O7] — [onboarding.md](docs/specs/onboarding.md)
- [ ] Sub-account path offered as an attempt, never as a precondition: `userRateLimit` returns `cumVlm`, so oppen may show distance to the gate as a labelled estimate, but nothing documents that the gate reads that counter or whether it is lifetime or windowed — so oppen still tries `createSubAccount` and classifies the refusal, and `Required:` / `Traded:` are displayed, never branched on [decisions.md V5, O3]
- [ ] `AGENTS.md` and the `skills/oppen` Claude Code skill written for real [23]
- [ ] Threat model finalized against the shipped code [2]
- [ ] Release builds: ad-hoc dmg + unsigned AppImage first; provenance attestations and checksums [1]
- **Gate:** fresh machine to a testnet trade in 10 minutes — measured from a wallet that already holds testnet USDC. The faucet pays 1,000 mock USDC only to an address that has previously deposited on **mainnet**, which is outside oppen and outside the ten minutes [decisions.md O2]

### Cut from v1 — decided, do not re-add

`get_chart_image` · attention tools (`focus_symbol`, `open_panel`) · stdio transport · agent-writable leverage / margin mode · HIP-3 dexes · `modify` tool and stop-limit · flatten-all coupled to the kill switch · in-app agent runtimes and replay · the 3-venue router from the design mock.

---

## Specified but not scheduled

These have written specs in [docs/specs/](docs/specs/). Most slot into the
versions below; the two written on 2026-09-04 are already scheduled inside v1 and
are listed here so the index is complete. A spec is not a commitment; each carries
open decisions that need an answer before it starts.

| Spec | Feature | Target |
|---|---|---|
| [onboarding.md](docs/specs/onboarding.md) | First-run ceremony: container per agent, agent wallet, builder fee, pairing | v1 · P7 |
| [venue-containers.md](docs/specs/venue-containers.md) | The container model and what Hyperliquid, Aster and Lighter each grant | v1 · P2 (model) / v2 (Aster, Lighter) |
| [workflows.md](docs/specs/workflows.md) | Trading workflows, triggers, schedulers, agent profiles | v1.5 / v2 |
| [history.md](docs/specs/history.md) | Durable trading history in the Portfolio tab | v1.1 |
| [charts.md](docs/specs/charts.md) | ASCII candles with real axes, arbitrary intervals, line chart, Quantoppen | v1 / v1.1 |
| [fair-value.md](docs/specs/fair-value.md) | Fair value engine: carry, basis, micro. Mark consumed not replicated (§14) | v1.5 |
| [signals.md](docs/specs/signals.md) | Opt-in signal publishing and the community layer | v2 |
| [mobile.md](docs/specs/mobile.md) | Read-only mobile companion with a panic button | v2 |

---

## v1.1 · Hardening and reach

- [ ] HIP-3 dexes: per-dex meta, `100000 + dex × 10000 + index` asset ids, `dex:coin` names, thin-book guardrails
- [ ] Attention tools behind a human-owned follow toggle
- [ ] `get_chart_image` as a human-shareable artifact, not an agent input
- [ ] stdio shim for MCP clients that cannot speak HTTP
- [ ] Phone push: ntfy / Telegram for the severity-tiered notifications
- [ ] `modify` tool and stop-limit orders
- [ ] Headless / tray mode
- [ ] Notarized macOS build, msi, full code-signing; auto-updater with an offline signing key
- [ ] **Durable trading history** in the Portfolio tab: backfill, gap detection, closed-position lifecycles, PnL split into price / funding / fees, CSV and JSONL export — [history.md](docs/specs/history.md)
- [ ] **Arbitrary chart intervals**: native, exact resampling, and forward-only local aggregation for sub-minute — [charts.md](docs/specs/charts.md) §3
- [ ] **Line chart mode** and the **Quantoppen** multi-asset watch grid — [charts.md](docs/specs/charts.md) §4–5

## v1.5 · Quant depth and the fleet

- [ ] `*_pctile_7d` self-normalization on every feature (gives the LLM the baseline it lacks)
- [ ] `tape_intensity_z` as a `set_alert` wakeup
- [ ] OI × price regime enum (`longs_opening`, `shorts_covering`, …) with raw deltas attached
- [ ] `suggest_size`: stop-based, vol-target, quarter-Kelly with the agent-declared edge logged for calibration grading
- [ ] Fleet crowding across containers and a `FLEET_CAP` guardrail
- [ ] Markout curves; implementation shortfall anchored on a `preflight` `snapshot_id`
- [ ] BYO-model runtime: a model loop hosted in-app, still behind the same guardrail path
- [ ] **Fair value engine**: mark replication, funding dead-zone censoring, min-variance component combination, `basis_bp` / `z` / `z_sigma`, and the five `fair_value.*` MCP tools — [fair-value.md](docs/specs/fair-value.md)
- [ ] **Workflow engine, layer one**: triggers, cron scheduler, conditions, loops, approval gates, `await_agent` nodes for external agents, run state on the existing ledger — [workflows.md](docs/specs/workflows.md) §4.1
- [ ] **Workflow templates**: `funding-carry`, `basis-dislocation`, `vol-regime`, `position-guardian`, `research-only`, `custom` — structure only, no alpha — [workflows.md](docs/specs/workflows.md) §10

## v2+ · Venues, strategies, scripts

- [ ] **Aster and Lighter venues.** Both grant sub-accounts with no volume gate — Aster at "All VIP levels", Lighter tier-capped at 4 free / 16 / 64 — so the container model lands on them more cleanly than on Hyperliquid, which is the worst of the three for this architecture [decisions.md V4] — [venue-containers.md](docs/specs/venue-containers.md)
- [ ] **Cross-venue aggregation and routing.** Positions on different venues never net: three venues is three margin pools and three liquidation prices, an economically flat book posts full margin on both legs, and one leg can liquidate while the other survives. Aggregate exposure is an oppen-enforced guardrail with no venue behind it, and is labelled as containment rather than a boundary [decisions.md V6]
- [ ] TWAP and scale orders
- [ ] Backtesting and strategy templates
- [ ] Portfolio analytics
- [ ] Vaults and spot
- [ ] Script runtime with sandbox, dry-run and replay
- [ ] **Workflow engine, layer two**: in-app agent nodes running unattended, same guardrail path — [workflows.md](docs/specs/workflows.md) §4.2
- [ ] **Opt-in signal publishing** and the community layer on the website, with no path from subscribed content to the signer — [signals.md](docs/specs/signals.md)
- [ ] **Mobile companion**: read-only from public venue state, plus a cancel-only panic wallet — [mobile.md](docs/specs/mobile.md)

## Rejected — not on any version

TA-indicator zoo · GARCH / ML vol · auto-Kelly from small samples · VPIN · ungated Sharpe (every stat carries `n` and a standard error) · raw-PnL leaderboards fed to agents · master-key paste box · guardrails in TypeScript, the MCP layer or a prompt.
