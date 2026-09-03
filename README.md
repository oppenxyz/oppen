# oppen

Local-first perps terminal built for agentic trading.

Agents trade through a built-in MCP gateway. You supervise from an operator console. Keys stay on your machine.

**Status: pre-alpha.** v1 targets Hyperliquid only. Testnet by default.

## What it is

- A desktop app (macOS, Windows, Linux) you download and run. No oppen backend.
- A local MCP server your agents connect to — Claude Code, or any MCP client. Agents get capabilities, never keys.
- Guardrails enforced in the Rust core immediately before signing. Symbol allowlist, notional caps, order rate, max slippage, loss circuit breaker. The model cannot change them.
- An operator console: every decision logged, every refusal explained, one kill switch.
- One Hyperliquid sub-account per agent. Real attribution, real isolation.

## What it is not

- Not a hosted service. Nothing runs anywhere but your machine.
- Not a human trading terminal. The activity stream is the hero surface, not the order ticket.
- Not a strategy. oppen computes features and enforces limits. Your agent decides.

## How keys work

The master wallet never enters oppen. Agent wallets are generated in-app and stored in the OS keychain. The master wallet signs three approvals once, in your own wallet, over WalletConnect. Agent wallets cannot withdraw.

Read [docs/threat-model.md](docs/threat-model.md) before trading real funds.

## Builder fee

Official builds attach a builder code to every order. The fee is small, capped by an approval you sign, and disclosed here. See [docs/spec.md](docs/spec.md#d7).

## Roadmap

| Phase | Scope | Gate |
|---|---|---|
| P0 | Scaffold, CI, testnet toggle | app launches, CI green |
| P1 | Hyperliquid protocol crate | signed testnet order via CLI |
| P2 | WS pool, reconcile, event ledger | zero fills lost across a 30s disconnect |
| P3 | Guardrails, kill switch, dead-man | no signer path without a guardrail check |
| P4 | MCP gateway | `claude mcp add` → paired → guarded testnet order |
| P5 | Operator console | parity with the design, stale overlay on socket loss |
| P6 | Quant features | cross-checked against hand computation |
| P7 | Approval mode, skill, threat model, release | fresh machine to testnet trade in 10 minutes |

Full specification: [docs/spec.md](docs/spec.md).

## Development

```sh
# prerequisites: Rust stable, bun, Tauri 2 system deps
bun install
cargo build
cargo tauri dev
```

Rust workspace in `crates/`, desktop app in `apps/desktop`. See [AGENTS.md](AGENTS.md) for conventions and invariants.

## License

Apache-2.0. Contributions require signing the [CLA](CLA.md).

oppen is a trademark. Forks may not ship under the oppen name.
