# Mobile companion

**Component specification, v0.1 — draft**
Scope: v2, and deliberately narrow

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
- **Another key on another device is a real cost.** The threat model's containment
  story rests on where agent keys live. Adding a second device with signing
  ability widens the surface for a convenience.

So the companion is not oppen. It is a **supervisor's window with a panic
button.**

---

## 2. What it does

**Read everything.** Positions, orders, fills, PnL, funding, liquidation
distance, margin runway, agent activity and refusals.

This works with no backend and no keys, because **Hyperliquid account state is
public**. Given the account address alone, the phone reads
`clearinghouseState`, `frontendOpenOrders`, `userFillsByTime` and
`metaAndAssetCtxs` directly from the venue. The address is not a secret and
cannot authorize anything.

What the phone cannot see this way is oppen's own layer: intents, reasons,
guardrail refusals, approval decisions. Those live in the desktop ledger. Two
options, and the recommendation is to ship without either at first:

- **LAN pairing** — the desktop serves a read-only, token-authenticated endpoint
  on the local network. Works at home, not away, and adds a listening socket to
  the desktop, which the threat model currently does not have beyond loopback.
- **Nothing** — the phone shows venue truth only, and the console remains the
  only place where reasons and refusals are visible.

Recommend shipping venue-only first. It is useful, it needs no new attack
surface, and it answers the question people actually open a phone to ask: *am I
liquidating?*

**Alert.** Liquidation distance, margin runway, circuit-breaker proximity, a
large fill, an agent gone quiet with a position open. Push notifications require a
relay, which is a server. Either accept that for alerts only, with no position
data in the payload, or use local notifications computed by the phone from
polled venue state while the app is foregrounded. The first is more useful and
costs a server; the second is honest to the no-backend claim and only works when
the app is open. This is a real trade-off and needs a decision, not a default.

---

## 3. The panic button

The one write capability, and the reason the companion is worth building.

The phone holds **its own agent wallet**, approved by the master wallet in the
same ceremony as any other, with one purpose: cancel. It can sign
`scheduleCancel` and `cancelByCloid` / `cancel` for the accounts it watches. It
cannot open a position, because the app contains no code path that constructs an
opening order.

Why this is worth the second key: it works when the desktop is dead. A laptop
that crashed, lost power or fell off the network leaves agents' resting orders
live at the venue and the dead-man switch un-refreshed. A phone that can cancel
directly against Hyperliquid is the only supervisor left. That is a genuine
failure mode, not a hypothetical.

The trade-off is stated in the threat model when this ships: a stolen, unlocked
phone can cancel orders. It cannot open positions, move funds, or read the
master key. Cancelling is the one destructive action whose worst case is a missed
opportunity.

Guard it behind device biometrics, and log every cancellation to the desktop
ledger on next sync so the audit trail stays complete.

---

## 4. What it does not do

Open positions · modify guardrails · pair agents · host a model · run workflows ·
approve proposals · change network · hold the master key.

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
top.

The design system ports directly. A character grid is a good fit for a small
screen, the ASCII candle renderer reflows by changing `cols`, and the layout is
already a single column at narrow widths in several panels.

Minimum useful screen: positions with liquidation distance, one chart, the alert
list, and the panic button. Four screens, not five tabs.

---

## 6. Acceptance gate

With the desktop powered off, the phone shows the correct positions and
liquidation distances for the watched account, and cancels a resting testnet
order successfully. The cancellation appears in the desktop ledger, correctly
attributed to the mobile wallet, when the desktop next starts.

---

## 7. Open decisions

1. **Alerts with a relay, or foreground-only.** §2.
2. **LAN pairing for oppen-layer data**, or venue-only. §2.
3. **Whether the panic wallet ships at all in the first release**, or whether the
   companion is read-only until the pattern is proven.
4. **Distribution.** App Store review of a trading app that connects to a
   decentralized perpetuals venue is not a formality and should be scoped before
   any code is written.
