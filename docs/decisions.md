# Decision log

Product and scope decisions taken outside the settled architecture set. D1–D8 in
[spec.md](spec.md) are the architecture decisions and are not re-litigated here;
this file records everything else, with the reasoning, so a later reader can tell
what was chosen deliberately from what was never considered.

Format: one row per decision, newest section first. A decision is only in this
file once it has been taken. Open questions live in the relevant spec's "Open
decisions" section until then.

---

## 2026-09-03 · Interview

Twenty decisions taken in one sitting, after the six feature specs in
[specs/](specs/) were written.

### Sequencing

| # | Decision | Chosen | Why |
|---|---|---|---|
| S1 | What to build after P1 | **P2 as planned** — WS pool, reconcile, hash-chained ledger | Everything queues behind it. History is a ledger projection, workflow run state lives in it, charts need the trades and candles feeds, and the fair value sampler needs a fixed clock over live components. |
| S2 | Mainnet readiness | **After P3** — guardrails, loss breaker, kill switch, dead-man, with the no-bypass property proven by a test | The whitepaper's central claim is only true of a build that has P3. Going to mainnet before it means the claim is not true of the build being used. P4 and P5 are not required; the CLI example can drive real money once the limits are real. |
| S3 | Public release gate | **All of P7** — fresh machine to a testnet trade in ten minutes | The repo is already public and anyone can build from source. A released binary with a checksum is a promise, and for a product whose pitch is safety the first binary is the promise that matters. |
| S4 | Approval mode | **Stays in v1, built last** | It is what makes "new agents start with approval mode ON" true, and that sentence is in the spec, the README and the pairing flow. It is also what makes oppen demonstrable to someone who does not trust it yet. |
| S5 | Paper trading | **No paper broker** — testnet is the paper mode | Testnet gives real fills, real rejections, real latency and real funding, and exercises the real signing path. A paper fill model is an assumption that would have to be maintained and would be optimistic in exactly the ways the assumptions are wrong. `workflows.md` §8 becomes "arm on testnet, promote to mainnet"; the execute node has one implementation. |
| S6 | Community signal layer | **Parked until after v1** | It is the only feature touching the architecture's central public claim, and it needs users before it needs code — a feed with three publishers looks abandoned. The spec stays written and costs nothing to defer. |
| S7 | Mobile companion | **v2, read-only first** | The phone reads public venue state from the account address alone: no keys, no backend, no pairing. The cancel-only panic wallet is a second signing key on a second device and has to be earned by the read-only version proving the pattern. |

### Interface

| # | Decision | Chosen | Why |
|---|---|---|---|
| U1 | Primary chart renderer | **ASCII, character grid** | One visual language, no foreign object in the shell, deterministic and snapshot-testable, ports free to mobile, and direction is encoded twice. Resolves the blocker on P5 and supersedes `lightweight-charts` in [spec.md](spec.md) item 31. |
| U2 | Canvas fallback | **Dropped entirely** | Zero chart dependencies in an auditable local-first binary. "Not enough resolution" is really "change the interval", which [charts.md](specs/charts.md) §3 already builds and which is one keystroke. An optional second renderer becomes a parity obligation for every overlay. |
| U3 | Up/down colour | **`--up: #2fbf71`, `--down: #e5484d`** | Measured: 8.3:1 and 5.0:1 on void, both clearing AA for graphics with margin. `--down` is deliberately dimmer than `--hazard` so hazard keeps its exclusive meaning. In greyscale the pair separates 5.8:1 to 4.4:1, and the glyph (`+` / `:`) already carries direction independently. |
| U4 | Quantoppen placement | **Its own tab** | Fourteen columns across fifty symbols needs full width, and it is the surface left open on a second monitor. Trade is about one symbol; Quantoppen is about many. |
| U5 | Workflows placement | **The existing Builder tab** | The design mock's Builder (source, policy, instructions, testnet run, arm) is already a workflow definition without a graph in it. No new tab, no navigation change, and the design work is largely done. |

### Money

| # | Decision | Chosen | Why |
|---|---|---|---|
| M1 | Builder fee schedule | **Volume-tiered: 0.3 / 0.2 / 0.1 bp** | Breaks at $1m and $25m of routed notional. `f` = 3, 2, 1 in tenths of a basis point. Most users never leave the first tier, which is why 0.3 is the number that matters. |
| M2 | Signed fee cap | **1 bp** (`maxFeeRate: "0.01%"`) | Roughly three times the entry rate: enough headroom to adjust without asking anyone to re-sign, small enough that the gap between the cap a user approves and the rate they pay stays defensible. The venue's own maximum for perps is 0.1%, so this is well inside it. |
| M3 | Volume source for tiering | **The local ledger** — sum of notional oppen actually routed | Free, aggregates across sub-accounts by construction, works offline, and measures exactly the thing being charged for. Editable by a determined user, which D7 already concedes: the source is open, so anyone willing to edit the database would simply fork and set `f` to zero. |
| M4 | `collateral_apr` | **4.5%** | The opportunity cost of USDC sitting as margin rather than in T-bills. Makes `carry_edge_apr` answer whether funding is worth tying up capital, not merely what funding pays. Must never default to the venue's hardcoded `0.01%/8h`, which is a mechanism constant and not a carry estimate. |

### Data and keys

| # | Decision | Chosen | Why |
|---|---|---|---|
| D-a | Master ceremony wallet | **MetaMask** | Settles an open question in [hl-signing.md](hl-signing.md): the app reads the chain id from the live WalletConnect session and supports Arbitrum One (`0xa4b1`) and Arbitrum Sepolia (`0x66eee`). |
| D-b | Agent wallet expiry | **90 days, warn from 14** | Long enough not to be a chore, short enough that an abandoned deployment stops trading within a quarter. At expiry: signing fails, oppen halts the agent and cancels resting orders, and **leaves positions open** — force-closing on a calendar event is a destructive action triggered by a clock. Makes "authority decays by default" literally true. |
| D-c | Default guardrails for a new agent | **Near-zero** | Empty symbol allowlist, $25 orders, $100 positions, $25 daily loss, 5 orders per 5 minutes, approval mode on, testnet. The first order is refused with a reason naming the limit to raise: the refusal is the onboarding. Default-deny all the way down. |
| D-d | First-run history backfill | **30 days foreground, the rest in the background** | The tab is useful in seconds; the background job walks backwards until the venue serves nothing older, yields to any risk-reducing request, and is pausable. The UI states the date from which history is provably complete. |
| D-e | Retention | **Never delete records; prune recomputable inputs** | Ledger events, fills, funding payments, closed positions, refusals and approvals are kept forever, because a gap in a hash chain is indistinguishable from tampering. 1-second fair value samples (default 7 days) and locally-aggregated sub-minute bars (default 30 days) are prunable; book snapshots are not stored at all. |

### Runtime and records

| # | Decision | Chosen | Why |
|---|---|---|---|
| R1 | Where a workflow runs | **Headless-capable core, same machine** | A cron with second resolution is fiction if nothing is awake at 05:00, and `position-guardian` watches nothing on a closed laptop. The constraint starts now: `oppen-core`, `oppen-hl` and `oppen-mcp` keep zero Tauri dependencies so the core can later run as a daemon with the UI as a client. This is already true on `main`; the decision is to keep it true. Nearly free before P2 lands, expensive after. |
| R2 | Sub-account ownership | **Schema carries an owner discriminator; the product rule stays open** | `owner_type ENUM('agent','workflow')` plus `owner_id`. The column is free today and impossible to add cleanly once the ledger has rows referencing sub-accounts. Whether a workflow gets its own sub-account or binds to an agent's is a product question that real usage will answer better than reasoning. |
| R3 | Which accounts get recorded | **Discover all, opt in per account** | oppen discovers every sub-account under the master and the operator ticks which ones it records. Default off for anything oppen did not provision: it watches what it made and asks before watching you. |
| R4 | Network isolation | **One database file and one hash chain per network** | Testnet and mainnet agents will run simultaneously. D6 makes the monotonic rowid the agent's `get_events` cursor, so a shared file means testnet row 4,812 and mainnet row 4,812 are the same cursor position. A mainnet number that is actually a testnet number is the worst bug this product can ship, and a file boundary makes it unrepresentable rather than filtered. |
| R5 | Hash chain scope | **Record of record chained, over content hashes** | Intents, decisions, refusals, fills and operator actions are chained. Candles, book snapshots, sub-minute bars and projections sit in unchained side tables with a disk budget. The chain commits to `hash(payload)` rather than the payload, so a row can be tombstoned without breaking verification — narrowing a chain later means rehashing, which destroys the property it exists for. |
| R6 | Decision-time market context | **Snapshot by reference** | Orders, fills and approvals store a nullable `snapshot_id` and `snapshot_hash`; the hash goes in the chained row and the body lives in a prunable `book_snapshots` table. Roughly forty lines at P2, no capture policy decided yet. The book at the moment an agent decided is the one class of data that cannot be backfilled, so the plumbing has to exist before the policy does. |

### Scope and risk posture

| # | Decision | Chosen | Why |
|---|---|---|---|
| P1 | v1 cut line | **P0–P7, console minimal** | P5's gate changes from "parity with the design" to a named list: activity stream, rejection explainability, roster with last-seen, staleness overlay, kill switches. Chart annotations, OS notifications and full parity move to v1.1. P6 quant stays, because it is what makes the MCP surface worth connecting to. Parity is a judgement, not a test, and an ungated judgement resolves as drift. |
| P2 | Unattended live trading | **Earned, after a clean record** | A workflow definition unlocks live-unattended execution only after N clean testnet runs with no guardrail trips. Editing the definition resets the counter, because a fork is a new definition. The unlock is a query against the ledger that already exists, not a new subsystem. |
| P3 | Template prompts | **Responsibilities, never thresholds** | "Try to falsify this thesis" ships. "Enter when annualised funding exceeds 12% for three consecutive intervals" does not. The design mock's Builder currently displays exactly that rule and must be rewritten before it goes public, because it contradicts both `specs/workflows.md` §3 and the whitepaper's "no alpha, no signals, no default agent behaviour". |
| P4 | Chart annotations | **Fill marks on the chart, reasons in the stream** | Uranium marks carry the fact; the stream carries the words, already labelled agent-authored and inert. On a character grid an untrusted `reason` string is made of the same characters as the chart itself, so escaping HTML does not stop an agent writing a label that reads as a price row or an axis rule. Follow-agent mode becomes the v1.1 upgrade. |
| P5 | Builder fee custody | **A dedicated hardware-wallet EOA** | Used for nothing else. The address is compiled into official builds, so changing it later silently stops revenue until every user re-signs; its custody has to be what you would choose at ten times the revenue. A Safe or any smart-contract wallet is disqualified: fees are withdrawn by that address signing for itself, and Hyperliquid cannot verify ERC-1271. |

### Fair value scope — decided by evidence, not preference

| # | Decision | Chosen |
|---|---|---|
| F1 | Engine scope | **Carry, basis and micro components with the full combination engine and all five MCP tools. Mark is consumed, not replicated.** |

An audit of the live Hyperliquid API settled this. See [specs/fair-value.md](specs/fair-value.md)
§14 for the evidence and the twenty-four corrections it produced. The short version:

- Every input the carry, basis and micro components need is live today from
  Hyperliquid alone, and §3.1's premium formula reproduces the published
  `premium` to under 1e-9 on 177 of 177 live mainnet assets.
- The funding constants hold across 4,627 mainnet records with zero dead-zone
  violations on either edge.
- **Mark replication is unreachable at any configuration**, so §10.1's gate is
  replaced rather than deferred. Even granting perfect clock alignment and a
  perfect component selector, the residual is 3.09 bp at p99 on HYPE. The venue's
  sampling instants are unobservable, and 52 of 233 assets have a single mark
  tick wider than 1 bp.
- The five centralised-exchange feeds would break the local-first posture to buy
  a worse estimate of a number Hyperliquid already publishes for free.

### Still open

Everything listed under "Open decisions" in each spec that is not resolved above.
The near-term ones: how many watched symbols Quantoppen actually supports given
the depth measurement problem in §14, and whether the sub-account owner rule
(R2) lands on agent or workflow.
