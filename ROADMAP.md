# oppen roadmap

Everything mapped so far, from the v1 MVP through the versions after it. Numbers in brackets are spec items in [docs/spec.md](docs/spec.md); D1–D8 are the settled architecture decisions there and are not re-opened here. Each phase has a gate: the phase is done when the gate passes on testnet, not on mocks.

Legend: `[x]` shipped on `main` or in an open PR · `[ ]` not started · **gate** = what proves the phase.

Status as of 2026-09-05: P0 and the P1 code are on `main`; the P1 gate is waiting on a funded testnet agent wallet. P2, P3 and P4 code has been landing since 2026-09-04 (PRs #9–#14), and the P4 boxes below are ticked as each lands. **The P2 and P3 boxes are stale** — the ws pool, the hash-chained ledger, the guardrail engine and the kill switch are on `main` and still read as not started; correcting them needs each bullet checked against the code and is a separate pass. Six feature specs are written, two more were commissioned by the 2026-09-04 venue audit ([onboarding.md](docs/specs/onboarding.md), [venue-containers.md](docs/specs/venue-containers.md)), and seventy-six product decisions are recorded in [docs/decisions.md](docs/decisions.md).

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
- **Gate:** app launches, CI green ✔

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
- [ ] Rate-budget manager: batch orders/cancels, reserve headroom for risk-reducing actions, throttle before the venue does [10]
- [ ] Testnet answers to the open questions in `docs/hl-signing.md`: `expiresAfter` encoding, accepted `signatureChainId` values, agent-wallet no-withdraw scope
- **Gate:** signed testnet order via the CLI (needs a funded testnet agent key)

### P2 · WS pool, reconcile, event ledger

- [ ] Shared socket pool per network, client pings, per-IP caps [9]
- [ ] Subscriptions: book, trades, candles, mark/oracle, user fills, order updates, funding context [9, 11]
- [x] Reconnect state machine: a drop un-reconciles, a reconnect asks for the gap, and only a returned reconcile clears the flag; live fills recorded on arrival and deduped by `tid` [9] — `oppen-core::feed`
- [x] Pumping a live `WsPool` into that session, and the backfill calls it asks for [9] — `oppen-core::feed::pump`, [decisions.md](docs/decisions.md) F1–F4. A drop now writes a `feed_gaps` row per subscription and a reconnect closes it, so `reconcile.rs` finally works a table something other than its own tests fills. Still driven by a fixture rather than a live socket in tests; the gate below wants a real 30 s outage
- [ ] Append-only, hash-chained SQLite ledger: the one source for `get_events`, the activity stream and the audit export [D6, 29]
- [ ] Event taxonomy: fills, order transitions, rejections, guardrail trips, approvals, kill-switch changes, wallet expiry warnings, WS state, alerts [18]
- [ ] Keys in the OS keychain, read only from `oppen-hl`; keychain hand-off zeroizes the hex string [2]
- [ ] Container registry: one venue account per agent — a **top-level Hyperliquid account** in v1, keyed by venue, address and container kind so V5's sub-account upgrade needs no migration of ledger rows — plus the `manual · external` bucket for outside fills [D1 as revised, decisions.md V1–V2, V5] — [venue-containers.md](docs/specs/venue-containers.md)
- **Gate:** zero fills lost across a 30 s disconnect

### P3 · Guardrails, kill switch, dead-man

- [ ] Per-agent guardrails: symbol allowlist, max position, notional cap, order-rate cap, reduce-only mode, max slippage, leverage cap; leverage and margin mode operator-set [24, D3]
- [ ] Guardrail config HMAC-checked with a keychain key [3]
- [ ] Loss circuit breaker: max daily loss / drawdown per agent and account-wide trips the kill switch [25]
- [ ] Kill switch per agent and global: pauses new orders, cancels resting, persists across restart, typed `trading_paused` [26]
- [ ] Dead-man's switch: `scheduleCancel` armed while any agent is active; quit dialog with cancel-all when positions are open [27] — **it is a daily budget, not a standing net**: minimum 5 s ahead, maximum 10 triggers per day resetting 00:00 UTC, so the arming policy is deliberate and the remaining count is shown in the risk console [decisions.md O1]
- [ ] Property test proving there is no signer path without a guardrail check [invariant 1]
- **Gate:** no signer path without a guardrail check

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
- [ ] `set_alert(condition)` [22]
- [x] Market = slippage-bounded IOC, limit GTC/IOC/ALO, stop-market, reduce-only, cloid on everything [12]
- [ ] Attached TP/SL with `positionTpsl`, and batched actions [12] — several orders in one action, and the engine clears one intent at a time; a batch that partially clears must not partially send
- [ ] Builder code attached by default via `OPPEN_BUILDER_ADDRESS`; missing approval prompts the ceremony, never drops the order path [5, D7]
- **Gate:** `claude mcp add` → paired → guarded testnet order. This is the demoable loop.

### P5 · Operator console

The gate was cut from "parity with the design" to the named list below on
2026-09-03 — see [decisions.md](docs/decisions.md) P1. Parity is a judgement, not
a test, and an ungated judgement resolves as schedule drift.

**In the gate:**

- [ ] Activity stream with rejection explainability: guardrail versus venue versus auth, attempted versus limit, inline link to edit [31]
- [ ] Agents / Control: roster with real PnL, last-seen, idle-with-open-position alert, guardrail utilization; policy panel; approvals queue; risk console with exposure, rate budget, feed health, kill switches [32]
- [ ] Manual escape hatch drawer; manual actions land as `manual · external` [33]
- [ ] Staleness: per-feed status, last-tick timestamps, stale overlay, execution fails closed during disconnect [34]
- [ ] Persistent MAINNET/TESTNET badge; boot sequence bound to real state [36, D4]
- [ ] Agent `reason` strings rendered as inert plain text, labelled agent-authored [30]
- [ ] **ASCII candle renderer** promoted from the marketing site: real X and Y axes on nice numbers at the asset's own precision, `--up` / `--down` colour with the glyph as a redundant channel — [charts.md](docs/specs/charts.md) §2
- **Gate:** every surface above renders correctly, and the stale overlay appears on socket loss

**Deferred to v1.1:**

- [ ] Agent fill marks on the chart, with reasons shown in the stream rather than in the plot [31, decisions.md P4]
- [ ] Follow-agent toggle
- [ ] OS notifications by severity [35]
- [ ] Designed empty state for every panel [4]
- [ ] Full parity with `docs/design/`

### P6 · Quant features — features, not signals

- [ ] `get_features(symbol)`: `spread_bps`, `depth_usd_{bid,ask}_{10,25,50}bps`, `book_imbalance`, `micro_tilt_bps`; funding pack (`funding_apr_pct`, predicted, `next_funding_s`, `basis_bps`); vol pack (EWMA-Parkinson `rv_1h_bps`, `rv_24h_bps`, `vol_ratio`)
- [ ] Position risk in σ-units: `liq_distance_sigma`, `margin_runway_h`, `carry_usd_per_day` in every snapshot
- [ ] Loss-budget utilization % as a continuous gauge before the breaker
- [ ] Vol-scaled notional cap guardrail option: `effective_cap = risk_budget / (2σ_day)`
- [ ] TCA: `arrival_mid` on every order, `slip_bps` per fill, PnL decomposition price / funding / fees, `get_execution_report` with `n=` and baselines on every stat
- **Gate:** cross-checked against hand computation

### P7 · Approval mode, skill, release

- [ ] Approval mode, built last: `pending_approval` with TTL, re-priced at approval time with drift shown, typed approved/rejected/expired events, visible in `get_state` [28]
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
