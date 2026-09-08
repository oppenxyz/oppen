# Supervised testnet alpha acceptance

Trace: spec 4, 7, 9, 15, 19, 24–29, 31–35; execution/recovery gates #41–43.
Status: procedure prepared, not executed. A successful local suite, CI run or
release build does not complete this procedure. Record evidence against the
exact installed build and account; leave unsupported cases open.

## Before account activity

1. Record the merged commit, green CI run, release version and installed version.
   Keep a distinction between a browser fixture, native loopback test and the
   installed app communicating with the actual testnet venue.
2. Obtain the operator's full dedicated-account identity and exclusive-use
   confirmation. Record prior pilot activity. Never infer exclusive use, empty
   history or consent from the presence of an API wallet or a flat position.
3. Inspect the existing registry, route, signer approval, authenticated policy,
   ledger integrity and original pilot state through supported read-only paths.
   Missing keys, policy or configuration are provisioning blockers. Do not
   repair them by editing a ledger, importing a browser wallet, inventing an
   authenticated grant or replacing a key.
4. Enforce the original cumulative limits: $15/order, $25 gross open exposure
   including resting orders, $150 executed notional, $5 realized loss including
   fees and 1x leverage. Stricter policy wins. Include earlier sessions, unknown
   liabilities, fees and reserved commitments; unknown accounting is not zero.
   Stop for an operator decision if any limit is exhausted.
5. Check current positions and orders before planning the lifecycle. Existing
   exposure/cleanup counts against the same pilot. Do not expand the budget to
   make a minimum-size order or recovery experiment fit. No funding, owner-wallet
   signature, mainnet action or credential replacement is part of this runbook.

## Operator path

Use the installed desktop's explicit ceremonies. Initial TESTNET pilot consent
requires the existing authenticated paused policy, registry and anchored history.
Its never-used attestation is valid only when the operator can truthfully make
it and the native evidence agrees. Existing consent is inspection-only; prior
activity follows preservation/recovery rather than a fresh zero baseline.

Pair the intended external client and verify its read-only connection/status.
Starting supervision does not activate orders. Review the exact account, route,
policy, approval mode, cumulative capacity and live evidence in the activation
panel, then use explicit confirmation. An existing stop requires its separate
reviewed release; permanent/exhausted pilot stops are not releasable here.

For an approved lifecycle, submit through the paired MCP client and guarded
runtime, observing approval/repricing where required. Do not substitute the
low-level `oppen-hl` signing example: that cannot prove the assembled runtime,
policy, accounting, desktop or client gate. Reconcile each outcome before another
action. Never retry an unknown submission as a new order.

## Evidence matrix

| Case | Evidence required | Stop condition |
| --- | --- | --- |
| Paired read-only connection and revocation | Correct agent/account scope; no operator mutation through MCP; revoked sessions denied | Foreign scope, unexpected permission or ambiguous revocation |
| Place, approved execution, fill and permitted cancellation/close | Correlated request, guarded outcome, venue order/fills and chained ledger rows; cumulative usage preserved | Missing correlation, incomplete ownership, unknown delivery or budget exhaustion |
| Partial fill or applied-but-lost reply | Filled and remaining liability preserved; no duplicate submission; authenticated ownership before discretionary cancellation | Venue evidence insufficient to establish ownership; preserve liability and mark case unresolved |
| 30-second feed loss and reconnect | Stale observations visible; execution admission refused until actual reconciliation; no lost fills | Reconnect alone restores eligibility or old frames freshen a new selection |
| Sleep/wake, process crash, quit/update and physical restart | Retained work drains when possible; durable usage/reservations/stops survive; restart requires new review | Budget reset, silent activation, detached signer or false cleanup acknowledgment |
| Cancellation retry | Original target retained; uncertain/failed delivery visible; eventual venue outcome reconciled | Retry target lost, unsupported ownership or cancellation reported as position closure |
| Locked keychain or failed durable write | Typed refusal and inhibited execution; no false receipt | Any signature without required authority/evidence or success before durability |
| Dead-man | Accepted schedule, refresh, actual firing/disarm evidence and durable per-account uncertainty/budget handling | Scheduler not implemented, venue refusal, unknown coverage or unknown remaining quota |
| First-run onboarding | Time from an already funded testnet wallet to the approved guarded order, with all required ceremonies | Hidden setup, missing provisioning or bypass of the supported UI path |

Execute destructive failure injection first against isolated synthetic fixtures.
An actual crash, keychain lock, network interruption or update on a live pilot
needs a prepared account-specific recovery plan and remaining capacity; do not
use real funds or unrelated local data as test fixtures. Do not report an
unexercised venue scenario as passed because its synthetic equivalent passed.

The dead-man runtime remains unimplemented at this checkpoint. Its documented
quota counts scheduled firings, not routine refreshes; see
[signing reference §10](../hl-signing.md#10-schedulecancel--the-dead-mans-switch-and-its-daily-budget).
Ambiguous-order ownership acquisition still requires the trusted-evidence gate
in [order provenance](../specs/order-provenance.md). Neither gap is resolved by
this procedure. Keep issue #43 and the alpha acceptance gate open.

## Completion record

For each row, record pass, fail, not run or blocked; build/commit; testnet account
and agent; UTC interval; initial/final cumulative usage and unresolved liabilities;
relevant venue/order/event identifiers; and sanitized evidence location. Never
include signing keys, bearer tokens, wallet secrets or unrestricted data exports.
End with current open positions/orders, retained stops and runtime state. A
stopped process or canceled order is not evidence that the account is flat.

Close #41–43 only when their actual acceptance evidence is satisfied. Then
advance the active queue in [ROADMAP.md](../../ROADMAP.md); do not turn this
checklist into an automatic account-activity runner.
