# Testnet provisioning runbook

For the current supervised alpha, follow
[supervised testnet acceptance](supervised-testnet-acceptance.md). This older
provisioning record includes a low-level signing example and historical account
observations; neither establishes current identity, consent, remaining pilot
capacity or guarded-runtime acceptance. Do not use the low-level order example
as a substitute for the supervised pilot path.

Done by hand in the Hyperliquid web UI. It cannot be automated: every step is a
signature from an account's own wallet, and D5 forbids a master-key path in the
app.

Network: **testnet** — <https://app.hyperliquid-testnet.xyz>
Account 1: `0xBF829199c1AE7f0Caf21FB6FC45e10EdFf25B7D2`

> **Revised 2026-09-04.** The previous version told you to create four
> sub-accounts. The venue refused:
>
> ```
> Cannot create sub-accounts until enough volume traded. Required: $100000. Traded: $0.
> ```
>
> That gate is protocol-enforced, it applies on testnet at the same threshold,
> and no API bypasses it. D1 was revised the same day: the unit of isolation is
> one venue account per agent — oppen calls it a **container** — a sub-account
> where the venue grants one, a top-level account otherwise
> ([specs/venue-containers.md](../specs/venue-containers.md),
> [decisions.md](../decisions.md) V1–V6). On Hyperliquid today you get top-level
> accounts, and this runbook provisions two of them.
> [Appendix A](#appendix-a--clearing-the-100k-sub-account-gate-on-testnet-optional)
> has the arithmetic for clearing the gate later. Nothing in v1 needs it.
> Source: <https://hyperliquid.gitbook.io/hyperliquid-docs/trading/sub-accounts>
> — "Up to 10 sub-accounts can be created after reaching $100,000 in volume."

---

## 0. Where you actually are

Verified against the live venue on 2026-09-04, on both networks.

| | Testnet | Mainnet |
|---|---|---|
| Spot USDC | 999.0 mock | 29.699177 |
| `clearinghouseState.marginSummary.accountValue` | 0.0 | 0.0 |
| `portfolio.accountValue` | 999.0 | 29.699177 |
| `webData2.cumLedger` — net deposits, **not** equity | 999.0 | 29.69 |
| Available to Trade, perps ticket | 999.00 USDC | 29.70 USDC |
| `userRole` | `user` | `user` |
| Sub-accounts | 0 | 0 |
| API (agent) wallets | **1 already approved** | **1 already approved** |

Three things this table settles, and the first two reverse what the 2026-09-03
version of this file told you to do.

- **Hyperliquid margin is unified. There is no spot-to-perps transfer to make.**
  The perps order ticket carries a `Cross | 20x | Unified` control and reads
  `Available to Trade 999.00 USDC` against a spot balance of 999 and a perps
  `accountValue` of 0.0. The spot USDC *is* the perps collateral.
- **`accountValue` is therefore not the balance.** It reads 0.0 on an account
  with 999 USDC of buying power, on both networks. Anything that gates on it —
  ours included — reports a funded account as empty. The real figure is
  `perps accountValue + spot at mark`, and `portfolio`'s `accountValueHistory`
  is the venue's own answer to it; the perps ticket agrees with both.
  **`webData2.cumLedger` is not the figure** — it is cumulative net deposits,
  and equals equity only while PnL is zero
  (mainnet: `29.699177 = 29.69 + 0.009177`). This is tracked as a bug against oppen's equity read,
  not a venue problem.
- **The faucet is already claimed.** It pays 1,000 mock USDC and only pays an
  address that has previously deposited on mainnet; the 29.699177 USDC mainnet
  deposit is what satisfied that, and it is why `userRole` reads `user` rather
  than `missing`. You do not need to claim anything again.

## 1. Nothing to move

This step used to say "move the testnet spot balance to perps" and it was wrong.
Under unified margin the balance is already collateral, `usdClassTransfer` is not
part of provisioning, and the perps ticket will let you place an order against
the spot 999 as it stands.

`usdClassTransfer` with `toPerp: true` is still a real user-signed action
([hl-signing.md](../hl-signing.md) §1 and §3.2) and oppen still implements it. It
is simply not a precondition for trading.

> If you went looking for that transfer in the UI and hit
> `Insufficient USDC or HYPE balance for token transfer gas.`, that is the **spot
> send** control, not a class transfer. Spot sends charge gas in the token being
> sent or in HYPE; class transfers charge nothing because no chain transaction
> exists. The error was telling you that you were on the wrong control, and the
> right answer was that neither control was needed.

## 2. One API wallet on account 1, then the P1 gate

**Before you start: there is already an agent on this account.** `webData2`
reports one on both networks, created by the Hyperliquid web app for its own
order flow:

| Network | `agentAddress` | `agentValidUntil` |
|---|---|---|
| Testnet | `0x7d6707a343e712a79cebced9e843c50d2978e82a` | 2026-09-18T11:57:33Z |
| Mainnet | `0x010a99f82d04b3ecaa3504b75b4a4c49db5c20ec` | 2026-09-18T11:57:57Z |

Either one *would* satisfy the P1 gate, and its private key is in the browser,
under the `localStorage` key `hyperliquid_agent_<your address>` on the matching
Hyperliquid origin. Do not use it for oppen. It is the web app's wallet: it is
unnamed, it expires in about two weeks rather than on oppen's 90-day schedule,
and reusing it means oppen and the web app share a nonce space, which is exactly
what spec item 7's nonce isolation exists to prevent. Create your own below.

**The ceremony, in order.** More → API.

1. Type the name into the name box.
2. Click **Generate**. This fills the API wallet *address* only. It does not
   authorize anything and it does not touch your wallet.
3. Click **Authorize API Wallet**. A modal opens.
4. The modal has a required **Days Valid** field with a `MAX` link beside it.
   It starts empty and the flow will not complete until you fill it. `MAX` is
   180; oppen's default is 90 (D-b).
5. The private key appears once, in a red box. Copy it to your password manager
   now.
6. Click **Authorize** and sign in MetaMask.

> **If Authorize appears to do nothing, the button is not the problem.** Verified
> on 2026-09-04: the button carries no `disabled` attribute, `pointer-events` is
> `auto` and the click lands. What fails is the wallet call behind it. In the
> observed case `wallet_getPermissions` returned `[]` and `eth_accounts` returned
> `[]` for `app.hyperliquid-testnet.xyz` while MetaMask was unlocked — the site
> had no account permission at all, and was showing the address from its own
> cache. The console says so at page load:
>
> ```
> Wallet did not respond to eth_accounts. Defaulting to prefetched accounts.
> Must call 'eth_requestAccounts' before other methods
> ```
>
> Every signature request then throws immediately and the page swallows it. A
> second symptom stacks on top: once one permission request is queued, MetaMask
> answers the next with `Request of type 'wallet_requestPermissions' already
> pending for origin ... Please wait.` and opens nothing. Fix it by clearing the
> pending MetaMask notification, then reconnecting the site to the account —
> after which `wallet_getPermissions` returns a non-empty array. Check that
> before re-reading any of oppen's signing code.


| Name | Signs for | Purpose |
|---|---|---|
| `oppen-alpha-2026q4` | account 1 | The P1 gate, then agent alpha |

**Copy the private key into your password manager the moment it is shown.** It
is displayed once and never again. Do not paste it into a chat, a file in this
repo, or a shell history — see [What not to do](#what-not-to-do).

Three rules that come from the protocol, not from taste:

- **Names must be unique forever.** Re-approving the same `agentName` replaces
  the previous agent silently. A rotation that reuses a name revokes the wallet
  you are still trading with, and tells you nothing.
- **You get 3 named agent wallets per account, plus 1 unnamed.** DOC-EXCH, quoted
  in [hl-signing.md](../hl-signing.md) §3.3: "An account can have 1 unnamed
  approved wallet and up to 3 named ones. And additional 2 named agents are
  allowed per subaccount." With zero sub-accounts that is 3 named wallets on
  account 1 — enough to hold an old and a new wallet live during a rotation.
- **Expiry is set in the modal, not in the name.** DOC-EXCH documents a name
  suffix: "A custom expiration can be set by appending `valid_until {timestamp}`
  after the name. The expiration can be at most 180 days in the future." The web
  UI does not need it — the authorize modal has its own **Days Valid** field,
  capped at the same 180, and that is what you fill. An earlier version of this
  file said it was unconfirmed whether the name box passed the suffix through;
  the question is moot in the UI. Keep the suffix in mind only for agents oppen
  approves programmatically. Decision D-b sets oppen's default at 90 days,
  warned from 14.

Then run the gate:

```sh
cd ~/projects/oppen
source ~/.cargo/env
export OPPEN_TESTNET_AGENT_KEY=0x…      # oppen-alpha-2026q4
export OPPEN_TESTNET_USER=0xBF829199c1AE7f0Caf21FB6FC45e10EdFf25B7D2
cargo run -p oppen-hl --example testnet_order
unset OPPEN_TESTNET_AGENT_KEY
```

Leave `OPPEN_TESTNET_VAULT` unset. It exists to route through a sub-account, and
under the revised D1 there is no sub-account: the agent wallet signs, and orders
land in account 1 directly.

Expected, line by line:

- `agent 0x…` equals the API wallet address shown in the app. If not, the key
  was pasted wrong.
- `equity …` — **expect 0 today, and that is not a failure.** It is read from
  `clearinghouseState.marginSummary.accountValue`, which reports 0.0 on a funded
  unified account (§0). Judge funding by `portfolio.accountValue` or the perps
  ticket, not by this line. Once oppen's equity read is fixed this line becomes
  meaningful again.
- `place [Resting { oid: … }]`
- `status open oid=… cloid=Some(…)`
- `cancel [Success]`
- `expires rejected as expected: …` — this closes nothing now, since the
  encoding was settled by differential recovery, but it confirms the venue
  agrees.
- `budget …/… used, cumVlm …` — note `cumVlm`. It matters in Appendix A.

## 3. A second agent is a second account

MetaMask → account menu → **Add account** → next account from the same seed. One
click, no new seed phrase, no new backup. Call it `oppen-beta` in MetaMask so the
signing prompts are legible.

Then, **in this order**:

1. **Fund it.** From account 1, `usdSend` to account 2's address. This moves USDC
   between Hyperliquid accounts and "does not touch the EVM bridge" — internal
   and instant. It is a user-signed action from account 1's own wallet
   ([hl-signing.md](../hl-signing.md) §1); an agent wallet cannot do it for you.
   Suggested split: **400 to account 2**, leaving ~599 on account 1. The $10
   minimum notional means anything under about $150 cannot hold a position and a
   stop at the same time.
2. **Then** create account 2's API wallet — More → API while connected as
   account 2, name `oppen-beta-2026q4`, same rules as step 2.

The order is load-bearing, and it is the same order the app uses. **Fund, then
authorise** is settled once in [specs/onboarding.md](../specs/onboarding.md) §3.3
— that paragraph carries the reasoning, the evidence and how strong the evidence
is, and this runbook does not re-argue it. The short version: `approveAgent` is
signed by account 2 itself, Hyperliquid rejects actions from an address that has
never been funded with `Must deposit before performing actions. User: 0x123...`
([hl-signing.md](../hl-signing.md) §1), and that rejection looks exactly like a
signing bug.

One honest caveat, because it changes what you should do if it goes wrong: that
error string is documented as a symptom of *signing bugs* — a bad signature
recovers a random, never-funded address — and no page read states funding as a
precondition of `approveAgent` in its own right. The inference is strong and it is
not proven, and **this runbook does not prove it either** — following the order
avoids the case rather than testing it. Proving it would mean deliberately
approving on an unfunded account, which is not asked for here. What follows from
that: if `approveAgent` on a *funded* account 2 fails anyway, it is a signing bug
and not an ordering problem. Record the exact message before changing anything.

Account 2 **cannot claim the faucet.** The faucet pays only an address with a
prior mainnet deposit, and account 2 has none. `usdSend` from account 1 is the
only way to fund it, which is also the per-agent cost the revised D1 accepts:
`usdSend` to fund, `approveAgent` to authorize, and later a third —
`approveBuilderFee` is a per-account approval, so an agent account must approve
the builder fee itself before D7's builder code can ride on its orders
([decisions.md](../decisions.md) O7). Two signatures to trade, three to trade
with the builder code attached.

*Not confirmed:* whether `usdSend` to an address with no prior Hyperliquid
activity creates that account, or whether the destination must be touched some
other way first. This step is the test. If the transfer is rejected, stop and
record what the venue said — the answer sets the real per-agent onboarding cost
and it is listed as open in [decisions.md](../decisions.md).

Run the P1 example against it as a smoke test, with `OPPEN_TESTNET_USER` set to
account 2's address and `OPPEN_TESTNET_AGENT_KEY` set to `oppen-beta-2026q4`.

## 4. The P2 gate

Same shape as step 2 — account 1, its own agent wallet, no `OPPEN_TESTNET_VAULT`.
P2's gate needs fills to reconcile, so leave one resting order alive when you are
done.

## What one agent account cannot do for another

An API wallet signs for its own account and that account's sub-accounts, and
never for an unrelated top-level account. Under the revised D1 every agent is an
unrelated top-level account, so:

- **There is no single operator key that can flatten every agent.** A manual
  close of agent beta's position is signed by account 2's wallet or account 2's
  agent wallet. Nothing on account 1 can reach it.
- **The kill switch is still global**, because it is oppen's own state and oppen
  holds every agent wallet. What is not global is any *venue-side* authority.
- **A shared fee tier is gone.** Volume no longer aggregates across accounts.
  This costs nothing at tier 0, which is where a $0-volume wallet is. It is not
  the *only* thing given up, and an earlier version of this line said it was:
  [specs/venue-containers.md](../specs/venue-containers.md) §4.3 gives the two
  bigger ones — the operator holds N+1 withdrawal-capable wallet keys instead of
  one, and there is no venue-side authority that can flatten the fleet, only
  oppen's own kill switch. Sub-accounts have no private keys at all, which is
  what makes that a key-custody argument rather than a fee argument.

Account 1 hosts agent alpha *and* your manual ticket, which makes it a shared
address in the sense of the shared-address guards recorded in
[decisions.md](../decisions.md): an agent may only cancel or modify order ids it
opened, and the venue's aggregate position book is never sliced per agent. If
that bothers you, move the manual ticket to a third account — the cost is the
same signatures as step 3.

**Account 1 is also the funding account, and that is the downgrade
[specs/onboarding.md](../specs/onboarding.md) §3.2 describes.** It is taken here
deliberately and it is worth naming rather than leaving implicit:

- Agent alpha's worst case is **the whole ~599 mock USDC left on account 1 after
  step 3**, not a figure you chose for it. Every future `usdSend` you make from
  account 1 raises that number back up before it lowers it.
- It is taken because on testnet account 1 is the *only* faucet-eligible address
  (§0), so a two-account setup necessarily has one agent sharing the funder.
  Avoiding it costs a third account and one more `usdSend`.
- The app's default on mainnet is the opposite: a dedicated container for every
  agent, including the first. Do not read this runbook as the recommended mainnet
  shape.

## What not to do

- **Never paste a private key into a chat, an issue, a commit, or a shell that
  records history.** Not this chat, not a subagent's, not a "just to check the
  address" paste. A key that has been in a chat log is burned: rotate it. The
  only place an agent key belongs is your password manager and, once P2's
  keychain path lands, the OS keychain.
- Do not reuse an agent name across rotations — it silently replaces the live
  agent (step 2).
- Do not reuse an agent *address* after rotating it. Pruning loses nonce state
  and previously signed actions can replay.
- Do not `approveAgent` on an unfunded account (step 3).
- Do not fund an account below about $150 and expect it to hold a position and a
  stop.
- Do not try to create a sub-account. It refuses, and the refusal is the message
  at the top of this file.

---

## Appendix A — clearing the $100k sub-account gate on testnet (optional)

Nothing in v1 requires this. The gate is an **upgrade path**: once volume clears
it, oppen can migrate an agent from a top-level account onto a real sub-account,
and get a shared fee tier back. Design for it; do not wait for it.

The upgrade **may** be cheaper per agent, depending on where the signature comes
from — and that is an open question, not a settled saving. An earlier version of
this appendix stated it as settled; it was wrong to.

`createSubAccount` and `subAccountTransfer` are **L1 actions**
([hl-signing.md](../hl-signing.md) §1), and the signer for that whole class is
listed there as "agent (API) wallet **or** master". Open question 3 in the same
document records that no page read states which actions an API wallet is barred
from. So:

- **If** an API wallet may sign them, oppen provisions and funds a sub-account
  with no signature from you at all — against `usdSend`, which is user-signed and
  therefore always a wallet popup.
- **If** it may not, both are ceremonies and the upgrade saves nothing on
  provisioning.

Nobody has tested it. It needs a testnet negative test before mainnet, it is
question 5 in [specs/venue-containers.md](../specs/venue-containers.md) §6, and
no document here may assume either answer. Note the second-order consequence if
the answer is yes: an agent key could move USDC between a master and its
sub-accounts, which is a capability no guardrail currently models.

What does not go away under either answer is `approveAgent`: giving each
sub-account its own agent wallet, which spec item 7's nonce isolation wants, is
user-signed.

**What the gate counts.** Notional traded, not deposits and not PnL. Only fills
count; a resting order that never fills counts nothing. Testnet and mainnet are
separate deployments with separate state, so testnet volume buys testnet
sub-accounts and nothing else.

**Do all of it in one account, and decide which one first.** The gate meters a
single account's volume and volume does not transfer, so two accounts each
trading $60,000 clear nothing while one account trading $100,000 clears it. On
this runbook's layout that account is account 1, which is already where the P1
and P2 gates run. The account that clears the gate is the one that becomes the
master of any sub-accounts — it does not have to be the account that funds the
others, though here it is
([specs/venue-containers.md](../specs/venue-containers.md) §2.1).

**The arithmetic.** On ~1,000 mock USDC, with BTC allowing 40x on testnet:

| Leverage | Position notional | Volume per round trip | Round trips to $100,000 |
|---|---|---|---|
| 40x | $39,960 | $79,920 | 1.25 |
| 20x | $19,980 | $39,960 | 2.5 |
| 10x | $9,990 | $19,980 | 5.0 |
| 5x | $4,995 | $9,990 | 10.0 |

A round trip is open + close, and both legs count, which is why the volume column
is twice the notional.

**The cost is the same in every row**, because fees are a function of total
volume rather than leverage:

| Path | Rate | Cost of $100,000 of volume |
|---|---|---|
| Taker both legs | 0.045% | **$45** |
| Maker both legs | 0.015% | $15 |

Rates are Hyperliquid's base tier (docs → Trading → Fees); yours are the base tier
because your traded volume is $0. Maker-only means posting ALO orders and waiting
for fills, which is slower and does not always fill. $45 of mock money is the
honest answer.

**Do it in small pieces.** At 40x the whole balance is the margin. Hyperliquid
sets maintenance margin at half the initial requirement, so a full-size 40x
position liquidates on roughly a 1.25% adverse move — *approximately, before fees
and funding*. Read the liquidation price the app shows before submitting and do
not trust that number over it. Ten round trips at 5x cost the same $45 and are
far harder to liquidate, and a liquidation partway through ends the exercise with
no balance left to trade. Crossing a funding hour with a position open also pays
or receives funding, which is real mock money either way.

**Checking progress.** `userRateLimit` returns `cumVlm`, documented as
"Cumulative volume" (`crates/oppen-hl/src/types.rs:639`), and the P1 example
already prints it on its last line
(`crates/oppen-hl/examples/testnet_order.rs`). It was verified live on
2026-09-04 in a response carrying
`{"cumVlm":"188908641154.22","nRequestsUsed":…}`.

That settles one thing and leaves two open. **Settled:** a cumulative-volume
counter *is* exposed. [decisions.md](../decisions.md) O3's flat statement that no
info endpoint exposes cumulative volume is **false** and is being corrected there;
an earlier version of this appendix logged the two as an unresolved disagreement,
which was the wrong call — one side of it is observable.

**Still open, and it is why nothing changes operationally:** whether the
sub-account gate reads that same counter — no documentation links them — and
whether the counter is lifetime or windowed. So `cumVlm` is a progress indicator
you may watch, labelled an estimate the venue does not confirm. The way to learn
whether the gate has cleared is still to attempt the creation and classify the
refusal, and `Required:` / `Traded:` in that message are for **display only**.
Never branch program logic on venue message text.

Two side effects worth knowing: the request budget is 1 request per 1 USDC traded
on top of a 10,000-request initial buffer (`docs/spec.md` item 10), so $100k of
volume also buys request headroom; and the volume that clears the gate is real
traded volume, so a wrong-sized order at 40x is a real mock-money loss.

*Not confirmed:* whether Hyperliquid's counter is lifetime-cumulative or
windowed. Everything observed points to cumulative and Hyperliquid has never
stated it. If it is windowed, an interrupted run can decay.

---

## Appendix B — what changed from the 2026-09-03 version

| Then | Now | Why |
|---|---|---|
| Step 3: create four sub-accounts (`agent-alpha`, `agent-beta`, `manual`, `spare`) | One top-level account per agent, two accounts total | The venue refused with `Required: $100000. Traded: $0`. D1 revised 2026-09-04 |
| "3 API wallets by default, plus 2 more per sub-account. Four sub-accounts allows 11" | 3 named wallets (plus 1 unnamed) on each account, because there are no sub-accounts | Same cause. The per-sub-account bonus is unreachable at $0 volume |
| Fund sub-accounts by internal transfer | Fund account 2 with `usdSend` from account 1 | Sub-account transfers do not apply between unrelated top-level accounts |
| `OPPEN_TESTNET_VAULT` set to the sub-account address for P2 | Unset | There is no sub-account to route through |
| A `manual` sub-account so no operator key sits on the master | The manual ticket signs from the account it is intervening in | An API wallet cannot sign for an unrelated top-level account, so a single operator signer no longer exists |
| A `spare` sub-account as a rotation target | A second named agent wallet on the same account | Rotation is a new wallet, not a new account. Three named slots is enough for an overlap |
| Step 1: move the spot balance to perps with `usdClassTransfer` | Nothing to move | Hyperliquid margin is unified. The perps ticket shows `Available to Trade 999.00 USDC` against a spot 999 and a perps `accountValue` of 0.0 |
| "`equity` is non-zero; if it reads 0, step 1 did not land" | Expect 0; judge funding by `portfolio.accountValue` | `accountValue` reads 0.0 on a funded unified account, on both networks |
| Step 2 was three sentences, with the `valid_until` name suffix flagged unconfirmed | Six numbered steps, including the required **Days Valid** field | The authorize modal has its own expiry field, so the suffix is moot in the UI |

### Corrected later the same day

Three claims in the first 2026-09-04 draft of this runbook were wrong or
overstated. They are listed rather than quietly edited, because someone may have
read the earlier version.

| Claim | Status | Where it is settled now |
|---|---|---|
| "`createSubAccount` and `subAccountTransfer` are L1 actions, so oppen's own agent wallet signs them" | **Overclaimed.** The signer class is "agent (API) wallet **or** master" and no page says which of those an agent may be. Untested | Appendix A above; [specs/venue-containers.md](../specs/venue-containers.md) §6 question 5 |
| "An earlier pass recorded that no info endpoint exposes cumulative volume, which `cumVlm` contradicts" | **Resolved; O3 now carries the correction.** `cumVlm` exists and was read live. What is still open is whether the gate reads it, and whether it is lifetime or windowed | Appendix A, "Checking progress" |
| "A shared fee tier … is the only thing lost by not using sub-accounts" | **False.** Key custody and the absence of a venue-side authority that can flatten the fleet are larger | [specs/venue-containers.md](../specs/venue-containers.md) §4.3 |

The ceremony order in step 3 did not change. What changed is where it is argued:
[specs/onboarding.md](../specs/onboarding.md) §3.3 now owns the rule for all four
documents, and it records that the rule rests on an inference nothing has yet
tested.
