# oppen roadmap

Everything mapped so far, from the v1 MVP through the versions after it. Numbers in brackets are spec items in [docs/spec.md](docs/spec.md); D1–D8 are the settled architecture decisions there and are not re-opened here. Each phase has a gate: the phase is done when the gate passes on testnet, not on mocks.

Legend: `[x]` shipped on `main` or in an open PR · `[ ]` not started · **gate** = what proves the phase.

Status as of 2026-09-03: P0 and the P1 code are on `main`; the P1 gate is waiting on a funded testnet agent wallet. Six feature specs are written, and twenty-five product decisions are recorded in [docs/decisions.md](docs/decisions.md).

---

## v1 · MVP — Hyperliquid only, agents via MCP, human supervises

### P0 · Scaffold — done

- [x] Rust workspace: `oppen-hl`, `oppen-core`, `oppen-mcp`, Tauri 2 desktop app with the Vue 3 console shell [1]
- [x] CI: fmt, clippy `-D warnings`, tests, `cargo deny`, web build; actions pinned by SHA, `--ignore-scripts` [1]
- [x] Apache-2.0 + CLA gate, `NOTICE`, `AGENTS.md`, `skills/oppen` stub [23]
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
- [x] Universe + asset validation: 5 significant figures, `6 − szDecimals`, `szDecimals`, $10 minimum notional, slippage price [8] — PR #2
- [x] `OrderSpec` → validated `OrderWire` [8, 12] — PR #2
- [x] `examples/testnet_order.rs`: place ALO, confirm by cloid, cancel by cloid, `expiresAfter` probe — PR #2
- [ ] Rate-budget manager: batch orders/cancels, reserve headroom for risk-reducing actions, throttle before the venue does [10]
- [ ] Testnet answers to the open questions in `docs/hl-signing.md`: `expiresAfter` encoding, accepted `signatureChainId` values, agent-wallet no-withdraw scope
- **Gate:** signed testnet order via the CLI (needs a funded testnet agent key)

### P2 · WS pool, reconcile, event ledger

- [ ] Shared socket pool per network, client pings, per-IP caps [9]
- [ ] Subscriptions: book, trades, candles, mark/oracle, user fills, order updates, funding context [9, 11]
- [ ] Reconnect: re-subscribe, backfill fills via `userFillsByTime`, reconcile orders via `frontendOpenOrders` + `orderStatus(cloid)`, backfill candles [9]
- [ ] Append-only, hash-chained SQLite ledger: the one source for `get_events`, the activity stream and the audit export [D6, 29]
- [ ] Event taxonomy: fills, order transitions, rejections, guardrail trips, approvals, kill-switch changes, wallet expiry warnings, WS state, alerts [18]
- [ ] Keys in the OS keychain, read only from `oppen-hl`; keychain hand-off zeroizes the hex string [2]
- [ ] Sub-account registry: one per agent, `manual · external` bucket for outside fills [D1]
- **Gate:** zero fills lost across a 30 s disconnect

### P3 · Guardrails, kill switch, dead-man

- [ ] Per-agent guardrails: symbol allowlist, max position, notional cap, order-rate cap, reduce-only mode, max slippage, leverage cap; leverage and margin mode operator-set [24, D3]
- [ ] Guardrail config HMAC-checked with a keychain key [3]
- [ ] Loss circuit breaker: max daily loss / drawdown per agent and account-wide trips the kill switch [25]
- [ ] Kill switch per agent and global: pauses new orders, cancels resting, persists across restart, typed `trading_paused` [26]
- [ ] Dead-man's switch: `scheduleCancel` armed while any agent is active; quit dialog with cancel-all when positions are open [27]
- [ ] Property test proving there is no signer path without a guardrail check [invariant 1]
- **Gate:** no signer path without a guardrail check

### P4 · MCP gateway

- [ ] Streamable HTTP on loopback only: `Origin`/`Host` validation, bearer on every request, constant-time compare, revocation closes live sessions, on/off toggle [14, D2]
- [ ] Default-deny pairing: approve dialog names the agent, binds a sub-account, assigns guardrails; new agents start in approval mode with tiny caps [15]
- [ ] `get_state`: versioned deterministic envelope, staleness flags, time-since-last-action, positions with liq distance, orders, balances, funding, guardrail utilization, pending proposals, kill state, rate budget, network badge [16]
- [ ] `get_meta` [17], `get_events(since_cursor)` with `resync_required` [18]
- [ ] `place`, `cancel`, `cancel_all`, `close_position` with required `reason`; synchronous result contract; `get_order_status(cloid|oid)` [19]
- [ ] Typed error taxonomy with retryability: `guardrail_reject`, `venue_reject{…}`, `rate_limited`, `timeout_unknown_outcome`, `trading_paused`, `auth_expired`, `pending_approval` [19]
- [ ] `preflight(order)`: margin, fees, live book walk, guardrail verdict, post-fill exposure, `max_size_usd_within_{5,10,25}bps` [20]
- [ ] `remember` / `recall` journal [21], `set_alert(condition)` [22]
- [ ] Market = slippage-bounded IOC, limit GTC/IOC/ALO, stop-market, attached TP/SL with `positionTpsl`, reduce-only, batched actions, cloid on everything [12]
- [ ] Builder code attached by default via `OPPEN_BUILDER_ADDRESS`; missing approval prompts the ceremony, never drops the order path [5, D7]
- **Gate:** `claude mcp add` → paired → guarded testnet order. This is the demoable loop.

### P5 · Operator console

- [ ] Trade / Watch: activity stream with rejection explainability, lightweight-charts with agent annotations and a human-owned follow toggle, features panel, manual ticket with preflight line [31]
- [ ] Agents / Control: roster with real PnL, last-seen, idle-with-open-position alert, guardrail utilization; policy panel; approvals queue; risk console with exposure, rate budget, feed health, kill switches [32]
- [ ] Manual escape hatch drawer; manual actions land as `manual · external` [33]
- [ ] Staleness: per-feed status, last-tick timestamps, stale overlay, execution fails closed during disconnect [34]
- [ ] OS notifications by severity: liq warning, trips, approval request, wallet expiry, WS down with positions, alert fired [35]
- [ ] Persistent MAINNET/TESTNET badge; boot sequence bound to real state [36, D4]
- [ ] Every panel has a designed empty state [4]
- [ ] **ASCII candle renderer** promoted from the marketing site: real X and Y axes on nice numbers at the asset's own precision, `--up` / `--down` colour with the glyph as a redundant channel — [charts.md](docs/specs/charts.md) §2
- [ ] Agent `reason` strings rendered as inert plain text, labelled agent-authored [30]
- **Gate:** the named list above renders correctly and the stale overlay appears on socket loss. Cut from "parity with the design" on 2026-09-03 — see [decisions.md](docs/decisions.md) P1

### P6 · Quant features — features, not signals

- [ ] `get_features(symbol)`: `spread_bps`, `depth_usd_{bid,ask}_{10,25,50}bps`, `book_imbalance`, `micro_tilt_bps`; funding pack (`funding_apr_pct`, predicted, `next_funding_s`, `basis_bps`); vol pack (EWMA-Parkinson `rv_1h_bps`, `rv_24h_bps`, `vol_ratio`)
- [ ] Position risk in σ-units: `liq_distance_sigma`, `margin_runway_h`, `carry_usd_per_day` in every snapshot
- [ ] Loss-budget utilization % as a continuous gauge before the breaker
- [ ] Vol-scaled notional cap guardrail option: `effective_cap = risk_budget / (2σ_day)`
- [ ] TCA: `arrival_mid` on every order, `slip_bps` per fill, PnL decomposition price / funding / fees, `get_execution_report` with `n=` and baselines on every stat
- **Gate:** cross-checked against hand computation

### P7 · Approval mode, skill, release

- [ ] Approval mode, built last: `pending_approval` with TTL, re-priced at approval time with drift shown, typed approved/rejected/expired events, visible in `get_state` [28]
- [ ] First-run onboarding: testnet default → sub-account + agent wallet per agent → WalletConnect ceremony → pair first agent with the `claude mcp add` snippet and a connection test [4, D5]
- [ ] `AGENTS.md` and the `skills/oppen` Claude Code skill written for real [23]
- [ ] Threat model finalized against the shipped code [2]
- [ ] Release builds: ad-hoc dmg + unsigned AppImage first; provenance attestations and checksums [1]
- **Gate:** fresh machine to a testnet trade in 10 minutes

### Cut from v1 — decided, do not re-add

`get_chart_image` · attention tools (`focus_symbol`, `open_panel`) · stdio transport · agent-writable leverage / margin mode · HIP-3 dexes · `modify` tool and stop-limit · flatten-all coupled to the kill switch · in-app agent runtimes and replay · the 3-venue router from the design mock.

---

## Specified but not scheduled

These have written specs in [docs/specs/](docs/specs/) and slot into the versions
below. A spec is not a commitment; each carries open decisions that need an
answer before it starts.

| Spec | Feature | Target |
|---|---|---|
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
- [ ] Fleet crowding across sub-accounts and a `FLEET_CAP` guardrail
- [ ] Markout curves; implementation shortfall anchored on a `preflight` `snapshot_id`
- [ ] BYO-model runtime: a model loop hosted in-app, still behind the same guardrail path
- [ ] **Fair value engine**: mark replication, funding dead-zone censoring, min-variance component combination, `basis_bp` / `z` / `z_sigma`, and the five `fair_value.*` MCP tools — [fair-value.md](docs/specs/fair-value.md)
- [ ] **Paper execution**: one trait, two brokers, real ledger events tagged `paper` — prerequisite for workflows — [workflows.md](docs/specs/workflows.md) §8
- [ ] **Workflow engine, layer one**: triggers, cron scheduler, conditions, loops, approval gates, `await_agent` nodes for external agents, run state on the existing ledger — [workflows.md](docs/specs/workflows.md) §4.1
- [ ] **Workflow templates**: `funding-carry`, `basis-dislocation`, `vol-regime`, `position-guardian`, `research-only`, `custom` — structure only, no alpha — [workflows.md](docs/specs/workflows.md) §10

## v2+ · Venues, strategies, scripts

- [ ] Lighter and Aster venues; cross-venue aggregation and routing
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
