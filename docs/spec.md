# oppen v1 specification

Local-first desktop terminal for agent-first perp trading on Hyperliquid. AI agents trade through a local MCP gateway; the human supervises from an operator console. Keys never leave the machine.

Stack: Tauri 2 · Rust core · Vue 3 + TypeScript · lightweight-charts · SQLite. v1 is Hyperliquid only.

This document was produced from a five-lens critique panel (agent ergonomics, Hyperliquid protocol, security, MVP scope, supervisor UX) over the working draft, plus a three-lens quant panel. Items marked **[panel]** were added by the critique.

---

## Architecture decisions

These are settled. Pull requests do not re-open them.

### D1 · One venue account per agent

**The rule.** One venue *account* per agent. A sub-account where the venue grants one; a top-level account where it does not. The container is the unit of isolation — attribution, capital segregation, guardrail truth and nonce ownership follow the account, never the agent's name — and which venue primitive provides that container is a venue detail. The roster maps 1:1 to containers. Fills made outside oppen land in an explicit `manual · external` bucket.

This decision is venue-agnostic and binds every venue oppen integrates. Aster and Lighter are first-class in it rather than exceptions to it: both grant sub-accounts with no volume gate, which fits the container model better than Hyperliquid does. First-class in the architecture is not a shipping commitment — the venue set v1 ships is scope, recorded under [Deferred](#deferred) and in [ROADMAP.md](../ROADMAP.md).

**Why a shared address was rejected.** Two agents on one address, opposite sides of the same coin, net to zero at the venue. Nothing downstream survives that:

- **Margin is fiction.** The venue posts margin against the net. A per-agent margin figure is oppen's invention and is not the number that governs anything.
- **The liquidation price is fiction.** There is exactly one, for the net position, and it belongs to neither agent. Each agent's `liq_distance_sigma` would describe a price the venue will never act on.
- **Funding cannot be reconstructed.** Funding accrues on the net position. Two agents flat against each other pay and receive nothing at all — there is no cash to split. This is not an allocation problem with a hard answer; it is an absence, and no local bookkeeping recovers money that never moved. Fees allocate cleanly because every fee attaches to a fill. Funding does not, because the payment did not happen.
- **Orders interfere.** `cancel_all`, flatten and close act on the address, not on whoever opened the order.

Hyperliquid offers no way out: there is no hedge mode — the exchange docs say of `updateIsolatedMargin`'s side parameter, "this parameter won't have any effect until hedge mode is introduced" — so a long and a short on one coin on one address are one position with one sign. Aster does support hedge mode, but it is an account-wide setting that cannot be changed while any position or order is open, and it yields one long slot and one short slot per symbol. Hedge mode changes the sign bookkeeping inside a container; it does not make a container shareable. Because it cannot be changed on a container that holds anything, oppen sets Aster's position mode at provisioning and treats it as fixed for the container's life, one-way by default ([decisions.md](decisions.md) V8).

**Venue capability matrix.** Read 2026-09-04 from the sources listed below.

| Venue | Sub-accounts | Gate to create | Free count on day one | Trade-only keys | Key count | Hedge mode | Netting unit |
|---|---|---|---|---|---|---|---|
| Hyperliquid | Yes | $100,000 cumulative volume unlocks the first 10; each further $100M unlocks 1 more, cap 50. Protocol-enforced, testnet included | **0** | Yes — an API wallet cannot withdraw | 3 per master, +2 per sub-account | No | Address, per coin |
| Aster | Yes | None — "VIP level requirement: All VIP levels" | 10 (VIP1–2), rising to 50 (MM tier 3) | Yes — agents carry `canWithdraw: false`; withdrawal on a sub-account is "Permanently disabled" | 30 master / 10 per sub-account | Yes, account-wide | Account, per symbol (two slots per symbol in hedge mode) |
| Lighter | Yes | None; capped by account tier | 4 (Standard, the free default) — 16 Plus, 64 Premium | Partial — a key can send *secure* withdrawals, but only to the L1 address that created the account | ~253 per account index (docs disagree: 253 / 254 / 256; indices 0–1 reserved) | No (read off the data model, never stated in the docs) | `(account_index, market_id)`, single sign field |

Hyperliquid is the only one of the three that gates the container primitive itself. The rest of the operational difference is in how a container is created and moved: Hyperliquid's `createSubAccount` is a master-signed L1 action; Aster's `POST /fapi/v3/createSubAccount` needs a dual signature — the sub-account's own key plus the master's EIP-712 signature on `chainId` 1666 — and hands back a generated wallet key **shown once and never stored by Aster**; Lighter's `L2CreateSubAccount` is an L2 transaction signed by an existing API key with no Ethereum key involved, which makes it the cleanest of the three. Neither Aster nor Lighter allows deleting a sub-account, and Aster does not support nesting. Lighter tier changes require no open positions, no open orders and 24 hours since the last change. Internal transfers are cheap everywhere: Aster's `subAccountTransfer` is instant and free in both directions including sub↔sub, and Hyperliquid's `usdSend` is instant and "does not touch the EVM bridge".

Lighter is the weak column, and D5's hardest guarantee does not survive it intact. Hyperliquid's API wallet and an Aster agent with `canWithdraw: false` cannot move funds at all; a Lighter API key can, and the restriction is only on the destination — a secure withdrawal goes to the L1 address that created the account, and Fast Withdrawals and Transfers require the L1 key. So a stolen Lighter key sends money to its owner rather than to an attacker, which bounds the loss without making it impossible. Lighter also offers read-only tokens (no trades, no withdrawals, expiry from 1 day to 10 years) and, on Premium, maker-only keys. This is a difference to state in the threat model before Lighter ships, not to paper over.

**The Hyperliquid v1 shape.** A brand-new Hyperliquid user gets **zero** sub-accounts, so v1 provisions **one top-level account per agent**. Eligibility is still never predicted, but the reason is narrower than "the number is invisible". The number is visible: `userRateLimit` returns `cumVlm`, documented as "Cumulative volume", modelled in `crates/oppen-hl` as `UserRateLimit::cum_vlm`, and observed live on 2026-09-04 returning `{"cumVlm":"188908641154.22","nRequestsUsed":…}`. What no source states is whether the sub-account gate meters *that* counter, or whether the counter is lifetime or windowed. So oppen **may** show distance to $100,000 computed from `cumVlm`, labelled on screen as an estimate the venue does not confirm, and it still **attempts, then classifies**: it calls `createSubAccount` when the operator asks for the upgrade and reads the answer off the venue's response, never off the counter. The `Required:` and `Traded:` figures in the venue's message are surfaced verbatim for display and are never branched on — venue message text is not a program input. Testnet is the same shape and the same gate, funded once: the faucet pays 1,000 mock USDC to an address that has previously deposited on mainnet, and `usdSend` spreads that across the agent accounts.

**Cost per agent: three signatures, two if the builder code is declined.** Under the container model no Hyperliquid signature is once-per-user, because every container *is* its own Hyperliquid user. The arithmetic in full:

| Signature | Signed by | Why it is per agent |
|---|---|---|
| `approveAgent` | the container itself | Authorises that container's own API wallet. An API wallet signs for its own master or that master's sub-accounts and never for an unrelated top-level account, so no approval is shared. |
| `approveBuilderFee` | the container itself | The approval is per *account*, not per user ([decisions.md](decisions.md) O7). Declining is supported: the order path continues with no builder code attached. |
| `usdSend` | the funding account | Moves USDC into the container. |

Three per agent with D7's builder code on, two with it declined, and **no reduction for the second and Nth agent** — the first agent costs the same as the tenth. The single exception is the downgrade in [decisions.md](decisions.md) V7, where an agent bound to the funding account itself skips `usdSend`; it is off by default and its confirmation prices the loss of isolation in dollars. Creating the container is not one of the three: deriving account *n* in the user's wallet is a local key derivation costing no signature and no gas, which is the reason the top-level model is affordable at all. All three are user-signed, so **D5 holds unchanged** — no account-owner key enters oppen and the app signs none of them. They come from two different addresses, which is why the wallet session's active account has to be shown next to the account the pending ceremony needs. An agent's container is therefore another address in the user's existing wallet, and the wallet is where all of those keys stay. The `approveAgent` scope rule above follows from the API-wallet scope rule rather than being quoted from a page that states it for this case. Each container gets a distinctly named API wallet: re-approving the same `agentName` silently replaces the previous one, and `valid_until` caps at 180 days (oppen's own expiry is 90 days — [decisions.md](decisions.md) D-b).

The count on the **sub-account upgrade path is not three, and is not yet known**: `createSubAccount` and `subAccountTransfer` are master-signed, but whether a sub-account needs its own `approveAgent` and whether it inherits its master's builder-fee approval are both open (below). Do not quote a number for that path until they are closed.

On the wire this changes the routing: an order for a top-level agent account carries **no `vaultAddress`** and is signed by that account's own API wallet. `vaultAddress` is the sub-account form ([hl-signing.md](hl-signing.md) §"Sub-account routing") and comes back with the upgrade below, so the route is a property of the container and must not be hardcoded either way.

The $100,000 path is an upgrade, not a requirement. The gate meters **one account's** volume, so the account that clears it is whichever one actually traded — under [decisions.md](decisions.md) V7 the funding account is not a container by default and may therefore never clear it, and nothing in v1 depends on any account clearing it. When one does, oppen migrates an agent onto a real sub-account: a container swap plus a funding transfer, not a redesign, because the schema records the container and its owner ([decisions.md](decisions.md) R2), not the venue primitive. Design for it now; do not require it. One precondition is not yet met: rooting sub-accounts under an *agent's* container is unsafe while it is unknown whether an API wallet approved on a master also signs for that master's sub-accounts (below) — if it does, every sibling container falls inside one agent key's signing scope.

**What this costs.**

- **The shared fee tier on Hyperliquid.** Volume no longer aggregates across the user's accounts, so each agent account climbs the fee schedule alone. Worth nothing at tier 0. It starts to matter at the volume where tiers matter, and a user routing that much has cleared the $100,000 gate and can move onto sub-accounts. Whether Hyperliquid then aggregates fee-tier volume across a master and its sub-accounts was **not confirmed** (below), so this is a cost we assume, not one we measured.
- **Stranded capital per container.** Every account posts its own margin. Capital sitting in agent A's account cannot back agent B's position without an explicit transfer, and idle margin per agent is the direct price of segregation. The price is the same at any venue; a sub-account only makes the transfer cheaper.
- **Three wallet prompts per agent on Hyperliquid**, two with the builder code declined, and no discount for the tenth agent. Onboarding an agent is not a click. This is the cost a user feels first.

**Cross-venue.** Positions never net across venues. Three venues is three margin pools, three liquidation prices and no cross-margin. A book that is economically flat — long a coin on one venue, short the same coin on another — posts full margin on both legs, pays funding on both, and **one leg can liquidate while the other survives untouched**. No venue can see the others. Aggregate exposure across venues is therefore the application's job: oppen computes it across containers and shows it in the risk console (item 32), but nothing at any venue enforces it. A cross-venue exposure number is a report, never a control. Its guardrail form is `FLEET_CAP` (section F, v1.5).

**Guards for any shared address.** A container is normally one agent's, but three paths break that: the `manual · external` bucket, where the operator trades the same address; any account oppen watches but did not provision ([decisions.md](decisions.md) R3); and the explicit downgrade in which an operator binds an agent to the funding account itself, which is off by default and carries a dollar-denominated warning ([decisions.md](decisions.md) V7). Wherever two or more actors share an address, all four hold:

- an agent may cancel or modify only order ids it opened;
- the venue's aggregate position book is never sliced per agent;
- `close_position` and flatten are refused while two or more agents share an address;
- account equity and margin are shown once for the address and never mirrored per agent.

**Not confirmed.** Stated here so silence is not read as certainty.

| Claim | Status |
|---|---|
| The sub-account gate meters `userRateLimit`'s `cumVlm` | Not confirmed. `cumVlm` exists and returns a number; no source links it to the gate. This is why the gate is attempted and never predicted, and why any distance-to-gate figure is labelled an estimate. |
| `cumVlm` is lifetime-cumulative rather than windowed | Everything points to cumulative. Hyperliquid has never said so. If it is windowed, the upgrade stops being a one-way ratchet and an interrupted volume run decays. |
| A Hyperliquid API wallet may sign `createSubAccount` or `subAccountTransfer` | Not stated either way ([hl-signing.md](hl-signing.md) open question 3). If it may, an agent key can move USDC between a master and its sub-accounts and no guardrail models that. Needs a testnet negative test before mainnet. |
| A sub-account inherits its master's builder-fee approval | Not confirmed (O7). It sets the signature count on the upgrade path. |
| Hyperliquid aggregates fee-tier volume across a master and its sub-accounts | Not confirmed either way. Aster documents that it does aggregate (VIP level is computed on the master+subs group); Hyperliquid is silent. |
| Aster caps the number of agents | No number appears in the docs. |
| An Aster wallet below VIP1 gets exactly 10 sub-accounts | The requirement reads "All VIP levels", but the table's lowest row is VIP1. |
| Aster sub-accounts work in Shield Mode / 1001x | Nothing found. Shield Mode is documented as not supporting hedge mode. |
| Lighter's exact API key count | Three official pages give 253, 254 and 256. |
| Lighter has no hedge mode | Inferred from the schema — one position per `(account_index, market_id)` with a single sign field. The docs never state it. |
| Hyperliquid's `scheduleCancel` is itself volume-gated | A reverse-engineered binary string suggests it; the docs are silent. Item 27 depends on the answer. |

**Sources**, all read 2026-09-04.

- Hyperliquid sub-account gate and cap: [Sub-accounts](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/sub-accounts) — "Up to 10 sub-accounts can be created after reaching $100,000 in volume. Every additional $100M in volume enables the ability to create 1 additional sub-account, up to a maximum of 50 sub-accounts."
- Protocol enforcement: an integration test in the `nktkas/hyperliquid` SDK asserts that `createSubAccount` rejects with "Cannot create sub-accounts until enough volume traded" — and that test runs against **testnet**, where the threshold is the same $100,000. No API bypass was found. Other volume-gated features share the mechanism: scheduled cancels, portfolio margin, extra agents beyond the baseline, referral codes.
- API wallet count ("starts at 3 for all master accounts and increases by 2 per sub-account"), the no-withdraw scope, the 180-day `valid_until` cap and silent replacement on name reuse: Hyperliquid's API-wallet and nonce pages, transcribed with line references in [hl-signing.md](hl-signing.md).
- No hedge mode: Hyperliquid exchange-endpoint docs, `updateIsolatedMargin`.
- `usdSend` internal transfer: Hyperliquid docs — a user-signed action, signed by the sending account itself, never an agent action.
- Aster: `docs.asterdex.com` — sub-accounts (VIP requirement, caps, "Each sub-account maintains its own positions, assets, and API keys", one generated wallet key shown once, "Sub-accounts only support V3 API"), `POST /fapi/v3/createSubAccount` (dual signature, present on mainnet and testnet; the sibling `/fapi/v3/sub-accounts/bind` *is* whitelist-gated, `createSubAccount` is not), `POST /fapi/v3/registerAndApproveAgent` (`canSpotTrade` / `canPerpTrade` / `canWithdraw`, expiry, `ipWhitelist` required when `canWithdraw` is true), `POST /fapi/v3/positionSide/dual`.
- Lighter: `docs.lighter.xyz` (account tiers and sub-account caps, "Sub accounts cannot be deleted", tier-change conditions) and `apidocs.lighter.xyz` (API keys, `L2CreateSubAccount`, the secure-withdrawal restriction to the creating L1 address, read-only tokens, maker-only keys on Premium).

### D2 · MCP transport is streamable HTTP on localhost only. No stdio.
A long-running GUI app that owns the keychain cannot be spawned by an MCP client; stdio is the wrong lifecycle. One transport, hardened: loopback bind, `Origin` and `Host` validation, bearer token on every request (constant-time compare), revocation closes live sessions. Claude Code connects with one `claude mcp add --transport http` line.

### D3 · Agents cannot write risk parameters
Leverage and margin mode are operator-set per symbol, readable by agents in `get_state`, never writable. No MCP tool or agent-reachable path can modify guardrails, the approval setting, the kill switch, or the agent registry.

### D4 · Testnet by default on first run
Mainnet is an explicit, persisted switch. A persistent MAINNET/TESTNET badge appears in the UI and in `get_state`.

### D5 · No account-owner key ever enters the app
No paste box for the key of any account that holds funds, ever. oppen generates agent wallets in-app and holds nothing else.

The revised D1 removes the single master this decision was originally written around. There are now **N account-owner keys** — one per container, plus the funding account's — and every one of them can withdraw. The guarantee does not weaken with N, because it was never a claim about how many such keys exist: **not one of them is oppen's**. They are accounts in the user's own wallet, and every action needing one is signed there over WalletConnect — `approveAgent` and `approveBuilderFee` by the container, `usdSend` by the funding account, `createSubAccount` and `subAccountTransfer` by the master where the venue grants a sub-account. oppen renders the payload, recovers the signer from the returned signature and refuses to submit when it does not match the account the ceremony named.

What oppen does hold is one agent-wallet private key per container, in the OS keychain. On Hyperliquid and on Aster (`canWithdraw: false`) that key cannot move funds out of the account, and that is the hardest containment guarantee the app has. On Lighter it is weaker — an API key can send a *secure* withdrawal, restricted to the L1 address that created the account — so the guarantee is stated per venue in [threat-model.md](threat-model.md) and never as a flat property of oppen.

What N does change is the user's side of the line, and it belongs here rather than implied: N containers means N withdrawal-capable keys for the user to hold, back up and keep straight. That cost is real. It is simply not oppen's to lose.

### D6 · One event system
`get_events`, the live activity stream and the audit log are the same append-only SQLite ledger. Monotonic rowid is the agent cursor; the stream renders from it; export dumps it; rows are hash-chained.

### D7 · Builder code default-on in official builds
Hard enforcement is impossible (open source, and the protocol requires a user-signed fee approval with a cap). So: default-on via `OPPEN_BUILDER_ADDRESS`, transparent in README and onboarding, small fee, trademark keeps forks from shipping as "oppen". Open core with a CLA — the fee constant lives in `oppen-hl`, which is Apache-2.0, so this stays true under L1 and is not an argument for closing the core. [decisions.md](decisions.md) L5 sizes the leak this concedes.

### D8 · Two screens plus a drawer
**Watch** (activity stream + annotated chart) and **Control** (roster + risk console + approval queue), with the manual escape hatch as a drawer. The stream is the heart of the app. The shipped design expands this to five tabs (Trade, Agents, Builder, Portfolio, Settings); Trade ≈ Watch + drawer, Agents ≈ Control.

---

## A · Shell and distribution

1. **Tauri 2 app — macOS, Windows, Linux.** GitHub Actions release builds. Staged signing: ad-hoc dmg + unsigned AppImage first; msi, notarization and full code-signing once the loop is proven. CI hardened from day 1: actions pinned by commit SHA, lockfiles, `--ignore-scripts`, `cargo deny`, provenance attestations and checksums. If an auto-updater ships, its signing key is held offline.
2. **Keys in the OS keychain, plus a written threat model** ([threat-model.md](threat-model.md)). **[panel]** On Windows/Linux any same-user process can read the stored key; guardrails are containment for cooperative agents, not a security boundary; the agent wallet's no-withdraw scope is the real damage bound.
3. **Local-first, no backend.** SQLite holds the event ledger (D6), agent registry, guardrail config (HMAC-checked with a key from the keychain), agent journals, settings.
4. **First-run onboarding.** **[panel]** Testnet default → one venue account + agent wallet per agent (D1) → WalletConnect ceremony (D5), **three signatures per agent and none of them once-per-user**: `approveAgent` and `approveBuilderFee` signed by the container, `usdSend` signed by the funding account (two if the builder code is declined — [decisions.md](decisions.md) O7) → pair the first agent: mint token, copy `claude mcp add` snippet, connection test. The ordering of the three within a ceremony is settled in [specs/onboarding.md](specs/onboarding.md) §3.3, not here. Every panel has a designed empty state.
5. **Builder fee on every order** (D7). Graceful handling when approval is missing: prompt for the ceremony, never silently fail the order path.

## B · Hyperliquid core (Rust, `crates/oppen-hl`)

6. **Signing correctness day 1.** **[panel]** msgpack action-hash field order matters; float wire values must be normalized strings (a trailing zero = different hash = opaque "User or API Wallet does not exist"). The official SDK's test vectors are ported into the Rust signer before anything else.
7. **Serialized per-signer nonce allocator.** **[panel]** HL keeps the 100 highest nonces per signer; concurrent tool calls collide. One monotonic allocator and submit queue per signer; one agent wallet per venue account (D1), sub-account or top-level. An API wallet signing for a master and its sub-accounts shares a single nonce set ([hl-signing.md](hl-signing.md)), so the container owns the signer, never the agent.
8. **Meta and order-validation layer.** **[panel]** Universe, `szDecimals`, max leverage, $10 min notional, numeric asset ids. Every order rounded and validated before signing: 5-significant-figure price rule, size to `szDecimals`.
9. **WS pool with reconnect and reconcile.** **[panel]** All agents multiplexed over a shared socket pool (per-IP caps), client pings (server drops idle ~60s). On reconnect: re-subscribe, backfill fills via `userFillsByTime`, reconcile orders via `frontendOpenOrders` + `orderStatus` (by cloid), backfill candles. HL has no server-side cursor — the ledger is the app's own and must never silently drop a fill across a laptop sleep.
10. **Rate-budget manager — one budget per container, not one for the fleet.** **[panel]** `userRateLimit` is **per address**: 1 request per 1 USDC traded, a 10,000-request initial buffer, then 1 req/10s. N containers is therefore N independent budgets, which is the one place the container model buys headroom instead of spending it — a fresh fleet of four starts with 4 × 10,000 requests, not one shared 10,000. They are **not fungible**: container A cannot lend headroom to container B, so an agent that burns its own address's budget is throttled alone and the others are unaffected. Consequences: batch orders and cancels, poll `userRateLimit` per container and surface each remaining budget separately in the risk console, throttle each agent against **its own** address before HL does, and reserve per-container headroom for risk-reducing actions. Whatever is metered per IP rather than per address is the exception — that is shared by every container on the machine, the WS pool already assumes a per-IP cap (item 9), and which limits are IP-scoped was not re-verified in this pass.
11. **Account and market data.** Account data is fetched per container, addressed by the account's own address — an API wallet signs and is never a query key ([hl-signing.md](hl-signing.md)). Positions, open orders, fills, balances, funding paid; candles, book (server-side `nSigFigs`), trades, mark/oracle; **current funding rate, predicted next funding, time-to-funding, open interest** per symbol.
12. **Execution set.** Market = slippage-bounded IOC limit (app computes price; max-slippage mandatory), limit (GTC/IOC/ALO), stop-market, attached TP/SL with `positionTpsl` grouping, reduce-only, cancel, `cancel_all(symbol?)`, batched actions, cloid on everything. Every execution tool is scoped to the calling agent's container; on a shared address it is scoped to that agent's own order ids (D1).
13. **Testnet toggle** (D4). URL + chain-id switch; badge in UI and `get_state`.

## C · Agent gateway (`crates/oppen-mcp`)

14. **Hardened localhost HTTP MCP server** (D2), on/off toggle in the UI.
15. **Default-deny pairing.** **[panel]** First connection is an explicit approve dialog: name the agent, bind it to its venue account (D1), assign guardrails. New agents start with approval mode ON and tiny caps. Token revocation closes live connections.
16. **`get_state` — the agent's eyes.** Versioned envelope, deterministic compact JSON, stable key order. Timestamp and time-since-your-last-action, per-feed health/staleness flags, positions with liq distance, open orders, balances, funding context, guardrail utilization, pending proposals, kill-switch state, remaining rate budget, network badge.
17. **`get_meta`.** **[panel]** Per-symbol specs: tick/lot rules, min notional, max leverage, funding interval, vol.
18. **`get_events(since_cursor)`.** Durable rowid cursor; cursor-too-old returns explicit `resync_required`. Taxonomy: fills, order state transitions, rejections, guardrail trips, approval decisions, kill-switch changes, agent-wallet expiry warnings, WS disconnect/reconnect, alerts.
19. **Execution tools with a real contract.** **[panel]** `place`, `cancel`, `cancel_all`, `close_position`, each requiring a `reason` string. Synchronous result `{status: resting|filled|rejected|pending_approval, oid, cloid, filled_sz, avg_px}`. `get_order_status(cloid|oid)` for reconcile after timeouts — the only safe move after `timeout_unknown_outcome` is query-by-cloid, never blind retry. Typed error taxonomy with retryability: `guardrail_reject`, `venue_reject` subtypes (`min_notional`, `tick_price`, `insufficient_margin`, `reduce_only_violation`), `rate_limited{retry_after_ms}`, `timeout_unknown_outcome`, `trading_paused`, `auth_expired`, `pending_approval`.
20. **`preflight(order)`.** Margin sufficiency, estimated fees and slippage (live book walk), guardrail verdict, post-fill exposure, `max_size_usd_within_{5,10,25}bps` — without executing.
21. **Agent journal — `remember` / `recall`.** Per-agent scratchpad in SQLite. Agents are amnesiac across sessions.
22. **`set_alert(condition)`.** Price cross, liq distance, fill, funding threshold, feature thresholds → event + OS notification. Agents do not experience time; wakeups replace polling.
23. **AGENTS.md and a Claude Code skill in the repo.** Agent onboarding is a file, not a tutorial.

## D · Guardrails and safety (`crates/oppen-core`, enforced pre-sign)

24. **Per-agent guardrails** (= per venue account, D1): symbol allowlist, max position size, notional cap, order-rate cap (plus the global budget), reduce-only mode, max slippage, leverage cap. Leverage and margin mode are operator-set (D3).
25. **Loss circuit breaker.** **[panel]** Max daily loss / drawdown per agent and account-wide trips the kill switch. Size caps do not stop an agent grinding the account to zero overnight.
26. **Kill switch — per agent and global.** Pauses new orders **and cancels resting orders**. State persists across restart. Agents receive typed `trading_paused`. Persistent banner. Flatten is decoupled: per position, with confirm.
27. **Dead-man's switch — armed per container, N times, not once.** **[panel]** HL `scheduleCancel` is **per address**: at least 5 seconds ahead, and a **maximum of 10 triggers per day per address, resetting 00:00 UTC** ([decisions.md](decisions.md) O1). N containers is N independent arming duties and N separate 10-trigger budgets. Both halves matter: one container burning its ten does not consume another's, and no container is covered by another's arming — an unarmed container is unprotected however many of its siblings are armed. The policy that follows: arm a container only while it holds a position or a resting order; refresh on a deliberate timer to maintain coverage and bound signed traffic. Current venue documentation counts scheduled firings, not ordinary refreshes (the 2026-09-08 correction to O1); repeated outages that allow firing can exhaust the allowance. Show confirmed or uncertain remaining triggers and the next 00:00 UTC reset **per container** in the risk console; a local clock or refresh count alone is not venue proof. State plainly what it does not do — a trigger cancels that one address's resting orders and **does not close positions**, and it reaches no other container. Quit dialog when positions are open, with a cancel-all option. Whether `scheduleCancel` is itself volume-gated is **not confirmed**; if it is, a freshly provisioned container has no dead-man at all, so the console shows arming state per container and never renders coverage it has not confirmed.
28. **Approval mode.** Default ON for new agents. `place` returns `{status: pending_approval, approval_id, expires_at}`; proposals carry a TTL and auto-expire; re-priced at approval time with drift shown; typed approved/rejected/expired events; pending proposals visible in `get_state`. All agent actions queue. *Built last in v1.*
29. **Hash-chained audit ledger.** Each row commits to the previous row's hash; verified on export; "chain broken" banner. Tamper-evident, not tamper-proof.
30. **Reasons are claims.** Agent `reason` strings render as inert plain text, labelled as agent-authored.

## E · Operator console (`apps/desktop`)

31. **Trade / Watch.** Activity stream with rejection explainability (guardrail vs venue vs auth, attempted vs limit, inline link to edit). Chart: lightweight-charts, candles + volume, agent entries/exits/reasons as annotations, "follow agent" toggle (human-owned). Features panel (spec F). Manual ticket with preflight line.
32. **Agents / Control.** Roster card per agent: the venue account it owns (venue, address, sub-account or top-level), real PnL, status with last-seen and idle-with-open-position alert, guardrail utilization. Policy panel. Approvals queue. Risk console: aggregate exposure across containers — summed, never netted across venues (D1) — guardrail editor, per-container rate budgets and dead-man arming state (items 10, 27), feed health, kill switches.
33. **Manual escape hatch.** Basic ticket, close per position, cancel per order. Manual actions land in the ledger under `manual · external` — the one bucket that routinely shares an address with an agent, so D1's shared-address guards apply to it.
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
- Fleet crowding + `FLEET_CAP` guardrail across agent accounts (D1), summed across venues because nothing nets there.
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
