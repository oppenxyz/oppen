# Desktop MCP Runtime

Execution-owner integration after ES19. Traces to spec items 9, 14, 15, 26,
27 and 34. This is not approval to activate the dedicated testnet account.

## Startup Authority

An explicit operator command starts one testnet agent/account supervisor. It
requires existing authenticated registry and policy authority, the matching
durable pilot authorization and account identity, usable pilot accounting, and
pairing bindings covered by that single account pump. Revoked pairings still
count because they retain cleanup obligations. At least one matching retained
binding is required: an empty store gives pause enforcement no account target.
ES35 permits an authenticated, identity-matching halted pilot with known
accounting to restart supervision. A durable trading stop must not suppress the
existing cleanup/retry worker. Startup preserves the original pilot budget,
baseline and stop and leaves order admission inhibited. It is not a resume,
policy acknowledgment, new consent or position-close operation. Authenticated
but unavailable accounting still refuses startup; that recovery limitation is
not resolved here. A running listener does not prove completed cancellation.

Missing or conflicting authority
blocks startup; startup never initializes, replaces, issues or acknowledges it.

Startup uses `Ledger::open_existing`: the ledger must have a valid existing
anchor and current schema. Missing authority cannot be adopted, and schema
upgrades require a separate explicit path. The follow-up on
`fix/mcp-existing-authority` replaces the previous create/adopt-capable opener.
A synthetic missing-anchor regression failed before the fix and passes after
it; missing-database startup also leaves the directory empty. The desktop suite
passes 74 native tests and the updater regression, with two live tests ignored;
all-target desktop clippy passes. This is startup-refusal evidence, not a live
supervision gate.

The console's configured account and requested account must agree. The runtime
owns retained startup work independently of the IPC observer. Local socket
ownership is established before venue work so an occupied port cannot leave an
orphan pump. Wallet material remains in Rust; status observation never opens
keys or databases.

## Lifecycle

The real Gateway, execution FeedSession, FeedPump and WsPool belong to the
desktop runtime. They are not simulated by console market freshness. Orders
remain inhibited until a separate future operator review/reconciliation and
acknowledgment path. Starting supervision can enforce existing pauses and cancel
resting orders, so it is not a read-only connection test.

Quit, update installation and context replacement cannot pass the retained MCP
startup/server/pump drain. Close MCP admission and await actual execution;
request pump quiescence and await its acknowledgment after active work finishes.
The quiescent pump keeps folding events but starts no subscriptions or new
reconciliation walks. Then stop and join socket producers, close the pump
receiver and consume queued events. Only after actual tasks finish may their
pairing ownership be released.
An interrupted observer does not abort ledger work or establish completion.
An in-flight socket frame can be lost at shutdown; restart reconciliation is
mandatory and a drained process is not evidence of a flat venue account.

Local persistence/application and malformed-gap failures latch for the session
lifetime. Later ticks and an empty reconciliation walk cannot hide them.
Transient venue failures remain retryable and clear reconciliation while
outstanding; they do not make a completed process drain a permanent failure.
Listener, account-feed health, reconciliation and pause-sweep observations stay
separate. A completed cleanup sweep is not policy verification or order admission.

## Evidence Gates

Use synthetic authenticated authority and local loopback fixtures to exercise
real MCP reads, refused order admission, occupied ports, wrong account/network,
duplicate ownership, partial startup, dropped observers, stalled work and queued
event drain. Separately verify UI empty/error/running/stopping states. Preserve
explicit unknown and stale observations; a bound port is not a completed MCP
handshake and neither is trading activation.

No fixture counts as a live pilot, position-aware quit, client verification or
installable build gate. Account confirmation and publication approval remain
separate requirements. Broader Connect Agent onboarding follows safety work.

### Halted Pilot Restart

ES35 needs a persisted fee-driven budget stop, reopened through the same
existing-authority startup path used by the desktop. Verify that opening does
not append new consent, change its baseline or clear its stop. Keep policy
acknowledgment inhibited and reject actual MCP order requests. Exercise the
existing cleanup worker with a resting order, an initial failed cancellation
and a later acknowledged retry. Separate those outcomes from listener startup.
The fixture has no ordinary policy kill, but startup admission inhibition stays
active and can itself request cleanup. This proves that a halted pilot can
restart the existing cleanup worker, not that its halt is the sole trigger.
After actual task drain and ledger reopen, original
executed notional, reservations, realized result and halt remain authoritative.
Missing or conflicting authority must still refuse startup. These tests cannot
establish live venue acceptance or authorize real-account cleanup.

## Local Verification

ES35 focused verification passes three synthetic/loopback tests. The startup
regression failed before the fix with the explicit halted-pilot rejection,
then passed after it. A second case runs the retained desktop supervisor with
no ordinary policy kill and startup inhibition still active, observes a scripted
cancellation failure, releases a
gated successful retry and verifies an actual MCP order refusal. Task drain,
physical ledger reopen and original pilot-state preservation are checked.
The third case confirms unavailable accounting still refuses startup. The full
workspace passes 1,178 Rust tests with 15 live/keychain tests ignored; all-target
Clippy, formatting and offline dependency checks pass. No frontend change or
frontend test rerun is included. Exact-head review and green CI remain required;
these results do not establish a live recovery gate.

Earlier ES20 verification:

- Workspace: 929 Rust tests passed, 15 live-gated/helper tests ignored;
  formatting and all-target clippy with warnings denied passed.
- Desktop: 116 JavaScript tests and the production web build passed.
- Real desktop-owned MCP fixture: authenticated event read, actual order
  request returning `trading_paused`, durable refusal, no order intent or
  submission, zero exchange requests, and ownership release after shutdown.
- Pump: 31 focused feed tests cover quiescence, queued-event persistence,
  dropped observers, malformed evidence, failure latching and venue retries.
- Browser fixture: default wide viewport and 1280x720 screenshots, long-error
  wrapping, missing-authority diagnostics, stop confirmation, terminal controls
  and Builder-to-Settings navigation. Temporary browser/server were closed.
- Normal/build feature graph excludes `test-support` from both protocol and
  MCP crates. Final request-capable native fixtures use loopback transports.

An initial zero-connection fixture was not isolated: the pool clamps that
limit upward and could attempt the default venue socket for a synthetic
account. It was replaced with a pre-shutdown loopback pool before final
verification. No real wallet keys, orders or dedicated-account operations
were used. Separate automated review cleared the corrected working diff;
committed-head review and CI remain required before any merge.
