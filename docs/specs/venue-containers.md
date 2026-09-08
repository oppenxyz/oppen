# Venue containers and the sub-account upgrade path

**Component specification, v0.1 — draft**
Scope: v1 · P2 for the model; v2 for Aster and Lighter

What oppen binds an agent to at a venue, why the answer differs per venue, and
how a container is **upgraded** when a venue later grants a better primitive.

D1 was revised on 2026-09-04 to make the unit of isolation venue-agnostic. That
decision and its reasoning are recorded in [../spec.md](../spec.md) D1 and
[../decisions.md](../decisions.md) V1–V6 and are not restated here. The ceremony
each venue demands is [onboarding.md](onboarding.md). This document owns three
things those do not: what a container has to supply and where each venue's
primitive falls short (§1), what the four container types cost and cap (§2), and
the migration itself — preconditions, ordering, and what it does to an
append-only ledger (§3).

---

## 1. The container abstraction

A **container** is one venue account bound to exactly one agent. It is the unit
of isolation. Every claim oppen makes about an agent's blast radius is a claim
about its container.

### 1.1 What oppen needs from one

| Requirement | Why | What breaks without it |
|---|---|---|
| **Isolated margin** | An agent's worst case must be its own balance | One agent's loss consumes another's collateral, and guardrail caps stop bounding anything |
| **Isolated positions** | Position, liquidation price and funding must be venue truth, not arithmetic | Netting makes per-agent PnL fiction. Funding is the unrecoverable case: at net zero no cash moves, so there is nothing to attribute ([../decisions.md](../decisions.md) V3) |
| **Venue-confirmed PnL** | `closedPnl` comes from the venue ([history.md](history.md) §5) | oppen and the venue disagree and there is no tiebreaker |
| **An independent nonce set** | Concurrent tool calls from different agents must not collide ([../spec.md](../spec.md) item 7) | Silent rejection under concurrency, at the worst possible moment |
| **A signing key that cannot withdraw** | Where a venue grants it, the one containment property that survives a compromised host — Hyperliquid and Aster do, Lighter does not ([../threat-model.md](../threat-model.md), stated per venue) | Guardrails become the only defence, and they are containment, not a boundary |

### 1.2 Which primitive supplies what

| Requirement | HL top-level | HL sub-account | Aster sub-account | Lighter sub-account |
|---|---|---|---|---|
| Isolated margin | Yes — own address | Yes — own address | Yes — "its own positions, assets, and API keys" | Yes — own account index |
| Isolated positions | Yes | Yes | Yes | Yes |
| Venue-confirmed PnL | Yes | Yes | Yes | Yes |
| Independent nonce set | Only via its own API wallet | Only via its own API wallet | Yes — own API keys | Yes — own account index |
| Key reaches this container only | **Certain** | **Unconfirmed** — see below | Yes — keys are per sub-account | Yes — keys are per account index |
| Key cannot withdraw | Yes, with one exception | Yes, with one exception | Yes — `canWithdraw: false` is a real setting | **No — weaker.** See below |

Five shortfalls, stated rather than smoothed over.

**Nonce independence is a property of the signer, not of the container.**
Hyperliquid's docs: "a single API wallet signing for a user, vault, or subaccount
all share the same nonce set", and "If users want to use multiple subaccounts in
parallel, it would easier to generate two separate API wallets under the master
account, and use one API wallet for each subaccount"
([../hl-signing.md](../hl-signing.md) §8). A container satisfies the fourth
requirement only when it is paired 1:1 with its own API wallet. That pairing is
part of the container, not an optimisation on top of it.

**On a Hyperliquid sub-account, whether one agent key reaches one container is
unconfirmed.** `DOC-NONCE` says API wallets sign "on behalf of the master account
or any of the sub-accounts"; `DOC-SUB` frames the allowance as 2 additional API
wallets per sub-account, which reads as per-sub-account scoping. The two are not
obviously compatible, and which is true decides whether a stolen agent key
reaches one container or all of them ([onboarding.md](onboarding.md) §3.9). On a
top-level container the answer is certain, because an API wallet never signs for
an unrelated top-level account. This is the single most important unknown in this
document: it is the one that could make §3 a downgrade.

**Lighter's keys can withdraw, to one address.** From the API docs: "API keys
provide both read and write permissions… as well as the ability to send
transactions and process withdrawals. Secure withdrawals can be executed without
signing the account's Ethereum private key if they are sent to the same L1
address. Fast Withdrawals and Transfers to other L1 addresses require signing
with the wallet's private key." A stolen Lighter trading key therefore moves
funds **to the owner's own L1 address**, not to an attacker's. That is a strong
mitigation and it is not the flat "cannot withdraw" that holds on Hyperliquid and
Aster. It must be written that way wherever the property is claimed; the
per-venue key table in [../threat-model.md](../threat-model.md) already is.
Read-only tokens (no trades, no withdrawals, expiry 1 day to 10 years) and
maker-only keys (Premium tier) exist and do not change the picture for a trading
key.

**Hyperliquid's exception: same-address asset movement.** `agentSendAsset` is
documented as "Similar to send asset, but can be signed by an agent. Destination
must match the source address." An agent key can therefore move collateral
between the same address's perp and spot balances. That is not a withdrawal — it
cannot reach another address, and `withdraw3` stays user-signed — but it is a
movement out of the perps margin pool that oppen did not initiate, and it belongs
beside "cannot withdraw" rather than folded into it.

**Hyperliquid, unconfirmed: `subAccountTransfer` may be agent-signable.**
`createSubAccount` and `subAccountTransfer` are **L1 actions**, and
[../hl-signing.md](../hl-signing.md) §1 records the signer for that whole class
as "agent (API) wallet or master"; its open question 3 records that no page
states which actions an API wallet is barred from. If an agent wallet can sign
`subAccountTransfer`, an agent could move USDC between a master and its
sub-accounts — which would make rebalancing ceremony-free and would also be a
capability no guardrail currently models. It needs a testnet negative test before
mainnet.

### 1.3 What a container is not

**Not cross-venue.** Positions on different venues never net
([../decisions.md](../decisions.md) V6). Three venues means three margin pools,
three liquidation prices and no cross-margin. An economically flat book posts
full initial margin on both legs, and one leg can liquidate while the other
survives. No venue can see the others, so a cross-venue exposure cap is an
oppen-enforced guardrail with no venue behind it — containment, not a boundary,
and labelled that way wherever it is shown.

**Not a hedge.** Hyperliquid has no hedge mode: of `updateIsolatedMargin`'s
`isBuy`, "this parameter won't have any effect until hedge mode is introduced".
Lighter is inferred to have none, from one position per `(account_index,
market_id)` with a single sign field — read off the data model, never stated in
Lighter's docs. Aster does support it (`POST /fapi/v3/positionSide/dual`),
account-wide, unchangeable with open positions or orders, and not in Shield Mode.
So on two of three venues an agent cannot be long and short the same asset inside
one container, and on the third it is an account-wide mode rather than a
per-order choice.

**Not an identity.** The agent's identity is `agent_id`, a column on every
chained event. A container is the address an agent trades in *at the moment*.
§3.5 is entirely about keeping those two apart.

---

## 2. The four container types

| | HL top-level | HL sub-account | Aster sub-account | Lighter sub-account |
|---|---|---|---|---|
| Created by | The user, as an ordinary wallet account | `createSubAccount` (L1 action) | `POST /fapi/v3/createSubAccount` | `L2CreateSubAccount` transaction |
| Signed by | Nothing — it is an EOA | Master, or possibly an API wallet (§1.2) | **Two signatures**: the child wallet, then the master, both wallet keys and explicitly not API keys | The account's **API key** — no Ethereum key |
| Has its own private key | Yes, the user's, and it can withdraw | **No.** "Subaccounts and vaults do not have private keys" | Yes, a distinct wallet | No — it is an account index |
| Gate | None | **$100,000 traded volume** | None documented for creation (§6) | None; tier caps the count |
| Cap | None | 10, then +1 per $100M, max 50 | 10 at VIP1–2, up to 50 at MM tier 3 | 4 Standard · 16 Plus · 64 Premium |
| Deletable | No — an address is permanent | Not documented | "Deleting not supported" | "Sub accounts cannot be deleted" |
| API keys | 1 unnamed + 3 named, per account | +2 named per sub-account, shared with the master | 30 master / 10 per sub | ~253 per account index (§6) |
| Per-agent ceremony | 3 signatures across 2 wallet accounts | 2–3 signatures, all from one master key | 2 signatures, one of them oppen's own child key | 1 API-key transaction |

Detail on the ceremonies is [onboarding.md](onboarding.md) §2–§5. What follows is
only what bears on the upgrade.

### 2.1 Hyperliquid top-level — what v1 uses

An address becomes a Hyperliquid account by being funded; there is no creation
call. Funding is `usdSend` from an account that already holds USDC — user-signed
EIP-712, internal, and it "does not touch the EVM bridge". Authorisation is
`approveAgent` signed by the **new account's own key**, with the expiry encoded
in the agent name (`valid_until`, capped by the venue at 180 days, set to 90 by
D-b). The builder-fee approval is per account (O7), so it is a third signature
per agent rather than a one-time ceremony.

**Fund first, then authorise.** Both approvals are signed by the container
itself, and Hyperliquid rejects actions from an address that has not deposited
(`Must deposit before performing actions. User: 0x123...`), so the funding
transfer comes first. The rule, the evidence behind it and how strong that
evidence is are settled once in [onboarding.md](onboarding.md) §3.3 and are not
re-argued here.

**The recurring cost is not the signatures. It is the keys.** N agents means N+1
wallet accounts the operator creates, funds, backs up and holds, and **every one
of those account-owner keys can withdraw** ([../decisions.md](../decisions.md),
"The container's own key"). They live in the user's wallet, never in oppen, which
is what keeps D5 intact — but the operator's withdrawal-capable key count grows
linearly with the roster.

**Designate the future master before the first trade — and it need not be the
funding account.** The $100,000 gate meters *one account's* volume, and volume
does not transfer between accounts, so N containers each trading V/N reach the
gate only at V ≥ N × $100,000. An operator who wants to reach §3 should therefore
concentrate early volume in **one nominated container** rather than spreading it.

An earlier draft of this section drew a stronger conclusion — that the funding
account must host the first agent, because a funder that never trades never
clears the gate and §3 could then never run. **That conclusion is withdrawn.**
The gate is per user and every container is its own Hyperliquid user, which is
why each needs its own `approveBuilderFee` (O7); whichever container actually
trades is the one that clears the gate and can then create sub-accounts under
itself. The master of a future sub-account fleet is a *container*, chosen for
where the volume will be, not the account that happens to hold the deposit.

Whether the funding account may nevertheless be a container is settled in
[onboarding.md](onboarding.md) §3.2 — it may, it is never the default, and it is
presented with the account's balance in dollars because that balance is the
agent's worst case. This document defers to that paragraph.

*Not confirmed:* that a container which is not the original funding account can
create sub-accounts under itself once it clears the gate (§6, question 13). It
follows from the two documented facts above and no page states it for this case.

Each top-level container carries its own baseline of 3 named agent wallets (O6)
and its own request budget — 1 request per 1 USDC traded on a 10,000-request
initial buffer ([../spec.md](../spec.md) item 10). Whether a sub-account gets its
own budget or draws on the master's is unconfirmed (§6), and it is the one axis
on which the upgrade might cost something measurable.

### 2.2 Hyperliquid sub-account — the upgrade target

`createSubAccount { name }` is already modelled in
`crates/oppen-hl/src/action.rs`. The gate: "Up to 10 sub-accounts can be created
after reaching $100,000 in volume. Every additional $100M in volume enables the
ability to create 1 additional sub-account, up to a maximum of 50 sub-accounts."
It is protocol-enforced and was **observed**, not only read — the attempt from
the project's own testnet wallet on 2026-09-04 returned `Cannot create
sub-accounts until enough volume traded. Required: $100000. Traded: $0`.

Funding is `subAccountTransfer { subAccountUser, isDeposit, usd }`, an L1 action
whose `usd` **unit is not documented** — open question 5 in
[../hl-signing.md](../hl-signing.md), and §3.4 step 8 moves money with it.
Orders are signed by an API wallet with `vaultAddress` set to the sub-account
address ([../hl-signing.md](../hl-signing.md) §9); `usdClassTransfer` is the
exception and encodes the sub-account in the amount string instead.

### 2.3 Aster sub-account — v2

Two wallet signatures: the child signs the body, then the master signs
`childAddress={…}&name={…}&nonce={…}&user={…}&childSignature={…}`. Present on
mainnet and testnet, EIP-712 `chainId` 1666.

**The child key is oppen's problem.** Created in Aster's web UI, the sub-account
wallet key is shown once, never stored by Aster, unrecoverable if lost. Created
through the API, the child wallet is one **oppen generates**, which means oppen
holds a key that is not an agent key: it can register agents on that sub-account.
That has no equivalent on Hyperliquid, where sub-accounts have no key at all. It
does not breach D5 — the master key still never enters the app — but it is why
the Aster container is not "the Hyperliquid design with different endpoints", and
[onboarding.md](onboarding.md) §4.5 carries the custody rule.

Agents are registered with `POST /fapi/v3/registerAndApproveAgent`, which takes
`canSpotTrade`, `canPerpTrade`, `canWithdraw` and an expiry, and requires
`ipWhitelist` when `canWithdraw` is true — so `canWithdraw: false` is a genuine
trade-only key. VIP level is computed on the aggregated master+subs group, so
Aster has no fee-tier penalty for splitting and therefore nothing resembling §3's
upgrade.

### 2.4 Lighter sub-account — v2

An `L2CreateSubAccount` transaction signed by the API key, with no Ethereum key
in the loop: the only container oppen can create without a wallet ceremony.
Registering the API key in the first place does require the L1 key
(`ChangePubKey`), which is the one ceremony Lighter does not avoid.

**The cap is readable in advance.** `GET /api/v1/accountLimits` returns
`user_tier` and `user_tier_name`; `GET /api/v1/accountsByL1Address` returns the
`sub_accounts` array with each `index`, `collateral` and `available_balance`. So
none of §3.2's attempt-and-classify machinery is needed here — oppen knows the
cap before it tries. That is the sharpest contrast with Hyperliquid in this
document.

---

## 3. The upgrade path

A Hyperliquid agent starts in a top-level container and may later move into a
sub-account once the operator crosses $100,000 of volume.
[../decisions.md](../decisions.md) V5 settles that this is designed for now and
required never. This section is how.

### 3.1 Why it is an upgrade and not a rewrite

Nothing about the agent changes. Its `agent_id`, guardrails, journal, allowlist,
loss budget, approval setting and history are keyed on the agent, not on the
address. What changes is where its orders route. Designing for that now costs a
column and an event kind; adding it once chained rows reference containers is the
expensive case R2 already describes.

### 3.2 Detection: attempt, then classify

**The gate cannot be pre-checked, and the reason is narrower than it first
looked.** [../decisions.md](../decisions.md) O3 states that no info endpoint
exposes cumulative volume. **That premise is false**, and every sentence
repeating it is being replaced across these documents: `userRateLimit` returns
`cumVlm`, documented as "Cumulative volume", verified live on 2026-09-04 in a
response carrying `{"cumVlm":"188908641154.22","nRequestsUsed":…}`, and oppen's
own type has the field (`crates/oppen-hl/src/types.rs:639`, printed by
`crates/oppen-hl/examples/testnet_order.rs`). The counter exists because
Hyperliquid's request budget is itself denominated in traded volume.

O3's **conclusion** nevertheless survives, on a narrower footing. What is not
documented anywhere read is whether the sub-account gate reads *that* counter, or
whether the counter is lifetime or windowed (§6, questions 3 and 4). So oppen may
show `cumVlm` as an informational distance-to-gate, labelled an estimate the
venue does not confirm, and must still attempt-then-classify rather than
predicting eligibility from it.

| Outcome | Meaning | oppen's action |
|---|---|---|
| `status: ok` | Created | Proceed to §3.4 step 5 |
| Error naming the volume requirement | Gate not cleared | Record a typed `upgrade_unavailable`; carry the raw message as an inert display field |
| Error, any other text | Unknown | Record verbatim, classify as unknown, do not retry automatically |
| Timeout or transport failure | Unknown outcome | Re-read `subAccounts` and look for the name oppen sent. Never blind-retry |

Three rules make that safe.

**Parse `Required:` / `Traded:` for display only.** Showing an operator "traded
$41,208 of $100,000" is worth having; branching on it is not. Venue message text
is not versioned and is part of no contract, so a text match that silently stops
matching turns a refusal into an unknown error — or, in the direction that
actually hurts, into a success path. [AGENTS.md](../../AGENTS.md) invariant 8
requires every rejection to be typed: the type is oppen's, the string is the
venue's, and the string renders as inert plain text.

**`createSubAccount` carries no cloid, so the only reconcile is re-reading
`subAccounts`.** That is sound only because oppen generates the name and records
it before the attempt, so a timeout resolves by lookup rather than by retry — the
rule [../spec.md](../spec.md) item 19 already sets for
`timeout_unknown_outcome`.

**The attempt is operator-only.** [AGENTS.md](../../AGENTS.md) invariant 3: no
agent-reachable path modifies the agent registry. No MCP tool exposes any step in
§3.4, and an agent cannot ask to be upgraded.

### 3.3 Preconditions

Each is checked against venue state, not against local belief.

1. **The agent is paused** — its per-agent kill switch on for the duration, so
   nothing places into the old container mid-migration ([../spec.md](../spec.md)
   item 26).
2. **Flat.** Zero open positions in the old container, from `clearinghouseState`.
   **This is the precondition that matters.** Hyperliquid documents collateral
   transfers between accounts and no way to move an open position, so a position
   left behind is orphaned: it keeps accruing funding, it can liquidate, and no
   agent is routed to the address that holds it. oppen refuses the migration
   rather than closing the position — force-closing for an operator's convenience
   is the destructive act D-b already declines to take on a calendar event.
3. **No resting orders**, confirmed by `frontendOpenOrders` returning empty after
   `cancel_all`.
4. **No unresolved in-flight order.** Any `timeout_unknown_outcome` is settled by
   cloid first.
5. **No pending approval proposals** for that agent. A proposal is priced against
   a container ([../spec.md](../spec.md) item 28); one that survived the move
   would execute in an account that is no longer the agent's.
6. **No open feed gap** for the old container's scope, and a contiguous fill
   sequence ([history.md](history.md) §3.3). Migrating across an unreconciled gap
   makes a missing fill indistinguishable from one that never happened.
7. **Dead-man budget.** `scheduleCancel` needs a time at least 5 seconds ahead
   and allows a **maximum of 10 triggers per day, resetting at 00:00 UTC** (O1).
   Current documentation counts scheduled firings, not ordinary disarm/re-arm
   operations. Migration must preserve each address's confirmed and uncertain
   schedule outcomes: a missed deadline during migration may have fired, and
   unknown disarm/re-arm results do not prove coverage on either account.

### 3.4 The migration

| # | Step | Signed by | Recorded as | If it fails |
|---|---|---|---|---|
| 1 | Pause the agent | — | `kill_switch_changed` | Abort; nothing has moved |
| 2 | Cancel resting orders, confirm empty | old agent wallet | `order_state_change` | Retry, then abort |
| 3 | Confirm flat and reconciled (§3.3) | — read only | — | Abort with a typed reason naming the position |
| 4 | `createSubAccount { name }` | master, or an API wallet (§1.2) | `operator_action` | Classify per §3.2; abort |
| 5 | Generate the new agent wallet in-app, **distinct name**, `valid_until` ≤ 180 d, 90 d by D-b | — keygen | `operator_action` | Abort; nothing has moved |
| 6 | `approveAgent` for the new wallet | master | `operator_action` | Abort; the sub-account exists, empty and unused |
| 7 | `approveBuilderFee` if the sub-account does not inherit the master's (O7, §6) | master | `operator_action` | The order path prompts the ceremony and never silently drops the order ([../spec.md](../spec.md) item 5) |
| 8 | Move collateral: `usdSend` old → master, then `subAccountTransfer` master → sub | the old account's own key, then master | `operator_action` ×2 | **The one step with a window.** See below |
| 9 | **Append `container_migrated`** | — local | `container_migrated` | The commit point. See §3.5 |
| 10 | Retire the old container row | — local | `operator_action` | Marked inactive, never deleted |
| 11 | Unpause the agent | — | `kill_switch_changed` | — |
| 12 | Delete the old agent key from the keychain | — local | `operator_action` | See below |

**Step 8's window.** Between the transfer landing and step 9 committing, the
funds are in the new container while routing still points at the old one. It is
detectable rather than silent: two containers for one agent, the new one funded,
the old one empty, and no `container_migrated` row between them. It resolves by
re-running step 9, never by moving the money back.

Whether step 8 can be one hop — a `usdSend` addressed directly to the
sub-account's address — is unconfirmed (§6). Two hops is specified here because
both actions are individually documented.

**Step 12 revokes nothing at the venue.** Hyperliquid deregisters a named API
wallet when the **same name** is re-approved (O4), and oppen never reuses a name
or an address — reuse allows previously signed actions to replay once the nonce
set is pruned ([../hl-signing.md](../hl-signing.md) §8). So the old key stays
valid until its `valid_until` passes. Its residual authority is over an empty
account, which is why this is acceptable and why the account must genuinely be
empty first.

### 3.5 What this does to the ledger

**The chain is append-only and is not rewritten.**
`crates/oppen-core/src/ledger/schema.rs` states it as a schema rule: "a migration
can add tables and indices but must never rewrite a chained row." A container
migration is therefore an **event in the chain**, not an edit to anything in it.
Adding an `EventKind` is additive — it changes no existing row's hash preimage,
so `Ledger::verify` is unaffected.

**One new event kind**, `container_migrated`, snake_case like the fourteen that
exist. Its payload names the agent, both containers, the amount moved, and the
sequence numbers that prove the preconditions:

```
{ agent_id, venue,
  from: { kind: "hl_top_level",   address },
  to:   { kind: "hl_sub_account", address, name },
  moved_usdc, flat_confirmed_seq, orders_cancelled_seq,
  new_agent_wallet, agent_wallet_valid_until }
```

**It is the commit point, and the only one.** Routing reads the chain: an agent
is in the container named by its most recent `container_migrated` row, or in its
original container if it has none. A crash anywhere before step 9 leaves the
agent on the old container regardless of what happened at the venue. That is the
rule [workflows.md](workflows.md) §7 already applies to workflow nodes — the
ledger append commits before the side effect is believed — and it is why the
migration has one commit row rather than a start row and an end row.

**Agent history spans the migration with no work.** `events.agent_id` is on every
chained row and does not change, so `get_events` and every per-agent query stay
correct across the move. That is not luck; it is the reason an agent's identity
is not its address.

**Address-keyed queries do not span it, and take the union.**

| Table | Key | What must change |
|---|---|---|
| `sub_accounts` | `address` PK | Two live rows per migrated agent — the new one active, the old one retired. Already representable: `owner_type` / `owner_id` are on the row (R2) |
| `fills` | `account` | Per-agent totals select over every address the agent has held |
| `funding_payments` | `account` | Same |
| `positions_closed` | `account` | Same |
| `backfill_state` | `account` | The new container starts its own backfill; the old one keeps its `complete_from_ms` |
| `feed_gaps` | `scope` | A new container is a new scope; the one-open-gap-per-scope index stays correct |

So `SELECT address FROM sub_accounts WHERE owner_type='agent' AND owner_id=?`
returns several rows after a migration, and every call site that assumed one has
to be found before the first migration runs, not after.

**V5's "re-point the registry row" is realised as two rows, not an update.** The
registry is keyed by address, and [history.md](history.md) §3.4 keeps a retired
container's past forever: rewriting the address on the existing row would detach
every fill that address produced. The old row is retired
(`Standing::Retired` — active 0, excluded from live views, counted in all-time
totals) and the new row is written beside it.

**The registry's single-master assumption has to go.**
`Discovered::from_venue` in `crates/oppen-core/src/accounts.rs` refuses any entry
whose `master` is not the expected one, returning `ForeignMaster`. That is right
for sub-accounts under one master and wrong for a fleet of top-level containers,
where there are several masters by construction. The check becomes per-container
rather than per-installation. It is not deleted: without it, somebody else's
account can enter the roster and be offered as a route to sign into, which is
what the original comment says it exists to prevent.

**And the table is now misnamed.** `sub_accounts` holds containers, and one kind
of container is not a sub-account. A V2 migration adds `container_kind` and
`venue`; both are additive and legal under the schema rule above. Renaming the
table is not worth the churn and is not proposed.

### 3.6 What is not migrated

| Not migrated | Why |
|---|---|
| The agent wallet | Approved per account, and re-approving a name silently revokes the prior agent (O4). A migration always mints a new key with a new name |
| Nonce state | Nonces are per signer. A new signer starts clean, and the old signer's set is never reused ([../hl-signing.md](../hl-signing.md) §8) |
| Open orders, positions | There are none — preconditions 2 and 3 |
| Fills, funding payments, closed positions | They belong to the address that produced them. The agent's totals are the union (§3.5). Rewriting them to point at the new address would be falsifying a record |
| The builder-fee approval | Per account (O7), and a new address is a new user — unless a sub-account inherits its master's, which is unconfirmed |
| Traded volume | Volume earned in a top-level container does not transfer. Under §2.1's recommendation the volume that cleared the gate was earned by the container that becomes the master, and stays with it |
| Guardrails, journal, alerts, allowlist, loss budget | Keyed on `agent_id`, so they carry over untouched. This is the payoff for §3.5's design |

### 3.7 Is there a downgrade path?

Mechanically yes, usefully no, and it is not symmetric.

- **The same twelve steps run in the other direction**, with a new top-level
  account as the target and `usdSend` in place of `subAccountTransfer`. Nothing
  in §3.5 assumes a direction.
- **The sub-account survives.** No container can be deleted on any of the three
  venues; whether Hyperliquid can delete one specifically is undocumented (§6). A
  downgrade leaves an empty retired container behind.
- **The volume is not spent.** Crossing $100,000 is not consumed by creating a
  sub-account, so the upgrade can be re-run.
- **What is not reversible is the history split.** Both addresses stay in the
  record forever and every address-keyed total for that agent is a union from
  then on. Each round trip adds one more address per agent. The cost is paid in
  query complexity, not in money.
- **The one real reason to downgrade** would be §1.2's unconfirmed key-scoping
  question resolving badly, or a sub-account turning out to share the master's
  request budget (§6). Both are reasons to answer those questions before
  migrating, not reasons to ship a downgrade button. v1 ships no UI for it; if it
  is ever needed it is a runbook.

### 3.8 The other venues

**Aster has nothing to upgrade from.** Containers are sub-accounts from the first
agent. The cap rises with VIP level — 10 at VIP1–2 to 50 at MM tier 3 — and a
higher cap needs no migration, because existing containers are untouched. What
cannot be undone is creation: sub-accounts cannot be deleted, so an
over-provisioned fleet is permanent.

**Lighter's upgrade is a tier change, and it is heavier than §3.4.** Moving from
Standard (4) to Plus (16) or Premium (64) requires no open positions, no open
orders, and at least 24 hours since the last change. As documented that is every
container under the L1 address going flat at once, not one agent flat for a few
minutes, so an operator who fills their four free containers cannot expand
mid-session. Whether the requirement is scoped to the account index or to the
whole L1 address could not be confirmed (§6); §3.8 assumes the wider reading,
which is the safe direction to be wrong in. Because the tier is readable from
`accountLimits`, none of §3.2 applies.

---

## 4. What the upgrade buys, and what it costs

### 4.1 A shared fee tier

Hyperliquid tiers fees on traded volume, and volume aggregates to the master
across its sub-accounts. Separate top-level accounts do not aggregate.

The structural cost is exact even without the schedule: with N containers each
trading V/N, a tier boundary at B is crossed by the group at V ≥ B but by no
single account until V ≥ N·B. **With four agents, the operator pays the entry
tier until four times the volume that would otherwise have earned the discount.**

The entry tier is 0.045% taker and 0.015% maker (Hyperliquid docs → Trading →
Fees). **The boundaries and the tiers below those rates are not quoted here
because they were not read**, and a fee number invented to make an argument look
quantified is exactly the kind of claim these documents exist not to make. Read
the fee schedule before any number reaches the UI.

Note the asymmetry: Aster computes VIP level on the aggregated master+subs group,
so it never has this problem.

### 4.2 The agent-wallet budget, which finances itself and is not a reason

Verbatim, via [../hl-signing.md](../hl-signing.md) §3.3: "An account can have 1
unnamed approved wallet and up to 3 named ones. And additional 2 named agents are
allowed per subaccount."

| Layout | Named wallets available | Peak demand | Headroom |
|---|---|---|---|
| k top-level containers | 3 each, independently | 2 per account during a rotation overlap | 1 per account |
| k sub-accounts under one master | 3 + 2k, shared | 2k + 1 — one per container, doubled during a rotation, plus the manual signer | 2 |

Rotation overlaps rather than swaps atomically, so a container needs two named
wallets during the swap window. The +2 per sub-account is exactly what pays for
that, and `3 + 2k ≥ 2k + 1` holds for every k, so the budget binds under neither
layout. O6 makes the same point from the other side: each top-level container
carries its own baseline of 3.

**So "+2 API wallets per sub-account" is not a benefit.** It is the absence of a
penalty — sub-accounts share a budget that top-level accounts each get in full.

### 4.3 The reasons that actually matter

**One wallet account instead of N+1, and one withdrawal-capable key instead of
N+1.** Under top-level containers the operator creates, funds, backs up and holds
a wallet account per agent, and every one of those owner keys can withdraw. After
migration the containers have **no private keys at all** — "Subaccounts and
vaults do not have private keys" — so the operator's withdrawal-capable key
surface collapses back to the master. For four agents that is the difference
between five withdrawal-capable keys and one. This is the strongest argument for
the upgrade and it is a key-custody argument, not a performance one.

**A venue-side authority that can reach every container.** An API wallet signs
for its own account and that account's sub-accounts, and never for an unrelated
top-level account. Under the top-level model there is therefore **no single key
that can flatten the fleet**: closing agent beta's position is signed by account
2's own wallet, and nothing on account 1 can reach it. oppen's kill switch is
still global, because it is oppen's own state and oppen holds every agent wallet
— but that is containment, and the venue-side authority a supervisor would want
in a genuine emergency does not exist until the containers are sub-accounts.

### 4.4 What the upgrade may cost

Stated with the same weight, because both are unresolved and either could
reverse the conclusion.

**Isolation may get worse.** §1.2's `DOC-NONCE` / `DOC-SUB` conflict: if an API
wallet approved on the master can sign for *any* of its sub-accounts, then a
stolen agent key reaches every container instead of one, and the migration trades
a certain property for an uncertain one. On the top-level model that property is
certain. **This must be settled by a testnet test before any migration ships**
([onboarding.md](onboarding.md) §3.9).

**Throughput may get worse.** [../spec.md](../spec.md) item 10's request budget
is per address, and separate top-level containers each carry their own
10,000-request initial buffer. Whether a sub-account gets its own or draws on the
master's is unconfirmed (§6). If it draws on the master's, the fleet's aggregate
request budget shrinks at exactly the moment the roster grows.

---

## 5. Acceptance gate

On testnet, with an agent that has traded in a top-level container and a master
that has cleared the volume gate:

1. Run §3.4 end to end. Afterwards, `get_events` filtered by that `agent_id`
   returns one contiguous history spanning both addresses, `Ledger::verify`
   reports an unbroken chain, and the agent's next order routes to the
   sub-account.
2. The retired container's fills still appear in the agent's all-time totals and
   are excluded from live views ([history.md](history.md) §3.4).
3. **The negative:** attempt a migration with one open position and confirm it is
   refused with a typed reason naming the position, with nothing signed and no
   funds moved.
4. **The refusal path:** attempt `createSubAccount` from an account below the
   gate and confirm oppen records a typed `upgrade_unavailable`, renders the
   venue's message as inert text, and takes no other action.

A prerequisite that is not part of the gate but blocks shipping it: the key-scope
test in §4.4. A migration that cannot state whether a stolen agent key reaches
one container or all of them should not run on mainnet.

---

## 6. Open questions

Those already recorded in [../decisions.md](../decisions.md) "Not confirmed" are
cross-referenced rather than restated. The ones this document adds are marked
**new**.

1. **Does an API wallet approved on a master sign for any of its sub-accounts, or
   only for the one it was scoped to?** `DOC-NONCE` and `DOC-SUB` disagree
   ([onboarding.md](onboarding.md) §3.9). Decides whether §3 preserves the
   one-key-one-container property. **The blocking question.**
2. **Does the address rate budget follow the sub-account or aggregate at the
   master?** **new.** [../spec.md](../spec.md) item 10's budget is per address
   and sub-accounts have addresses. If it aggregates, §4.4's second cost is real.
3. **Does the sub-account gate read `userRateLimit`'s `cumVlm`?** **new.** No
   documentation links them, which is why §3.2 shows the number and never
   branches on it.
4. **Is the volume counter lifetime-cumulative or windowed?** Everything points
   to cumulative; Hyperliquid has never stated it. If windowed, the upgrade stops
   being a one-way ratchet and an interrupted volume run can decay.
5. **Can an API wallet sign `subAccountTransfer` and `createSubAccount`?**
   [../hl-signing.md](../hl-signing.md) open question 3. If yes, an agent key can
   move USDC between a master and its sub-accounts and no guardrail models it —
   and the §3 upgrade provisions and funds a container with no wallet prompt at
   all. Needs a testnet negative test before mainnet. **No document in this repo
   may state either answer as settled**; the runbook's Appendix A previously did,
   and was corrected on 2026-09-04.
6. **What is the unit of `usd` in `subAccountTransfer`?**
   [../hl-signing.md](../hl-signing.md) open question 5. §3.4 step 8 moves money
   with it.
7. **Does a sub-account inherit its master's builder-fee approval?** O7. Decides
   whether §3.4 step 7 exists.
8. **Does a `usdSend` addressed to a sub-account credit it directly?** **new.**
   Would collapse step 8 from two hops to one.
9. **Can a Hyperliquid sub-account be deleted?** **new.** Nothing read says
   either way; Aster and Lighter both say no.
10. **Does sub-account volume count toward the master's gate for creating further
    sub-accounts?** **new.** Fee tiers aggregate; the gate is not stated to.
11. **Is Aster's `createSubAccount` whitelist-gated?** **new.** The whitelist note
    — "Sub-account creation is restricted to whitelisted addresses only. Users
    must contact the project team" — appears in the docs under **Bind
    Sub-Account**, and the research pass read it as governing
    `/fapi/v3/sub-accounts/bind` and not `createSubAccount`. That reading is
    plausible and unproven. If it governs both, §2.3 changes materially and Aster
    is not open-signup.
12. **Is Lighter's tier-change precondition scoped to the account index or the L1
    address?** **new.** §3.8 assumes the wider reading.
13. **Can a container that is not the original funding account create sub-accounts
    under itself once it clears the gate?** **new.** §2.1 now depends on this: the
    gate is documented as per user and each container is its own user, so it
    should hold, but no page states it for this case. If it does not hold — if
    only some privileged first account can ever create sub-accounts — then §2.1's
    withdrawn conclusion comes back and the funding account has to be the trading
    master after all. This supersedes the earlier form of this question ("should
    the first agent later move off the master"), which assumed the answer.
14. Also open, and recorded in [../decisions.md](../decisions.md): whether
    `scheduleCancel` is itself volume-gated (O1); whether a `usdSend` creates an
    untouched account; whether Aster caps agents; whether a below-VIP1 Aster
    wallet gets 10 sub-accounts; whether Aster sub-accounts work in Shield Mode;
    Lighter's exact API-key count; whether Lighter truly lacks hedge mode;
    whether Lighter gates account creation by invitation.

---

## 7. Sources

Read on 2026-09-04. "Verbatim" means the wording was read while writing this
document. "Research pass" means it comes from the 2026-09-04 venue audit against
the same official docs and was not re-read here — the audit's own evidence table
is [../decisions.md](../decisions.md), "What the three venues actually grant".

| Claim area | Source | Read |
|---|---|---|
| Sub-account gate and caps | Hyperliquid docs, *Trading › Sub-accounts* — <https://hyperliquid.gitbook.io/hyperliquid-docs/trading/sub-accounts> | Verbatim |
| The refusal, observed | The project's own testnet wallet, 2026-09-04: `Cannot create sub-accounts until enough volume traded. Required: $100000. Traded: $0` — [../runbooks/testnet-provisioning.md](../runbooks/testnet-provisioning.md) | Observed |
| Testnet enforcement of the same threshold | `nktkas/hyperliquid` SDK integration test | Research pass |
| `subAccounts`, `sendAsset`, `agentSendAsset`, `usdClassTransfer`, `approveAgent` | Hyperliquid docs, *For developers › API › Info endpoint* and *Exchange endpoint* | Verbatim |
| L1 vs user-signed classes, `vaultAddress` routing, nonce sharing, agent-wallet counts and expiry cap, open questions 3 and 5 | [../hl-signing.md](../hl-signing.md) §1, §2.1, §3.3, §8, §9 — each cell there cites the venue page it was read from | Verbatim |
| `cumVlm` on `userRateLimit`, documented as "Cumulative volume" | Hyperliquid docs, *Info endpoint*; `crates/oppen-hl/src/types.rs:639`, printed by `crates/oppen-hl/examples/testnet_order.rs`; and a live response on 2026-09-04 carrying `{"cumVlm":"188908641154.22","nRequestsUsed":…}` | **Observed** — this is what corrects [../decisions.md](../decisions.md) O3 |
| Entry fee rates 0.045% / 0.015% | Hyperliquid docs → Trading → Fees, via [../runbooks/testnet-provisioning.md](../runbooks/testnet-provisioning.md) Appendix A | Research pass |
| No hedge mode; `scheduleCancel` limits; builder-fee limits | Hyperliquid docs; recorded as O1 and O7 in [../decisions.md](../decisions.md) | Research pass |
| Aster `createSubAccount` dual signature, master message body, the whitelist note under *Bind Sub-Account* | `asterdex/api-docs`, V3 futures API | Verbatim |
| Aster caps, key limits, hedge mode, `registerAndApproveAgent`, transfers, withdrawal permissions | docs.asterdex.com | Research pass |
| Lighter API-key permissions and secure-withdrawal scope; `accountsByL1Address`; `accountLimits`; `L2CreateSubAccount` | apidocs.lighter.xyz, *API keys*, *Account types*, endpoint reference | Verbatim |
| Lighter tier caps, tier-change preconditions, deposit minimums, key-count conflict | docs.lighter.xyz | Research pass |
| Ledger append-only rule, `events` and `sub_accounts` schema, the `ForeignMaster` check | `crates/oppen-core/src/ledger/schema.rs`, `crates/oppen-core/src/accounts.rs` | Verbatim |
