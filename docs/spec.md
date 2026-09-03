# oppen v1 specification

Local-first desktop terminal for agent-first perp trading on Hyperliquid. AI agents trade through a local MCP gateway; the human supervises from an operator console. Keys never leave the machine.

Stack: Tauri 2 · Rust core · Vue 3 + TypeScript · lightweight-charts · SQLite. v1 is Hyperliquid only.

This document was produced from a five-lens critique panel (agent ergonomics, Hyperliquid protocol, security, MVP scope, supervisor UX) over the working draft, plus a three-lens quant panel. Items marked **[panel]** were added by the critique.

---

## Architecture decisions

These are settled. Pull requests do not re-open them.

### D1 · One Hyperliquid sub-account per agent
HL nets positions per coin per account. A shared account makes per-agent PnL fiction and lets agents cancel each other's orders. Sub-accounts give real attribution, capital segregation, per-agent guardrail truth, and their own agent wallets (+2 named each), which also removes nonce contention. The roster maps 1:1 to sub-accounts. Fills made outside oppen land in an explicit `manual · external` bucket.

### D2 · MCP transport is streamable HTTP on localhost only. No stdio.
A long-running GUI app that owns the keychain cannot be spawned by an MCP client; stdio is the wrong lifecycle. One transport, hardened: loopback bind, `Origin` and `Host` validation, bearer token on every request (constant-time compare), revocation closes live sessions. Claude Code connects with one `claude mcp add --transport http` line.

### D3 · Agents cannot write risk parameters
Leverage and margin mode are operator-set per symbol, readable by agents in `get_state`, never writable. No MCP tool or agent-reachable path can modify guardrails, the approval setting, the kill switch, or the agent registry.

### D4 · Testnet by default on first run
Mainnet is an explicit, persisted switch. A persistent MAINNET/TESTNET badge appears in the UI and in `get_state`.

### D5 · The master wallet key never enters the app
No master-key paste box, ever. Agent wallets are generated in-app. `approveAgent`, sub-account creation and `approveBuilderFee` are signed once via WalletConnect in an onboarding ceremony. The agent wallet's no-withdraw scope is the hardest containment guarantee the app has.

### D6 · One event system
`get_events`, the live activity stream and the audit log are the same append-only SQLite ledger. Monotonic rowid is the agent cursor; the stream renders from it; export dumps it; rows are hash-chained.

### D7 · Builder code default-on in official builds
Hard enforcement is impossible (open source, and the protocol requires a user-signed fee approval with a cap). So: default-on via `OPPEN_BUILDER_ADDRESS`, transparent in README and onboarding, small fee, trademark keeps forks from shipping as "oppen". Apache-2.0 with a CLA.

### D8 · Two screens plus a drawer
**Watch** (activity stream + annotated chart) and **Control** (roster + risk console + approval queue), with the manual escape hatch as a drawer. The stream is the heart of the app. The shipped design expands this to five tabs (Trade, Agents, Builder, Portfolio, Settings); Trade ≈ Watch + drawer, Agents ≈ Control.

---

## A · Shell and distribution

1. **Tauri 2 app — macOS, Windows, Linux.** GitHub Actions release builds. Staged signing: ad-hoc dmg + unsigned AppImage first; msi, notarization and full code-signing once the loop is proven. CI hardened from day 1: actions pinned by commit SHA, lockfiles, `--ignore-scripts`, `cargo deny`, provenance attestations and checksums. If an auto-updater ships, its signing key is held offline.
2. **Keys in the OS keychain, plus a written threat model** ([threat-model.md](threat-model.md)). **[panel]** On Windows/Linux any same-user process can read the stored key; guardrails are containment for cooperative agents, not a security boundary; the agent wallet's no-withdraw scope is the real damage bound.
3. **Local-first, no backend.** SQLite holds the event ledger (D6), agent registry, guardrail config (HMAC-checked with a key from the keychain), agent journals, settings.
4. **First-run onboarding.** **[panel]** Testnet default → create sub-account + agent wallet per agent → one-time WalletConnect ceremony (D5) → pair the first agent: mint token, copy `claude mcp add` snippet, connection test. Every panel has a designed empty state.
5. **Builder fee on every order** (D7). Graceful handling when approval is missing: prompt for the ceremony, never silently fail the order path.

## B · Hyperliquid core (Rust, `crates/oppen-hl`)

6. **Signing correctness day 1.** **[panel]** msgpack action-hash field order matters; float wire values must be normalized strings (a trailing zero = different hash = opaque "User or API Wallet does not exist"). The official SDK's test vectors are ported into the Rust signer before anything else.
7. **Serialized per-signer nonce allocator.** **[panel]** HL keeps the 100 highest nonces per signer; concurrent tool calls collide. One monotonic allocator and submit queue per signer; one agent wallet per sub-account.
8. **Meta and order-validation layer.** **[panel]** Universe, `szDecimals`, max leverage, $10 min notional, numeric asset ids. Every order rounded and validated before signing: 5-significant-figure price rule, size to `szDecimals`.
9. **WS pool with reconnect and reconcile.** **[panel]** All agents multiplexed over a shared socket pool (per-IP caps), client pings (server drops idle ~60s). On reconnect: re-subscribe, backfill fills via `userFillsByTime`, reconcile orders via `frontendOpenOrders` + `orderStatus` (by cloid), backfill candles. HL has no server-side cursor — the ledger is the app's own and must never silently drop a fill across a laptop sleep.
10. **Rate-budget manager.** **[panel]** HL address budget: 1 request per 1 USDC traded, 10k initial buffer, then 1 req/10s. Batch orders and cancels, surface remaining budget (`userRateLimit`) in the risk console, throttle agents before HL does, always reserve headroom for risk-reducing actions.
11. **Account and market data.** Positions, open orders, fills, balances, funding paid; candles, book (server-side `nSigFigs`), trades, mark/oracle; **current funding rate, predicted next funding, time-to-funding, open interest** per symbol.
12. **Execution set.** Market = slippage-bounded IOC limit (app computes price; max-slippage mandatory), limit (GTC/IOC/ALO), stop-market, attached TP/SL with `positionTpsl` grouping, reduce-only, cancel, `cancel_all(symbol?)`, batched actions, cloid on everything.
13. **Testnet toggle** (D4). URL + chain-id switch; badge in UI and `get_state`.

## C · Agent gateway (`crates/oppen-mcp`)

14. **Hardened localhost HTTP MCP server** (D2), on/off toggle in the UI.
15. **Default-deny pairing.** **[panel]** First connection is an explicit approve dialog: name the agent, bind it to a sub-account, assign guardrails. New agents start with approval mode ON and tiny caps. Token revocation closes live connections.
16. **`get_state` — the agent's eyes.** Versioned envelope, deterministic compact JSON, stable key order. Timestamp and time-since-your-last-action, per-feed health/staleness flags, positions with liq distance, open orders, balances, funding context, guardrail utilization, pending proposals, kill-switch state, remaining rate budget, network badge.
17. **`get_meta`.** **[panel]** Per-symbol specs: tick/lot rules, min notional, max leverage, funding interval, vol.
18. **`get_events(since_cursor)`.** Durable rowid cursor; cursor-too-old returns explicit `resync_required`. Taxonomy: fills, order state transitions, rejections, guardrail trips, approval decisions, kill-switch changes, agent-wallet expiry warnings, WS disconnect/reconnect, alerts.
19. **Execution tools with a real contract.** **[panel]** `place`, `cancel`, `cancel_all`, `close_position`, each requiring a `reason` string. Synchronous result `{status: resting|filled|rejected|pending_approval, oid, cloid, filled_sz, avg_px}`. `get_order_status(cloid|oid)` for reconcile after timeouts — the only safe move after `timeout_unknown_outcome` is query-by-cloid, never blind retry. Typed error taxonomy with retryability: `guardrail_reject`, `venue_reject` subtypes (`min_notional`, `tick_price`, `insufficient_margin`, `reduce_only_violation`), `rate_limited{retry_after_ms}`, `timeout_unknown_outcome`, `trading_paused`, `auth_expired`, `pending_approval`.
20. **`preflight(order)`.** Margin sufficiency, estimated fees and slippage (live book walk), guardrail verdict, post-fill exposure, `max_size_usd_within_{5,10,25}bps` — without executing.
21. **Agent journal — `remember` / `recall`.** Per-agent scratchpad in SQLite. Agents are amnesiac across sessions.
22. **`set_alert(condition)`.** Price cross, liq distance, fill, funding threshold, feature thresholds → event + OS notification. Agents do not experience time; wakeups replace polling.
23. **AGENTS.md and a Claude Code skill in the repo.** Agent onboarding is a file, not a tutorial.

## D · Guardrails and safety (`crates/oppen-core`, enforced pre-sign)

24. **Per-agent guardrails** (= per sub-account): symbol allowlist, max position size, notional cap, order-rate cap (plus the global budget), reduce-only mode, max slippage, leverage cap. Leverage and margin mode are operator-set (D3).
25. **Loss circuit breaker.** **[panel]** Max daily loss / drawdown per agent and account-wide trips the kill switch. Size caps do not stop an agent grinding the account to zero overnight.
26. **Kill switch — per agent and global.** Pauses new orders **and cancels resting orders**. State persists across restart. Agents receive typed `trading_paused`. Persistent banner. Flatten is decoupled: per position, with confirm.
27. **Dead-man's switch.** **[panel]** HL `scheduleCancel` armed while any agent is active. Quit dialog when positions are open, with a cancel-all option.
28. **Approval mode.** Default ON for new agents. `place` returns `{status: pending_approval, approval_id, expires_at}`; proposals carry a TTL and auto-expire; re-priced at approval time with drift shown; typed approved/rejected/expired events; pending proposals visible in `get_state`. All agent actions queue. *Built last in v1.*
29. **Hash-chained audit ledger.** Each row commits to the previous row's hash; verified on export; "chain broken" banner. Tamper-evident, not tamper-proof.
30. **Reasons are claims.** Agent `reason` strings render as inert plain text, labelled as agent-authored.

## E · Operator console (`apps/desktop`)

31. **Trade / Watch.** Activity stream with rejection explainability (guardrail vs venue vs auth, attempted vs limit, inline link to edit). Chart: lightweight-charts, candles + volume, agent entries/exits/reasons as annotations, "follow agent" toggle (human-owned). Features panel (spec F). Manual ticket with preflight line.
32. **Agents / Control.** Roster card per agent: real PnL, status with last-seen and idle-with-open-position alert, guardrail utilization. Policy panel. Approvals queue. Risk console: aggregate exposure, guardrail editor, rate budget, feed health, kill switches.
33. **Manual escape hatch.** Basic ticket, close per position, cancel per order. Manual actions land in the ledger under `manual · external`.
34. **Staleness treatment.** **[panel]** Per-feed status, last-tick timestamps, stale overlay after N seconds, execution tools fail closed during disconnect with a typed event.
35. **OS notifications, severity-tiered.** **[panel]** Liq warning, guardrail/circuit trip, approval request, agent-wallet expiry, WS down with open positions, alert fired.
36. **Dark-only theme; persistent MAINNET/TESTNET badge.** Design system in `docs/design/`.

## F · Quant layer — features, not signals

The app computes honest deterministic numbers; agents decide. LLM-native shapes only: scalars in bps / $/day / σ-units, enums, percentiles. Never matrices, never pixels, never recommendations.

**v1**
- `get_features(symbol)`: `spread_bps`, `depth_usd_{bid,ask}_{10,25,50}bps`, `book_imbalance`, `micro_tilt_bps`; funding pack (`funding_apr_pct`, predicted, `next_funding_s`, `basis_bps`); vol pack (EWMA-Parkinson `rv_1h_bps`, `rv_24h_bps`, `vol_ratio`).
- Position risk in σ-units: `liq_distance_sigma`, `margin_runway_h`, `carry_usd_per_day` in every snapshot.
- Loss-budget utilization % — continuous gauge before the breaker.
- Vol-scaled notional cap (guardrail option): `effective_cap = risk_budget / (2σ_day)`.
- TCA foundation: `arrival_mid` stamped on every order, `slip_bps` per fill, per-agent PnL decomposition price / funding / fees, `get_execution_report` with `n=` and baselines on every stat.

**v1.5**
- `*_pctile_7d` self-normalization on every feature.
- `tape_intensity_z` as a `set_alert` wakeup.
- OI × price regime enum (`longs_opening`, `shorts_covering`, …) with raw deltas attached.
- `suggest_size`: stop-based, vol-target, quarter-Kelly with agent-declared edge logged for later calibration grading.
- Fleet crowding + `FLEET_CAP` guardrail across sub-accounts.
- Markout curves, implementation shortfall with a `preflight` snapshot as decision anchor.

**Rejected**: TA-indicator zoo, GARCH/ML vol, auto-Kelly from small samples, VPIN, ungated Sharpe, raw-PnL leaderboards fed to agents.

---

## Cut from v1

| Item | Why |
|---|---|
| `get_chart_image` PNG tool | Contradicts "structured state over screenshots"; flaky capture plumbing. v2 as a human-shareable artifact. |
| Attention tools (`focus_symbol`, `open_panel`) | A misbehaving agent can steer the supervisor's eyes. UI auto-follows the stream instead. |
| stdio MCP transport | Wrong lifecycle (D2). |
| Agent-writable leverage / margin mode | D3. |
| HIP-3 dexes | Per-dex meta, asset-id arithmetic, thin books. v1.1. |
| `modify` tool, stop-limit | Cancel + replace with a fresh cloid; stop-market + attached TP/SL cover protective needs. |
| Flatten-all coupled to the kill switch | Market-dumping everything is itself destructive. |
| In-app agent runtimes (BYO model loop, script sandbox), replay | External agents via MCP only in v1. BYO v1.5, scripts + replay v2. |

## Deferred

**v1.1**: HIP-3, attention tools behind a follow toggle, chart image, stdio shim, phone push (ntfy / Telegram), stop-limit, modify, headless/tray mode, BYO-model runtime.
**v2+**: Lighter, Aster, cross-venue aggregation and routing, TWAP/scale, backtesting, strategy templates, portfolio analytics, vaults, spot, script runtime.

---

## Build order

1. keychain + Rust HL core (signing vectors, nonce, meta/validation)
2. WS pool + reconcile + event ledger
3. guardrails + kill switch + dead-man
4. MCP transport + pairing
5. `get_state` / `get_meta` / `get_events`
6. `place` / `cancel` + cloid + `preflight`
7. activity stream + kill switch UI
8. chart + annotations
9. roster + risk console
10. onboarding + notifications
11. journal + alerts + AGENTS.md + skill
12. approval mode

The demoable loop — agent connects, sees state, places a guarded order, human watches it live — is step 6. Everything after is polish.
