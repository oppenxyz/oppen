# Desktop Runtime Ownership

Implementation contract, ES19. Follows authenticated policy authority (ES18).
Traces to spec items 9, 14, 27, 31 and 34 and the supervised-alpha shutdown and
recovery gates. This work owns the desktop's existing tasks; it does not imply
that an MCP runtime is running or authorize trading.

## Required Outcome

One desktop owner coordinates existing feed tasks and blocking local reads.
Network replacement, quit and explicit update installation must not abandon
work or start a replacement while the previous context still owns tasks.
Retain completion evidence through dropped IPC futures, window reloads and
timed-out observers. A stalled task keeps the owner stopping until actual drain.

- Retain WebSocket connection-task completion and the console event-loop task.
  Signaling shutdown is not proof of completion. Continue consuming queued
  events while socket tasks drain so backpressure cannot deadlock shutdown.
- Serialize context replacement. Close admission before draining the old feed;
  publish the replacement only after old work finishes. Context includes the
  network and data directory, resolved consistently across desktop readers.
- Bound local operator, pilot and existing keychain-availability reads in
  independent slots. The actual blocking closure retains
  ownership even if its IPC future is dropped. Shutdown rejects new reads and
  waits for admitted work without blocking the UI or holding a state lock over
  I/O. Read failures remain distinct from venue failures.
- Tag feed events with their network and owner generation. The frontend rejects
  old queued events and stale setup responses, including testnet-mainnet-testnet
  transitions. A new connection is not reconciliation or trading readiness.
- Route app exit and operator-approved update installation/restart through the
  same drain barrier. No installer or replacement process may run ahead of it.
  An incomplete drain must not be labeled stopped or silently forced complete.
- Surface actual lifecycle failures, not a synthetic healthy runtime flag.
  Observing lifecycle status does not open a database, read keys, start a venue
  connection, initialize policy or grant authority.

## Boundaries

Preserve existing read-only market behavior and the updater's separate approval
requirements. Do not introduce MCP activation, wallet access, policy adoption,
pairing issuance, acknowledgment, pilot authorization or automatic venue cleanup.
The later execution owner must attach the real gateway/feed pump and retain its
existing session, supervision and execution drain guarantees. This prerequisite
does not complete that activation gate or the position-aware quit ceremony.

## Evidence

Use controlled streams, local loopback fixtures and blocked closures to prove
normal drain, concurrent stop, dropped waiters, task failure, delayed writes,
replacement ordering and old-event rejection. Tests must observe actual task
completion and refused admission, not just a cancellation flag. No live venue,
dedicated account key or trading budget is needed for this slice.

Local branch verification: 905 Rust tests passed with 15 live-gated tests
ignored; 104 desktop tests, production web build, formatting and all-target
clippy passed. Independent automated review cleared the implementation after
regressions fixed unnecessary first-feed waits and late failures that could
reverse a completed shutdown. Exact committed-head review and CI remain gates.

Browser fixture checks exercised stopping, stopped-with-error and normal
running. At 390px, the banner's right edge was 390px and its text had no
horizontal overflow; desktop layouts were checked at 1280px and 1440px.
Only the safety banner is narrow-window compatible; the existing console
retains its desktop minimum width. Fixture reads never invoke native commands.

Socket shutdown may discard an in-flight frame: completion is not a lossless
flush or venue reconciliation. A blocked read can hold shutdown indefinitely.
Real app exit/installation/restart, venue reconnect and trading activation still
need their separate assembled gates. No live account or budget was used here.
