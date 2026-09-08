# Desktop Operator Halt

ES21 implements spec items 26, 32 and 34 for the currently supervised TESTNET
agent. It follows desktop runtime ownership, not trading activation. No real
account action is authorized by this implementation work.

## Operator Contract

The explicit halt command targets the running supervisor's exact agent and
account. It does not target an editable setup form or an unverified roster row.
An idle, starting, terminal, differently bound or unavailable runtime refuses
the request. One-account supervision cannot promise fleet-wide cancellation:
the control is labeled **Halt agent**, never **Halt all**.

Durable pause scope is the agent identity, not an account-specific policy.
It remains engaged if that identity is later assigned a different container.
Cancellation scope is the supervisor's confirmed account; registry revalidation
must refuse cleanup if the agent has been rebound. Do not send cancellation to
the replacement account or claim successful cleanup of the previous account.

Use the existing engine and its authenticated policy journal. The core halt
first engages its local emergency stop, then attempts durable persistence.
Return admission separately from completion. A failed or uncertain write must
not become "halt unchanged" or successful durable persistence. Do not reconstruct
an engine, initialize authority, replace credentials, release a kill, acknowledge
policy, or enable orders from this path.

The runtime retains admitted work independently of the IPC observer. Duplicate
requests must not create concurrent mutations. Shutdown closes operator admission
and joins actual admitted work before releasing the existing server, pump and
authority. An unavailable supervisor cannot supply cancellation completion.

## Cancellation Evidence

Reuse the existing periodic pause-enforcement loop, account execution queue and
final registry/signing checks. Wake that loop after the persistence attempt;
do not add a second cancellation implementation or turn halt into shutdown.
The supervisor remains running to retry incomplete cleanup.

Every sweep carries monotonically increasing started and completed sequence
numbers. Capture the started sequence after the halt persistence attempt and
request another sweep. Only a completed sweep with a greater sequence can
provide cancellation evidence for this request. Neither an older success nor
completion of a sweep already in progress at that boundary qualifies.

Keep durable halt status distinct from cancellation status. Pending, retrying,
unavailable and acknowledged cancellation are different outcomes. A successful
sweep means its initial order snapshot was empty or targeted cancellation
requests received acknowledgments. It does not prove a later empty order book,
closed positions, dead-man coverage or a flat account. Existing partial/error
results remain incomplete and eligible for the existing retry loop.

## Console Contract

Show the exact agent/account and cancellation effect at confirmation. Preserve
the halt observation across view changes and command-observer loss. A late
command response must not overwrite newer persistence, cancellation or terminal
status. Polling and status refresh never invoke halt. Unknown or stale status
must remain visible, including when an earlier durable halt was observed.

The command's typed `halt_not_admitted` error is reserved for rejection before
work is retained. Only that result, followed by a fresh read of the same
listening binding with an idle halt, permits a new deliberate attempt. Discard
reads begun before rejection. Transport interruption and unknown outcomes remain
latched; error message text is not admission evidence.

Stopping the runtime, halting orders and closing positions remain separate
actions. There is no resume or flatten control in this change.

## Acceptance

- Existing authenticated authority receives a durable agent kill that survives
  reopen; subsequent real MCP order requests remain refused.
- Cancellation evidence belongs to a post-request sweep; old/in-flight successes,
  partial results, failures and supervisor termination cannot produce success.
- Dropped command observers and concurrent stop retain actual mutation and
  cancellation-attempt ownership. Missing/wrong/terminal runtime requests refuse.
- A retired/rebound route retains the agent pause but cannot redirect old-account
  cleanup or produce a false cancellation acknowledgment.
- Local fixtures verify persistent results, refusal records and cancellation
  transport outcomes without real keys or venue traffic.
- Console tests cover explicit admission, stale/error status, late responses,
  exact target binding and persistence versus cancellation reporting.

Green local tests and independent review are not supervised testnet acceptance.
The dedicated-account identity, trading limits, publication approvals, full
onboarding and installable-build gates remain unchanged.

## Local Verification

- The full Rust workspace passed with live-gated tests left ignored. Native
  desktop tests include durable halt/reopen, typed admission refusal, retained
  blocked mutation and drain observers, failed persistence, unavailable
  supervision, periodic cleanup retry, and authenticated retire/regrant.
- The actual MCP loopback fixture holds an in-flight sweep across the halt,
  rejects two cancellation attempts, then acknowledges a later retry. Resting
  orders remain after failure and disappear after retry; the position does not
  change. All exchange requests are isolated fixture requests.
- Frontend tests cover late response ordering, explicit retry after typed
  non-admission, unknown-outcome latching, and inert rendering of diagnostics.
- Browser fixtures verified the confirmation identity, navigation-persistent
  banner, pending/uncertain/retrying/acknowledged states, 1280px desktop layout,
  and 390px diagnostic wrapping. The wider console remains desktop-only.
  The temporary browser and fixture server were closed.

The halt API does not return the exact committed policy revision. Its nullable
revision field stays unknown rather than substituting a later cached revision.
Persistence success and cancellation acknowledgment remain separate facts.
Final committed-head review and CI are still required before merge; publication
and account-identity approvals remain outstanding.
