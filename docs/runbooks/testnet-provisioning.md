# Testnet provisioning runbook

Do this once, in the Hyperliquid web UI. It cannot be automated: every step is a
signature from the master wallet, and D5 forbids a master-key path in the app.

Wallet: `0xBF829199c1AE7f0Caf21FB6FC45e10EdFf25B7D2`
Network: **testnet** — <https://app.hyperliquid-testnet.xyz>

Limits, confirmed from the docs on 2026-09-03: a master gets **3 API wallets by
default, plus 2 more for every sub-account created**. Four sub-accounts therefore
allows 11. Agent addresses must never be reused across rotations, because pruning
loses nonce state and previously signed actions can replay.

---

## 0. Prerequisite, already done

The faucet only pays an address that has **previously deposited on mainnet**. You
deposited 29.699177 USDC to mainnet spot, and `userRole` flipped from `missing` to
`user`, which satisfies it.

## 1. Claim the faucet

Connect the wallet at the testnet app and claim. It pays **1,000 mock USDC**.

If it lands in spot, transfer it to perps before anything else. Nothing below
works against a zero perps balance.

## 2. Create four sub-accounts

Use exactly these names. The app reads them back and the runbook below assumes
them.

| Name | Purpose |
|---|---|
| `agent-alpha` | First real agent. Carries the P2 reconcile gate. |
| `agent-beta` | Second agent. Proves isolation and independent nonce sets. |
| `manual` | The escape hatch signer, so no operator key ever sits on the master. |
| `spare` | Rotation target, so an agent-wallet swap never orphans a position. |

## 3. Fund them

Leave a working balance on the master and split the rest. The $10 minimum
notional means anything under about $150 cannot hold a position and a stop at the
same time.

| Account | Mock USDC |
|---|---|
| master | 250 |
| `agent-alpha` | 300 |
| `agent-beta` | 250 |
| `manual` | 150 |
| `spare` | 50 |

## 4. Create the API wallets

More → API. Create each one, and **copy the private key into your password
manager the moment it is shown** — it is displayed once and never again. Do not
paste any of them into a chat, a file in this repo, or a shell history.

Name each with a period suffix so a rotation overlaps rather than swapping
atomically:

| Name | Signs for |
|---|---|
| `oppen-p1-2026q4` | master — the P1 gate only |
| `alpha-2026q4` | `agent-alpha` |
| `beta-2026q4` | `agent-beta` |
| `manual-2026q4` | `manual` |

Set the expiry to **90 days** where the UI allows it. Decision D-b: authority
decays by default, warned from 14 days out.

A separate API wallet per sub-account is required, not merely tidy. Nonces are
tracked per signer, so one wallet across several sub-accounts makes them share a
nonce set and collide.

## 5. Run the P1 gate

```sh
cd ~/projects/oppen
source ~/.cargo/env
export OPPEN_TESTNET_AGENT_KEY=0x…      # oppen-p1-2026q4
export OPPEN_TESTNET_USER=0xBF829199c1AE7f0Caf21FB6FC45e10EdFf25B7D2
cargo run -p oppen-hl --example testnet_order
unset OPPEN_TESTNET_AGENT_KEY
```

Expected, line by line:

- `agent 0x…` equals the API wallet address shown in the app. If not, the key was
  pasted wrong.
- `place [Resting { oid: … }]`
- `status open oid=… cloid=Some(…)`
- `cancel [Success]`
- `expires rejected as expected: …` — this closes nothing now, since the encoding
  was settled by differential recovery, but it confirms the venue agrees.

## 6. Then, for the P2 gate

Same key, plus `OPPEN_TESTNET_VAULT` set to the `agent-alpha` sub-account address
so orders route through it. P2's gate needs fills to reconcile, so leave one
resting order alive when you are done.

## What not to do

- Do not reuse an agent address after rotating it.
- Do not put an operator key on the master. That is what `manual` exists for.
- Do not fund a sub-account below about $150 and expect it to hold a position.
