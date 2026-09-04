# Portfolio tab: durable trading history

**Component specification, v0.1 — draft**
Scope: v1.1, building on the P2 ledger

---

## 1. The question this answers

The Portfolio tab should show the full trading history. The hard part is not the
table. It is that **the venue does not keep your history for you**, so if oppen
does not capture a fill, that fill is gone from your records permanently.

Three facts drive the whole design:

1. `userFillsByTime` is windowed and capped — the venue returns a bounded number
   of fills per query and does not serve an unbounded archive. History older than
   the retention window cannot be recovered later.
2. Fills happen while oppen is closed, asleep, or disconnected. They happen from
   the Hyperliquid web app, from another tool, and from liquidations that no agent
   requested.
3. Containers and agent wallets rotate. An agent retired six months ago still has
   PnL that belongs in the operator's history, and an agent that later moved from
   a top-level account onto a sub-account
   ([venue-containers.md](venue-containers.md) §3) has history under two
   addresses that both stay in the record.

So: **the local database is the only durable record, and its completeness is a
property that must be actively maintained**, not assumed.

---

## 2. Two sources, one truth

| Source | Authoritative for | Not authoritative for |
|---|---|---|
| **Venue** (`userFillsByTime`, `clearinghouseState`) | What happened: fills, prices, fees, funding, liquidations | Why it happened |
| **Ledger** (D6, hash-chained) | Why it happened: intent, agent, reason, guardrail verdict, approval | What actually filled |

The reconciler joins them on `cloid` where oppen placed the order, and on
`(oid, tid)` otherwise. The join produces one of three outcomes per fill:

- **Attributed** — matched to a ledger intent. Carries agent, workflow, reason.
- **Manual** — matched to an operator action taken in oppen's own ticket.
- **External** — no matching intent. Came from outside oppen, or from a
  liquidation. Bucketed as `manual · external`, which `../spec.md` item 33
  already reserves.

A fill is never dropped for failing to match. An unmatched fill is a finding, not
an error: it means either the operator traded elsewhere or oppen missed an event,
and the two are distinguished by whether the ledger has a gap over that window.

**The account a fill arrived on does not name the agent.** Attribution is by
`cloid` and ledger intent, never by address. D1 binds one container to one agent,
but the operator's own manual ticket trades in a container too — the testnet
runbook puts the manual ticket in the same account as the first agent — and
`../spec.md` item 33 routes those fills to `manual · external` by design. So two
fills on one address can belong to an agent and to the operator, and only the
join separates them. The converse holds after a container migration: one agent's
fills are spread across every address it has held, so per-agent totals are a
union over addresses ([venue-containers.md](venue-containers.md) §3.5). Neither
direction is one-to-one, which is why the `account` column is a fact about where
a fill landed and never a substitute for the join.

---

## 3. Making it persist

### 3.1 Storage

SQLite in the OS application-data directory, one file, WAL mode. The ledger and
the history projection live in the same database so a backup is one file and a
join never crosses a process boundary.

Tables added beyond the P2 ledger:

```
fills            tid PK, oid, cloid, coin, side, px, sz, fee, fee_token,
                 builder_fee, closed_pnl, start_position, dir, crossed,
                 ts_ms, account, source(attributed|manual|external),
                 ledger_seq NULL, ingested_at, hash_prev, hash

funding_payments account, coin, ts_ms, usdc, rate_1h, position_sz

positions_closed lifecycle rows: open_ts, close_ts, coin, account,
                 max_sz, entry_vwap, exit_vwap, realized_pnl,
                 funding_paid, fees_paid, agent, workflow, thesis_ref

backfill_state   account, cursor_ms, complete_from_ms, last_run_ms
```

`fills` is append-only and hash-chained on the same scheme as the ledger, for the
same reason: tampering must be detectable. `positions_closed` is a derived
projection and may be rebuilt from `fills` at any time. `account` in every one of
these tables is a container address ([venue-containers.md](venue-containers.md)
§1), which on Hyperliquid v1 is a top-level account rather than a sub-account.

**The container registry is part of the durable record, not an index.** Under the
revised D1 a Hyperliquid container is a top-level account, and no info endpoint
links it back to the account that funded it — [../decisions.md](../decisions.md)
records this as the revision to R3: "there is nothing to discover for a top-level
container… the registry oppen writes at provisioning time is the only record
oppen has". Nothing at the venue can rebuild the list of addresses oppen ought to
be backfilling. Losing the registry therefore loses the history of every
container it named, which is the same class of loss as missing a fill, and the
reason the registry lives in the same file and the same backup as `fills`.

### 3.2 Backfill

On first run for an account, and after every gap:

1. Read `backfill_state.complete_from_ms`. If absent, start at now.
2. Page **backwards** with `userFillsByTime(start, end)` in windows, narrowing
   until each window returns fewer rows than the cap, until the venue returns
   nothing older.
3. Record `complete_from_ms` = the oldest timestamp proven contiguous. Everything
   before it is explicitly marked unknown rather than assumed empty.
4. Page **forwards** from the last known fill to now, closing the gap created
   while oppen was down.

The Portfolio tab shows `complete_from_ms` as "history complete since ..." and
labels anything earlier as partial. An honest boundary beats a chart that starts
at an arbitrary date and implies that is when trading began. With N containers
there are N boundaries; the tab shows the per-container date and, for any total
spanning containers, the **oldest date at which every container in the total is
complete**. A single headline date computed from the best container would be a
claim about data oppen does not have.

**Backfill is per container, and so is the request budget.** `backfill_state` is
keyed by account, so a roster of N containers runs N independent backfills. Each
Hyperliquid address carries its own budget — `userRateLimit` is queried per
address, 1 request per 1 USDC traded on a 10,000-request initial buffer
([../spec.md](../spec.md) item 10) — so N top-level containers do not compete for
one allowance. That is a property of the v1 shape and not of containers in
general: whether a *sub-account's* budget aggregates at its master is
**unconfirmed** ([venue-containers.md](venue-containers.md) §6, question 2), and
if it aggregates, this paragraph stops being true for any agent that takes the
§3 upgrade.

Two consequences worth having in writing:

- **The budget scales with notional; the backfill scales with fill count.** They
  usually move together — a container with a year of fills has, by construction,
  traded enough notional to have earned budget, and a container that has traded
  little has little to page back — but they are not the same quantity. A
  container that traded many small fills earns less budget per request spent
  paging them back than one that traded few large ones. Convenient shape, not a
  guarantee: the backfill respects the budget manager rather than assuming
  headroom.
- **N address budgets sit under one machine.** Every container's backfill leaves
  from the same process and the same IP. Nothing read for this document
  establishes where an IP-level REST limit sits relative to N per-address
  budgets, so treat the per-address numbers as N ceilings beneath one unmeasured
  shared ceiling, and make the backfill scheduler yield the same way it yields to
  risk-reducing requests.

**What N containers do to D-d.** [../decisions.md](../decisions.md) D-d chose
"30 days foreground, the rest in the background" when the design still assumed
one account per operator. The depth is not what changed; the fan-out is. The
foreground cost is now N × 30 days of paging, paid before the tab is useful, and
it grows by one container every time the operator onboards an agent. Two ways
out, neither of them decided here: foreground only the containers the operator is
actually looking at — or that hold an open position — and background the rest; or
keep 30 days foreground for all N and page the containers concurrently against
their separate budgets, which is only defensible once the shared ceiling in the
previous paragraph has actually been measured. D-d needs re-taking for N
containers before the Portfolio tab ships. Recorded as open decision 1.

### 3.3 Gap detection

Every WS disconnect writes a gap record with its start and end, **per container**:
`feed_gaps` is keyed by scope and a container is a scope, so one disconnect opens
one gap per subscribed container and the one-open-gap-per-scope index stays
correct ([venue-containers.md](venue-containers.md) §3.5). On reconnect, the
reconciler backfills exactly those windows before the UI drops its stale overlay.
The P2 gate — zero fills lost across a 30-second disconnect — is the same
machinery; this feature extends it from seconds to the arbitrary downtime of a
laptop that was closed for a week.

### 3.4 Retired containers

An agent's container keeps producing history until it is emptied, and its history
stays relevant afterwards. Containers are never deleted from the local database
when an agent is retired; they are marked inactive, excluded from live views by
default, and included in all-time totals. Deleting an agent in the UI deletes the
pairing, not the past.

**Retirement is a local standing, not a venue state.** No container can be
deleted at any of the three venues — a Hyperliquid address is permanent, Lighter
states "sub accounts cannot be deleted", and Aster does not support deletion
([../threat-model.md](../threat-model.md), "Retirement is revocation, not
deletion"). `Standing::Retired` is therefore oppen's word about an account that
still exists, still holds whatever was left in it, and can still receive a fill.
A retired container is excluded from live views, **not** from reconciliation: it
keeps its `backfill_state` row, and a fill that arrives on it is ingested and
shown as belonging to a retired container rather than dropped. Anything else
would make the completeness property in §1 conditional on the operator's UI
choices.

**A migrated agent has two rows, not a rewritten one** — the old container
retired, the new one active ([venue-containers.md](venue-containers.md) §3.5).
Per-agent totals select over both. Rewriting the address on the existing row
would detach every fill that address produced, which is why V5's "re-point the
registry row" is realised as an insert beside a retirement rather than an update.

### 3.5 Backup and export

- **Export**: CSV and JSON Lines for `fills`, `funding_payments` and
  `positions_closed`, plus the hash-chain verification report. This is also the
  tax-and-accounting path, which is the most common reason anyone opens a
  history tab.
- **Verify**: a command that walks the chain and reports the first broken link.
  `../spec.md` item 29 already requires this for the ledger; it extends to fills.
- **Backup**: the database file is portable. Document where it lives, in the
  README, next to the keychain note.

---

## 4. What the tab shows

Four views over one dataset.

**Positions** — current, as today.

**History** — closed position lifecycles, one row each: symbol, direction, size,
entry and exit VWAP, holding period, realized PnL split into price, funding and
fees, the agent or workflow responsible, and a link to the thesis that opened it.
This is the view that answers "what did this agent actually do", and it is the
one the current Portfolio tab lacks entirely.

**Fills** — the raw stream, filterable by account, symbol, source and window. The
audit surface. Every row links to its ledger event and, through it, to the intent
and the guardrail decision.

**Analytics** — cumulative PnL, PnL by agent and by symbol, funding paid versus
earned, fee drag, and win rate with `n` and a standard error on every statistic.
`../spec.md` section F already forbids an ungated Sharpe; the same rule applies
here. A win rate over eleven trades is not a number, and the UI says so by
printing `n` next to it rather than by hiding it.

**PnL decomposition is not optional.** Price, funding and fees are separate
columns everywhere. On a perp venue a strategy can be right about direction and
still lose to carry, and a single net number hides exactly the failure that
matters most.

---

## 5. Correctness rules

- **Realized PnL comes from `closedPnl` on the fill**, not from a local
  recomputation, so oppen agrees with the venue. Local recomputation runs anyway
  as a cross-check and raises a discrepancy banner when it disagrees, the same
  pattern as the mark-price divergence check in [fair-value.md](fair-value.md).
- **Funding is a separate ledger of its own.** It accrues without a fill and is
  invisible if you only read fills.
- **Fees include the builder fee**, listed separately, because oppen charges it
  and hiding it in a total would be indefensible for a project whose pitch is
  bounded authority.
- **All money is decimal**, never `f64`, on every path that reaches a stored
  number or a rendered one.
- **Timestamps are venue milliseconds**, stored raw, rendered in the operator's
  timezone with the zone shown.

---

## 6. Acceptance gate

Close oppen for 24 hours while trades execute from the Hyperliquid web app.
Reopen. The Portfolio tab shows every one of those fills, bucketed as
`manual · external`, with no gap in the fill sequence for any recorded container
and an unbroken hash chain. Export to CSV and reconcile the total against the
venue's own fill list, to the cent.

Run it with **at least two containers** on the roster and trade in both. One
container exercises none of the per-container fan-out — separate backfills,
separate budgets, separate completeness dates, one gap per scope (§3.2, §3.3) —
and that fan-out is where the container model actually changes this feature.

---

## 7. Open decisions

1. **How the foreground backfill behaves across N containers.** The depth is
   settled: [../decisions.md](../decisions.md) D-d chose 30 days immediately and
   the rest as a background job that respects the budget manager and can be
   paused. What D-d did not settle is the fan-out, because it was taken for a
   single account (§3.2). Recommend: foreground the containers that hold an open
   position or an active agent, background the rest, and show a completeness
   date per container instead of one headline date. Not decided, and it is a
   revision to D-d rather than a new decision, so it belongs in decisions.md.
2. **Whether to store the full L2 snapshot** at each fill for later TCA. Valuable
   for `slip_bps` attribution, expensive on disk. Recommend storing only the
   arrival mid and top-of-book, which `../spec.md` section F already requires.
3. **Aggregation across containers** in the analytics view: a single portfolio
   number spanning containers is the same portfolio-level view the whitepaper
   defers past v1. The revised D1 changes what there is to aggregate over. There
   is no master whose sub-accounts get rolled up — on Hyperliquid v1 the roster
   is N top-level accounts, several of which are masters by construction
   ([venue-containers.md](venue-containers.md) §3.5) — so the set is whatever
   the registry holds, and the number is a **sum** across containers, never a
   net, because nothing nets across addresses and nothing nets across venues
   ([../spec.md](../spec.md) D1).
