# Mobile companion

**Component specification, v0.2 — draft**
Scope: v2, and deliberately narrow

Revised 2026-09-04 against the new D1 ([../spec.md](../spec.md)): the unit of
isolation is one venue **account** per agent — a sub-account where the venue
grants one, a top-level account otherwise. v1's Hyperliquid containers are
top-level accounts, and a top-level account is not reachable by another
account's API wallet. §2 and §3 are rewritten because of that; §7.3 is
reconsidered because of §3.

---

## 1. The honest answer

A full oppen on a phone is the wrong product, and the reasons are architectural
rather than a matter of effort.

- **Supervision requires a running process.** The kill switch, the dead-man
  switch and the guardrail engine all assume oppen is alive. Mobile operating
  systems suspend background apps aggressively and on their own schedule. An app
  that is asleep cannot supervise, and one that claims to is worse than none.
- **The MCP gateway has no meaning on a phone.** Agents connect to a loopback
  HTTP server on the machine where they run. Nothing connects to a phone.
- **Keys on another device are a real cost, and under D1 it is not one key.**
  The threat model's containment story rests on where agent keys live. A phone
  that can sign for N containers holds N keys (§3), which is a different
  proposition from the single key this document assumed in v0.1.

So the companion is not oppen. It is a **supervisor's window with a panic
button.**

---

## 2. What it does

### 2.1 Read everything, per container

Positions, orders, fills, PnL, funding, liquidation distance and margin runway —
everything the venue itself publishes. Agent activity and refusals are oppen's
own layer and are not in this list; §2.4 says what it would take to see them.

This works with no backend and no keys, because **Hyperliquid account state is
public**. Given an account address alone, the phone reads `clearinghouseState`,
`frontendOpenOrders` and `userFillsByTime` for that address and one shared
`metaAndAssetCtxs` for marks and funding, directly from the venue. An address is
not a secret and cannot authorize anything.

What changed under D1 is the count. The roster maps 1:1 to containers, so the
phone does not watch *the* account — it watches **N addresses**, one per agent,
plus any account the operator added by address ([../decisions.md](../decisions.md)
R3). Every read below is per container.

Everything in this document is Hyperliquid. D1 is venue-agnostic, but whether
Aster and Lighter expose account state to an unauthenticated reader the way
Hyperliquid does was not checked while writing this, so no claim here carries to
them. Both venues are v2+ ([ROADMAP.md](../../ROADMAP.md)), as is this companion.

### 2.2 The phone has to be told which addresses, and that is oppen-layer data

There is no venue-side link from a top-level container back to a master:
[../decisions.md](../decisions.md) records it flatly — "there is nothing to
discover for a top-level container, since no info endpoint links it to the
master. The registry oppen writes at provisioning time is the only record oppen
has." With one account a user types an address. With N, the phone needs the
roster, and the roster only exists on the desktop.

- The list is **not a credential**. Addresses authorise nothing, so the export
  is a QR code or a paste of N `{label, venue, address, kind}` rows, not a
  pairing secret, and it needs no listening socket on the desktop.
- It **goes stale**. A new agent adds a row; a sub-account upgrade *changes* an
  agent's address and leaves the old one holding history and no positions
  ([venue-containers.md](venue-containers.md) §3). So the export carries the
  date it was captured, the phone displays that date, and a container that the
  venue does not recognise renders as an error, never as an empty position list.
- It means the "venue-only, needs nothing from the desktop" claim in v0.1 was
  true only for a single hard-coded address. Venue-only still needs no live
  connection and no key — it needs one one-way copy of a public list.

### 2.3 Polling cost

Per cycle: three requests per container plus one shared meta request — `3N + 1`.

| Containers | Requests per cycle | At one cycle / 5 s |
|---|---|---|
| 1 | 4 | 48 / min |
| 4 | 13 | 156 / min |
| 10 | 31 | 372 / min |

Two things follow. **Tier the reads**: `clearinghouseState` is the one that
answers "am I liquidating" and runs every cycle; open orders and fills can run
every sixth cycle without changing what the screen is for. And **degrade by
lengthening the interval, never by dropping containers** — a container that
stopped being polled is the one that liquidates, and it is invisible precisely
because nothing is watching it.

**Not established here:** Hyperliquid's info-endpoint weight budget per IP was
not read while writing this document, so no interval above is defensible yet —
read the limit, then choose. Whether one WS connection carrying N per-user
subscriptions is cheaper than `3N + 1` polls was not measured either, and
`crates/oppen-hl`'s WS layer models only the feeds the desktop uses today. Both
are §7.5.

### 2.4 What the phone cannot see this way

oppen's own layer: intents, reasons, guardrail refusals, approval decisions.
Those live in the desktop ledger. Two options, and the recommendation is to ship
without either at first:

- **LAN pairing** — the desktop serves a read-only, token-authenticated endpoint
  on the local network. Works at home, not away, and adds a listening socket to
  the desktop, which the threat model currently does not have beyond loopback.
- **Nothing** — the phone shows venue truth only, and the console remains the
  only place where reasons and refusals are visible.

Recommend shipping venue-only first. It is useful, it needs no new attack
surface, and it answers the question people actually open a phone to ask: *am I
liquidating?*

### 2.5 Alert

Liquidation distance, margin runway, circuit-breaker proximity, a large fill, an
agent gone quiet with a position open. All of these are per container, and the
last one is only meaningful with §2.2's roster: "agent beta is quiet" is a
statement about a label the venue has never heard of.

Push notifications require a relay, which is a server. Either accept that for
alerts only, with no position data in the payload, or use local notifications
computed by the phone from polled venue state while the app is foregrounded. The
first is more useful and costs a server; the second is honest to the no-backend
claim and only works when the app is open. This is a real trade-off and needs a
decision, not a default.

---

## 3. The panic button

The one write capability, and the reason the companion is worth building.

### 3.1 What v0.1 said, and why it cannot work

v0.1 said the phone holds "its own agent wallet, approved by the master wallet in
the same ceremony as any other… for the accounts it watches." Under the revised
D1 that is false, and not by a detail. An API wallet signs for **its own account
or that account's sub-accounts, and never for an unrelated top-level account**
([../spec.md](../spec.md) D1; [../hl-signing.md](../hl-signing.md) §3.3). v1's
containers *are* unrelated top-level accounts — that is the whole point of the
model. One phone key approved by one account reaches that one account and
nothing else.

### 3.2 What it costs instead: N ceremonies, N keys, N names

| | v0.1 assumed | Under D1 |
|---|---|---|
| Wallet signatures to enrol the phone | 1 | **N**, one `approveAgent` per container, each signed by that container's own account ([onboarding.md](onboarding.md) §2.2, H1) |
| Keys on the phone | 1 | **N**, one per container |
| Agent names to keep distinct | 1 | **N**, and none of them may collide with the desktop's |
| Named-wallet slots consumed | 1, somewhere | **1 in each container**, out of 3 |

Four consequences, all of them concrete.

**The name collision is the dangerous one.** Re-approving the same `agentName`
silently deregisters the previous wallet of that name — no error, no event
([../decisions.md](../decisions.md) O4). If the phone's ceremony reuses the
desktop's agent name, enrolling the phone stops the desktop agent from signing
and nothing anywhere says so. Phone wallets carry their own name per container
(`oppen-mobile-<container>`, with `valid_until` appended as usual), forever, and
the acceptance gate tests this specifically (§6, test 5).

**The named-wallet budget binds exactly.** An account has 1 unnamed and up to 3
named API wallets ([../hl-signing.md](../hl-signing.md) §3.3). The desktop uses
one named wallet per container in steady state and two during a rotation overlap
([venue-containers.md](venue-containers.md) §4.2). The phone's is the third.
Peak demand is then 3 of 3: it fits, with **zero headroom**. A second phone, a
tablet, or any other signing device does not fit at all on the top-level model.

**Cancelling across the fleet is N signed actions and is not atomic.** Each
container has its own signer and its own nonce set
([../hl-signing.md](../hl-signing.md) §8), so a fleet-wide panic is N
independent submissions, any of which can fail alone. The UI shows a per
container result, and "cancelled" is a claim only when every container returned
one. Partial success with a dead desktop is the realistic case, not the edge
case.

**Phone keys expire like every other agent wallet.** 90 days by D-b, 180 days
maximum at the venue. A panic button whose key expired silently is worse than no
panic button, so expiry is shown on the phone and warned from day 14, per
container.

### 3.3 Cancel, not scheduleCancel

`scheduleCancel` allows a **maximum of 10 triggers per day per address**,
resetting 00:00 UTC, and the scheduled time must be at least 5 seconds ahead
([../hl-signing.md](../hl-signing.md) §10). The desktop's dead-man switch is
already spending that budget on every container it watches, and no source read
exposes a remaining-trigger count at the venue — it is oppen's own tally, kept
on the desktop. So the phone cannot read it, least of all in the one scenario the
phone exists for, where the desktop is dead.

The phone's panic path is therefore plain `cancel` / `cancelByCloid`: no daily
cap, and it acts now rather than at a deadline. `scheduleCancel` from the phone stays
available for the narrow case of leaving a deadline behind after closing the app,
and when it is used the phone states that it is spending from a budget it cannot
see.

### 3.4 Why it is still worth building

It works when the desktop is dead. A laptop that crashed, lost power or fell off
the network leaves agents' resting orders live at the venue and the dead-man
switch un-refreshed. A phone that can cancel directly against Hyperliquid is the
only supervisor left. That is a genuine failure mode, not a hypothetical.

What changed is the price, not the argument: it is now linear in the roster.
§7.3 is where that is weighed.

### 3.5 Blast radius, stated for the threat model

A stolen, unlocked phone can cancel orders in **every container it holds a key
for** — under full coverage, the whole fleet. It cannot open a position, because
the app contains no code path that constructs an opening order. It cannot
withdraw: Hyperliquid API wallets cannot, and that is the one hard boundary in
[../threat-model.md](../threat-model.md) — documented by the venue, not yet
proven by oppen's own negative test
([../hl-signing.md](../hl-signing.md) open question 3). Cancelling is the one
destructive action whose worst case is a missed opportunity.

N keys on a second device is N things to steal from one device rather than one,
and the containment argument holds per key rather than per phone. Guard the app
behind device biometrics, and log every cancellation to the desktop ledger on
next sync, attributed to the container and to the mobile wallet, so the audit
trail stays complete.

### 3.6 The one-key form exists only on the sub-account model, and only if an open question resolves permissively

If an API wallet approved on a master signs for **every** sub-account of that
master, then after the sub-account upgrade one phone key covers the fleet in one
ceremony. That is exactly the reading whose consequence is that a stolen agent
key reaches every container instead of one — the blocking unknown in
[venue-containers.md](venue-containers.md) §1.2 and §6 question 1, and
[onboarding.md](onboarding.md) §3.9. The arrangement that makes the panic button
cheap is the arrangement that makes isolation weaker. Do not let the convenience
of a one-key phone become a reason to hope that question resolves one way.

---

## 4. What it does not do

Open positions · modify guardrails · pair agents · host a model · run workflows ·
approve proposals · change network · hold any container-owner key. (There is no
single master under D1 — there are N container-owner keys, all withdrawal-capable
and all in the user's own wallet, [../decisions.md](../decisions.md) "The
container's own key".)

Approval mode deserves a note, because approving a proposal from a phone is the
obvious next request. It is a signing decision made on the smaller screen with
the least context, under the most time pressure, in the place where a person is
most likely to be distracted. Not in v2.

---

## 5. Build

Tauri 2 supports iOS and Android, and the Rust core, the type definitions and
the ASCII renderer are all reusable. The venue client (`oppen-hl`) already
compiles without the signer if the crate is split, which it should be for this
purpose: `oppen-hl-read` for info endpoints and types, `oppen-hl` for signing on
top. A read-only build is then genuinely smaller rather than nominally so — it
carries no signer and no keystore.

The design system ports directly. A character grid is a good fit for a small
screen, the ASCII candle renderer reflows by changing `cols`, and the layout is
already a single column at narrow widths in several panels.

Minimum useful screen: positions with liquidation distance **grouped by
container**, one chart, the alert list, and the panic button. Four screens, not
five tabs. The container is a first-class row in the UI, because under D1 it is
what every number belongs to.

---

## 6. Acceptance gate

On testnet, with **at least two containers** in the roster and the desktop
powered off:

1. The phone shows the correct positions and liquidation distances for **every**
   container in the export, with a container that holds no position rendered as
   flat rather than absent.
2. It cancels a resting order in **two different containers**, each signed by
   that container's own mobile wallet, and reports the two outcomes separately
   rather than as one "done".
3. Both cancellations appear in the desktop ledger on next start, attributed to
   the right container and the right mobile wallet.
4. **The negative:** with one container's mobile key deliberately removed, the
   panic button reports that container as unreachable and does not report
   success for the fleet.
5. **The collision test:** after the phone's ceremony on a container, that
   container's *desktop* agent wallet still signs an order successfully — proving
   the phone did not silently deregister it (§3.2, O4).

---

## 7. Open decisions

1. **Alerts with a relay, or foreground-only.** §2.5.
2. **LAN pairing for oppen-layer data**, or venue-only. §2.4. Note that either
   way a one-way roster export exists (§2.2), so "no desktop dependency at all"
   is not one of the options.
3. **Whether the panic wallet ships, and with what coverage.** Reopened by §3:
   the v0.1 recommendation was written for one key and one ceremony, and that is
   not what D1 leaves.

   | Option | Coverage | Ceremonies | Keys on the phone | Named slots used |
   |---|---|---|---|---|
   | Read-only first | none | 0 | 0 | 0 |
   | One nominated container | 1 of N | 1 | 1 | 1 of that container's 3 |
   | Every container | N of N | N | N | 1 of 3 in each |
   | One key on the sub-account model | fleet — **only if** §3.6's question resolves permissively | 1 | 1 | 1 |

   Read-only first is unchanged and already decided ([../decisions.md](../decisions.md)
   S7). The recommendation that *is* new: when the panic wallet is earned, ship
   the **one nominated container** form before full coverage. It is one ceremony
   and one key, it exercises the entire path end to end, and it lets the operator
   put the button where it is worth having — the container running unattended.
   Full coverage is the same flow repeated N times and can be added without
   redesign. This supersedes v0.1's "ship it or not" framing, which had no
   middle option because it assumed a single key.
4. **Distribution.** App Store review of a trading app that connects to a
   decentralized perpetuals venue is not a formality and should be scoped before
   any code is written.
5. **Poll or subscribe, and at what interval.** §2.3 — blocked on reading
   Hyperliquid's info-endpoint weight budget per IP, which no document in this
   repo currently states.
6. **Roster export format and staleness policy.** §2.2 — what the phone does
   when a container migrates to a sub-account mid-life, and whether a stale
   export should refuse to arm the panic button or merely warn.
