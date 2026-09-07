# Agent connections and client support

Component specification, initial requirements. Requested 2026-09-07.
Scheduled after the current execution, authority and supervision safety work.
This is planned scope, not a compatibility or verification claim.

## MVP

- An in-app **Connect Agent** flow names the agent, selects its network/account,
  assigns permissions and guardrails, and creates a revocable pairing through
  the operator-only authority path. Client selection never grants permissions.
- Account and permission assignment must preserve the same server-side account
  isolation, approval mode, risk limits, cumulative budgets and final Rust
  signing checks for every client. No client-specific bypasses or weaker defaults.
- Provide guided configuration for an initial set of verified MCP clients,
  generic configuration for compatible clients, and setup for external
  local-model runners. Select and verify the initial matrix during onboarding
  implementation; do not label an untested product or version as verified.
- Connection testing performs the authenticated MCP handshake and a scoped
  read-only tool request. Check protocol/result shape, network and account,
  declared permissions and refusal of unauthorized operations. A connection test
  must not place an order, require a funding transaction or consume trading
  notional. Transport success does not prove trading activation.
- Show factual connection/setup status, last successful test, current client
  verification status and actionable failure states. A configured snippet does
  not establish an active connection. Unknown/stale observations stay explicit.
- List pairings and provide in-app revocation with durable outcome reporting.
  Revocation must close client sessions and preserve the same execution-drain
  and reconciliation rules across all clients.
- Generic configuration must describe the actual supported MCP transport,
  endpoint and authentication contract. Keep a manual path when a client lacks
  a verified integration; do not imply that every MCP-branded client supports
  the required transport and authentication features.

Local-model support means connecting an external runner through the same MCP
contract and safeguards. It does not automatically add embedded model hosting,
an in-app agent runtime, stdio transport, or paid inference to MVP scope.

## Beta

- Expand the verified client/version matrix using the same acceptance suite.
- Add permission-based one-click client configuration. Show the proposed changes,
  request permission for the identified client/configuration, preserve unrelated
  settings, and support safe rollback. No silent file discovery followed by
  credential injection or blanket permission across clients.
- Design connectivity for cloud clients, including clients such as ChatGPT,
  under a separate security review before implementation or enablement. Resolve
  authentication, token audience/scope, transport security, relay trust, remote
  access boundaries, revocation, data exposure, abuse controls and recovery.
  Do not expose the loopback server, create a tunnel or deploy a relay merely
  because a cloud client cannot reach localhost.

Cloud connectivity remains planned and unverified until both its security gate
and client acceptance suite pass. Paid services, production publication and
other existing approval gates remain unchanged.

## Compatible Versus Verified

**Compatible** describes a client or runner whose documented/observed MCP
transport, authentication and tool behavior meet Oppen's contract, but whose
complete supported workflow has not passed the verification suite. State any
known limitations; generic configuration alone is not verification.

**Verified** requires recorded evidence for the named client, version, platform
and configuration: setup, authenticated scoped reads, permission refusals,
network/account isolation, reconnect, stale/error status and durable revocation.
Use safe fixtures for negative execution tests. Record test date and limitations;
changes to relevant client behavior require revalidation. A previous verified
version does not automatically verify every newer version.

Expose these distinctions in setup and support documentation. Maintain one
server-side authorization model and one safety suite, rather than treating
verification as a different permission tier.

## Credential Boundary

Wallet private keys never reach client configuration or the frontend. Pairing
credentials are separate, scoped capabilities: reveal only through the explicit
operator setup/handoff, avoid logs and event exports, and use supported secure
client credential mechanisms where available. Configuration previews must
redact credentials outside the intentional handoff. Revocation, not deleting a
snippet, is what removes authority.

See [onboarding.md](onboarding.md) for venue/account ceremonies and
[mcp-contract.md](../mcp-contract.md) for the actual transport/tool contract.
