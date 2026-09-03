# oppen threat model

Read this before trading real funds. It states what oppen's safety features do and do not defend against.

## The setup

oppen runs on your machine. So do your agents. An agent with shell access on the same machine, under the same OS user, is inside the trust boundary of everything oppen stores locally.

## What is a hard boundary

**The agent wallet cannot withdraw.** Hyperliquid agent (API) wallets can trade but cannot move funds out of the account. This is enforced by the venue, not by oppen. It is the only containment property that holds against a fully compromised machine.

**The master wallet never enters oppen.** Onboarding signs `approveAgent`, sub-account creation and `approveBuilderFee` in your own wallet over WalletConnect. oppen never sees the key, so oppen cannot leak it.

**One sub-account per agent.** Capital segregation is enforced by the venue. An agent's guardrail caps bound its sub-account, and its worst case is that sub-account's balance.

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
