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
- **One Hyperliquid sub-account per agent.** Attribution and capital segregation
  enforced by the venue, not by bookkeeping.

## What it is not

- **Not a hosted service.** Nothing runs anywhere but your machine.
- **Not a human trading terminal.** The order ticket exists for intervention, not
  discretionary trading.
- **Not a strategy.** oppen ships no alpha, no signals and no default agent
  behaviour. It computes features and enforces limits. Your agent decides.
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

**The master wallet never enters oppen.** There is no code path that accepts a
master private key or seed phrase.

Agent wallets are generated in-app and stored in the OS keychain — Keychain on
macOS, Credential Manager on Windows, Secret Service on Linux. The key is used by
the signer inside the Rust core and never reaches the frontend, the gateway, the
MCP transport or a log line.

Your master wallet signs three approvals once, in your own wallet, over
WalletConnect:

1. **Agent wallet authorization**, with an explicit expiry.
2. **Sub-account provisioning**, so each agent's capital is isolated at the venue.
3. **Builder fee approval**, a maximum rate you sign and oppen cannot exceed.

None can be initiated by an agent. **Agent wallets cannot withdraw** — that is
enforced by Hyperliquid, not by oppen, and it is the one containment property
that holds even against a fully compromised machine.

Authorizations expire. An abandoned deployment stops being able to trade on its
own.

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
restart) and a **dead-man switch** (`scheduleCancel` armed while any agent runs).

---

## Quickstart

Nothing is released yet. To run from source:

**Prerequisites.** Rust stable (via [rustup](https://rustup.rs)),
[bun](https://bun.sh) 1.2+, and the
[Tauri 2 system dependencies](https://tauri.app/start/prerequisites/) for your
platform.

```sh
git clone https://github.com/gkssxf/oppen
cd oppen
bun install --cwd apps/desktop
cargo build
cd apps/desktop && bun run tauri dev
```

Once the app runs, first-run onboarding will walk you through testnet setup:
create a sub-account and agent wallet, complete the WalletConnect ceremony, then
pair your first agent. Until phase P4 lands, that flow does not exist yet — see
[ROADMAP.md](ROADMAP.md) for what is real today.

### Connecting an agent

When the gateway ships, pairing an agent is one line:

```sh
claude mcp add --transport http oppen http://127.0.0.1:<port>/mcp \
  --header "Authorization: Bearer <token from the pairing dialog>"
```

The endpoint binds to loopback only, validates `Origin` and `Host`, and requires
a bearer token on every request including the handshake. Pairing is default-deny:
a new client gets nothing until you approve it in the console, name it, bind it
to a sub-account and assign its guardrails. New agents start in approval mode
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
docs/                Specification, threat model, MCP contract, signing
                     reference, design system, feature specs
skills/oppen         Claude Code skill shipped to trading-agent users
```

## Documentation

| Document | What it covers |
|---|---|
| [docs/spec.md](docs/spec.md) | The normative v1 specification. Architecture decisions D1–D8 and items 1–36 |
| [docs/threat-model.md](docs/threat-model.md) | What is a hard boundary, what is only containment. Read before real funds |
| [docs/hl-signing.md](docs/hl-signing.md) | Every Hyperliquid signing rule, with the source each is read from |
| [docs/mcp-contract.md](docs/mcp-contract.md) | The versioned agent-facing contract |
| [docs/specs/](docs/specs/) | Feature specs beyond v1: workflows, history, charts, fair value, signals, mobile |
| [docs/design/](docs/design/) | Design tokens and rules, brand book, app mock |
| [ROADMAP.md](ROADMAP.md) | Item-level checklist per phase, and every later version |
| [AGENTS.md](AGENTS.md) | Conventions and invariants for anyone, human or model, writing code here |

## Roadmap

| Phase | Scope | Acceptance gate |
|---|---|---|
| P0 | Scaffold, CI, testnet toggle | App launches, CI green |
| P1 | Hyperliquid protocol crate | Signed testnet order via CLI |
| P2 | WS pool, reconcile, event ledger | Zero fills lost across a 30 s disconnect |
| P3 | Guardrails, kill switch, dead-man | No signer path without a guardrail check |
| P4 | MCP gateway | `claude mcp add` → paired → guarded testnet order |
| P5 | Operator console | Parity with the design; stale overlay on socket loss |
| P6 | Quant features | Cross-checked against hand computation |
| P7 | Approval mode, skill, threat model, release | Fresh machine to testnet trade in 10 minutes |

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
- The architecture decisions D1–D8 in [docs/spec.md](docs/spec.md) are settled
  and are not re-opened in a pull request.

## Security

Report vulnerabilities through a
[GitHub security advisory](https://github.com/gkssxf/oppen/security/advisories/new).
Do not open a public issue for an exploitable bug.

Read [docs/threat-model.md](docs/threat-model.md) before trading real funds. In
short: oppen defends against a misbehaving *agent*, not a compromised *host*. On
Windows and Linux any process running as your user can read the stored agent key.
The agent wallet's inability to withdraw is what bounds the damage.

## Builder fee

Official oppen builds attach a builder code to every order. The fee is small,
bounded by the maximum-rate approval you sign at setup, visible in the console
and recorded per order in the ledger. oppen cannot exceed the signed cap without
a new signature from your master wallet.

This is the project's revenue mechanism, stated here rather than buried. A system
whose value proposition is bounded authority cannot have an unbounded fee.
See [docs/spec.md](docs/spec.md) decision D7.

## License

[Apache-2.0](LICENSE), with a [CLA](CLA.md) for contributions. Third-party
attributions in [NOTICE](NOTICE).

"oppen" is a trademark. The code may be forked and modified freely; forks may not
ship under the oppen name. The safety claims here are claims about a particular
build with particular invariants — a fork that removes the guardrail check and
keeps the name would be a security problem for users, not merely a branding one.
