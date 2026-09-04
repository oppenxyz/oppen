# AGENTS.md — for agents building oppen

This file is for AI agents (Claude Code and others) working on this repository. Trading-agent onboarding lives in `skills/oppen/` — that is a different audience.

## What this is

Local-first, agent-first perps terminal. v1 ships Hyperliquid only; D1 is venue-agnostic, and Aster and Lighter are first-class in the architecture and v2 in scope. Tauri 2 shell, Rust core, Vue 3 operator console. Full spec: `docs/spec.md`. Read it before non-trivial work; the spec's architecture decisions (D1–D8) are settled and not up for re-litigation in a PR — they change only through a recorded decision in `docs/decisions.md`, which is how D1 was revised on 2026-09-04 to "one venue **account** per agent" (a sub-account where the venue grants one, a top-level account otherwise; oppen calls it a **container**).

## Layout

```
crates/oppen-hl     Hyperliquid protocol: signing, info/exchange clients, WS, nonce, meta + order validation
crates/oppen-core   ledger (SQLite, hash-chained), guardrails, kill switch, alerts, journal, features
crates/oppen-mcp    MCP server (rmcp, localhost HTTP), pairing tokens, tool schemas, error taxonomy
apps/desktop        Tauri 2 app: src-tauri (thin commands) + Vue 3 operator console
skills/oppen        Claude Code skill shipped to trading-agent users
docs/               spec, decisions, threat model, MCP contract (versioned), signing reference,
                    component specs in docs/specs/, runbooks in docs/runbooks/
```

## Invariants — violating any of these is a blocking review finding

1. **Guardrails are checked in Rust immediately before signing.** Never in TypeScript, never in the MCP layer, never in a prompt. There must be exactly one code path to the signer and it must run the guardrail check.
2. **No private key ever reaches TypeScript.** Keys live in the OS keychain and are read only from `oppen-hl`. The Vue app renders; it does not sign.
3. **No agent-reachable path modifies guardrails, the approval setting, the kill switch, or the agent registry.** Those are operator-only Tauri commands from the UI.
4. **No account-owner key ever enters the app.** Onboarding signs `approveAgent`, account provisioning (`usdSend` to fund a new top-level account, or `createSubAccount` where the venue grants one) and `approveBuilderFee` over WalletConnect. Note the plural: under the revised D1 there is no single master — one account-owner key per agent, every one of them able to withdraw, all of them in the user's own wallet. oppen holds agent wallets and nothing else.
5. **Testnet is the default.** Mainnet is an explicit, persisted switch. Every network-dependent constant is selected by that switch, never hardcoded.
6. **Deterministic JSON on the MCP surface.** Stable key order, versioned envelope, units in field names (`slip_bps`, `carry_usd_per_day`). Timestamps at the precision the field needs, never finer.
7. **The event ledger is append-only and hash-chained.** One table is the source for `get_events`, the activity stream and the audit export. Do not build a second event store.
8. **Every rejection is typed.** Extend the error taxonomy in `oppen-mcp`; never return a bare string.
9. **Agent `reason` strings are untrusted text.** Render as plain text only. No `v-html`, no markdown, no HTML in chart annotations.
10. **Builder code is attached by default** in official builds via `OPPEN_BUILDER_ADDRESS`. Handle a missing approval gracefully; never silently drop the order path. The approval is per venue account, so each container signs its own (`docs/decisions.md` O7).
11. **"The agent key cannot withdraw" is a per-venue claim, and is written with the venue named.** True on Hyperliquid, and on Aster for an agent registered `canWithdraw:false`. **False on Lighter**, whose API keys process "secure" withdrawals to the L1 address that created the account — funds return to the owner rather than reaching an attacker, which bounds the loss without making it impossible. Two qualifications on Hyperliquid: `agentSendAsset` lets an agent move collateral between the same address's perp and spot balances (destination must equal source), and which *other* user-signed actions an API wallet is barred from is unconfirmed — `docs/hl-signing.md` open question 3, still awaiting a testnet negative test. Only "cannot withdraw" is load-bearing.

## Leanness — binding, checked on the diff

Project owner, 2026-09-04: keep the repo lean, so the codebase does not grow without control. Clean, only necessary work, easy to run and easy to maintain.

These are review findings like the invariants above. Size is the symptom, not the target: there is no line budget, and nothing here asks for clever compression. Each rule asks the same question — what does this construct buy, and who pays for it.

1. **Every construct traces to a source.** A numbered item in `docs/spec.md`, a recorded decision in `docs/decisions.md`, or an invariant above. The PR body states the trace. Code that traces to nothing is speculative and does not ship, however good it is; if the idea is right, record the decision first and then write it.
2. **No abstraction without a second implementation or a real test seam.** A trait with one impl and no test double is indirection nobody pays for. Write the concrete type. Extract the trait when the second caller exists, or when a test genuinely needs to substitute the thing.
3. **No `pub` without a caller.** Every public item is a maintenance obligation and a cross-crate promise. `pub(crate)` until something outside the crate needs it. Clippy does not catch this; the reviewer does.
4. **No configurability nobody asked for.** An option with one caller is a constant. A parameter every call site passes the same value for is a constant. A default nobody overrides is that constant with extra steps and a doc comment.
5. **Typed errors, no dead variants.** Invariant 8 requires the taxonomy. A variant no code constructs is deleted. Variants a caller handles identically are one variant — the test is to name the caller behaviour each one produces, including its retryability; two identical answers mean one variant.
6. **No defensive branch for a state the type system forbids.** An `unreachable!()` on an exhausted enum, a `None` check on a value that cannot be `None` by construction. Make the state unrepresentable instead. A reachable fail-closed refusal is not this rule's business — see below.
7. **Tests are exempt from the size rule and not from the value rule.** Write as many as the behaviour needs. But a test that would pass against a stubbed implementation proves nothing: assert observable behaviour — the refusal and its type, the chained hash, the nonce order, the signed bytes — not that a mock was called. Delete the ones that only prove wiring.
8. **Delete your own leftovers in the same PR.** Code your change orphaned goes with it. Pre-existing dead code is named in the PR body and left alone; that is a separate change with a separate trace.
9. **A dependency is a decision.** New crates and npm packages get a line in `docs/decisions.md` saying what was rejected. `cargo deny` gates licences and advisories; nothing gates "we could have written twelve lines".

**Never cut for size.** Leanness is a rule about weight that buys nothing. These buy something, and a review comment asking to trim one of them is answered with this paragraph:

- fail-closed branches — the refusal when the engine cannot evaluate: stale feed, missing reference price, unreconciled state;
- typed refusals a caller acts on differently, and the reason string that names the predicate, the observed value and the limit;
- guardrail predicates, and the single code path to the signer that runs them (invariant 1);
- the signer seal — the key never leaving Rust (invariant 2) and the operator-only surface (invariant 3);
- tamper-evidence — hash-chained ledger rows, the HMAC on the guardrail config, and the verification code that reads them (invariant 7).

## Signing correctness

msgpack action-hash field order matters. Float wire values must be normalized strings — a trailing zero produces a different hash and an opaque "User or API Wallet does not exist" rejection. The signer has the official SDK's test vectors in `crates/oppen-hl/tests/`; any change to signing must keep them green.

## Conventions

- Rust: edition 2024, `cargo fmt`, `cargo clippy -D warnings`, `cargo deny check`. Errors via `thiserror`. No `unwrap` outside tests.
- TypeScript: bun, `vue-tsc --noEmit` clean, no `any` on the MCP boundary types (generated from the Rust schemas).
- Design system: `docs/design/` — Space Mono + Archivo, void `#0A0B0C`, uranium `#FFD400` at ≤2% of any surface, no shadows, gradients or radius. Brightness marks a fact, never a mood.
- Venue message text is display-only. Program logic branches on the typed error, never on the string (invariant 8); the raw message is stored and rendered verbatim, labelled venue-authored. `Required:` / `Traded:` in Hyperliquid's sub-account refusal are for the operator's eyes, not for a match arm.
- Provisioning attempts and classifies; it never predicts a gate. `userRateLimit` does return `cumVlm`, documented as "Cumulative volume" (`crates/oppen-hl/src/types.rs`), so a distance-to-gate may be **displayed as an estimate** — but whether the sub-account gate reads that counter, and whether it is lifetime or windowed, is unconfirmed. No code path gates oppen's own behaviour on it.
- Commits: conventional commits. PRs: state which spec items they implement by number.
- Tests before "done": a signer change needs vectors; a guardrail change needs a property test proving no bypass; a ledger change needs the disconnect-reconcile test.

## What "done" means per phase

See the roadmap table in `README.md`. A phase is not done until its gate passes on testnet, not on mocks.
