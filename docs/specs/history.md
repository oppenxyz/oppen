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
3. Sub-accounts and agent wallets rotate. An agent retired six months ago still
   has PnL that belongs in the operator's history.

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
projection and may be rebuilt from `fills` at any time.

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
at an arbitrary date and implies that is when trading began.

### 3.3 Gap detection

Every WS disconnect writes a gap record with its start and end. On reconnect, the
reconciler backfills exactly that window before the UI drops its stale overlay.
The P2 gate — zero fills lost across a 30-second disconnect — is the same
machinery; this feature extends it from seconds to the arbitrary downtime of a
laptop that was closed for a week.

### 3.4 Retired accounts

An agent's sub-account keeps producing history until it is emptied, and its
history stays relevant afterwards. Sub-accounts are never deleted from the local
database when an agent is retired; they are marked inactive, excluded from live
views by default, and included in all-time totals. Deleting an agent in the UI
deletes the pairing, not the past.

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
`manual · external`, with no gap in the fill sequence and an unbroken hash chain.
Export to CSV and reconcile the total against the venue's own fill list, to the
cent.

---

## 7. Open decisions

1. **How far back to backfill on first run.** Full available history is the
   honest default and can be thousands of requests against the rate budget.
   Recommend: 30 days immediately, then the rest as a background job that
   respects the budget manager and can be paused.
2. **Whether to store the full L2 snapshot** at each fill for later TCA. Valuable
   for `slip_bps` attribution, expensive on disk. Recommend storing only the
   arrival mid and top-of-book, which `../spec.md` section F already requires.
3. **Multi-account aggregation** across master and sub-accounts in the analytics
   view: a single portfolio number spanning sub-accounts is the same
   portfolio-level view the whitepaper defers past v1.
