# oppen threat model

Read this before trading real funds. It states what oppen's safety features do and do not defend against.

## The setup

oppen runs on your machine. So do your agents. An agent with shell access on the same machine, under the same OS user, is inside the trust boundary of everything oppen stores locally.

## What is a hard boundary

**A trading key oppen holds cannot withdraw — on Hyperliquid and on Aster. On Lighter it can.** That is the single most important safety difference between the three venues, so it is stated per venue before anything else rather than summarised into one sentence:

- **Hyperliquid.** An API wallet cannot move funds to any other address. `withdraw3`, `usdSend` and `spotSend` are user-signed actions, signed by the account itself ([hl-signing.md](hl-signing.md) §1). There is one exception and it stays inside the same address: `agentSendAsset` is documented as "Similar to send asset, but can be signed by an agent. Destination must match the source address", so an agent can move collateral between that one address's own balances. That is not a withdrawal — it cannot reach a second address — and it is still a movement of margin oppen did not initiate.
- **Aster.** An agent registered with `canWithdraw: false` cannot withdraw, and withdrawal from a sub-account is "Permanently disabled" outright. `ipWhitelist` is required only when `canWithdraw` is true, which oppen never requests.
- **Lighter.** An API key **can** process withdrawals. The restriction is on the destination, not on the capability: a *secure* withdrawal "can only be sent to the same L1 address that created the account", and Fast Withdrawals and Transfers to other addresses need the L1 key, which oppen never holds. So a stolen Lighter key forces funds out of the venue to their owner rather than to an attacker. That bounds the loss. It does not make the movement impossible, and it is a weaker guarantee than the other two venues give. Anyone trading Lighter through oppen must be told this in these words; "trade-only key" is false there.

Where it holds — Hyperliquid and Aster — this is enforced by the venue rather than by oppen, and it is the only containment property that survives a fully compromised machine. Two further qualifications are in [The multi-account model](#the-multi-account-model) below: on Hyperliquid the scope is documented but not yet proven by oppen's own negative test, and several neighbouring "cannot" claims are inferences rather than venue statements. v1 ships Hyperliquid only, so the Lighter row above is what will be true when that venue ships, not a property of the app today.

**No account-owner key ever enters oppen.** Onboarding signs, in your own wallet over WalletConnect, the `usdSend` that funds each agent's account, then `approveAgent`, then `approveBuilderFee`. That order is deliberate and is settled once in [specs/onboarding.md](specs/onboarding.md) §3.3. oppen never sees those keys, so oppen cannot leak them.

Note the plural. Under the revised D1 there is one account-owner key **per agent**, not a single master: `usdSend` is signed by the funding account and `approveAgent` by the new account itself, because an API wallet signs only for its own account and that account's sub-accounts. Every one of those keys can withdraw. All of them live in your wallet, and that is exactly why none of them is oppen's to lose.

**One venue account per agent.** Capital segregation is enforced by the venue rather than by oppen's bookkeeping. An agent's guardrail caps bound its account, and its worst case is that account's balance. That sentence is only as good as the account it names: oppen's default is a dedicated container for every agent, including the first, and an operator who instead points an agent at the account holding their whole Hyperliquid balance has widened the worst case to that balance. It is offered, it is not the default, and the confirmation states the figure in dollars ([specs/onboarding.md](specs/onboarding.md) §3.2). D1 was revised on 2026-09-04 to make the unit venue-agnostic — oppen calls it a **container**: a sub-account where the venue grants one, a top-level account otherwise. On Hyperliquid a new user gets zero sub-accounts, because they are gated behind $100,000 of traded volume, so v1 uses one top-level account per agent. The model is specified in [specs/venue-containers.md](specs/venue-containers.md) and the reasoning is [decisions.md](decisions.md) V1–V6.

## The multi-account model

One agent, one venue account, one key. This section says what that key can do, what it cannot, and what the arrangement does not defend against.

**v1 ships Hyperliquid only** ([spec.md](spec.md); Lighter and Aster are v2+ in [ROADMAP.md](../ROADMAP.md)). The Aster and Lighter columns are recorded now because the D1 revision promoted both venues into the architecture. They describe what will be true when those venues ship, not what oppen does today.

### What an agent key can do, per venue

| | Hyperliquid API wallet | Aster agent, `canWithdraw:false` | Lighter API key |
|---|---|---|---|
| Place and cancel orders | yes | yes, under `canPerpTrade` | yes |
| Withdraw to an arbitrary address | no | no | no |
| Withdraw to the owner's own L1 address | no | no | **yes** — "secure" withdrawals |
| Move assets **within** its own address | **yes** — `agentSendAsset`, "Destination must match the source address" | not in the permission set | yes — it is the same account index |
| Transfer to **another** account | **unconfirmed.** `usdSend` is user-signed, and `subAccountTransfer` is an L1 action whose signer class is "agent (API) wallet or master" — no page read says which of those an agent is barred from | not in the permission set | **yes, within the same L1 account** — an API key moves collateral master↔its own sub-accounts. To any other L1 address: no, that needs the L1 key |
| Approve another agent | **unconfirmed.** `approveAgent` is user-signed; nothing read states an API wallet cannot sign it | not in the permission set | on the account the key is registered to: no — `ChangePubKey` needs the L1 key. On a **sub-account index**: unconfirmed ([specs/onboarding.md](specs/onboarding.md) §5.6) |
| Expiry | `valid_until`, 180 days maximum | explicit expiry timestamp | 1 day to 10 years on read-only tokens |

Sources, all read 2026-09-04: Hyperliquid docs, Nonces and API wallets, and the exchange endpoint's `agentSendAsset` entry; the L1-vs-user-signed split is transcribed with line references in [hl-signing.md](hl-signing.md) §1. `docs.asterdex.com`, `POST /fapi/v3/registerAndApproveAgent`, whose permission set is exactly three booleans — `canSpotTrade`, `canPerpTrade`, `canWithdraw`, with `ipWhitelist` required only when `canWithdraw` is true. `docs.lighter.xyz` on API keys, and [specs/onboarding.md](specs/onboarding.md) §2.4 for the same-master transfer.

**Why some cells say "unconfirmed" rather than "no".** This table is what a person reads before committing real funds, so an unverified "no" in it is worse than an honest "unknown": a reader who trusts a wrong "no" builds on a boundary that is not there. Three different standards of evidence appear above and they are not interchangeable.

| Basis | Cells | What that is worth |
|---|---|---|
| Stated by the venue | Lighter's withdrawal, transfer and `ChangePubKey` cells; Aster's "permanently disabled" withdrawal; Hyperliquid's `agentSendAsset` same-address restriction | Load-bearing |
| Argued from an enumerated surface | Aster's "not in the permission set" — transfer and agent approval are not among the three booleans, so an agent cannot reach them | Strong, but no Aster page denies it outright |
| Not settled by any source read | Hyperliquid's cross-account transfer and agent approval | Do not build a flow that depends on an agent being unable to do these. [hl-signing.md](hl-signing.md) open question 3 is the test that would close it |

Hyperliquid's "cannot withdraw" row is the exception to that last line: it is documented and both reference SDKs sign `withdraw3` with the account key, and it is the one Hyperliquid claim this document treats as load-bearing.

Three things to be plain about:

- **Lighter's key is the weak one, and the third row is why.** Lighter's own documentation says API keys "enable both write and read permissions… and process withdrawals". The mitigation is that only *secure* withdrawals are in scope, and a secure withdrawal "can only be sent to the same L1 address that created the account". Fast Withdrawals and Transfers need the L1 key, which oppen never holds. So a stolen Lighter key cannot send funds to an attacker; it can force them out of the venue to the owner's own address. That is theft-resistant and not disruption-resistant, and it is a lower guarantee than Hyperliquid's or Aster's. Say so to users of that venue rather than describing all three keys as "trade-only". Lighter also offers read-only tokens, and maker-only keys on the Premium tier.
- **Hyperliquid's agent-wallet scope is documented and not yet tested by us.** [hl-signing.md](hl-signing.md) open question 3 records that no page read on 2026-09-03 stated *which* user-signed actions an API wallet is barred from — `withdraw3`, `usdSend` and `approveAgent` alike — and the testnet negative test that would close it is still open in [ROADMAP.md](../ROADMAP.md) P1. The claim rests on the venue's documentation and on both reference SDKs signing every one of those paths with the master key. Treat it as well supported by documentation and not yet verified by oppen's own test.
- **Aster does not document an agent cap.** The API-key counts are documented — 30 on a master, 10 per sub-account, and "sub-accounts only support V3 API" — but no page states a limit on the number of agents. Do not build a rotation scheme that assumes headroom exists.

### Blast radius of one compromised agent key

| Reached | Not reached |
|---|---|
| The full balance of that one account, through trading: adverse fills, maximum leverage, fee burn, a loop that keeps adding | Any other agent's account, on any venue |
| Every resting order on that account | Any account-owner key — none of them is in the keychain at all |
| That account's position book in full, because positions net per account | Withdrawal **to another address**, on Hyperliquid and on Aster |
| On Hyperliquid: `agentSendAsset` between that same address's own balances — margin can leave the perps pool without leaving the address | The signed builder-fee cap, which only an account-owner signature can raise |
| On Lighter only: a secure withdrawal to the owner's own L1 address, and a transfer to that account's own sub-accounts | Nothing else *confirmed*. See the "unconfirmed" cells above: on Hyperliquid, whether the key reaches a cross-account transfer or an agent approval is not settled |

On netting: Hyperliquid has no hedge mode, stated in the `updateIsolatedMargin` specification itself — the position-side parameter "won't have any effect until hedge mode is introduced". Aster does support it, per account, applied to every symbol at once. Lighter appears not to, but this is *inferred* from a data model that carries one position per `(account_index, market_id)` with a single sign field; Lighter's documentation never states it either way.

This is an improvement worth naming. The alternative D1 considered — several agents on one shared address — put every agent's capital, orders and margin inside the reach of any one compromised key, because two agents on opposite sides of the same asset net to zero at the venue and no local bookkeeping can undo that. Under the revised D1 a compromised key reaches one balance and stops.

### What this does not protect

- **A compromised host reaches every key.** One account per agent bounds the compromise of one *key*. It does nothing for the compromise of the *host*, where the attacker reads the keychain and holds every agent key at once. The bound is then the sum of all agent balances, not one of them.
- **More agents means more keys.** The isolation is at the venue; the keychain is one place. Two agents are two things to steal from one file.
- **Your own wallet is usually on the same machine.** D5 keeps every account-owner key out of oppen, which is what makes withdrawal unreachable *through oppen*. It is not a claim about the browser extension sitting next to it. If your wallet lives on the machine the agents run on, the withdrawal path exists — it just does not run through this app.
- **Positions on different venues never net.** Three venues are three margin pools, three liquidation prices and no cross-margin. An economically flat book posts full margin on both legs, and one leg can liquidate while the other survives. No venue can see the others, so aggregate exposure is oppen's problem to compute and oppen's problem to get wrong.
- **Retirement is revocation, not deletion.** A Hyperliquid address is permanent, Lighter states "sub accounts cannot be deleted", and Aster gives "Deleting not supported". Whether a Hyperliquid *sub-account* can be deleted is undocumented either way ([specs/venue-containers.md](specs/venue-containers.md) §6, question 9) — assume not. Retiring an agent means revoking its key and emptying its container; the address stays, and so does anything you leave in it.
- **Nothing here bounds loss to less than the account balance.** Guardrails bound loss to the limits you set, and the next section says what that is worth. The account model bounds it to one account. Neither makes a badly chosen limit safe.

## What is containment, not a boundary

Guardrails, the approval queue and the kill switch are enforced in the Rust core immediately before signing. They defend against **cooperative but mistaken agents**: a bad prompt, a hallucinated size, a retry loop, a model that misread the book. That is the overwhelmingly common failure, and the guardrails stop it.

They do **not** defend against an agent that deliberately routes around oppen:

- On **Windows** (Credential Manager) and **Linux** (Secret Service), any process running as the same OS user can read the stored agent-wallet key. An agent with shell access can then sign orders directly against Hyperliquid, bypassing every guardrail.
- On **macOS**, the Keychain prompts per application by default. This is a real barrier against casual access, not against a determined process with your user's privileges.
- The guardrail configuration and the event ledger live in a local SQLite file. A shell-capable agent can edit them. oppen HMAC-checks the guardrail config and hash-chains the ledger, so tampering is **detectable**, not preventable.

If you run agents you do not trust, run them as a different OS user, or on a different machine, and give them only the MCP endpoint.

## The local MCP endpoint

The server binds to loopback only, validates `Origin` and `Host` on every request (DNS rebinding), and requires a bearer token on every request including the initial handshake. Tokens are per agent and revocable; revocation closes live sessions. Pairing is default-deny: a new client gets nothing until you approve it in the console.

## Supply chain

Official releases are built by GitHub Actions from tagged commits, with actions pinned by commit SHA, dependency lockfiles, `cargo deny` gating, and published checksums. If an auto-updater ships, its signing key is held offline and is not a repository secret. A compromised build would be able to sign trades on every machine that installed it. Verify checksums.

## Reporting

Security reports: open a GitHub security advisory on the repository. Do not open a public issue for an exploitable bug.
