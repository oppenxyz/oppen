# AGENTS.md — for agents building oppen

This file is for AI agents (Claude Code and others) working on this repository. Trading-agent onboarding lives in `skills/oppen/` — that is a different audience.

## What this is

Local-first, agent-first perps terminal for Hyperliquid. Tauri 2 shell, Rust core, Vue 3 operator console. Full spec: `docs/spec.md`. Read it before non-trivial work; the spec's architecture decisions (D1–D8) are settled and not up for re-litigation in a PR.

## Layout

```
crates/oppen-hl     Hyperliquid protocol: signing, info/exchange clients, WS, nonce, meta + order validation
crates/oppen-core   ledger (SQLite, hash-chained), guardrails, kill switch, alerts, journal, features
crates/oppen-mcp    MCP server (rmcp, localhost HTTP), pairing tokens, tool schemas, error taxonomy
apps/desktop        Tauri 2 app: src-tauri (thin commands) + Vue 3 operator console
skills/oppen        Claude Code skill shipped to trading-agent users
docs/               spec, threat model, MCP contract (versioned)
```

## Invariants — violating any of these is a blocking review finding

1. **Guardrails are checked in Rust immediately before signing.** Never in TypeScript, never in the MCP layer, never in a prompt. There must be exactly one code path to the signer and it must run the guardrail check.
2. **No private key ever reaches TypeScript.** Keys live in the OS keychain and are read only from `oppen-hl`. The Vue app renders; it does not sign.
3. **No agent-reachable path modifies guardrails, the approval setting, the kill switch, or the agent registry.** Those are operator-only Tauri commands from the UI.
4. **Master wallet key never enters the app.** Onboarding signs `approveAgent`, sub-account creation and `approveBuilderFee` over WalletConnect.
5. **Testnet is the default.** Mainnet is an explicit, persisted switch. Every network-dependent constant is selected by that switch, never hardcoded.
6. **Deterministic JSON on the MCP surface.** Stable key order, versioned envelope, units in field names (`slip_bps`, `carry_usd_per_day`). Timestamps at the precision the field needs, never finer.
7. **The event ledger is append-only and hash-chained.** One table is the source for `get_events`, the activity stream and the audit export. Do not build a second event store.
8. **Every rejection is typed.** Extend the error taxonomy in `oppen-mcp`; never return a bare string.
9. **Agent `reason` strings are untrusted text.** Render as plain text only. No `v-html`, no markdown, no HTML in chart annotations.
10. **Builder code is attached by default** in official builds via `OPPEN_BUILDER_ADDRESS`. Handle a missing approval gracefully; never silently drop the order path.

## Signing correctness

msgpack action-hash field order matters. Float wire values must be normalized strings — a trailing zero produces a different hash and an opaque "User or API Wallet does not exist" rejection. The signer has the official SDK's test vectors in `crates/oppen-hl/tests/`; any change to signing must keep them green.

## Conventions

- Rust: edition 2024, `cargo fmt`, `cargo clippy -D warnings`, `cargo deny check`. Errors via `thiserror`. No `unwrap` outside tests.
- TypeScript: bun, `vue-tsc --noEmit` clean, no `any` on the MCP boundary types (generated from the Rust schemas).
- Design system: `docs/design/` — Space Mono + Archivo, void `#0A0B0C`, uranium `#FFD400` at ≤2% of any surface, no shadows, gradients or radius. Brightness marks a fact, never a mood.
- Commits: conventional commits. PRs: state which spec items they implement by number.
- Tests before "done": a signer change needs vectors; a guardrail change needs a property test proving no bypass; a ledger change needs the disconnect-reconcile test.

## What "done" means per phase

See the roadmap table in `README.md`. A phase is not done until its gate passes on testnet, not on mocks.
