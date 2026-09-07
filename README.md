<div align="center">

# oppen

**Local-first perps terminal built for agentic trading.**

Agents trade through a built-in MCP gateway. You supervise from an operator
console. Keys stay on your machine.

[Specification](docs/spec.md) · [Threat model](docs/threat-model.md) ·
[Roadmap](ROADMAP.md) · [Feature specs](docs/specs/) · [For agents](AGENTS.md)

</div>

> **Status: pre-alpha.** v1 targets Hyperliquid only. Testnet by default. There is
> no release yet — the gates in [ROADMAP.md](ROADMAP.md) say exactly what works.
> Do not point this at real funds.

---

## The problem

Giving a language model your trading keys is giving it unbounded authority over
your capital, on the assumption it will not misuse it. That assumption fails in
ordinary ways: a loop that does not terminate, a hallucinated size, a feed that
lies, an injected instruction inside a data field, a strategy that quietly
stopped working at 3am on a venue that never closes.

The usual mitigations are instructions in a prompt, and a prompt is not an
enforcement mechanism. A model that can be asked to stay under a limit can be
argued out of it.

oppen answers structurally instead. Authority is bounded by the protocol and by
deterministic code the model cannot reach. Cognition stays flexible; authority
stays fixed; custody never moves.

## What it is

- **A desktop app** for macOS, Windows and Linux that you download and run. No
  oppen backend, no account, no telemetry carrying positions or keys.
- **A local MCP server** your agents connect to — Claude Code, or any MCP client.
  Agents receive capabilities, never credentials.
- **Guardrails in Rust**, evaluated on a fully constructed order immediately
  before signing: symbol allowlist, notional caps, order rate, max slippage, loss
  circuit breaker. There is no path to the signer that skips them, and no path
  from the model to the limits.
- **An operator console** where the activity stream is the hero surface. Every
  intent, every refusal with its reason, every fill, one kill switch.
- **One venue account per agent.** Attribution and capital segregation enforced by
  the venue, not by bookkeeping. oppen calls that account a *container*: a
  sub-account where the venue grants one, a top-level account otherwise. On
  Hyperliquid sub-accounts are gated behind $100,000 of traded volume, so a new
  user gets zero of them and v1 gives each agent its own top-level account
  (decision D1, revised 2026-09-04). Once your volume clears the gate a container
  can be migrated onto a real sub-account — oppen attempts the creation and
  classifies the refusal, rather than predicting whether you are eligible.

## What it is not

- **Not a hosted service.** Nothing runs anywhere but your machine.
- **Not a human trading terminal.** The order ticket exists for intervention, not
  discretionary trading.
- **Not a strategy.** oppen ships no alpha, no signals and no default agent
  behaviour. It computes features and enforces limits. Your agent decides.
- **Not a cross-venue margin system.** Positions never net across venues: three
  venues means three margin pools, three liquidation prices and no cross-margin. A
  book that is economically flat pays full margin on both legs, and one leg can
  liquidate while the other survives. When a second venue ships, oppen sums exposure
  across containers and displays it — a report, never a control, because no venue
  can see the others.
- **Not a defence against a compromised host.** See the
  [threat model](docs/threat-model.md), which is short and worth reading in full.
- **Not a guarantee against loss.** Guardrails bound loss to the limits you set.
  An operator who configures a 50% circuit breaker has authorized a 50% loss.

---

## How it works

```
┌──────────────────────────────────────────────────────────────┐
│  Agent (Claude Code, or any MCP client)      UNTRUSTED       │
│  Reasons, plans, requests. Holds no keys.                    │
└───────────────────────────┬──────────────────────────────────┘
                            │  MCP over loopback HTTP
┌───────────────────────────▼──────────────────────────────────┐
│  MCP gateway                                 SEMI-TRUSTED    │
│  Capability surface, schema validation, per-agent scoping.   │
└───────────────────────────┬──────────────────────────────────┘
                            │  in-process, typed
┌───────────────────────────▼──────────────────────────────────┐
│  Rust core                                   TRUSTED         │
│  guardrails → signer (keychain) · event ledger · WS pool ·   │
│  reconcile · quant features · kill switch · dead-man         │
└───────────────────────────┬──────────────────────────────────┘
                            │
         ┌──────────────────┴──────────────────┐
   ┌─────▼──────┐                     ┌────────▼─────────┐
   │ Hyperliquid│                     │ Operator console │
   └────────────┘                     └──────────────────┘
```

The trust gradient is the architecture. Everything above the gateway is assumed
compromisable. The trusted core is deliberately small: order construction,
guardrail evaluation, signing, reconciliation, ledger writes.

## How keys work

**No account-owner key ever enters oppen.** There is no code path that accepts a
private key or a seed phrase, on any venue, at any step — not behind an advanced
toggle, not to import an existing account (decision D5).

Note the plural. Under the revised D1 there is no single master. In v1 each agent's
container is a top-level Hyperliquid account — a second, third, fourth account in
your own wallet — so there is one account-owner key per agent, every one of them
able to withdraw, and every one of them staying in your wallet. oppen holds agent
wallets and nothing else. (Where a venue grants sub-accounts the shape differs:
Hyperliquid sub-accounts have no private key at all, and a Lighter sub-account is an
account index rather than a keypair.)

Agent wallets are generated in-app and stored in the OS keychain — Keychain on
macOS, Credential Manager on Windows, Secret Service on Linux. The key is used by
the signer inside the Rust core and never reaches the frontend, the gateway, the
MCP transport or a log line.

### The ceremonies

Signed in your own wallet over WalletConnect. None can be initiated by an agent.
Hyperliquid, v1, one top-level container per agent:

| Ceremony | Signed by | How often | What it cannot authorize |
|---|---|---|---|
| `approveAgent` | the agent's own container | once per agent, and on every key rotation | withdrawal; signing for any account that is not that container or one of its sub-accounts |
| `approveBuilderFee` | the agent's own container | once per agent; again only if the cap changes | any movement of funds; any rate above the signed cap |
| `usdSend` | the funding account | every funding and rebalancing move | anything recurring — it is one amount to one address |

**Three signatures per agent, not a one-time setup.** Two if you decline the
builder fee, which is supported and does not break the order path. Creating the
container itself costs nothing — it is another account derived in your own wallet,
no signature and no gas — but `approveAgent` and `approveBuilderFee` are per
Hyperliquid account and each container is its own account, so ten agents is thirty
wallet approvals. That is what it costs to have the venue enforce segregation
instead of oppen's bookkeeping, and it is the first thing you will feel about this
design. The full per-venue counts are in
[docs/specs/onboarding.md](docs/specs/onboarding.md).

Authority decays. Agent approvals carry a 90-day expiry, warned from day 14; the
venue caps `valid_until` at 180 days. An abandoned deployment stops being able to
trade on its own. Rotation mints a new key under a **new** name: re-approving the
same `agentName` replaces the previous wallet silently — no error, no event, the
old key simply stops signing.

### What an agent key cannot do, per venue

| | Hyperliquid API wallet | Aster agent, `canWithdraw:false` | Lighter API key |
|---|---|---|---|
| Place and cancel orders | yes | yes | yes |
| Withdraw to any other address | no | no | no |
| Withdraw to the owner's own L1 address | no | no | **yes** — "secure" withdrawals |

v1 ships Hyperliquid only; the other two columns are recorded so that the claim is
never inherited across venues. On Hyperliquid and on Aster, "the agent key cannot
withdraw" is enforced by the venue and is the one containment property that holds
even against a fully compromised machine. **On Lighter it is false**: a stolen key
sends funds to the owner's own L1 address rather than to an attacker's, which
bounds the loss without making it impossible.

Two qualifications on the Hyperliquid column, stated rather than smoothed over:

- `agentSendAsset` lets an agent wallet move collateral between the **same
  address's** perp and spot balances — the destination must equal the source, so it
  reaches no other account, but it is a movement out of the perps margin pool that
  oppen did not initiate.
- Which *other* user-signed actions an API wallet is barred from — `usdSend`,
  `approveAgent`, `subAccountTransfer` — is stated on no page we have read, and the
  testnet negative test that would settle it is still open
  ([docs/hl-signing.md](docs/hl-signing.md) open question 3). Only "cannot
  withdraw" is treated as load-bearing.

## Guardrails

Deterministic predicates, evaluated in Rust against a fully constructed, fully
priced order, immediately before signing.

| Guardrail | Prevents |
|---|---|
| Symbol allowlist | Being steered into an unintended or illiquid market |
| Notional caps | Fat-finger sizing; a loop that keeps adding |
| Order rate | Runaway loops, thrashing, fee burn, venue bans |
| Max slippage | Market orders into thin books or a dislocated feed |
| Loss circuit breaker | The slow bleed from a strategy that stopped working |

Two behaviours make this a control rather than a suggestion:

**Fail-closed.** If the engine cannot evaluate — stale data, missing reference
price, unreconciled state — the order is refused. Uncertainty about whether a
limit is breached is treated as a breach.

**Structured refusal.** A refusal returns the failed predicate, the observed
value and the configured limit, so the agent can adapt and you can read why.
Refusals are the most informative telemetry the system produces.

Above them sit a **kill switch** (halts entry, cancels resting orders, survives
restart) and a **dead-man switch** (`scheduleCancel`, armed while agents are
active).

The dead-man is a daily budget rather than a standing guarantee, and the README is
where that is easiest to overclaim: Hyperliquid takes a time at least 5 seconds
ahead and allows a **maximum of 10 triggers per day per address**, resetting at
00:00 UTC, so re-arming on every reconnect would exhaust it inside one bad hour.
Whether `scheduleCancel` is itself volume-gated is unconfirmed — if it is, a
brand-new account has no dead-man at all, and oppen will say so rather than promise
one.

---

## Quickstart

Nothing is released yet. To run from source:

**Prerequisites.** Rust stable (via [rustup](https://rustup.rs)),
[bun](https://bun.sh) 1.2+, and the
[Tauri 2 system dependencies](https://tauri.app/start/prerequisites/) for your
platform.

```sh
git clone https://github.com/oppenxyz/oppen
cd oppen
bun install --cwd apps/desktop
cargo build
cd apps/desktop && bun run tauri dev
```

Once the app runs, first-run onboarding will walk you through testnet setup:
generate an agent wallet, point it at the account that will be its container — a
second account in your own wallet, on Hyperliquid v1 — complete the three
WalletConnect ceremonies, then pair your first agent. Until phase P4 lands, that
flow does not exist yet — see [ROADMAP.md](ROADMAP.md) for what is real today.

One prerequisite is outside oppen and surprising enough to state here: the
Hyperliquid testnet faucet pays 1,000 mock USDC and only to an address that has
**previously deposited on mainnet**. Agent containers are then funded from that one
account with `usdSend`, not by claiming the faucet again per container.

### Connecting an agent

When the gateway ships, pairing an agent is one line:

```sh
claude mcp add --transport http oppen http://127.0.0.1:<port>/mcp \
  --header "Authorization: Bearer <token from the pairing dialog>"
```

The endpoint binds to loopback only, validates `Origin` and `Host`, and requires
a bearer token on every request including the handshake. Pairing is default-deny:
a new client gets nothing until you approve it in the console, name it, bind it
to its container and assign its guardrails. New agents start in approval mode
with small caps.

Agent-facing documentation lives in [`skills/oppen/`](skills/oppen/).

---

## Repository layout

```
crates/oppen-hl      Hyperliquid protocol: signing, info and exchange clients,
                     websocket, nonce allocation, meta and order validation
crates/oppen-core    Event ledger, guardrails, kill switch, alerts, journal,
                     quant features
crates/oppen-mcp     MCP gateway: transport, pairing tokens, tool schemas,
                     typed error taxonomy
apps/desktop         Tauri 2 shell and the Vue 3 operator console
docs/                Specification, decision log, threat model, MCP contract,
                     signing reference, design system, component specs, runbooks
skills/oppen         Claude Code skill shipped to trading-agent users
```

## Documentation

| Document | What it covers |
|---|---|
| [docs/spec.md](docs/spec.md) | The normative v1 specification. Architecture decisions D1–D8 and items 1–36 |
| [docs/decisions.md](docs/decisions.md) | Every product and scope decision outside D1–D8, with its reasoning and what it revised |
| [docs/threat-model.md](docs/threat-model.md) | What is a hard boundary, what is only containment. Read before real funds |
| [docs/hl-signing.md](docs/hl-signing.md) | Every Hyperliquid signing rule, with the source each is read from |
| [docs/mcp-contract.md](docs/mcp-contract.md) | The versioned agent-facing contract |
| [docs/specs/](docs/specs/) | Component specs. v1: onboarding ceremonies, venue containers. Beyond v1: workflows, history, charts, fair value, signals, mobile |
| [docs/design/](docs/design/) | Design tokens and rules, brand book, app mock |
| [ROADMAP.md](ROADMAP.md) | Item-level checklist per phase, and every later version |
| [AGENTS.md](AGENTS.md) | Invariants, leanness rules and conventions for anyone, human or model, writing code here |

## Roadmap

| Phase | Scope | Acceptance gate |
|---|---|---|
| P0 | Scaffold, CI, testnet toggle | App launches, CI green |
| P1 | Hyperliquid protocol crate | Signed testnet order via CLI |
| P2 | WS pool, reconcile, event ledger | Zero fills lost across a 30 s disconnect |
| P3 | Guardrails, kill switch, dead-man | No signer path without a guardrail check |
| P4 | MCP gateway | `claude mcp add` → paired → guarded testnet order |
| P5 | Operator console | The named surfaces render; stale overlay on socket loss |
| P6 | Quant features | Cross-checked against hand computation |
| P7 | Approval mode, skill, threat model, release | Fresh machine to a testnet trade in 10 minutes, measured from a wallet that already holds testnet USDC |

Each phase is gated on a falsifiable test rather than a feature list. Beyond v1:
trading workflows, durable history, the fair value engine, Quantoppen, an opt-in
community layer and a mobile companion — all specified in
[docs/specs/](docs/specs/) before they are built.

---

## Development

```sh
cargo test --workspace                  # Rust tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo deny check                        # licenses, advisories, sources
cd apps/desktop && bun run build        # typecheck + web build
```

CI runs all of the above on every pull request, with actions pinned by commit
SHA and dependencies installed with `--ignore-scripts`.

Conventions live in [AGENTS.md](AGENTS.md). Two that matter most: guardrails are
checked in Rust immediately before signing and nowhere else, and no private key
ever reaches TypeScript.

## Contributing

Pull requests are welcome. Contributions require signing the
[CLA](CLA.md) — comment on your first PR and the bot records it.

- Conventional commit messages. PRs state which spec items they implement by
  number.
- A signer change needs test vectors. A guardrail change needs a property test
  proving no bypass. A ledger change needs the disconnect-reconcile test.
- Keep the diff lean. Every construct traces to a numbered spec item, a recorded
  decision or an invariant, and the PR body states the trace. The rules — and the
  short list of things that are never cut for size, starting with the fail-closed
  branches — are in [AGENTS.md](AGENTS.md).
- The architecture decisions D1–D8 in [docs/spec.md](docs/spec.md) are settled
  and are not re-opened in a pull request. They change only through a recorded
  decision: D1 was revised on 2026-09-04 from "one sub-account per agent" to "one
  venue account per agent" ([docs/decisions.md](docs/decisions.md) V1–V6), and that
  is the entire process for revising one.

## Security

Report vulnerabilities through a
[GitHub security advisory](https://github.com/oppenxyz/oppen/security/advisories/new).
Do not open a public issue for an exploitable bug.

Read [docs/threat-model.md](docs/threat-model.md) before trading real funds. In
short: oppen defends against a misbehaving *agent*, not a compromised *host*. On
Windows and Linux any process running as your user can read the stored agent key.
On Hyperliquid, that key's inability to withdraw is what bounds the damage — a
property of the venue rather than of oppen, and one that does not hold on every
venue (see the per-venue table above).

## Builder fee

Official oppen builds attach a builder code to every order. The fee is small,
bounded by the maximum-rate approval you sign, visible in the console and recorded
per order in the ledger. oppen cannot exceed the signed cap without a new signature
from that container's owner key.

The approval is per Hyperliquid account and each container is its own account, so
every agent signs its own — the third of the three signatures above. Hyperliquid
allows at most 10 active builder approvals per account, which is a limit per
container and not a limit on how many agents you run.

This is the project's revenue mechanism, stated here rather than buried. A system
whose value proposition is bounded authority cannot have an unbounded fee.
See [docs/spec.md](docs/spec.md) decision D7.

## License

Open core. The line is the one [docs/threat-model.md](docs/threat-model.md)
already draws, not a commercial one:

| | Licence | What it is |
|---|---|---|
| `crates/oppen-hl`, `crates/oppen-core`, `crates/oppen-mcp` | [Apache-2.0](LICENSE) | Signing, guardrails, the ledger, the MCP surface — the code that carries every safety claim |
| `apps/desktop` | [Commercial Source](apps/desktop/LICENSE) | The Tauri shell, the operator console, the quant layer, the workflow engine — source published and readable, not open source |

The core is open because its claims are claims about an *absence*: exactly one
code path to the signer, no private key in TypeScript, no agent-reachable path to
the guardrails, no account-owner key in the app. You cannot observe an absence
from outside a binary, so a closed core would reduce all four to *trust us* — the
posture this project exists to replace. Everything above the core renders and
does not sign, carries no such claim, and is licensed accordingly. The source of
both halves is published; only the core may be redistributed.

The commercial licence does not restrict trading: build it, run it, trade it for
profit, on your own account or your firm's, with no separate agreement. What it
reserves is redistribution and resale. Reasoning, and the two alternatives that
were rejected, are decisions L1–L7 in [docs/decisions.md](docs/decisions.md).

Contributions to either half are under the [CLA](CLA.md), which already licenses
them "under any license" and so needs no amendment. Third-party attributions in
[NOTICE](NOTICE). Source opens Q4 2026 for the Apache-2.0 crates (L6).

"oppen" is a trademark. The core may be forked and modified freely; forks may not
ship under the oppen name. The safety claims here are claims about a particular
build with particular invariants — a fork that removes the guardrail check and
keeps the name would be a security problem for users, not merely a branding one.

### Private desktop updates

Owner installations can follow main through the [signed private update channel](docs/runbooks/desktop-updates.md). This personal Apple Silicon channel does not change the roadmap gates or imply general release readiness.
