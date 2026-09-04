# Venue onboarding and ceremonies

**Component specification, v0.1 — draft**
Scope: v1 (Hyperliquid), plus the venue-agnostic form of D1 that Aster and Lighter
now sit inside

This document is the path from *downloaded oppen* to *an agent is trading*, per
venue, counted in signatures. The signature count is the onboarding cost and it is
the number that decides whether a user finishes setup.

It is also the document a person reads when deciding whether to trust oppen with a
signature. Every ceremony below therefore states what it **cannot** authorise
alongside what it can, and says so explicitly where that limit could not be
confirmed from a primary source.

---

## 0. Sources and what was verified when

| Tag | Source | Verified |
|---|---|---|
| `DOC-SUB` | <https://hyperliquid.gitbook.io/hyperliquid-docs/trading/sub-accounts> | 2026-09-04, re-read this session |
| `DOC-EXCH` | Hyperliquid exchange endpoint reference | 2026-09-04, re-read this session |
| `DOC-INFO` | Hyperliquid info endpoint reference | 2026-09-04, re-read this session |
| `DOC-NONCE` | Hyperliquid "Nonces and API wallets" | 2026-09-03, via [hl-signing.md](../hl-signing.md) |
| `ASTER` | <https://docs.asterdex.com> — sub-accounts, agents, V3 API | 2026-09-04 research pass, not re-read this session |
| `LIGHTER` | <https://docs.lighter.xyz> and <https://apidocs.lighter.xyz> | 2026-09-04 research pass, not re-read this session |
| `SDK-TEST` | `nktkas/hyperliquid` integration test asserting `createSubAccount` rejects below the volume gate, running against **testnet** | 2026-09-04 research pass |

Hyperliquid's two load-bearing numbers — the sub-account volume gate and the API
wallet allowance — were re-read from `DOC-SUB` while writing this file and are
quoted verbatim in §2.2. The Aster and Lighter facts come from the 2026-09-04
research pass against those venues' official documentation and were **not**
independently re-read here. Where the research pass itself recorded that something
could not be confirmed, this document repeats that rather than resolving it.

---

## 1. What onboarding must achieve, and what it may never break

### 1.1 Definition of done

Onboarding is complete for one agent when all six hold:

1. A **venue account** exists that belongs to that agent and to no other agent.
2. That account holds enough collateral to carry one position and one protective
   order at the venue's minimum notional.
3. A **trading key that oppen holds** is authorised to sign for that account, with
   an expiry.
4. The agent is **paired** over MCP: named, bound to that account, carrying
   guardrails, approval mode on (spec item 15, decision D-c).
5. The pairing has passed a **connection test** — the agent has called `get_state`
   and received a well-formed envelope.
6. Every step above is **in the ledger** (D6), so the account can be reconstructed
   from the record rather than from memory.

Nothing about onboarding is complete because a screen was dismissed. Each of the
six is a query against venue state or the local ledger.

### 1.2 The invariants onboarding may never break

| Invariant | Source | What it forbids in this flow |
|---|---|---|
| No account-owner key ever enters the app | D5, AGENTS.md 4 | No paste box for a private key or a seed phrase, on any venue, at any step. Not behind an advanced toggle, not for "import an existing account". |
| Testnet by default | D4, AGENTS.md 5 | Onboarding runs on testnet unless mainnet was explicitly and persistently switched on. The network badge is visible on every ceremony screen, and the network is part of the signed message on Hyperliquid (`hyperliquidChain`). |
| Default-deny pairing | spec item 15 | A connecting MCP client gets nothing until the operator approves it, names it, binds it to an account and assigns guardrails. |
| Near-zero starting guardrails | D-c | Empty symbol allowlist, $25 orders, $100 positions, $25 daily loss, 5 orders per 5 minutes, approval mode on. The first order is refused, and the refusal names the limit to raise. |
| Authority decays | D-b | 90-day expiry on every trading key oppen holds, warned from 14 days. Where a venue caps expiry lower, the venue's cap wins; where a venue has no expiry, §7 says what oppen does instead. |
| No private key reaches TypeScript | AGENTS.md 2 | Key generation, storage and use happen in Rust. The console renders an address; it never sees a key. |
| One account per agent | D1, revised 2026-09-04 | The container is the unit of isolation. A sub-account where the venue grants one, a top-level account otherwise. |

### 1.3 D1 as it now reads

> **One venue account per agent.** A sub-account where the venue grants one; a
> top-level account otherwise. The container is the unit of isolation; which venue
> primitive provides it is a venue detail.

The shared-address model is dead for v1 and this document assumes it is gone. Two
agents on opposite sides of the same asset net to zero at the venue, so per-agent
margin, liquidation price and funding accrual all become fiction, and no local
bookkeeping recovers funding that never moved.

The guards recorded in [decisions.md](../decisions.md) for any address that is
nevertheless shared — an agent may cancel only order ids it opened, the aggregate
position book is never sliced per agent, close-all and flatten are refused while
two or more agents share an address, and equity and margin are shown once for the
address — remain in force. Onboarding's job is to make that case rare, not to
pretend it cannot happen: a user who points two agents at one container is doing
something the app permits and must warn about.

### 1.4 What onboarding does not do

- It does not move funds onto a venue. Depositing is an on-chain transaction in the
  user's own wallet, on the user's own chain, and oppen neither constructs nor
  signs it.
- It does not create the user's wallet, back it up, or hold any part of it.
- It does not verify that the user has enough capital to trade profitably. It
  verifies only that a container clears the venue's minimum notional.
- It does not enable mainnet. That is a separate, explicit, persisted switch (D4).
- It does not make a venue's gate go away. Where a venue refuses to provision
  something, onboarding takes the other path and says which one it took.

---

## 2. Ceremony inventory

### 2.1 Three classes of signature, and only one of them is a ceremony

| Class | Key holder | Prompted where | Costs the user |
|---|---|---|---|
| **Ceremony** | The user | Their own wallet, over WalletConnect (D-a: MetaMask) | An explicit approve in a wallet UI |
| **App action** | oppen, in the OS keychain | Nowhere — signed in the Rust core | Nothing; it is a function call |
| **On-chain transaction** | The user | Their own wallet | An approve *and* gas *and* block time |

Only ceremonies and on-chain transactions are onboarding *cost*. App actions are
free, and the venue comparison in §6 is really a count of how much of each venue's
provisioning can be pushed from the first column into the second.

oppen's rule for every ceremony, on every venue:

- The typed data is rendered in full, in the app, before it is sent to the wallet.
  A user who cannot read what they are signing has not consented to it.
- The **expected signer address** is displayed next to the request, and the
  signature is recovered and compared to it **before** submission. A mismatch is a
  refusal, not a warning. §7 explains why this one check matters more than the
  others.
- The intent is written to the ledger before submission and reconciled from venue
  state after, because a submitted action whose response was lost is
  indistinguishable from one never sent (the same problem as
  `timeout_unknown_outcome`, spec item 19).

### 2.2 Hyperliquid

| # | Ceremony | Signed by | Authorises | Cannot authorise | Repeat when |
|---|---|---|---|---|---|
| H1 | `approveAgent` (EIP-712 `HyperliquidTransaction:ApproveAgent`) | The container account | One named agent address to sign trading actions for that account until `valid_until` | **Withdrawal to any other address.** See the limits note below | Per container, and on every key rotation |
| H2 | `approveBuilderFee` (`HyperliquidTransaction:ApproveBuilderFee`) | The container account | The oppen builder address to charge at most `maxFeeRate` on that account's orders | Any movement of funds; any rate above the signed cap | Per container; again only if the cap changes |
| H3 | `usdSend` (`HyperliquidTransaction:UsdSend`) | The funding account | A one-off internal USDC transfer to a named destination | Nothing recurring; it is a single amount to a single address | Every funding and rebalancing action |
| H4 | `createSubAccount` (L1 action) | The master account — **or possibly an API wallet oppen holds; unconfirmed**, see below | Creation of one sub-account under that master | Nothing else | Only on the upgrade path, §3.9 — **not reachable for a new user**, see below |

**The gate on H4, verbatim from `DOC-SUB`:** "Up to 10 sub-accounts can be created
after reaching $100,000 in volume. Every additional $100M in volume enables the
ability to create 1 additional sub-account, up to a maximum of 50 sub-accounts."

**Who signs H4 is not settled.** `createSubAccount` and `subAccountTransfer` are
**L1 actions**, and [hl-signing.md](../hl-signing.md) §1 gives the signer for that
whole class as "agent (API) wallet or master". Its open question 3 records that no
page read states which actions an API wallet is barred from. So an API wallet
oppen already holds *may* be able to provision and fund a sub-account with no
wallet prompt at all — which would make the §3.9 upgrade dramatically cheaper —
and it may not. It is recorded here as unresolved, it needs a testnet negative
test before mainnet, and no document in this repo may state either answer as
settled ([venue-containers.md](venue-containers.md) §6 question 5).

This is protocol-enforced, not a UI guard, and the threshold is the same on
testnet: `SDK-TEST` is an integration test that asserts `createSubAccount` rejects
with "Cannot create sub-accounts until enough volume traded" while running against
testnet. There is no API that bypasses it. A brand-new Hyperliquid user gets
**zero** sub-accounts, which is the whole reason D1 was rewritten.

**The API wallet allowance.** Quoted from the source oppen transcribed with a line
reference — `DOC-EXCH`, "Approve an API wallet", via
[hl-signing.md](../hl-signing.md) §3.3: "An account can have 1 unnamed approved
wallet and up to 3 named ones. And additional 2 named agents are allowed per
subaccount." `DOC-SUB` states the same allowance in different words, which this
repo's documents have quoted inconsistently ("Master accounts are provided with 3
API wallets by default…" here previously, "starts at 3 for all master accounts and
increases by 2 per sub-account" in [../spec.md](../spec.md)). The number is not in
dispute and the wording is not re-quoted as verbatim until the page is read again.
`approveAgent` is not itself volume gated at baseline, so a new account can approve
agents even though it cannot create sub-accounts.

**Limits on H1 that are load-bearing, and one that is not confirmed.**

- An API wallet **cannot withdraw to another address**. This is the single hard
  boundary in [threat-model.md](../threat-model.md) and it survives a fully
  compromised machine. The exception is inside the same address: `agentSendAsset`
  is "Similar to send asset, but can be signed by an agent. Destination must match
  the source address", so an agent can move collateral between its own account's
  balances. Not a withdrawal, and still a movement of margin oppen did not
  initiate.
- An API wallet signs for its master **or that master's sub-accounts**, never for
  an unrelated top-level account. In the v1 top-level model of §3, that means a
  stolen agent key reaches exactly one container and has no sibling to move to.
- What is **not** confirmed: which other user-signed actions an API wallet is
  barred from — `usdSend`, `withdraw3`, `approveAgent` itself. No source read
  states it; this is [hl-signing.md](../hl-signing.md) open question 3. Until a
  testnet negative test settles it, treat only "cannot withdraw" as load-bearing
  and do not build a flow that depends on an agent being *unable* to do anything
  else.
- `valid_until` caps at 180 days (`DOC-NONCE`). oppen uses 90 (D-b).
- Re-approving the **same `agentName`** deregisters the previous wallet of that
  name (`DOC-NONCE`). The revocation is silent — there is no error, no event, and
  the old key simply stops working. §7 makes name reuse a forbidden operation.

### 2.3 Aster

| # | Ceremony | Signed by | Authorises | Cannot authorise | Repeat when |
|---|---|---|---|---|---|
| A1 | `createSubAccount` master half (EIP-712, `chainId` 1666) | The master wallet | Creation of one sub-account, bound to a child address the caller supplies | Nothing about trading; it provisions a container | Per agent |
| A1c | `createSubAccount` child half (`childSignature`) | The sub-account's own generated key | Proof that the caller holds the child key | Same | Per agent — **app action, not a ceremony** |
| A2 | `subAccountTransfer` | The master wallet | An internal transfer, master↔sub or sub↔sub | Any external withdrawal | Every funding and rebalancing action |
| A3 | `registerAndApproveAgent` | The account the agent will trade for — see the note | One agent key with explicit `canSpotTrade` / `canPerpTrade` / `canWithdraw` booleans and an expiry | Withdrawal, when `canWithdraw` is false | Per agent, and on every rotation |

`ASTER` states no volume gate on sub-accounts — "VIP level requirement: All VIP
levels" — with a cap of 10 at VIP1–2 rising to 50 at market-maker tier 3. Nesting
is not supported and **sub-accounts cannot be deleted**. `POST
/fapi/v3/createSubAccount` carries the dual signature described above and exists on
both mainnet and testnet. Its sibling `/fapi/v3/sub-accounts/bind` *is*
whitelist-gated; `createSubAccount` is not.

`canWithdraw: false` on A3 yields a genuine trade-only key, and `ipWhitelist` is
required only when `canWithdraw` is true — which is itself evidence that the
withdraw-capable variant is treated as a different risk class by the venue. oppen
only ever requests `canWithdraw: false`.

**Not confirmed:** whether A3 is signed by the sub-account's own key (an app
action, since oppen holds it) or by the master (a ceremony). The research pass did
not settle it. §6 therefore quotes Aster's per-agent cost as a range, and the flow
in §4 is written so that either answer changes the count without changing the
sequence. Also unconfirmed: whether Aster caps the number of agents per account at
all.

### 2.4 Lighter

| # | Ceremony | Signed by | Authorises | Cannot authorise | Repeat when |
|---|---|---|---|---|---|
| L1 | `ChangePubKey` — API key registration | The L1 account key | One API key index on one account | Fast Withdrawals and Transfers, which need the L1 key | Per key, and on every rotation |
| L2 | `L2CreateSubAccount` | The **API key** | Creation of one sub-account under the account index | Nothing else | Per agent — **app action, not a ceremony** |
| L3 | Same-master transfer | The **API key** | Moving collateral between the master and its sub-accounts | An external destination | Every funding action — **app action** |

Lighter's provisioning is the cleanest of the three precisely because L2 and L3 are
signed by a key oppen already holds. `LIGHTER` states no volume gate; the cap is by
tier — Standard (free, default) 4 sub-accounts, Plus 16, Premium 64 — and staking
buys fee discounts and latency, not sub-account count. **Sub-accounts cannot be
deleted**, and changing tier requires no open positions, no open orders, and at
least 24 hours since the last change.

**The custody caveat that makes Lighter different, stated plainly.** A Lighter API
key is *not* a trade-only key. `LIGHTER`: API keys "enable both write and read
permissions … and process withdrawals", limited to *secure* withdrawals, which "can
only be sent to the same L1 address that created the account". So a stolen Lighter
key drains to the **owner's** address, not to an attacker's. That is a materially
weaker containment property than Hyperliquid's or Aster's. The residual risk on
Lighter is denial of capital and forced realisation of open positions, not theft.

[threat-model.md](../threat-model.md) previously carried this as one unscoped
sentence — "the agent wallet cannot withdraw … the only containment property that
holds against a fully compromised machine" — which is true of Hyperliquid and
Aster and **false of Lighter**. As of 2026-09-04 that document states the
guarantee per venue in its opening section, so the scoping is done rather than
pending. Whichever surface says it next, it is said per venue.

Lighter also offers **read-only tokens** (no trades, no withdrawals, expiry from 1
day to 10 years) and maker-only keys on Premium. The read-only token is the right
primitive for any observer that is not meant to trade.

**Not confirmed, and it decides whether Lighter onboarding is in-app at all:**
whether `ChangePubKey` (L1) is authorised by an Ethereum *signature* over the new
public key, or genuinely requires the raw L1 private key in the signing process.
§5.2 gives oppen's behaviour under each answer. Under neither answer does oppen
accept the L1 private key.

### 2.5 What no ceremony on any venue authorises

No signature requested by oppen, on any venue, at any point in onboarding,
authorises:

- an external withdrawal to any address;
- a change to a guardrail, the approval setting, the kill switch or the agent
  registry — those are operator-only Tauri commands and no signed venue action
  touches them (AGENTS.md 3);
- a builder fee above the signed `maxFeeRate` — 1 bp per M2, against a venue
  maximum of 0.1% for perps, and an actual charge of 0.3, 0.2 or 0.1 tenths of a
  basis point by tier (M1);
- anything on a venue other than the one named in the request. Hyperliquid's
  `hyperliquidChain` field puts the network inside the signed message, so a testnet
  approval cannot be replayed on mainnet.

---

## 3. Hyperliquid

### 3.1 Prerequisites

| # | Prerequisite | Cost | Notes |
|---|---|---|---|
| 1 | A wallet the user controls, with at least two accounts available | 0 | Deriving a second account in MetaMask costs no signature and no gas. This matters: it is what makes the top-level model affordable. |
| 2 | **Mainnet**: USDC in the funding account's Hyperliquid perps balance | 1 on-chain Arbitrum transaction, possibly 2 with an ERC-20 approve | Outside oppen. oppen links to the venue's own deposit page and waits on `userRole`. |
| 3 | **Testnet**: a faucet claim | See below | The faucet pays 1,000 mock USDC and requires a **prior mainnet deposit from the same address**. |

The testnet faucet's mainnet-deposit precondition is the single most surprising
prerequisite in this document, and it is worth stating in the UI in exactly those
words. It also means the faucet cannot be claimed once per agent container: each
new address would need its own prior mainnet deposit. Testnet funding for agent
containers therefore flows through `usdSend` from the one faucet-eligible account,
not through repeated claims.

**Not confirmed:** whether `usdSend` to an address with no prior Hyperliquid
activity succeeds, or whether the destination must already exist. Hyperliquid does
have a notion of account existence — `userRole` returns `missing` before a first
deposit, which is how the funded-wallet precondition was verified for the
[testnet provisioning runbook](../runbooks/testnet-provisioning.md) — and
`DOC-EXCH` states for `reserveRequestWeight` that a destination address is only
usable "provided that the destination account already exists". Whether `usdSend`
shares that requirement is not stated. Onboarding must therefore **query
`userRole` on the container before the first `usdSend`** and, if the transfer
fails, present the venue's message verbatim and offer the deposit path instead of
retrying.

### 3.2 The container question

On Hyperliquid a new user has no sub-accounts, so the container for each agent is a
**top-level account the user controls** — another account in their own wallet.
oppen never holds that key, because a top-level account key *can* withdraw and D5's
boundary is exactly the line between keys that can move funds and keys that cannot.

**May the funding account itself be a container? Settled here, for all four
documents: it may, it is not the default, and it is presented as a downgrade.**
[venue-containers.md](venue-containers.md) §2.1 and the
[testnet runbook](../runbooks/testnet-provisioning.md) both reference this
paragraph rather than restating it.

The trade-off is real in both directions and neither side is small.

| | Funding account as the first container | A dedicated container for every agent |
|---|---|---|
| Ceremonies for agent 1 | 2 — `approveAgent`, `approveBuilderFee` | 3 — `usdSend` first, then the other two (§3.3) |
| Agent 1's worst case | The funding account's whole balance, which is every dollar not yet pushed out to another container, and which grows on every deposit | That container's balance, which the operator chose |
| Wallet accounts to hold and back up | N | N + 1 |
| Effect on the §3.9 upgrade | None. Both layouts need one container to concentrate $100,000 of volume, and either can be that container — see the last note below | None |

**Why the default is a dedicated container.** The threat model's central sentence
about the container model is "an agent's worst case is that account's balance". If
the first agent trades in the funding account, that sentence is false for the
agent most likely to exist — the only one on a single-agent install — and it is
false in the direction that costs the most, because the funding account is where
a deposit lands. The saving is one signature, once. The cost is the app's main
safety claim, permanently, for that agent.

**Why it is still offered.** On testnet the funding account is the only
faucet-eligible address (§3.1), so a two-account testnet setup necessarily has an
agent sharing the funder; the runbook takes exactly that path and says so. On
mainnet a user with $200 of total capital is not made safer by splitting it below
the venue's $10 minimum notional. Refusing the configuration outright would push
those users into pretending, not into safety.

**How it is offered.** Never silently. The confirmation names the current balance
of that account in dollars, states that the figure rises with every deposit, and
records an `operator_action` in the ledger. It is not reachable from a default
path and it is not the pre-selected option.

**What this does *not* cost, contrary to an earlier draft of
[venue-containers.md](venue-containers.md) §2.1.** It was argued that the funding
account must host an agent because the $100,000 sub-account gate meters one
account's volume, and a funder that never trades never clears it. The premise is
right and the conclusion does not follow: the gate is per account, and every
container is its own Hyperliquid user (which is why each needs its own
`approveBuilderFee`, §3.4). Whichever container actually trades is the one that
clears the gate, and it can then create sub-accounts under **itself**. The master
of a future sub-account fleet does not have to be the funding account. What is
true, and is the useful half of that argument, is that volume does not transfer
between accounts: N containers each trading V/N reach a $100,000 gate only at
V ≥ N × $100,000. An operator who intends to reach sub-accounts should therefore
decide **which container will be the master before the first trade** and
concentrate volume there. That is a recommendation about which *container*, not an
argument for using the funding account as one.

*Not confirmed:* that an account which is not the operator's original funding
account can create sub-accounts under itself once it clears the gate. It follows
from the documented rule that the gate is per user plus the fact that each
container is a separate user, and no page states it for this case.

### 3.3 First agent, step by step

Numbered. **[C]** = ceremony, in the user's wallet. **[A]** = app action, free.
**[U]** = a user action outside oppen that costs no signature.

1. **[A]** oppen generates the agent wallet: a secp256k1 keypair in the Rust core,
   private key straight into the OS keychain, address surfaced to the console. The
   key never reaches TypeScript (AGENTS.md 2), never appears in a log line, and is
   never displayed.
2. **[U]** The user adds or selects a second account in their wallet. This is the
   container, `C1`. oppen records the address only. No signature, no gas.
3. **[A]** oppen queries `userRole` and `clearinghouseState` for `C1` and displays
   what it found, so the user can see they are pointing at an empty account rather
   than at something already in use.
4. **[C]** **`usdSend`, signed by the funding account**, destination `C1`. Internal
   and instant; `DOC-EXCH`: "This transfer does not touch the EVM bridge."
   Minimum useful amount is set by the venue's $10 minimum notional — the testnet
   runbook's figure is that anything under roughly $150 cannot hold a position and
   a stop at the same time. oppen then re-reads `userRole` and
   `clearinghouseState` for `C1` and does not advance until the balance is
   visible at the venue. If the transfer is rejected, present the venue's message
   verbatim and offer the deposit path — see §3.1, and the open question there
   about a destination with no prior Hyperliquid activity.
5. **[C]** **`approveAgent`, signed by `C1`.** `agentAddress` is step 1's address;
   `agentName` is a fresh unique name carrying a period suffix and a
   `valid_until` 90 days out (D-b, capped by the venue at 180);
   `hyperliquidChain` comes from the D4 switch. oppen recovers the signer from the
   signature and refuses to submit unless it equals `C1`.
6. **[C]** **`approveBuilderFee`, signed by `C1`.** `builder` is
   `OPPEN_BUILDER_ADDRESS`, `maxFeeRate` is `"0.01%"` (M2). Declining is permitted
   and supported: the order path continues with no builder code attached, which is
   what invariant 10 requires — a missing approval prompts, and never silently
   drops an order.
7. **[A]** oppen writes the container, the agent wallet address, the agent name and
   the expiry into the registry and the ledger, and reconciles against venue state:
   the approval is confirmed by reading the account's approved-agent list back,
   never by assuming the submission succeeded.
8. **[A]** The operator pairs the agent: the default-deny dialog names it, binds it
   to `C1`, assigns the D-c guardrails, mints a bearer token and produces the
   `claude mcp add` line.
9. **[A]** Connection test: the agent calls `get_state` and the console shows the
   envelope it received, with the network badge in it.

**Ceremonies: 3** (steps 4, 5, 6). **Two** if the builder fee is declined.

#### The ceremony order is fund, then authorise — settled here

**This paragraph is the single home of that rule.** The
[testnet runbook](../runbooks/testnet-provisioning.md) §3 and
[venue-containers.md](venue-containers.md) §2.1 reference it and do not re-argue
it. An earlier version of this section ordered the steps *authorise, then fund*
and called that deliberate; the runbook ordered them the other way and called
that load-bearing. Both cannot be right, and the mechanical argument beats the
ergonomic one.

**The mechanical argument.** `approveAgent` and `approveBuilderFee` are
user-signed actions performed **by `C1` itself** ([hl-signing.md](../hl-signing.md)
§1). Hyperliquid rejects actions from an address that has not deposited, with
`Must deposit before performing actions. User: 0x123...`. A container that has
never held funds is exactly such an address, so authorising before funding risks
a rejection that arrives *looking like a signing bug* — which is the worst
possible failure here, because the operator's next move is to re-check a
signature that was correct. Funding first removes the ambiguity: after step 4 the
account provably exists, and any later rejection is genuinely about the
signature.

**How strong that evidence is, stated rather than assumed.** The error string is
documented in `DOC-SIGN` and transcribed in [hl-signing.md](../hl-signing.md) §1,
but it is documented there as a symptom of *signing bugs* — a wrong signature
recovers a random address, and a random address has never deposited. No page read
states "an account must be funded before it can `approveAgent`" as a precondition
in its own right. The precondition is inferred from the error the venue defines,
and it is a strong inference because the message names precisely that
requirement. It is **unproven**, and following this order does not prove it —
the order avoids the case rather than testing it. The test is a deliberate
`approveAgent` on an unfunded testnet address, which nothing currently asks for.
Until someone runs it, the ordering rule stands on an inference, and this
document says so rather than presenting it as a documented precondition.

**What the reversed order was protecting, and what happens to it.** The
ergonomic argument was that an abandoned onboarding leaves capital parked in an
account with no agent watching it. That is real, and it is cheap: the funds are
in an account whose key is in the user's own wallet, oppen records the container
before the transfer (step 3 and step 7's reconcile), and §3.8's "funded, never
paired" row already gives the recovery — pair it, or `usdSend` it back. Against
that, the reversed order risks a rejection the operator will misdiagnose.

**One dependency, and it is open.** Fund-first assumes `usdSend` to a container
with no prior Hyperliquid activity succeeds. That is *not confirmed* (§3.1,
§9 question 3). If it turns out a destination must exist before it can be sent
to, the fix is not to swap the order back — `approveAgent` would face the same
wall — it is that a container must be touched by a deposit before either
ceremony, and onboarding must say so instead of retrying.

### 3.4 Second and Nth agent

Identical, minus nothing. Every step in §3.3 repeats, because every container is a
distinct Hyperliquid user with its own approvals.

**Marginal cost per additional agent: 3 ceremonies** — `approveAgent`,
`approveBuilderFee`, `usdSend` — or 2 with the builder fee declined.

Two notes on that number:

- An earlier draft of D1 quoted **two** signatures per additional agent, counting
  `usdSend` and `approveAgent`. The recorded figure is **three** (`spec.md` D1,
  `decisions.md` O7), because `approveBuilderFee` is per Hyperliquid account and
  every container is its own account. Two is right for a build
  with the builder code off. Official builds have it on by default (D7), the
  approval is per Hyperliquid user, and each container is a separate user — so the
  honest figure for the shipped default is three. This document uses three and
  states the two-signature case as the builder-declined variant rather than as the
  headline.
- Hyperliquid caps active builder approvals at 10 **per user**. Each container is
  its own user with its own allowance of 10, so this is not a limit on agent count.

**What is not a cost:** creating the container. Deriving account *n* in a wallet is
a local key derivation. The user pays no signature and no gas, and this is the
reason the top-level model is viable at all.

Switching the WalletConnect session to the right account between agents is friction
without being a signature. oppen must display which account the wallet session is
currently on, next to which account the pending ceremony needs, and must not send
the request when they disagree.

### 3.5 What oppen generates, and what the user provides

| Generated by oppen, held in the keychain | Provided by the user |
|---|---|
| The agent wallet keypair, one per container | The funding account, and its Hyperliquid balance |
| The agent name, with a rotation suffix and `valid_until` | The container account address |
| The MCP bearer token, one per agent | Every signature in §3.3 |
| `cloid`s, nonces, guardrail configuration | — |

oppen holds no key on Hyperliquid that can withdraw, and generates no account the
user does not control.

### 3.6 Funding and rebalancing

`usdSend` is internal and instant, and moves USDC between Hyperliquid accounts
without touching the EVM bridge (`DOC-EXCH`). Every rebalance between containers is
therefore one ceremony, from the sending account, with no gas and no bridge delay.

Consequences worth building for:

- Rebalancing is a **ceremony per move**. An agent that runs out of margin at 3am
  cannot be topped up without the user's wallet. Design the loss circuit breaker
  (spec item 25) around that fact rather than around an assumed refill.
- oppen never initiates a transfer. An agent has no tool that moves capital, and
  there is no MCP surface for it.
- Collateral does not net across containers. Two containers of $200 are not one
  container of $400, and the risk console must show them as what they are.

### 3.7 Key custody

| Key | Held by | Can | Cannot |
|---|---|---|---|
| Funding account | The user's wallet | Deposit, withdraw, `usdSend`, approve agents | — |
| Container `Cn` | The user's wallet | Same, scoped to that container | — |
| Agent wallet `An` | oppen, OS keychain | Sign trading actions for `Cn` only, and `agentSendAsset` **within `Cn`'s own address** | Withdraw to any other address. Sign for any account that is not `Cn` or one of its sub-accounts. Whether it can sign a cross-account transfer or an `approveAgent` is *unconfirmed* — [threat-model.md](../threat-model.md), "What an agent key can do, per venue" |
| MCP bearer token | oppen, revocable | Reach the gateway as that agent | Sign anything; it never touches a key |

The threat model's platform caveat applies to the agent wallet: on Windows and
Linux any process running as the same OS user can read it from the credential
store. Its inability to withdraw is what bounds the damage, and that is enforced by
Hyperliquid, not by oppen.

### 3.8 Failure and recovery

| Interrupted at | Observable state | Recovery | Unrecoverable? |
|---|---|---|---|
| Agent wallet generated, nothing signed | A keypair in the keychain, unused | Discard it and generate a new one. Never carry it to a second attempt | No |
| `usdSend` landed, `approveAgent` never signed — the first interrupt point under §3.3's order | A funded container with no agent authorised on it. Visible in `clearinghouseState` | Resume at step 5, or `usdSend` the balance back to the funding account. The container key is the user's, so the funds are never stranded | No |
| `usdSend` rejected, container never funded | Nothing moved | Read the venue's message: if it names the destination, the account may need a deposit before it can receive (§3.1). Do not proceed to `approveAgent` — it signs from the same unfunded address | No |
| `approveAgent` submitted, response lost | Unknown until queried | **Query the account's approved-agent list.** Never blind-retry: a retry with the same name and a *different* address silently revokes the first approval | No |
| `approveAgent` confirmed, `approveBuilderFee` abandoned | Agent authorised, no builder code | Resume, or trade without the builder code | No |
| Funded, never paired | Capital sitting in a container with no agent | Pair, or `usdSend` it back | No |
| Same `agentName` approved twice | The first wallet is deregistered, **silently** | Approve a fresh name with a fresh address | The old key is dead. The account is fine |
| Agent wallet key lost | The container cannot be traded by oppen | Approve a new agent under a **new name and new address**. Positions stay open and the container key still controls them | The key. Not the funds |
| Container key lost | Everything in that container | None from oppen | **Yes** — this is the user's wallet, and the reason oppen never holds it |

Rotation, which is the same procedure as recovery: generate a new keypair, approve
it under a **new** name, cut over, and let the old approval expire. Never reuse an
agent address — `DOC-NONCE` warns that pruning loses nonce state and "previously
signed actions can be replayed once the nonce set is pruned".

### 3.9 The sub-account upgrade path, and why it is not obviously an upgrade

When a user's traded volume clears $100,000, `createSubAccount` starts to succeed
and each agent could move onto a real sub-account. Design for it now; do not
require it.

Three things make this a migration rather than a rename:

1. **A sub-account is a different address.** Positions cannot move with it. The
   agent must be flat and have no resting orders before the migration starts.
2. **Fee tiering changes.** On the top-level model, volume does not aggregate
   across containers, so every agent sits in its own fee tier — irrelevant at tier
   0, which is exactly where a user who cannot create sub-accounts is. On the
   sub-account model, volume aggregates under the master. That is the only thing
   the top-level model gives up, and the only thing the migration wins back.
3. **Isolation may get worse, not better.** `DOC-NONCE` says API wallets sign "on
   behalf of the master account or any of the sub-accounts", while `DOC-SUB`
   frames the allowance as "2 additional API wallets for every sub-account
   created", which reads as per-sub-account scoping. These two statements are not
   obviously compatible, and **which one is true decides whether the migration
   preserves the property that a stolen agent key reaches exactly one container**.
   On the top-level model, that property is certain. This must be settled by a
   testnet test before any migration ships.

**Not confirmed and relevant to the same question:** whether a sub-account inherits
its master's `approveBuilderFee`, or needs its own; and whether an API wallet may
sign `createSubAccount` or `subAccountTransfer`, which would make provisioning and
rebalancing on the sub-account model free of ceremonies (§2.2, and
[venue-containers.md](venue-containers.md) §6 question 5).

**Pre-checking the gate — a correction, made 2026-09-04.** An earlier research
pass recorded that no info endpoint exposes cumulative volume. **That is false and
any sentence saying it must be replaced.** `userRateLimit` returns `cumVlm`,
documented as "Cumulative volume"; Hyperliquid's request budget is itself
denominated in traded volume, which is why the counter is there. It was verified
live on 2026-09-04 — a response carrying `{"cumVlm":"188908641154.22",
"nRequestsUsed":…}` — and oppen's own type has the field
(`crates/oppen-hl/src/types.rs:639`, printed by
`crates/oppen-hl/examples/testnet_order.rs`).

What remains genuinely unknown is narrower, and it is what keeps the conclusion
standing: whether the sub-account gate reads *that* counter, and whether the
counter is lifetime or windowed. So:

- `cumVlm` may be shown as an informational distance-to-gate, labelled as an
  estimate that Hyperliquid does not confirm.
- Program logic still **attempts and classifies**. oppen calls `createSubAccount`
  and handles the refusal; it never gates its own behaviour on `cumVlm`, and it
  never parses `Required:` / `Traded:` out of the venue's message for anything but
  display.

---

## 4. Aster

Aster is a better structural fit for D1 than Hyperliquid: sub-accounts have no
volume gate, they are the venue's own isolation primitive, and
`ASTER` states plainly that "each sub-account maintains its own positions, assets,
and API keys, enabling effective risk isolation across different strategies".

### 4.1 Prerequisites

| # | Prerequisite | Cost |
|---|---|---|
| 1 | A wallet the user controls | 0 |
| 2 | An Aster account with collateral | 1 on-chain deposit, outside oppen |
| 3 | VIP level sufficient for sub-accounts | See below |

`ASTER` gives the VIP requirement for sub-accounts as "All VIP levels", with a cap
of 10 at VIP1–2 rising to 50 at market-maker tier 3. **Not confirmed:** whether a
never-traded wallet, which is below VIP1, gets exactly 10 — the requirement says
all levels but the table's lowest row is VIP1. Onboarding must therefore attempt
and classify here too, and must not tell the user how many containers they can have
before the venue has answered. Also unconfirmed: whether sub-accounts function in
Shield Mode / 1001x.

### 4.2 First agent, step by step

1. **[A]** oppen generates the **child keypair** for the sub-account, in the Rust
   core, into the keychain.
2. **[A]** oppen produces `childSignature` with that key.
3. **[C]** **`createSubAccount` master half**, EIP-712 on `chainId` 1666, signed by
   the master wallet. Both signatures go in one `POST /fapi/v3/createSubAccount`.
4. **[A]** **Show the child key once, and require the user to save it.** See §4.5 —
   this is the step that differs from every other in this document.
5. **[A]** oppen sets the sub-account's **position mode** before anything else can
   create a position. Aster supports hedge mode via `POST
   /fapi/v3/positionSide/dual`; it applies to every symbol on the account and
   **cannot be changed while positions or orders exist**. oppen sets one-way mode,
   so that the position model is identical across all three venues and the
   guardrail engine has one shape to reason about. If a later decision adopts hedge
   mode, it must be set here, at provisioning, and never afterwards.
6. **[A]** oppen generates the agent keypair.
7. **[C or A]** **`registerAndApproveAgent`** with `canSpotTrade` and
   `canPerpTrade` as configured, `canWithdraw: false`, and an expiry 90 days out
   (D-b). `ipWhitelist` is required only when `canWithdraw` is true, which oppen
   never requests. Whether this is signed by the sub-account key oppen holds (app
   action) or by the master (ceremony) is unconfirmed — §2.3.
8. **[C]** **`subAccountTransfer`** from the master, to fund the container. Instant
   and free, master↔sub or sub↔sub.
9. **[A]** Registry, ledger, pairing, connection test — as Hyperliquid steps 7–9.

**Ceremonies: 2, or 3** depending on step 7. Aster has no builder-fee analogue in
this flow, so there is no third approval of that kind.

### 4.3 Second and Nth agent

The same sequence, once per agent. **Marginal cost: 2 or 3 ceremonies**, the same
range, up to the venue's sub-account cap for the user's tier.

Two limits to enforce locally rather than discover: **API keys are capped at 30 on
the master and 10 per sub-account**, and **sub-accounts only support the V3 API**.
A client that falls back to an older API version against a sub-account will fail in
a way that has nothing to do with the request.

VIP level is computed on the aggregated master-plus-subs group, so unlike
Hyperliquid's top-level model, splitting agents across containers costs nothing in
fee tier.

### 4.4 What oppen generates, and what the user provides

| Generated by oppen | Provided by the user |
|---|---|
| The sub-account child keypair — **and this is the one the user must also keep** | The master wallet and its balance |
| The agent keypair, per agent | The master-half signature on `createSubAccount` |
| The MCP bearer token | The funding transfer |

### 4.5 Key custody, and the one that is shown once

`ASTER`: each sub-account gets its own generated wallet address and private key,
**shown once and never stored by Aster**, unrecoverable if lost.

In the API path, the caller supplies the child key, so oppen generates it. That
does not make oppen a backup. oppen's keychain is one machine, and a machine can
be lost. So step 4 of §4.2 is a real ceremony with no signature in it: the child
key is displayed once, the user is required to confirm they have stored it
elsewhere, and the display is never repeated.

| Key | Held by | Can | Cannot |
|---|---|---|---|
| Master wallet | The user | Everything, including withdrawal | — |
| Sub-account child key | oppen keychain **and the user's own backup** | Act as that sub-account | **Withdraw externally** — `ASTER` gives withdrawal as master Yes, sub No |
| Agent key, `canWithdraw:false` | oppen keychain | Trade for that account until expiry | Withdraw. API withdrawal permission on a sub is "permanently disabled" |

Why oppen still registers a separate agent key rather than trading with the child
key it already holds: the agent key **expires** and can be rotated, which is what
makes D-b's "authority decays by default" literally true. The child key cannot be
rotated, because the sub-account cannot be deleted and the key was shown once. The
child key is provisioning authority; the agent key is trading authority; they are
not the same job.

### 4.6 Funding and rebalancing

`subAccountTransfer` is instant, free, and works master↔sub and sub↔sub. Whether it
can be signed by the sub-account's own key — which would make rebalancing an app
action — is unconfirmed. Assume a ceremony until tested.

### 4.7 Failure and recovery

| Interrupted at | Recovery | Unrecoverable? |
|---|---|---|
| Child key generated, `createSubAccount` not signed | Discard the key, start again | No |
| `createSubAccount` submitted, response lost | Query the sub-account list before retrying. A retry that succeeds twice creates a second container that **cannot be deleted** | No, but the extra container is permanent |
| Child key display dismissed without saving | oppen still holds it in the keychain, so trading works. The user has no backup | Not yet — but a lost keychain then is |
| Child key lost from both oppen and the user | The sub-account cannot be signed for. Funds cannot be withdrawn from a sub-account in any case; whether the master can sweep them without the child key is **unconfirmed** | **Assume yes until tested** |
| Agent key lost | Register a new agent key with a new expiry | No |
| Position mode set wrong, positions opened | It cannot be changed with open positions or orders | No — but it requires flattening first |

The unconfirmed cell is the most important one on this page. Until someone
demonstrates on Aster testnet that a master can recover a sub-account's balance
without the child key, onboarding must treat the child key as **capital-critical**
and say so in those words at step 4.

---

## 5. Lighter

Lighter's provisioning is the cheapest of the three in signatures and the least
settled in custody.

### 5.1 Prerequisites

| # | Prerequisite | Cost |
|---|---|---|
| 1 | A wallet the user controls | 0 |
| 2 | A deposit of **≥ 1 USDC** direct, or **5 via CCTP**, which mints the master account index | 1 on-chain transaction, outside oppen |
| 3 | An account tier that permits enough sub-accounts | 0 at Standard, which is free and default and gives 4 |

`LIGHTER` documents no invite gate today, but it also carries no affirmative
statement that account creation is open to all. That is worth one line in the UI
rather than a promise.

### 5.2 The key-registration fork

Registering an API key is `ChangePubKey`, which `LIGHTER` says requires the L1
private key. Whether that means an L1 *signature* over the new public key or the
raw key inside the signing process was not settled by the research pass. oppen's
behaviour under each answer:

- **If it is a signature.** oppen renders it, the user signs it in their wallet as
  a ceremony, and the flow is fully in-app.
- **If it genuinely requires the raw private key.** oppen does not implement it.
  Onboarding sends the user to Lighter's own key-registration surface, waits, and
  accepts only the resulting API key string. There is no code path in oppen that
  accepts an L1 private key, on any venue, under any answer to this question (D5,
  AGENTS.md 4).

The rest of §5 assumes the signature case and marks the count as conditional.

### 5.3 First agent, step by step

1. **[U]** Deposit ≥ 1 USDC. The master account index is minted by the deposit.
2. **[A]** oppen generates the API keypair.
3. **[C, conditional]** **`ChangePubKey`** registering that key on the master
   account index. See §5.2.
4. **[A]** **`L2CreateSubAccount`**, signed by the API key. No Ethereum key
   involved, no ceremony. This is the reason Lighter is the cleanest of the three
   operationally.
5. **[A/C]** Register the agent's own API key on the sub-account. Whether this
   needs another L1-authorised `ChangePubKey` per account index, or whether the
   master's key suffices, is **unconfirmed** — §5.6.
6. **[A]** Same-master transfer to fund the sub-account, signed by the API key.
7. **[A]** Registry, ledger, pairing, connection test.

**Ceremonies: 1**, conditional on §5.2 and on step 5.

### 5.4 Second and Nth agent

Steps 4–7. If step 5 needs no ceremony, **the marginal cost of an additional agent
on Lighter is zero signatures** — the only venue of the three where adding an agent
is a pure app action. If it does, it is one.

The caps are tier caps, not volume gates: 4 sub-accounts on Standard, 16 on Plus,
64 on Premium. Changing tier requires no open positions, no open orders and at
least 24 hours since the last change, so a tier upgrade is a planned operation, not
something to do while agents are running.

**Not confirmed:** the exact API key count per account index. Three official
Lighter pages disagree — 253, 254 and 256 — with indices 0 and 1 reserved. Treat
the limit as "around 253" and read the venue's own error rather than a constant.

### 5.5 Key custody

| Key | Held by | Can | Cannot |
|---|---|---|---|
| L1 account key | The user's wallet | Everything, including Fast Withdrawals and Transfers | — |
| API key | oppen keychain | Trade, read, create sub-accounts, transfer between the master and its subs, and process **secure withdrawals to the L1 address that created the account** | Send funds to any other address. Fast Withdrawals. Transfers |
| Read-only token | Optional, expiry 1 day to 10 years | Read | Trade. Withdraw |

The API key row is the one to read twice, and §2.4 states the consequence: on
Lighter the containment property is *funds return to the owner*, not *funds cannot
move*. That is weaker than the other two venues and the threat model must say so
per venue before Lighter ships.

### 5.6 Failure, recovery, and what is unconfirmed

| Interrupted at | Recovery | Unrecoverable? |
|---|---|---|
| Key generated, `ChangePubKey` not completed | Discard and regenerate | No |
| `L2CreateSubAccount` submitted twice | Two containers exist. **Sub-accounts cannot be deleted** | No, but the extra container is permanent |
| API key lost | Register a replacement on a free key index | No |
| L1 account key lost | Everything | **Yes** — the user's wallet |

Unconfirmed on Lighter, carried forward from the research pass and not resolved
here:

1. Whether `ChangePubKey` needs the raw L1 key or a signature — §5.2.
2. Whether registering a key on a **sub-account** index needs its own L1
   authorisation.
3. Whether hedge mode is truly absent. The research pass inferred one position per
   `(account_index, market_id)` with a single sign field **from the data model**;
   Lighter's documentation never states it. Inference from a schema is not a
   citation and it is recorded here as inference.

---

## 6. Comparison

Ceremonies only — on-chain deposits are counted separately because they cost gas
and block time as well as an approve.

| | Hyperliquid | Aster | Lighter |
|---|---|---|---|
| **Container primitive in v1** | Top-level account the user controls | Sub-account | Sub-account |
| **Gate on containers** | $100k traded volume for sub-accounts, so none available to a new user | None. Cap 10 → 50 by VIP tier | None. Cap 4 / 16 / 64 by account tier |
| **On-chain transactions before the first agent** | 1 deposit (mainnet); testnet needs a prior mainnet deposit to unlock the faucet | 1 deposit | 1 deposit, ≥ 1 USDC direct or 5 via CCTP |
| **Ceremonies to the first trade** | **3** (2 with builder fee declined) | **2–3** (see §2.3) | **1**, conditional on §5.2 |
| **Ceremonies per additional agent** | **3** (2 without builder fee) | **2–3** | **0–1** |
| **Cost shape as agents grow** | Linear | Linear | Flat, if key registration is master-authorised |
| **Container creation cost** | 0 — a wallet key derivation | 1 ceremony, dual-signed | 0 — an app action signed by the API key |
| **Funding a container** | `usdSend`, 1 ceremony, internal, instant, no bridge | `subAccountTransfer`, 1 ceremony, instant, free | Same-master transfer, app action |
| **Trading key can withdraw?** | No — venue-enforced | No — `canWithdraw:false`, and sub-account withdrawal is disabled outright | **Yes, to the owner's own L1 address only** |
| **Trading key expiry** | `valid_until`, ≤ 180 days; oppen uses 90 | Explicit expiry field; oppen uses 90 | Not documented for API keys; read-only tokens expire 1 day – 10 years |
| **Hedge mode** | No. "This parameter won't have any effect until hedge mode is introduced" | Yes, account-scoped, immutable once positions or orders exist. oppen sets one-way | Not documented; absence inferred from the schema |
| **Unrecoverable if lost** | The user's wallet key. Agent keys are replaceable | **The sub-account child key** — shown once, never stored by Aster. Sub-accounts cannot be deleted | The L1 account key. Sub-accounts cannot be deleted |
| **Silent-failure trap** | Re-approving the same `agentName` deregisters the prior wallet with no error | A duplicate `createSubAccount` leaves a permanent extra container | A duplicate `L2CreateSubAccount` leaves a permanent extra container |

**The line that matters for product decisions:** on Hyperliquid, the cost of a
fleet is linear in the fleet — ten agents is thirty wallet approvals. On Lighter,
if key registration is master-authorised, ten agents is the same one signature as
one agent. Aster sits between them. That ordering is the opposite of the venue
priority in the current roadmap, and it is the reason D1 was rewritten to be
venue-agnostic rather than to be about sub-accounts.

**Wall-clock.** The only step whose duration oppen does not control is the
on-chain deposit, which is a chain property. Every internal transfer named above —
`usdSend`, `subAccountTransfer`, Lighter's same-master transfer — is documented as
instant. Against P7's gate of "fresh machine to a testnet trade in 10 minutes", the
budget is dominated by the faucet on Hyperliquid and by the deposit confirmation on
the other two, not by the ceremonies.

**Cross-venue, said once because it is easy to forget.** Positions on different
venues never net. Three venues is three margin pools, three liquidation prices, and
no cross-margin. An economically flat book posts full margin on both legs, and one
leg can liquidate while the other survives. Aggregating exposure across venues is
the application's job; no venue can see the others.

---

## 7. What oppen must never do during onboarding

1. **Never accept a master private key or a seed phrase.** No paste box, no import
   flow, no advanced mode, on any venue. Any input that parses as 64 hex characters
   or as a BIP-39 mnemonic is refused with a named refusal that says *why* — not
   "invalid format", which teaches the user to try harder.
2. **Never branch program logic on a venue's error message text.** Hyperliquid's
   "Cannot create sub-accounts until enough volume traded" and its
   `Required:` / `Traded:` figures are rendered for the human and stored verbatim
   in the ledger, labelled venue-authored. The program's behaviour on *any*
   `createSubAccount` failure is identical: fall back to the top-level container
   path. A wording change at the venue must not change what oppen does.
3. **Never reuse an agent address across rotations.** `DOC-NONCE`: previously
   signed actions can be replayed once the nonce set is pruned. A rotation
   generates a new keypair; the old address is retired permanently.
4. **Never reuse an agent name on Hyperliquid.** Re-approving the same name
   deregisters the previous wallet silently. Names carry a monotonic suffix and a
   `valid_until`.
5. **Never submit a ceremony without checking the recovered signer.** This is the
   highest-severity onboarding failure available: signing `approveAgent` from the
   funding wallet instead of from the container authorises the agent over the
   account holding everything, and the venue will happily apply it. The recovered
   address is compared to the intended container **before** submission, and a
   mismatch is a refusal.
6. **Never blind-retry an authorisation.** A lost response is not a failure. Query
   the venue's own list — approved agents, sub-accounts, `userRole` — and act on
   what is there. A blind retry of `approveAgent` with a fresh address revokes the
   previous one.
7. **Never generate or hold a key that can withdraw externally.** On Hyperliquid
   that means the container key stays in the user's wallet. On Aster the child key
   cannot withdraw. On Lighter it is not true — so say it, per §2.4, rather than
   inherit a claim from another venue.
8. **Never present a container as isolated before the venue has confirmed it
   exists.** Every "done" in §1.1 is a query, not a dismissed dialog.
9. **Never let onboarding be the thing that raises a guardrail.** A new agent
   leaves onboarding with D-c's near-zero caps and approval mode on. The first
   refusal is the onboarding, and it must name the limit to raise.
10. **Never claim a safety property the shipped build does not have.** If the
    dead-man switch cannot be armed on this account, onboarding says so — see the
    open question in §9 about whether `scheduleCancel` is itself volume-gated.

---

## 8. Acceptance gate

On testnet, on a machine with no prior oppen state:

1. Two agents are onboarded on Hyperliquid into two distinct top-level containers,
   in **six wallet signatures total**, prompted in §3.3's order — `usdSend` before
   `approveAgent` on each container — and each agent's `get_state` reports its own
   container's balance and no part of the other's.
2. An `approveAgent` request whose wallet session is on the wrong account is
   **refused before submission**, with the expected and recovered addresses both
   shown.
3. Onboarding is killed mid-flight between the `approveAgent` submission and its
   response; on relaunch, oppen determines the true state by querying the venue and
   resumes without issuing a second approval.
4. A `createSubAccount` attempt on the same account fails, the raw venue message is
   stored verbatim in the ledger, and oppen proceeds down the top-level path with no
   code path having read that message.
5. Every step of the run appears in the hash-chained ledger, and the chain verifies.

Aster and Lighter gates are written when those venues are scheduled. They are the
same five, with the child-key backup confirmation added to Aster's.

---

## 9. Open decisions and unconfirmed facts

Unconfirmed facts are listed here as well as inline, so that nobody has to trust
that a claim in the body was checked.

**Hyperliquid**

1. Whether the volume counter behind the sub-account gate is lifetime-cumulative or
   windowed. Everything points to cumulative; Hyperliquid has never stated it.
   (**Not** open: whether a cumulative-volume counter is exposed at all. It is —
   `userRateLimit.cumVlm`, §3.9.)
2. Whether `userRateLimit.cumVlm` is the same counter the gate reads. It is
   documented as "Cumulative volume" and the request budget is denominated in
   traded volume, but nothing connects it to `createSubAccount`. Display only.
3. Whether `usdSend` to an address with no prior Hyperliquid activity succeeds.
   §3.3's fund-then-authorise order depends on this, and the testnet runbook's
   step 3 is the test — account 2 there has no prior activity.
   - The mirror question, which §3.3's order rests on and which **nothing
     currently tests**: whether an account that has never been funded can
     `approveAgent` at all. Following the order avoids the case. Proving it needs
     a deliberate `approveAgent` on an unfunded testnet address.
4. Which user-signed actions an API wallet is barred from beyond withdrawal —
   [hl-signing.md](../hl-signing.md) open question 3. Needs a testnet negative test.
   This is also what leaves two cells of [threat-model.md](../threat-model.md)'s
   per-venue key table reading "unconfirmed" rather than "no".
5. Whether an API wallet approved by a master can sign for **all** its
   sub-accounts, or only the one it was allowanced against. This decides whether
   the §3.9 migration preserves per-container blast radius.
6. Whether a sub-account inherits its master's `approveBuilderFee`.
7. Whether an API wallet may sign `createSubAccount` or `subAccountTransfer`. If it
   can, the §3.9 upgrade provisions and funds a container with no wallet prompt;
   if it cannot, both are ceremonies. No document may state either answer as
   settled until a testnet test does.
   - And, on the same page of the docs: whether an account that is *not* the
     operator's original funding account can create sub-accounts under itself once
     **it** clears the gate (§3.2). It follows from the gate being per user and
     each container being its own user; no page states it for this case.
8. Whether `scheduleCancel` is itself volume-gated. A reverse-engineered binary
   string suggests it; official docs are silent. If it is, a brand-new account has
   no dead-man switch and onboarding must not promise one. Separately, the
   documented budget is a minimum of 5 seconds ahead and a **maximum of 10 triggers
   per day, resetting 00:00 UTC** — whether a "trigger" is an arm or a firing is
   not stated, and P3's design depends on the answer.
9. Whether the builder address falling below the required 100 USDC perps balance
   rejects the order or silently drops the fee. Invariant 10 forbids the order path
   being dropped silently, so this must be tested before mainnet.

**Aster**

10. Whether `registerAndApproveAgent` is signed by the sub-account key or the
    master. This is the difference between 2 and 3 ceremonies per agent.
11. Whether a never-traded wallet, below VIP1, gets 10 sub-accounts.
12. Whether the master can recover a sub-account's balance without the child key.
    Treated as **no** until demonstrated, which is what makes the child key
    capital-critical.
13. Whether there is any cap on agents per account.
14. Whether sub-accounts function in Shield Mode / 1001x.

**Lighter**

15. Whether `ChangePubKey` needs a signature or the raw L1 key — §5.2. Decides
    whether Lighter onboarding is in-app at all.
16. Whether a sub-account's own key registration needs separate L1 authorisation.
    Decides whether the marginal agent costs 0 or 1 signature.
17. The exact API key count per account index; three official pages disagree.
18. Whether hedge mode is truly absent. Inferred from the schema, never stated.

**Product decisions this spec assumes but does not own**

19. Aster position mode: §4.2 sets one-way at provisioning so the position model is
    uniform across venues. If hedge mode is ever wanted, it must be set at
    provisioning and can never be changed afterwards.
20. ~~Whether the first agent may use the funding account as its container.~~
    **Settled 2026-09-04 in §3.2**, and recorded here so the question is not
    re-opened by someone reading only this list: it may, it is never the default,
    and it is presented as a downgrade with the account's balance shown in
    dollars. What remains open is question 7's sub-bullet, not this.
21. Whether an operator may bind two agents to one container at all, given that
    §1.3's shared-address guards exist precisely for that case.
