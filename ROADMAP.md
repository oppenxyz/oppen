# oppen roadmap

Everything mapped so far, from the v1 MVP through the versions after it. Numbers in brackets are spec items in [docs/spec.md](docs/spec.md); D1–D8 are the settled architecture decisions there and are not re-opened here. Each phase has a gate: the phase is done when the gate passes on testnet, not on mocks.

Legend: `[x]` implementation on `main` or in an open PR · `[ ]` incomplete (may
include partial/local implementation) · **gate** = what proves the phase.
Neither a checkbox nor an open PR proves installed or live acceptance.

## Active delivery queue — recovered 2026-09-08

The owner's original goal is a supervised Hyperliquid **testnet alpha**:
guarded execution, durable reconciliation, desktop supervision, failure/recovery
gates, onboarding and an installable macOS build. Continue through the later
roadmap after that goal, in milestone order. Quantoppen's detailed design starts
at its scheduled milestone. The subsequent venue reprioritization puts Lighter
and Aster in v1.2 before v1.5.

This queue tracks delivery separately from implementation. The detailed phase
lists below remain the feature inventory; avoid treating old unchecked rows as
proof that code is absent. GitHub gates [#41](https://github.com/oppenxyz/oppen/issues/41),
[#42](https://github.com/oppenxyz/oppen/issues/42) and
[#43](https://github.com/oppenxyz/oppen/issues/43) are still open at recovery.

| Order | Work | Current evidence | Remaining completion gate |
| --- | --- | --- | --- |
| 1 | Finish TRADE realtime correction: bid/ask/depth, chart source handling, per-channel health and selected-stream ownership | PRs #88–91; selected head `effafee` passed independent exact-head review; 1,298 Rust and 246 frontend tests pass | Green CI and dependency-ordered merges, installed/native-to-UI and public testnet observation; broader subscription adoption remains separately scoped |
| 2 | Deliver activation, reviewed kill release and initial pilot consent (ES37–39) | PRs #85–87; commits `7208956`, `97806dc`, `ddb576b` have recorded independent exact-head reviews | Green CI and dependency-ordered merges, installed explicit consent/review/activation workflow; no implicit account activity |
| 3 | Close execution, supervision and recovery gates #41–43 | Guarded lifecycle, durable accounting, pairing/policy/approval/HALT and adverse-order fixture coverage exist | Authorized venue lifecycle and failure matrix, physical restart, dead-man accounting/confirmed coverage, trustworthy ambiguous-order evidence; preserve unresolved liabilities |
| 4 | Complete remaining v1 operator and onboarding surfaces | Detailed P1–P7 rows below distinguish implemented portions from omissions | Account/container model, Connect Agent and verified client matrix, first-run ceremony, UI/notification/manual-action gaps, signing questions and measured ten-minute onboarding |
| 5 | Deliver and harden the alpha artifact | Public repo and macOS release pipeline exist; last recorded installed version 0.1.178 | Verify current installed artifact, provenance/checksums, signing-host isolation and recovery drills; PR84 credential migration and Apple signing account remain owner-dependent |
| 6 | v1.1 hardening, then v1.2 Lighter/Aster | Scheduled below, not delivered by the current feed changes | Complete each milestone's implementation/review/CI and actual acceptance before venue execution or v1.5 expansion |
| 7 | v1.5 Quantoppen/harness/workflows, then v2+ | Scheduled contracts and feature lists below | Plan agent-consumable quant features when reached; implement and verify each milestone without weakening shared safeguards |

Standing authorization: focused branches, issues and PRs, with merges only after
green CI and separate review. Before any trading, cancellation or position
cleanup, obtain the owner's dedicated-testnet-account identity confirmation.
Preserve cumulative limits across sessions: $15/order, $25 total open exposure
including resting orders, $150 executed notional, $5 realized loss including
fees, maximum 1x leverage. Exhaustion requires a new owner decision, never a reset.
Mainnet, new wallet-owner signatures, funding, credential replacement and paid
services are outside this authorization. PR84's previously rejected credential
migration is a separate explicit approval gate. Progress on unaffected work
continues while owner-dependent gates remain open.

Current action: complete CI and merge the reviewed #85–91 dependency stack,
then verify the resulting artifact and advance the remaining recovery gates.
The interrupted workspace validation has completed; the larger goal has not.

## Execution audit follow-up (2026-09-07)

The component checkboxes below are not production-readiness claims. The assembled
execution path failed review despite its passing unit tests. Follow this order
before expanding features:

Next activation implementation contract: [operator-activation.md](docs/specs/operator-activation.md).
Review and explicit confirmation must bind fresh account evidence to the existing
runtime's authority; initial consent and kill release stay separate operations.
Initial consent and reviewed kill release are implemented locally, with the
verification evidence below. They remain delivery gates until remote CI,
installed-artifact checks and authorized live acceptance pass; do not satisfy
them by editing a real ledger or auto-releasing stops. Synthetic operator
tests do not prove the first-run desktop order workflow.
The reviewed kill-release contract is
[reviewed kill release](docs/specs/operator-kill-release.md) (ES38), including
HALT re-arming and stale-generation refusal. Initial consent remains separate.
ES38 is implemented locally, not installed or accepted for a real account.
The final combined Rust suite passed 1,236 tests with 15 ignored. All 207 frontend
tests, production build, QA typecheck, strict all-target clippy and formatting
passed. Thirteen core release regressions cover scoped release, nonzero pilot
accounting, permanent stops, physical reopen and publication races. Deadline
coverage proves post-acquisition clock resampling, not a measured blocked wait.
Native tests cover retained HALT persistence, lock contention, fresh activation
review after release, and truthful failed-cleanup shutdown without false
acknowledgment. The UI preserves historical release evidence across activation's
own inhibition-generation advance while fencing newer HALTs and stale polls.
Mock browser confirmation, receipt, expiry, unknown outcome and complete
release/activation-review/renewed-HALT checks passed. Desktop and 390px standalone
panel screenshots were inspected; mobile-console support is not claimed.
Combined independent automated working-tree review found no remaining safety
blockers. Local implementation commit
`97806dc2183d6df61a6f1e5c767fb365570be46d` received independent automated
exact-head review with no blocking findings; this is not human approval. Remote
CI, installed-artifact checks and authorized real-testnet gates remain pending.
Explicit publication approval for the local activation/release commits has been
requested; the local audit and security handoff remain excluded. Initial consent
was implemented as a separate gate. Its
[ES39 contract](docs/specs/operator-pilot-consent.md), committed as `e1eff9e`,
passed independent design review and is now implemented locally. Consent owns
the pre-MCP runtime, binds fresh evidence and the exact baseline transactionally,
requires explicit never-used attestation, and refuses prior execution requiring
preservation. Fill-walk coverage is bound to the same ledger instance and retained
across other gap retries, but discarded on monitor replacement. Existing consent
is inspection-only; no signing key, implicit activation or stop release is added.

Combined Rust validation passed 1,258 tests with zero failures and 15 ignored.
All 221 frontend tests, production build, QA typecheck, strict all-target workspace
Clippy and formatting passed. The native quiet-account regression confirms after
the original freshness window using actual loopback WebSocket frames and seven
fresh REST reads, preserving policy and exact review correlation. Native tests
also prove retained startup-drain failures, late teardown recovery-required,
receipt preservation and physical reopen. All remain synthetic, not live gates.

Browser checks passed initial review/discard/fresh review, confirmation, receipt,
expiry, existing/legacy inspection, unknown-outcome reconciliation and terminal
recovery-required fencing. Desktop and 390px standalone-panel screenshots were
inspected without text/control overlap; mobile-console support is not claimed.
Late status replies cannot overwrite newer work; definitive refusal permits
identity correction without polling overwriting the draft. Unknown/terminal
failure cannot be cleared by stale success or an unrelated owner observation.

Aggregate independent automated working-tree review found no remaining blocking
findings. Local implementation commit
`ddb576b6310565778abfab627d25b76df18636f2` received independent automated
exact-head review with no blocking findings; this is not human approval.
Remote CI, installed-artifact verification and
authorized live acceptance remain pending. No publication or account action is
implied by these local checks.
Current checkpoint (2026-09-08): PRs #55-83 were merged after their checks;
fetched main is `14e478ae86034e44a14a12f96a3cbbee26cf7fd8`, with the same source
tree as reviewed PR83. Earlier publication-hold notes below are historical,
not current merge blockers. `oppenxyz/oppen` is public; the website repository
remains private. Installed macOS version is 0.1.178. None of this proves
account activation or a live gate.

ES37 activation remains local and unaccepted. Final combined Rust validation
passed 1,218 tests with 15 ignored, including core and native publication/ingress
race proofs. All 194 frontend tests, build/QA typechecking, strict clippy and
formatting passed. Mock-only browser checks passed confirmation, receipt,
stale and unknown states. Offline dependency checks passed with existing
warnings. Aggregate automated working-tree review and focused final race-test
review found no remaining blockers. Local commit
`7208956244e68b04447e75e73b9e9837f02106b5` received separate automated exact-head
review with no blocking findings; this is not human approval. Remote CI and
installed-artifact verification remain pending. The TRADE audit and local
security handoff are excluded from that commit and have not been published.
These local checks do not establish current remote CI. See the activation
contract for remaining acceptance evidence; real testnet identity/activity
authorization is still required. Repository hardening PR84 is separately
owned, and its specific credential-migration approval remains unresolved.

Security handoff follow-through (2026-09-08; all remain acceptance gates):
- [ ] Prove applied-but-lost replies, partial fills, network loss, sleep/wake,
  crashes, cancellation retries, update/quit during activity and physical restart
  without duplicate orders, budget resets or silent reactivation. Synthetic
  regressions supplement, but do not replace, authorized venue acceptance.
- [ ] Complete trustworthy ambiguous-order recovery and venue dead-man coverage.
  Preserve unresolved liability; the provenance acquisition trust decision stays
  open. Cancellation does not establish position closure.
- [ ] Document and test signing-host isolation for untrusted shell-capable agents.
  Keep MCP loopback-only unless a separately reviewed authenticated connectivity
  design is approved; do not equate a pairing token with host isolation.
- [ ] Before broad distribution, verify build provenance/attestations and updater
  signing compromise/recovery drills. Apple Developer ID, Hardened Runtime and
  notarization require the owner's account; independent security review remains
  required before real-funds use. Maintainer MFA/recovery and legal review are
  owner-dependent and unverified.
- [ ] Resolve the Linux `glib` 0.18.5 advisory GHSA-wrw7-89jp-8q8g /
  RUSTSEC-2024-0429 before Linux distribution. The handoff reports the published
  Apple Silicon dependency graph excludes it; the alert remains open. Passing
  cached offline dependency checks is not evidence of zero open alerts.

1. **Baseline:** repositories moved to `oppenxyz` with private visibility and
   history preserved; local remotes updated. PR #40 merged as `df4fdbd` after
   every CI job passed and a separate automated review found no blockers.
2. **Guarded execution:** correct the production ledger adapter, independent
   buy/sell exposure, per-container submission serialization, signing-time policy
   checks, and cancellation delivery with retries. Local regressions now cover
   these repairs, including the real SQLite sink and test signer, clipped fill
   permutations, dropped requests, revocation, and cancellation failures. The
   state bridge now values every symbol's opening orders at quoted reference
   marks and refuses incomplete valuation; limit prices remain display-only.
   Cross-symbol buy/sell regressions exercise the real engine (PR #45). The
   `oppen-mcp::tools::execution_fixture` now exercises actual MCP dispatch,
   guarded signing, loopback HTTP, partial fills, cancellation, exact small
   position closes, deduplicated startup reconciliation, and physical restart
   after an applied-but-malformed response. An optional operator-set gross
   account exposure cap counts positions and opening commitments across symbols
   without opposite-side netting; it is separate from the per-symbol cap and
   leverage. Cumulative pilot accounting now distinguishes executed turnover
   from unfilled reservations, persists exhaustion inside the fill's chained
   record, and retains the ledger lock through signing. Local lifecycle tests
   cover $150 across opening/closing fills, canceled-order liability, and a $5
   fee-driven stop that survives restart and blocks reduce-only orders.
   Pilot stops now drive the existing cancellation sweep, including retries and
   retained revoked bindings. Loopback checks cover a fee-driven stop with a
   resting remainder, failed cancellation followed by retry, ordinary resume,
   and unchanged positions; a stop is not automatic flattening. Local status
   preserves verified halt evidence when accounting projection fails.
   Transport-interruption variants, operator activation/baseline verification,
   end-to-end delivery reporting, and the authorized testnet run remain open
   gates; loopback acceptance is not venue acceptance.
3. **Operator supervision:** desktop MCP lifecycle, pairing/revocation, real
   positions and orders, policy editing, approval decisions, and a working halt.
   The shell now reads local pilot evidence independently of venue requests and
   shows a persistent stopped/reconciling/unavailable banner. Browser fixtures
   cover view changes, venue outage, stale local reads and network isolation;
   the safety banner fits narrow windows, while the wider console layout remains
   desktop-only. This read-only surface does not start or authorize execution.
   Durable pairing authority uses authenticated issuance/revocation records
   that survive restart. Credential records remain hidden
   from agent event reads, and one runtime owns the pairing cache until its
   sessions and actual method tasks drain. Local tests cover tampered authority,
   interrupted persistence, process death, disconnected requests, network
   mismatch and stalled durable writes without blocking HTTP or shutdown.
   Desktop lifecycle controls, verified registry/policy setup and operator
   activation remain open. See decision ES16.
   Registry-to-signing authority merged in PR #52 as `be4cc507` after green
   exact-head CI and separate automated review (ES17). This is not an activation
   gate passed. Review fixes have local regressions for cancellation with
   unavailable pilot evidence, idempotent anchor-publication retries, legacy
   address aliases, expiry crossed during signing waits, and asynchronous
   decision-worker shutdown/owner retention. After integrating current main,
   the workspace run passed 825 tests with 15 live-gated tests ignored; 89
   desktop tests, the build, formatting and clippy passed. The roster renders
   without the removed policy-vault cache and reports its route unavailable.
   Authenticated policy storage is implemented on `feat/authenticated-policy`
   with complete HMAC snapshots, reviewed paused migration, revision-bound final
   signing and explicit restart acknowledgment. Idle supervision refreshes policy
   on a tracked worker and retains registry-verified cleanup when policy fails.
   PR #55 passed separate automated exact-head review and every CI check at
   `39a60b8`; the integrated local suite passed 876 Rust tests (15 live-gated
   ignored), 90 desktop tests, the build, formatting and all-target clippy.
   Merge is held for owner
   confirmation of the newly automatic private macOS update publication. See
   [policy-authority.md](docs/specs/policy-authority.md). Desktop ownership
   is in progress on `feat/desktop-runtime-ownership` (ES19): retained socket
   and event tasks, bounded local reads, scoped feed generations and a drain
   barrier before quit/update installation. The full local suite passes 905
   Rust tests (15 live-gated ignored) and 104 desktop tests; build, formatting
   and all-target clippy pass. Separate automated review cleared the native
   startup and late-failure fixes. Exact-head review and CI remain PR gates.
   The lifecycle banner wraps at 390px while the console remains desktop-only.
   Real native installer/restart and MCP activation remain unverified. See
   [desktop-runtime-ownership.md](docs/specs/desktop-runtime-ownership.md).
   PR #56 now has green exact-head CI at `5bc94379` and separate automated
   review; the publication approval hold still prevents merging. A follow-up
   on `fix/mcp-supervisor-failure` closes MCP admission when pause supervision
   terminates, retains actual execution drain, and reports failure instead of
   successful shutdown. This is a prerequisite, not desktop MCP activation.
   Desktop-owned MCP integration is implemented on `feat/desktop-mcp-runtime`
   (ES20): existing-authority-only testnet startup, pre-bound loopback listener,
   actual gateway/pump ownership, explicit queued-event drain and operator
   start/status/stop controls. Local verification passes 929 Rust tests
   (15 live-gated/helper tests ignored), 116 desktop tests, the web build,
   formatting and workspace all-target clippy. Separate automated review
   cleared the failure-latch, quiescence and late-status fixes. Exact committed
   head review and CI remain PR gates; no live gate is claimed. See
   [desktop-mcp-runtime.md](docs/specs/desktop-mcp-runtime.md).
   PR #58 now has green exact-head CI and separate automated review at
   `9021e038`; the publication hold still prevents merging. Two CI-discovered
   test-fixture races have dedicated regressions; the final native desktop
   suite passes 49 tests. The bound-agent operator halt is implemented on
   `feat/desktop-operator-halt` (ES21): durable kill persistence and correlated
   cancellation evidence, distinct from stopping supervision or flattening.
   The agent pause survives account reassignment; cleanup cannot redirect to
   the replacement account. The full local workspace passes; native desktop
   coverage has 56 tests and the frontend has 132. Independent automated
   working-diff review found no blockers. PR #59 has green exact-head CI and
   separate automated review at `4b3f8a6`; the publication hold prevents merging.
   See [desktop-operator-halt.md](docs/specs/desktop-operator-halt.md).
   Before operator activation, authenticate pilot consent: current consent is
   hash-chain checked but not HMAC-authenticated, and core pilot checks remain
   optional when no applicable consent exists. Repair must preserve the original
   cumulative budget and all liabilities/stops, with explicit legacy review and
   no automatic trust or reset. This is an open safety gate, not an unconditional
   signing-bypass claim. See
   [pilot-consent-authority.md](docs/specs/pilot-consent-authority.md).
   ES22 is implemented on `feat/authenticated-pilot-consent`: authenticated new
   consent and explicit legacy adoption, mandatory supervised-alpha consent at
   reservation/final signing, read-only preflight checks and separately labelled
   inspection trust. V8 preserves existing history; adoption never resets budgets
   or releases stops. Local verification passes 953 Rust tests (15 live-gated
   ignored), 136 frontend tests and the web build; separate automated working-diff
   review found no blockers after the status-snapshot fix. Exact-head review and
   CI remain gates. End-to-end forged-consent reproduction remains unavailable
   under the documented tooling restriction. No operator activation or live gate
   is claimed.
   Paused operator policy setup is implemented on `feat/paused-policy-setup`
   (ES23): native-owned review and persistence, preserving complete policy and
   stops without activation. The existing frontend inspection type is partial
   and must not be used as a replacement payload. Local verification passes 977
   Rust tests (15 live-gated ignored), 152 frontend tests and the web build.
   Synthetic browser checks cover explicit review/retry, retained stops and
   nonretryable recovery-required evidence at the supported desktop minimum.
   Exact-head review and CI remain PR gates; no activation or live gate is
   claimed. See
   [paused-policy-setup.md](docs/specs/paused-policy-setup.md).
   PR #62 now has green exact-head CI and separate automated review at
   `a8ae379`; publication was skipped and the merge hold remains. A follow-up
   on `fix/mcp-existing-authority` applies its existing-only ledger opener to
   MCP startup too: the prior startup path could adopt a missing anchor.
   PR #63 has green exact-head CI and independent automated review at
   `bf47746`; publication was skipped and the merge hold remains.
   Operator approval work follows: first exact proposal route binding (ES24),
   then authenticated durable lifecycle, native-owned execution and pricing
   review. The current in-memory queue and best-effort rejection audit are not
   completion evidence. See [operator-approvals.md](docs/specs/operator-approvals.md).
   ES24 route binding is implemented on `fix/approval-route-binding`; synthetic
   replacement-account clearance reproduced before the fix now refuses, and
   the full workspace passes 981 Rust tests (15 live-gated ignored). Durable
   lifecycle, original-request repricing and operator UI remain open.
   PR #64 has green exact-head CI and independent automated review at
   `a35fa08`; publication was skipped. ES25 durable approval lifecycle is in
   implemented on `feat/durable-approval-lifecycle`, with journal/engine integration
   and production-constructor failure/restart tests. Local verification passes
   1,008 Rust tests (15 live-gated ignored), 152 frontend tests, build, formatting
   and workspace all-target Clippy. PR #65 at `125e899` has separate exact-head
   automated review; CI run `34186908204` did not start because GitHub reports
   account payment/spending-limit restrictions. No billing changes or CI bypass.
   ES26 original-request evidence is implemented on
   `feat/approval-request-evidence`, including distinct market/IOC/stop/close
   origins, historical-unknown handling and same-CLOID quote-observation retries.
   Local workspace verification passes 1,020 Rust tests (15 live-gated ignored),
   152 frontend tests, build, formatting and all-target Clippy; independent review
   and exact-head CI remain gates. PR #66 at `bfad1b0` has separate exact-head
   automated review; CI run `34187897072` also did not start due to the GitHub
   payment/spending-limit gate. While tracing native approval execution, ES27
   reproduced a signature at proposal expiry after approval one millisecond
   earlier. `fix/approval-signing-deadline` carries the private deadline through
   final signing. Local verification passes 43 focused approval tests and 1,024
   Rust workspace tests (15 live-gated ignored), 152 frontend tests, build,
   formatting and all-target Clippy. Separate diff review found no blocking
   issues; exact-head review and CI remain gates;
   original-request repricing and native approval UI remain open. This is not a
   completed live gate.
   ES28 is implemented locally on `feat/native-approval-queue`: desktop-owned
   scoped pending reads and durable rejection, retained single-flight work,
   shutdown drain even after parent failure, and row-local confirmation.
   Cached decisions cannot replace another proposal's unresolved rejection;
   explicit retry requires a successful fresh queue observation. Workspace
   Rust tests, 168 frontend tests, production build, formatting and all-target
   Clippy pass; independent automated diff review found no blocking issues.
   Browser fixtures verified row-local confirmation, rejection, uncertainty and
   explicit refresh at wide and 1280x800 desktop sizes. The separate QA project
   typecheck fails on missing `bun:test`/`node:url` types in existing test files;
   it is not a passing gate. Exact-head review and CI remain pending.
   Native pricing review, approval execution, exact-head CI and live gates remain
   open. This queue/rejection work does not enable order submission.
   Follow-up `fix/desktop-qa-typecheck` supplies the missing Bun development
   declarations and adds the QA typecheck to the existing web CI job without
   changing release behavior. QA1 keeps browser and test globals explicit;
   frozen install, QA typecheck, production build, 168 frontend tests and three
   release-script unit tests pass. Independent automated diff review found no
   blocking issues; exact-head review/CI remain separate from live gates.
   ES29 is in progress on `feat/native-approval-review`: retained full-action
   pricing review, one-shot native confirmation through the existing gateway,
   authenticated review/receipt commitments, pinned pairing authority at final
   signing, and retained shutdown work. Workspace tests (1,062 passed, 15
   live-gated ignored), frontend tests (172), build and QA typecheck pass.
   Separate automated working-diff review found no blocking findings; exact-head
   review and CI remain gates. Healthy live WebSocket acceptance and dedicated
   native route-read timeout regression coverage remain open. This is not a
   live-activation or release-readiness claim.
   Follow-up `test/native-route-timeout-drain` adds a dedicated actual five-second
   route-read timeout fixture: the caller times out while server drain retains
   the blocking worker, then completes only after release. All 154 affected MCP
   tests pass. Removing native route tracking made the exact drain assertion
   fail; production code was restored. Separate automated diff review found no
   blocking issues; exact-head review and CI remain gates. Next approval scope is discretionary agent
   cancellation with a frozen reviewed target set; HALT/pause cleanup must remain
   immediate. Dead-man supervision is a separate unwired requirement, not an
   existing agent-accessible disarm tool.
   ES30 is implemented locally on `feat/cancellation-approval`: typed discretionary
   cancellation proposals and retained native target review through the same
   gateway, with immediate internal HALT cleanup preserved. Workspace tests pass
   (1,089 Rust, 15 live-gated ignored), along with 174 frontend tests, build, QA
   typecheck and dependency-policy checks. Separate automated working-diff review
   found no blocking issues; exact-head review and CI remain gates. Browser
   fixtures verified protective target details, row-local partial/uncertain
   feedback and retained uncertainty at 1280x800 and wide desktop sizes.
   V12 preserves historical event bytes; older writers must stop before upgrade,
   and rollback must preserve all V12 history without lowering schema or budgets.
   D1 own-order provenance remains a
   separate activation gate: account binding alone does not exclude manual
   orders, and operator review does not establish that the agent opened them.
   ES31's [provenance contract](docs/specs/order-provenance.md) records the
   inspected reservation/signing gap, required authenticated evidence and
   recovery acceptance tests. Signer publication ordering and sufficient venue
   identity evidence are tracked separately below; full ownership enforcement
   is not yet implemented.
   ES31a on `fix/exchange-response-identity` preserves the exchange envelope
   discriminator and rejects wrong response types or single-order cardinality.
   Wrong-kind errors retain pending reservations. Synthetic real-HTTP tests
   apply requests despite mismatched replies and verify non-retryable uncertainty;
   removing the reservation kind check made the regression fail. Workspace tests
   pass (1,093 Rust, 15 live/keychain-gated ignored). Separate automated diff
   review found no blocking issues; exact-head CI remains required. This is a
   response-classification fix, not completed ownership or live recovery.
   ES31b on `feat/authenticated-submission-evidence` adds authenticated signed
   and direct-response accepted evidence to the existing ledger (V13). Opaque
   submission capabilities retain the single guarded signer; only a digest,
   never a replayable signature, is persisted. Bounded retained workers own
   account locks and session authority through publication and HTTP completion;
   cancellation is rechecked after ledger waits. Publication uncertainty keeps
   pending liability. Workspace tests pass (1,121 Rust, 15 live/keychain-gated
   ignored), as do 174 frontend tests, build, QA typecheck, three release-script
   fixture tests, all-target Clippy, formatting and dependency-policy checks.
   Separate automated working-diff review found no blocking issues; exact-head
   review/CI remain required. Test synchronization now waits for actual native
   task completion and a running supervisor; canonical digest fixtures also
   cover workspace JSON feature unification without changing the wire format.
   Discretionary cancellation provenance enforcement and sufficient lost-response
   ownership recovery remain open. Stop older writers before upgrading; rollback
   must preserve V13 history and cumulative accounting without schema downgrade.
   ES31c on `feat/cancellation-ownership` implements the discretionary
   cancellation gate: authenticated per-target links across proposal, retained
   review, final signing and consuming dispatch; immutable order identity with
   observed TIF/protective metadata; and partial-fill size decreases without
   target substitution. Unknown/manual targets refuse the whole request, while
   runtime HALT cleanup remains independent. Operator review displays observed
   TIF without historical defaults. A reproduced legacy MCP session-retention
   bug is fixed by explicit session closure plus actual handler/worker drain.
   Workspace tests pass (1,139 Rust, 15 live/keychain-gated ignored), along with
   175 frontend tests, build, QA typecheck, three release-script fixtures,
   all-target Clippy, formatting and dependency policy. Separate automated
   working-diff review found no blocking issues; exact-head review and green CI
   remain required. V14 preserves absent historical metadata without granting
   ownership; stop older writers and preserve all history on rollback. Lost-response
   ownership recovery and dedicated-account live acceptance remain open.
   Recovery acquisition investigation now identifies a concrete provider
   action/response association, but no authenticated testnet capture has been
   obtained. An operator-controlled official node is a candidate, not an enabled
   dependency. Source trust, real testnet evidence and exact signed-digest
   reconstruction are prerequisites to implementing recovery; see the
   [acquisition gate](docs/specs/order-provenance.md#acquisition-gate).
   No node provisioning, subscription or paid archive download is authorized.
   ES32 on `fix/disconnect-signing-admission` addresses a separately reproduced
   failure: a processed disconnect during key loading still allowed a previously
   evaluated order to return resting. Bind snapshots and final Rust admission to
   the engine's exact network/account feed and invalidation stamp, including
   reduce-only orders. Reconciliation cannot revive old clearances; runtime
   cleanup stays independent. The full workspace passes 1,153 Rust tests with
   15 live/keychain-gated tests ignored; 175 frontend tests, build, QA typecheck,
   all-target Clippy, formatting and dependency-policy checks pass. Separate
   automated working-diff review found no remaining blockers. Exact-head review
   and green CI remain required. Synthetic regressions cover account-read,
   key/ledger/publication/dispatch waits and recovery epoch changes; they do not
   pass the live disconnect or sleep/wake gate. Expired-silence buffered-frame
   handling is tracked in ES33 below.
   ES33 on `fix/buffered-frame-gap` checks expired silence before a buffered
   text frame can refresh its receipt clock, preserving the prior gap anchor.
   The actual loopback socket regression first failed, then passed for market,
   pong and unknown-channel text. A separate injected-lifecycle regression keeps
   one pump alive through the gap, blocks admission during catch-up, records the
   recovered fill once and latches its exhausted loss budget before another
   order can submit. Full workspace: 1,155 Rust tests pass, 15 live/keychain-gated
   tests ignored; all-target Clippy, formatting and dependency checks pass.
   Separate automated working-diff review found no blockers; exact-head review
   and green CI remain required. This keeps the existing idle timeout (75 seconds
   by default); it does not claim detection of every shorter pause, post-admission
   pause or pump backlog, nor completion of the live sleep/wake gate.
   ES34 on `fix/account-event-admission` is in progress: retain account-event
   admission inhibition from parsed ingress through durable application, and
   preserve receipt time across queue delays. The native feed's 13 focused tests
   pass, including blocked application, pending-account evidence and clean
   completion. The 12 ingress-wrapper tests and workspace all-target compile
   check also pass. The queued-loss MCP regression now refuses a fresh order
   before the pending fill is applied, with no additional signature, acceptance
   record or order POST; pump drain and physical ledger release also pass.
   Initial independent review identified continued-ingestion, closed-status
   and admission-overtaking defects; a scoped source re-review found those
   addressed. Full workspace validation passes: 1,175 Rust tests, with 15
   live/keychain-gated tests ignored. All-target Clippy, formatting and offline
   dependency checks pass. Aggregate automated review found no blockers;
   exact-head review and green CI remain required. This is not a completed live
   gate.
   ES35 on `fix/halted-pilot-supervision` corrects a reproduced restart gap:
   desktop startup refused a halted pilot before the existing cancellation
   supervisor could retry cleanup. The startup regression failed before the
   narrow fix and passes afterward, preserving authenticated identity, known
   accounting, the original cumulative budget and order inhibition through
   physical ledger reopen. That PR left unavailable-accounting startup as a
   separate limitation, addressed by ES36 below. Three focused regressions cover
   reopen, assembled cancellation
   failure/retry with an actual MCP order refusal, and unavailable-accounting
   startup refusal. Full workspace: 1,178 Rust tests pass, 15 live/keychain
   tests ignored; all-target Clippy, formatting and offline dependency checks
   pass. Independent review corrected an overclaim about the cleanup trigger;
   startup inhibition remains active. Exact-head review and green CI remain
   required; no live recovery gate is claimed.
   ES36 on `fix/unavailable-pilot-supervision` extends that recovery boundary to
   an authenticated, identity-matching pilot whose accounting projection is
   unavailable. The regression failed at the known-accounting startup check,
   then passed after removing only that projection requirement. Cleanup failure
   and retry, actual MCP order refusal, unavailable status and original consent
   records are preserved. Missing or failed authority still refuses startup.
   Actual startup reconciliation ingests a subsequent fill exactly once through
   physical reopen without clearing unavailable accounting. The full workspace
   passes 1,178 Rust tests (15 live/keychain tests ignored); 20 focused UI status
   tests pass without fabricated totals. All-target Clippy, formatting and
   offline dependency checks pass. Aggregate automated review found no
   blockers. Exact-head review and green CI remain required; this does not
   authorize live cleanup or establish a live recovery gate.
   No dedicated-account activity has been performed during this development.
4. **Recovery:** PR #44 merged as `06fc0e3` after green CI and separate automated
   review. The durable submission journal has SQLite reopen, independent
   handle, stale-revision, corruption and dropped-request regressions. Starts and
   resolutions use the existing hash chain; unknown outcomes still block after
   restart. Complete exchange/restart reconciliation, reconnect/sleep/network
   races, feature freshness, config HMAC, and confirmed dead-man behavior remain
   gates. A timeout or missing order status is not permission to retry.
5. **Release gate:** measured first-run onboarding and a small supervised testnet
   pilot. No live-trading readiness claim until these gates pass.

Work in progress is not a completed phase. The P1 signed-order, live-disconnect,
and supervised-agent gates remain open.

Tracked gates: [execution #41](https://github.com/oppenxyz/oppen/issues/41),
[supervision #42](https://github.com/oppenxyz/oppen/issues/42),
[recovery #43](https://github.com/oppenxyz/oppen/issues/43).

Status as of 2026-09-07: P0 and the P1 code are on `main`; the P1 gate is waiting on a funded testnet agent wallet. P2, P3, P4 and P6 code has been landing since 2026-09-04 (PRs #9–#34). **The P2 and P3 boxes were audited against the code on 2026-09-07** and are now accurate: the ws pool, the hash-chained ledger, the event taxonomy, the keychain, the per-agent guardrails, the loss breaker, the kill switch and invariant 1's property test are ticked with the module that implements each. Four bullets are deliberately still open and say what landed and what did not — the rate-budget manager (no batching), the container registry (no venue or container-kind keying), the guardrail-config HMAC (the key exists, nothing uses it) and the dead-man's switch (no daily trigger budget). **Spec F's v1 quant layer is complete** (PRs #28–#32), and the vol-scaled cap's σ-staleness caveat is closed down to a five-minute cache residue (B1–B4). Six feature specs are written, two more were commissioned by the 2026-09-04 venue audit ([onboarding.md](docs/specs/onboarding.md), [venue-containers.md](docs/specs/venue-containers.md)), and one hundred and twenty-nine product decisions are recorded in [docs/decisions.md](docs/decisions.md).

**D1 was revised on 2026-09-04.** The unit of isolation is one venue *account* per agent — a sub-account where the venue grants one, a top-level account where it does not. Hyperliquid gates sub-accounts behind $100,000 of protocol-enforced traded volume, on testnet as well as mainnet, so v1 provisions one top-level account per agent. Where this roadmap used "sub-account" to mean the unit of isolation, it now says **container**; where it names the venue's own `subAccounts` endpoint, it still means a sub-account. Reasoning: [decisions.md](docs/decisions.md) V1–V6.

---

## v1 · MVP — Hyperliquid only, agents via MCP, human supervises

### P0 · Scaffold — done

- [x] Rust workspace: `oppen-hl`, `oppen-core`, `oppen-mcp`, Tauri 2 desktop app with the Vue 3 console shell [1]
- [x] CI: fmt, clippy `-D warnings`, tests, `cargo deny`, web build; actions pinned by SHA, `--ignore-scripts` [1]
- [x] Open core (Apache-2.0 `crates/*`, commercial `apps/desktop`) + CLA gate, `NOTICE`, `AGENTS.md`, `skills/oppen` stub [23, L1–L7]
- [x] Testnet-default `Network` type, every network constant selected by it [13, D4]
- [x] Design system in `docs/design/`, ASCII primitives and the shared 90 ms motion clock in the app shell [36]
- [x] Ink ladder raised so labels and rules clear WCAG AA
- **Gate:** app launches; organization CI restored and green in merged PR #40.

### P1 · Hyperliquid protocol crate — code merged, gate pending

- [x] L1 signing: msgpack action hash, phantom agent, k256 signer, 40 official SDK vectors, mutation-tested [6] — PR #1
- [x] `WireFloat` normalization so a trailing zero can never reach the hash [6] — PR #1
- [x] EIP-712 typed data for the WalletConnect ceremony: `approveAgent`, `approveBuilderFee` [D5] — PR #1
- [x] Exchange envelope + per-signer nonce allocator [7] — PR #1
- [x] Info client: meta, asset contexts, book with `nSigFigs`, candles, mids, predicted fundings, clearinghouse state, open orders, fills by time, order status by cloid, rate limit, sub-accounts [11] — PR #2
- [x] Exchange client and typed status parsing (resting / filled / error / success) [12] — PR #2
- [x] Universe + asset validation: 5 significant figures, `6 − szDecimals`, `szDecimals`, $10 minimum notional, slippage price [8] — PR #2; `slippage_price_bounded`, which rounds toward the mid so a price *at* a slippage limit cannot be refused *for* it — PR #17
- [x] `OrderSpec` → validated `OrderWire` [8, 12] — PR #2
- [x] `examples/testnet_order.rs`: place ALO, confirm by cloid, cancel by cloid, `expiresAfter` probe — PR #2
- [ ] Rate-budget manager: batch orders/cancels, reserve headroom for risk-reducing actions, throttle before the venue does [10] — **two of three landed** with the guardrail engine: `GlobalRateBudget` reserves headroom (`spend_global` refuses an order once the remainder would fall to the reserve, while a cancel still goes through) and throttles before the venue does. **Batching has not**, and is the same blocker as item 12's batched actions below
- [ ] Testnet answers to the open questions in `docs/hl-signing.md`: `expiresAfter` encoding, accepted `signatureChainId` values, agent-wallet no-withdraw scope
- **Gate:** signed testnet order via the CLI (needs a funded testnet agent key)

### P2 · WS pool, reconcile, event ledger

- [x] Shared socket pool per network, client pings, per-IP caps [9] — `oppen-hl::ws`: `WsPool` with `max_connections`, `MAX_SUBSCRIPTIONS_PER_IP`, client pings and black-holed-socket detection
- [x] Subscriptions: book, trades, candles, mark/oracle, user fills, order updates, funding context [9, 11] — all seven, as `l2Book`, `trades`, `candle`, `activeAssetCtx` (mark, oracle **and** funding in one frame), `userFills`, `orderUpdates`, plus `bbo` for the microprice (fair-value.md §14.4 correction 4)
- [x] Reconnect state machine: a drop un-reconciles, a reconnect asks for the gap, and only a returned reconcile clears the flag; live fills recorded on arrival and deduped by `tid` [9] — `oppen-core::feed`
- [x] Pumping a live `WsPool` into that session, and the backfill calls it asks for [9] — `oppen-core::feed::pump`, [decisions.md](docs/decisions.md) F1–F4. A drop now writes a `feed_gaps` row per subscription and a reconnect closes it, so `reconcile.rs` finally works a table something other than its own tests fills. Still driven by a fixture rather than a live socket in tests; the gate below wants a real 30 s outage
- [x] Append-only, hash-chained SQLite ledger: the one source for `get_events`, the activity stream and the audit export [D6, 29] — `oppen-core::ledger`, with `verify`, anchors, redaction tombstones and CSV/JSONL export
- [x] Event taxonomy: fills, order transitions, rejections, guardrail trips, approvals, kill-switch changes, wallet expiry warnings, WS state, alerts [18] — all nine, plus `OrderIntent`, `AgentDecision`, `OperatorAction` and `PayloadRedacted`
- [x] Keys in the OS keychain, read only from `oppen-hl`; keychain hand-off zeroizes the hex string [2] — `oppen-core::keys`: `KeychainKeyStore`, agent records, rotation, permanent address retirement, D-b expiry states, and `SecretText`/`HmacKey` zeroizing on drop
- [ ] Container registry: one venue account per agent — a **top-level Hyperliquid account** in v1, keyed by venue, address and container kind so V5's sub-account upgrade needs no migration of ledger rows — plus the `manual · external` bucket for outside fills [D1 as revised, decisions.md V1–V2, V5] — **the store and the bucket landed**, as `ledger::SubAccount` (address, owner, `recorded`, `provisioned_by_oppen`) and `reconcile`'s `external` / `manual` attribution. **The keying has not**: there is no `venue` or container-kind column, which is precisely the part that exists so V5's upgrade needs no migration — [venue-containers.md](docs/specs/venue-containers.md)
- **Gate:** zero fills lost across a 30 s disconnect — **held against a fixture, not a socket.** `no_fill_is_lost_across_a_disconnect` drives a real outage through the feed session and the ledger, but the venue is a test double; the gate wants a live 30 s drop

### P3 · Guardrails, kill switch, dead-man

- [x] Per-agent guardrails: symbol allowlist, max position, notional cap, order-rate cap, reduce-only mode, max slippage, leverage cap; leverage and margin mode operator-set [24, D3] — all eight on `AgentGuardrails`, with D-c's near-zero defaults, plus spec F's `max_risk_usd` vol-scaled cap (N1–N5), whose σ is corrected by the last hour's realised vol so a twenty-four-bar statistic cannot leave the cap wide through a regime change (B1–B4)
- [x] Guardrail config HMAC-checked with a keychain key [3] — complete authenticated snapshots, explicit reviewed migration, deleted-history refusal and final signing revision checks are in PR #55 (ES18), with green exact-head CI and separate automated review. Merge awaits approval of automatic private-update publication. Key provisioning and desktop runtime activation are not automatic; see [policy-authority.md](docs/specs/policy-authority.md).
- [x] Loss circuit breaker: max daily loss / drawdown per agent and account-wide trips the kill switch [25] — `guardrail::breaker`, exhausted **at** the limit rather than past it, plus spec F's continuous gauge over the same predicate (L1–L4)
- [ ] Kill switch per agent and global: pauses new orders, cancels resting, persists across restart, typed `trading_paused` [26] — core pause predicates exist; runtime cancellation delivery/retry and restart behavior must pass the assembled gate before this is complete
- [ ] Dead-man's switch: `scheduleCancel` armed while any agent is active; quit dialog with cancel-all when positions are open [27] — **it is a daily budget, not a standing net**: minimum 5 s ahead, maximum 10 triggers per day resetting 00:00 UTC, so the arming policy is deliberate and the remaining count is shown in the risk console [decisions.md O1] — **the lead-time half landed** in `guardrail::deadman` (`DEAD_MAN_MIN_LEAD_MS`, a 60 s arm refreshed at 20 s remaining) and `clear_schedule_cancel` clears it. **Confirmed coverage and daily accounting have not**: the helper is not wired to a supervised scheduler. The 2026-09-08 official documentation reread clarifies that scheduled firings, not ordinary refreshes, consume the daily count. Persist confirmed and unknown outcomes per account, preserve uncertainty across restart and UTC boundaries, and verify venue acceptance before claiming protection; see [signing reference §10](docs/hl-signing.md#10-schedulecancel--the-dead-mans-switch-and-its-daily-budget)
- [x] Property test proving there is no signer path without a guardrail check [invariant 1] — `no_input_produces_a_signable_value_without_passing_every_predicate`, 20,000 fuzzed cases with a vacuity guard, each cleared case re-derived predicate by predicate in `verify_every_predicate`
- **Gate:** no signer path without a guardrail check — **met.** `no_input_produces_a_signable_value_without_passing_every_predicate` searches 20,000 fuzzed configurations and re-derives every predicate on each cleared case; `sign_cleared` takes a `Cleared` whose only constructor is the success branch of `decide`

### P4 · MCP gateway

- [x] Streamable HTTP on loopback only: `Origin`/`Host` validation, bearer on every request, constant-time compare, revocation closes live sessions [14, D2] — PR #11, #12. The on/off toggle is an operator surface and waits on P5
- [x] Default-deny pairing in the crates: a token binds one named agent to one container, and every tool resolves its identity from the token presented [15] — [decisions.md](docs/decisions.md) C9–C10
- [ ] The approve dialog itself, and assigning guardrails from it [15] — operator surface, waits on P5
- [x] `get_state`: versioned deterministic envelope, staleness flags, positions with liq distance, orders, balances [16] — PR #13. Time-since-last-action, funding, guardrail utilization, pending proposals, kill state and rate budget are not in the envelope yet
- [x] `get_meta` [17] — PR #12
- [x] `get_events(since_cursor)` with `resync_required` [18] — scoped to the calling agent, [decisions.md](docs/decisions.md) C6
- [x] `place`, `cancel`, `cancel_all`, `close_position` with required `reason`; synchronous result contract; `get_order_status(cloid|oid)` [19] — PR #14, #17
- [x] Typed error taxonomy with retryability: `guardrail_reject`, `venue_reject{…}`, `venue_error`, `rate_limited`, `timeout_unknown_outcome`, `trading_paused`, `pending_approval` [19] — PR #17, [decisions.md](docs/decisions.md) C1–C5. `auth_expired` is omitted until something constructs it: the door refuses an unpaired agent before a tool runs, and wallet expiry is a P2 item
- [x] `preflight(order)`: margin, live book walk, guardrail verdict, post-fill exposure, `max_size_usd_within_{5,10,25}bps` [20]
- [ ] `preflight` estimated fees [20] — needs a `userFees` read audited against the live API; a guessed fee tier is worse than an absent one
- [x] `remember` / `recall` journal [21]
- [x] `set_alert(condition)`, with `get_alerts` and `cancel_alert` [22] — `oppen-core::alert`, [decisions.md](docs/decisions.md) G1–G6. Price cross, fill and funding rate; liquidation distance and feature thresholds deferred with named blockers, and the OS-notification half is P5
- [x] Market = slippage-bounded IOC, limit GTC/IOC/ALO, stop-market, reduce-only, cloid on everything [12]
- [ ] Attached TP/SL with `positionTpsl`, and batched actions [12] — several orders in one action, and the engine clears one intent at a time; a batch that partially clears must not partially send
- [ ] Builder code attached by default via `OPPEN_BUILDER_ADDRESS`; missing approval prompts the ceremony, never drops the order path [5, D7]
- **Gate:** `claude mcp add` → paired → guarded testnet order. This is the demoable loop.

### P5 · Operator console

The gate was cut from "parity with the design" to the named list below on
2026-09-03 — see [decisions.md](docs/decisions.md) P1. Parity is a judgement, not
a test, and an ungated judgement resolves as schedule drift.

**In the gate:**

- [ ] **TRADE realtime correctness follow-up** [30, 31, 34] - the
  local, unpublished 2026-09-08 TRADE audit found stale derived
  spread, missing explicit bid/ask, absent-side retention, REST/WS ordering
  gaps, bootstrap failure recovery gaps, and coarse channel health. Existing
  live-feed checkmarks below describe delivered plumbing, not completion of
  these newly reproduced defects. Repair quote/chart correctness first, then
  expand aggregate-market and account subscriptions with separate durable
  accounting review. Public testnet sampling is not a live trading gate.
  Quote remediation is implemented locally on `fix/trade-live-quotes`: separate
  touch/depth observations from REST-derived features, clear missing sides,
  fence late L2/REST, recover live books without REST bootstrap, and show explicit
  bid/ask/latest-observed spread with advancing observation ages. All 228 frontend
  tests, production build and QA typecheck passed. Browser replays proved live
  quote bootstrap despite REST failure, missing-side clearing, independent depth
  ordering, identical reconnect observations and age unaffected by context ticks.
  Screenshots were inspected at 1440px and the supported 1280px minimum. Aggregate
  independent automated working-tree and exact-head review found no blockers in
  local commit `55657ad29a80058bfb20ac13e8b87d2d13b4ec03`; this is not human
  approval. Venue and host timestamps stay separate; quiet change-driven BBO
  is not a disconnect signal. No Rust, execution or account behavior changed.
  Delayed same-generation/same-symbol WS A-B-A frames remain indistinguishable
  under the current native event contract. Chart/health/subscription remediation,
  public-feed and installed-artifact verification remain open.
  Chart remediation is implemented locally on `fix/trade-live-chart`. The
  [source-separated chart contract](docs/specs/live-chart-observations.md), local
  commit `743c20b`, passed independent automated design review. It replaces
  optimistic venue/tape blending with Rust-owned venue-preferred bars and a
  separate trade marker, bounded deduplication, and exact selection/history
  ownership. A fresh chart-only receiver incarnation addresses queued A-B-A
  frames without replacing account supervision. Combined Rust validation passed
  1,282 tests with zero failures and 15 ignored, including all 119 native tests.
  All 233 frontend tests, production build, QA typecheck, strict all-target
  workspace Clippy and formatting passed.
  Loopback regressions cover stale wire incarnations, retained drain, quiet
  rollover without false freshness, emission/consumer failure and chart-only
  refusal/retry without stopping account supervision. A reproduced shutdown race
  now preserves Stopping while actual account drain remains held. Host-clock
  rollback cannot reopen elapsed bars; chart projections cannot restore shared
  market health. Independent automated working-tree review found no remaining
  blockers. Browser replays and 1440px/1280px screenshots prove mock UI behavior,
  not installed or live acceptance. Remote CI, public-feed and installed-artifact
  verification remain pending; no account or publication action was performed.
  Local commit `47421844a6419c81ad1774ae3b21b0adaa009456` received independent
  automated exact-head review with no blocking findings; this is not human
  approval. Coarse per-channel health, broader subscription coverage and quote
  wire-incarnation follow-up remain open in the audit.
  Per-channel health work is now on `fix/trade-channel-health`, following the
  [console channel health contract](docs/specs/console-channel-health.md).
  A fresh production-shell diagnostic replay reproduced three defects with nine
  assertions: reconnect-without-data remains OK, quarantine/context traffic masks
  channel problems, and mixed venue/host timestamps poison the aggregate clock.
  Passing this replay proves the baseline defects, not their correction. Reuse
  native registry facts through nonblocking, selection-bound status observations;
  retain independent five-second status freshness, channel age budgets and quote
  validity. No signing threshold or subscription has changed. The local
  implementation now exposes five expected channels, nonblocking pool samples,
  selection-bound publication, independent age budgets, retained scoped losses
  and terminal consumer failures. Independent automated aggregate source review
  found no remaining blockers. Frontend: 240 tests passed; production build and
  QA typecheck passed. Browser replay and inspected 1280px/1440px screenshots
  verified null-poll expiry, selection rejection, inert retained diagnostics and
  quote/health separation. These synthetic checks are not installed or live
  acceptance. Full-workspace Rust validation passed with 1,291 tests and 15
  intentionally ignored live/environment gates. The first sandbox-only attempt
  failed on denied loopback fixture binds; the permitted local-fixture rerun
  passed. Strict workspace lint, formatting and diff checks passed. Local commit
  `4b23774383cfd3a36a714755ad970ebd651d8ede` received independent automated
  exact-head review with no blocking findings; remote CI and installed/live
  verification remain pending. No publication or account action was performed;
  the broader subscription inventory is in the contract.
  Contract commit `e101cd9eb7fd5f58a9a731369fc3947319969dba` passed separate
  automated design review with no blocking gaps. This is implementation guidance,
  not a live gate.
  The next correction is on `fix/selected-market-incarnation`: production
  controller replay reproduced delayed first-BTC quote/context acceptance after
  BTC -> ETH -> BTC (one diagnostic test, three assertions). Contract `abcc1e7`
  in [selected market incarnation](docs/specs/selected-market-incarnation.md)
  received independent design review. Reuse the existing selected socket for
  all five public streams with immutable producer binding, preserve the account
  owner, and separate applied account observations from market/reconnect ticks.
  Implementation is complete locally; the original diagnostic proves the defect,
  while the subsequent regressions exercise the correction.
  Frontend validation now passes 246 tests, build and QA typecheck. Browser
  replay through the actual frontend listener rejects retired quote/context/book,
  status and failure payloads, missing scope and interval round trips. Account
  status leaves public quotes unchanged; selected failure is recoverable without
  clearing an account failure. Inspected 1280px/1440px screenshots preserve quote
  clocks and position visibility, including expanded inert account diagnostics.
  The prior channel-health browser replay also passes under selected ownership.
  Independent source review cleared frontend and native findings, including the
  missed-first-ack account failure and canceled-drain failure-scope cases. Native
  validation passed all 132 tests. Resumed full-workspace verification passed
  1,298 Rust tests with zero failures and 15 intentionally ignored live/environment
  gates; strict workspace lint, formatting, diff checks, 246 frontend tests,
  production build, QA typecheck and three release-script tests passed.
  Dependency checks passed using the cached advisory database (`--offline`).
  Independent exact-head review of `effafee39a2305bd714d101e820f5b590ca5cc73`
  found no issues. PR #91 is open; installed, remote-CI and live acceptance
  remain pending. No account activity has occurred.

- [x] **Market data in the console** [30, 31] — [decisions.md](docs/decisions.md) Z1–Z4. `oppen-core::market` projects the rail and a per-symbol snapshot; TradeView's Markets, strip, Book and Features panels read them. A bookless asset is dimmed rather than hidden and a market with no mid keeps its row, because `allMids` answers for exactly those with a frozen print (H1). `micro_tilt_bps` is still absent, but no longer for the reason given here: the console now subscribes `bbo` (see the live feed line below), so the input exists and only the computation is unbuilt
- [ ] Activity stream with rejection explainability: guardrail versus venue versus auth, attempted versus limit, inline link to edit [31]
- [ ] Agents / Control: roster with real PnL, last-seen, idle-with-open-position alert, guardrail utilization; policy panel; approvals queue; risk console with exposure, rate budget, feed health, kill switches [32]
- [ ] Manual escape hatch drawer; manual actions land as `manual · external` [33]
- [x] **Initial staleness display** [34] — [decisions.md](docs/decisions.md) W1, W4. The original aggregate indicator is superseded locally by per-channel native observations in `4b23774`; delivery and live gates remain open above. The selected-market incarnation follow-up also corrects desktop account-observation semantics. Neither display is proof of reconciliation or signing eligibility; those remain owned by the guarded runtime.
- [ ] Persistent MAINNET/TESTNET badge; boot sequence bound to real state [36, D4]
- [ ] Agent `reason` strings rendered as inert plain text, labelled agent-authored [30]
- [x] **ASCII candle renderer** promoted from the marketing site: real X and Y axes on nice numbers at the asset's own precision, `--up` / `--down` colour with the glyph as a redundant channel — [charts.md](docs/specs/charts.md) §2, [decisions.md](docs/decisions.md) E1–E4. `oppen-core::market::chart` splits the venue's rows into closed buckets and the one still forming; `CandleChart.vue` measures its grid and maps the renderer's ink codes to tokens. **Native intervals only** — `1m 5m 15m 1h 4h 1d`; the resampler exists in `oppen-core::candles` and nothing calls it yet, which is the v1.5 arbitrary-intervals line
- [x] **Initial console live feed** [31, 34] — [decisions.md](docs/decisions.md) W1–W6. Five selected-symbol channels remain: `activeAssetCtx`, `bbo`, `l2Book`, `trades` and `candle`. Local chart correction `4742184` keeps venue candles separate from partial observed-trade bars and an independent latest-trade marker. The rail still polls every 10 seconds; aggregate venue channels exist but are not adopted. Earlier public-feed tests and short measurements do not establish current installed behavior or a cadence guarantee. The selected transport and broader-subscription acceptance work remains open above.
- **Gate:** every surface above renders correctly, and the stale overlay appears on socket loss

**Deferred to v1.1:**

- [ ] Agent fill marks on the chart, with reasons shown in the stream rather than in the plot [31, decisions.md P4]
- [ ] Follow-agent toggle
- [ ] OS notifications by severity [35]
- [ ] Designed empty state for every panel [4]
- [ ] Full parity with `docs/design/`

### P6 · Quant features — features, not signals

- [x] `get_features(symbol)`: `spread_bps`, `depth_usd_{bid,ask}_{10,25,50}bps`, `book_imbalance`, `micro_tilt_bps`; funding pack (`funding_apr_pct`, predicted, `next_funding_s`, `basis_bps`); vol pack (EWMA-Parkinson `rv_1h_bps`, `rv_24h_bps`, `vol_ratio`) — `oppen-core::features`, [decisions.md](docs/decisions.md) J1–J4. Each depth band carries whether the venue's ladder actually reached it (§14.5)
- [x] Position risk in σ-units: `liq_distance_sigma`, `margin_runway_h`, `carry_usd_per_day` in every snapshot — [decisions.md](docs/decisions.md) K1–K4. Filled by `get_state`, never by the signing path; `margin_runway_h` is account-level because free margin is shared
- [x] Loss-budget utilization % as a continuous gauge before the breaker — [decisions.md](docs/decisions.md) L1–L4. The breaker's own predicate read as a dial, both budget kinds and both scopes, in `get_state`; the same pass gave `Utilization` the `drawdown_pct` the breaker had always fired on and the block had never reported
- [x] Vol-scaled notional cap guardrail option: `effective_cap = risk_budget / (2σ_day)` — [decisions.md](docs/decisions.md) N1–N5. `risk.max_risk_usd` in dollars, off by default and only ever tightening the fixed cap; σ rides on `MarketRef` so the engine stays a pure function of its inputs, and a cap that cannot be computed refuses
- [x] TCA foundation: `arrival_mid` on every order (it was already the clearance's `reference_px`), `slip_bps` per fill stamped against it, `get_execution_report` with `n=` and a maker/taker baseline on every stat — [decisions.md](docs/decisions.md) Q1–Q5. PnL decomposition is price and fees; **funding is not**, and needs a `userFunding` read audited against the live API
- **Gate:** cross-checked against hand computation

### P7 · Approval mode, skill, release

- [ ] Approval mode, built last: `pending_approval` with TTL, re-priced at approval time with drift shown, typed approved/rejected/expired events, visible in `get_state` [28]
- [x] **The console can see the keychain** [2, 4] — [decisions.md](docs/decisions.md) X1–X5. `KeyStore::reachable`, a read-only probe, behind one Tauri command: whether the store answers, never what is in it. Turns the tracker's first milestone into a real check and makes a locked keychain visible instead of surfacing as an unrelated failure three steps later
- [x] **Guided walkthrough and setup tracker** [4] — [decisions.md](docs/decisions.md) W1–W4. A spotlight pass over every screen that points at the live controls and names what each is for, plus a milestone tracker toward a first paired agent. The tracker reports a step it cannot check as **unverifiable** with the missing read named, never as merely pending, and counts only the checkable ones
- [ ] First-run onboarding: testnet default → one container + agent wallet per agent (`usdSend` to fund it, `approveAgent` to authorise the agent wallet, `approveBuilderFee` for the builder code — every signature in the user's own wallet, never a container key in the app) → pair first agent with the `claude mcp add` snippet and a connection test [4, D5, decisions.md V2, O7] — [onboarding.md](docs/specs/onboarding.md)
- [ ] **MVP Connect Agent and broad MCP-client setup**, after the current safety work: in-app account/permission assignment, read-only connection test, factual status and durable revocation; an initial verified client matrix, generic compatible-client configuration and external local-model runners. Compatible is not verified; all clients retain identical server-side safeguards. See [agent-connections.md](docs/specs/agent-connections.md).
- [ ] **Beta client expansion:** more verified clients, permission-based one-click configuration, and separately security-reviewed connectivity for cloud clients such as ChatGPT. No automatic public endpoint, tunnel or relay; existing approval gates remain unchanged. See [agent-connections.md](docs/specs/agent-connections.md).
- [ ] Sub-account path offered as an attempt, never as a precondition: `userRateLimit` returns `cumVlm`, so oppen may show distance to the gate as a labelled estimate, but nothing documents that the gate reads that counter or whether it is lifetime or windowed — so oppen still tries `createSubAccount` and classifies the refusal, and `Required:` / `Traded:` are displayed, never branched on [decisions.md V5, O3]
- [ ] `AGENTS.md` and the `skills/oppen` Claude Code skill written for real [23]
- [ ] Threat model finalized against the shipped code [2]
- [ ] Release builds: ad-hoc dmg + unsigned AppImage first; provenance attestations and checksums [1]
- **Gate:** fresh machine to a testnet trade in 10 minutes — measured from a wallet that already holds testnet USDC. The faucet pays 1,000 mock USDC only to an address that has previously deposited on **mainnet**, which is outside oppen and outside the ten minutes [decisions.md O2]

### Cut from v1 — decided, do not re-add

`get_chart_image` · attention tools (`focus_symbol`, `open_panel`) · stdio transport · agent-writable leverage / margin mode · HIP-3 dexes · `modify` tool and stop-limit · flatten-all coupled to the kill switch · in-app agent runtimes and replay · the 3-venue router from the design mock.

---

## Agent harness direction

Owner direction (2026-09-08): Oppen is a trading-specific data, controlled
execution and durable supervision layer for external agent harnesses. Bring
your agent; keep the model and its orchestration replaceable. Do not prioritize
a competing general-purpose harness, proprietary model hosting or elaborate
multi-agent orchestration ahead of reliable execution and onboarding.

- **MVP/beta:** deliver the existing Connect Agent plan for external harnesses and local-model runners. Preserve compatible-versus-verified support and identical Rust-enforced account permissions, budgets, exposure limits and approvals. MCP connectivity is not host isolation; cloud connectivity retains its separate security review.
- **Shared foundation:** expose versioned, typed observations with units, timestamps, provenance and explicit stale/missing/uncertain states. Use the existing append-only ledger for action receipts, outstanding liabilities and recovery evidence; model context is never the source of truth for orders or budgets. Operator review, activation, HALT and revocation remain outside agent authority.
- **v1.1 evaluation foundation:** extend developer scenario/replay tests for harness integration, measuring duplicate actions, stale-data use, recovery correctness and execution latency. Measure model cost when model calls are actually observable; otherwise mark it unavailable. These are reliability evaluations, not evidence of profitable strategies or substitutes for live acceptance.
- **v1.5 Quantoppen and workflows:** at that milestone, plan an agent-consumable contract covering typed market/risk observations, uncertainty, event subscriptions and condition-triggered wakeups, constrained proposals/actions, and execution/outcome evidence. Reuse the feature, alert, TCA and workflow infrastructure rather than introducing a parallel tool surface or agent loop. Detailed Quantoppen design remains deferred until its scheduled work begins.
- **Optional in-app harness:** retain the planned v1.5 BYO-model runtime and later workflow capabilities behind the same execution path. External harness support must not require adopting Oppen's own runtime. Customer-facing replay remains outside MVP; developer regression fixtures do not imply that product has shipped.
- **Engineering practice:** use reproducible environments, bounded implementation tasks, independent review and verified completion when agents build Oppen. Track repeated failures and recovery outcomes; internal coding-harness improvements are not customer-facing feature completion.

This direction does not move work ahead of the current safety gates or the
v1.2 Lighter/Aster integrations. Harness-facing contracts must preserve their
meaning and safeguards across clients, models and venues; none grants new
authority to sign, change policy or bypass execution checks.

## Specified but not scheduled

These have written specs in [docs/specs/](docs/specs/). Most slot into the
versions below; the two written on 2026-09-04 are already scheduled inside v1 and
are listed here so the index is complete. A spec is not a commitment; each carries
open decisions that need an answer before it starts.

| Spec | Feature | Target |
|---|---|---|
| [onboarding.md](docs/specs/onboarding.md) | First-run ceremony: container per agent, agent wallet, builder fee, pairing | v1 · P7 |
| [venue-containers.md](docs/specs/venue-containers.md) | The container model and what Hyperliquid, Aster and Lighter each grant | v1 · P2 (model) / v1.2 (Aster, Lighter) |
| [workflows.md](docs/specs/workflows.md) | Trading workflows, triggers, schedulers, agent profiles | v1.5 / v2 |
| [history.md](docs/specs/history.md) | Durable trading history in the Portfolio tab | v1.1 |
| [charts.md](docs/specs/charts.md) | ASCII candles with real axes, arbitrary intervals, line chart, Quantoppen | v1 / v1.1 |
| [fair-value.md](docs/specs/fair-value.md) | Fair value engine: carry, basis, micro. Mark consumed not replicated (§14) | v1.5 |
| [signals.md](docs/specs/signals.md) | Opt-in signal publishing and the community layer | v2 |
| [mobile.md](docs/specs/mobile.md) | Read-only mobile companion with a panic button | v2 |

---

## v1.1 · Hardening and reach

- [ ] HIP-3 dexes: per-dex meta, `100000 + dex × 10000 + index` asset ids, `dex:coin` names, thin-book guardrails
- [ ] Attention tools behind a human-owned follow toggle
- [ ] `get_chart_image` as a human-shareable artifact, not an agent input
- [ ] stdio shim for MCP clients that cannot speak HTTP
- [ ] Phone push: ntfy / Telegram for the severity-tiered notifications
- [ ] `modify` tool and stop-limit orders
- [ ] Headless / tray mode
- [ ] Notarized macOS build, msi, full code-signing; auto-updater with an offline signing key
- [ ] **Durable trading history** in the Portfolio tab: backfill, gap detection, closed-position lifecycles, PnL split into price / funding / fees, CSV and JSONL export — [history.md](docs/specs/history.md)
- [ ] **Arbitrary chart intervals**: native, exact resampling, and forward-only local aggregation for sub-minute — [charts.md](docs/specs/charts.md) §3
- [ ] **Line chart mode** and the **Quantoppen** multi-asset watch grid — [charts.md](docs/specs/charts.md) §4–5

## v1.2 · Lighter and Aster integrations

Owner reprioritization (2026-09-08): bring both venues forward from v2+ to
before v1.5. Keep the Hyperliquid supervised-alpha live acceptance, onboarding,
recovery and v1.1 hardening gates first. Deliver one venue at a time; integration
order remains to be selected. This is a sequencing commitment, not a calendar
date or authorization for live activity, funding, signatures or credentials.

- [ ] Complete venue-aware account identity and accounting before enabling another execution adapter. Preserve existing ledger history, cumulative limits and separate venue margin pools; positions across venues never net for exposure checks.
- [ ] **Lighter integration:** read-only market/account feeds and desktop/MCP visibility first, then reviewed onboarding and supervised execution through the shared Rust safeguards — [venue-containers.md](docs/specs/venue-containers.md).
- [ ] **Aster integration:** read-only market/account feeds and desktop/MCP visibility first, then reviewed onboarding and supervised execution through the shared Rust safeguards — [venue-containers.md](docs/specs/venue-containers.md).
- **Gate per venue:** verify current API, account and signer capabilities; independent adapter/signing review; green CI; explicitly authorized test-environment acceptance covering orders, fills, cancellation, disconnects and physical restart without duplicate orders, accounting resets or silent activation. Mock-only success does not complete integration. If a suitable test environment is unavailable, pause execution acceptance rather than substituting mainnet.

Cross-venue aggregation/routing remains v2+; v1.5 quant and workflow expansion
follows these integrations.

## v1.5 · Quant depth and the fleet

- [ ] `*_pctile_7d` self-normalization on every feature (gives the LLM the baseline it lacks)
- [ ] `tape_intensity_z` as a `set_alert` wakeup
- [ ] OI × price regime enum (`longs_opening`, `shorts_covering`, …) with raw deltas attached
- [ ] `suggest_size`: stop-based, vol-target, quarter-Kelly with the agent-declared edge logged for calibration grading
- [ ] Fleet crowding across containers and a `FLEET_CAP` guardrail
- [ ] Markout curves; implementation shortfall anchored on a `preflight` `snapshot_id`
- [ ] BYO-model runtime: a model loop hosted in-app, still behind the same guardrail path
- [ ] **Fair value engine**: mark replication, funding dead-zone censoring, min-variance component combination, `basis_bp` / `z` / `z_sigma`, and the five `fair_value.*` MCP tools — [fair-value.md](docs/specs/fair-value.md)
- [ ] **Workflow engine, layer one**: triggers, cron scheduler, conditions, loops, approval gates, `await_agent` nodes for external agents, run state on the existing ledger — [workflows.md](docs/specs/workflows.md) §4.1
- [ ] **Workflow templates**: `funding-carry`, `basis-dislocation`, `vol-regime`, `position-guardian`, `research-only`, `custom` — structure only, no alpha — [workflows.md](docs/specs/workflows.md) §10
- [ ] **Agent-consumable Quantoppen contract:** typed observations, event-driven wakeups, constrained actions and outcome evidence, as scoped in [Agent harness direction](#agent-harness-direction); detailed planning begins with this milestone.

## v2+ · Cross-venue routing, strategies, scripts

- [ ] **Cross-venue aggregation and routing.** Positions on different venues never net: three venues is three margin pools and three liquidation prices, an economically flat book posts full margin on both legs, and one leg can liquidate while the other survives. Aggregate exposure is an oppen-enforced guardrail with no venue behind it, and is labelled as containment rather than a boundary [decisions.md V6]
- [ ] TWAP and scale orders
- [ ] Backtesting and strategy templates
- [ ] Portfolio analytics
- [ ] Vaults and spot
- [ ] Script runtime with sandbox, dry-run and replay
- [ ] **Workflow engine, layer two**: in-app agent nodes running unattended, same guardrail path — [workflows.md](docs/specs/workflows.md) §4.2
- [ ] **Opt-in signal publishing** and the community layer on the website, with no path from subscribed content to the signer — [signals.md](docs/specs/signals.md)
- [ ] **Mobile companion**: read-only from public venue state, plus a cancel-only panic wallet — [mobile.md](docs/specs/mobile.md)

## Rejected — not on any version

TA-indicator zoo · GARCH / ML vol · auto-Kelly from small samples · VPIN · ungated Sharpe (every stat carries `n` and a standard error) · raw-PnL leaderboards fed to agents · master-key paste box · guardrails in TypeScript, the MCP layer or a prompt.
