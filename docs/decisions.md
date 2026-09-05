# Decision log

Product and scope decisions taken outside the settled architecture set. D1–D8 in
[spec.md](spec.md) are the architecture decisions and are not re-litigated here;
this file records everything else, with the reasoning, so a later reader can tell
what was chosen deliberately from what was never considered.

Format: one row per decision, newest section first. A decision is only in this
file once it has been taken. Open questions live in the relevant spec's "Open
decisions" section until then.

---

## 2026-09-05 · The MCP result contract

Spec item 19 names the statuses, the error taxonomy and the retryability rule, and
leaves four things to whoever implements it. C1–C5 are those, plus how a market
close is priced; C6 is the one question item 18 leaves open, C7–C8 the two
item 20 leaves open, and C9–C10 the two item 15 leaves open. The mapping from
`oppen_core::guardrail::Refusal` onto the wire taxonomy is otherwise the one that
enum's own documentation already states, and is unchanged.

| # | Decision | Choice | Why |
|---|---|---|---|
| C1 | Where a rejection lives on the wire | **A refusal is a successful tool call carrying `status: "rejected"`; only a failure to answer is a protocol error** | Item 19 lists `guardrail_reject` alongside `rate_limited` and `timeout_unknown_outcome` in one "error taxonomy", but the two halves are different events. A guardrail refusal is the system working: the agent asked for something outside a limit and got a typed answer naming the predicate, the observed value and the limit. Returning that as a JSON-RPC error puts it in the same bucket as "the venue is down", and the standard response to an error is a retry — which is exactly the wrong response to a cap. So the split is by *what happened*, not by *whether it succeeded*: a refusal, a fill, a rest and a pending approval are statuses; a venue that refused, a request whose outcome is unknown, an unreadable feed and a bad parameter are errors. Both halves are typed and both carry `retryable`, so nothing is lost by the split. **What this gives up:** an agent that only checks for a protocol error sees a refusal as a success and must read `status` to notice. That is the trade, and it is the right way round — the failure mode of the alternative is a retry storm against a limit that will never pass. |
| C2 | The two rate refusals | **`rate_limited`, not `guardrail_reject`** | Refines the mapping in `Refusal`'s own doc comment, which put everything except `VenueRule`, `TradingPaused` and `ApprovalRequired` under `guardrail_reject`. `OrderRate` and `GlobalRateBudget` are the only refusals in that enum that clear without the agent changing anything, and both already carry `retry_after_ms` — the exact wait until a token refills. Leanness rule 5 asks what caller behaviour a variant produces: "back off for 10 s and re-send the identical order" and "this order will be refused identically forever" are two answers, so they are two codes. Item 19 names `rate_limited{retry_after_ms}` separately for the same reason. The wrapped refusal still travels, so an operator can still tell the per-agent cap from the address-wide budget — the fixes differ. |
| C3 | oppen's pre-sign venue catch vs. the venue's own refusal | **Two codes: `venue_reject` and `venue_error`** | Item 19 names one `venue_reject` with subtypes `min_notional`, `tick_price`, `insufficient_margin`, `reduce_only_violation`. Three of those four are rules oppen already evaluates itself before signing, as typed `VenueRule` and `Refusal` variants — so they arrive as structured subtypes and nothing parses a venue sentence to produce them, which is what `AGENTS.md`'s "venue message text is display-only" requires. The fourth, insufficient margin, is only knowable from the venue's answer. Collapsing both into one code would mean either classifying venue prose into subtypes — the thing the convention forbids — or losing the subtypes oppen legitimately has. They are also different facts about the world: `venue_reject` means nothing was sent and no nonce was spent; `venue_error` means the venue saw the request and said no. An agent behaves differently, so they are two codes. **What this gives up:** a caller matching on the single `venue_reject` item 19 names must now match two codes. |
| C4 | A status for cancels | **`canceled{requested, canceled, failed[]}`** | Item 19's result shape is order-shaped — `oid`, `filled_sz`, `avg_px` — and a cancel has none of those. Partial success is the normal case rather than an edge one, because an order that filled a moment ago cannot be cancelled and `cancel_all` sends many at once, so the failures are itemised and paired positionally with what was sent rather than collapsed into a count. An "already gone" cancel is reported here, not as an error: the caller wanted the order gone and it is gone. |
| C5 | How a market close is priced | **The operator's `max_slippage_bps`, rounded toward the mid** | Hyperliquid has no market order type: a market order is an IOC priced through the book, so `close_position` has to choose a price, and whose bound it uses is a real decision. Not the caller's — item 24 and D3 make slippage operator-set, and an agent that could widen the bound to close could widen it to open. So it is the agent's configured `max_slippage_bps`, read from the engine. That creates a second problem the obvious implementation gets wrong: `Asset::slippage_price` rounds to *nearest*, which can land up to one tick further from the mid at each of its two rounding steps, and `oppen-core` refuses when observed slippage is `>` the limit — so an order priced at exactly the operator's bound can be refused *for* that bound, on a rounding step nobody chose and with no price the caller could have asked for instead. `slippage_price_bounded` rounds toward the mid at both steps, making the returned price's adverse slippage never more than requested, so the order clears by construction rather than by luck. **What this gives up:** at most one tick of fill probability. That is the right side to be wrong on — an unfilled close is retried, a refused one cannot be priced at all. **What it does not fix:** a mid below the asset's own tick still yields no valid buy price, and is refused as `NonPositivePrice`; no rounding mode invents one, and `slippage_price` has the same edge. |
| C6 | What one agent's `get_events` may read | **Its own rows, plus the account-wide ones no agent owns** | Item 18 names the cursor and `resync_required` and says nothing about scope, and R4 puts every agent on one network on one file and one chain — so the unscoped read is the one that falls out of the schema, and it hands each agent every other agent's order intents and `reason` strings. Those reasons are the agent's own account of its edge (item 30), which makes the default a strategy leak between containers that D1 exists to separate. Scoping to `agent_id = me OR agent_id IS NULL` keeps item 18's taxonomy whole — the kill switch, feed drops and alerts belong to no agent and still reach everyone — while an agent's intents stay its own. **What this costs:** an agent can no longer see a sibling's fills, which do move shared margin on a shared address; that visibility belongs in `get_state`'s account view, which is already account-wide and carries no reason strings. **The subtlety it forces:** `next_cursor` must advance past rows that were scanned and filtered out, or an agent on a ledger full of a sibling's events sits at the same cursor forever and reports itself permanently behind `head_seq`. A short page therefore advances to the head, a full page stops at its last row, and both are tested. Operator surfaces keep the unscoped `Ledger::get_events`; the activity stream and the audit export are the human's view of the whole chain. |
| C7 | What a preflight may cost | **Nothing: no rate token, no global request, no proposal, no ledger row** | Item 20 says "without executing", and the tempting reading is that only the *order* must not be sent. But an agent charged an order-rate token to ask would stop asking, which defeats the tool — and one charged nothing while being answered by a *restatement* of the guardrails would be trusting a second copy of them that can drift from the real one. So `preflight` runs the engine's own `decide` with a third `Mode`, which changes only what answering costs: the rate bucket is refilled and read rather than taken, the global budget is not drawn, and approval is reported rather than minted. Every predicate still runs, in the same order, against the same state. **The seal it must not break:** it cannot return a `Cleared`. That type *is* the capability to sign, so a preflight that produced one would be a path to the signer that skipped the rate token — precisely the second route invariant 1 forbids. It returns a `Verdict`, which carries the numbers and no authority. **What this gives up:** a clear verdict is not a promise. The book moves, and the token preflight did not spend may be gone when the order is sent; that is inherent to asking in advance and is stated on the tool rather than papered over. |
| C8 | Estimated fees in `preflight` | **Deferred, not approximated** | Item 20 names fees and oppen has no fee schedule: `Fill.fee` records what a fill actually cost, and nothing reads the venue's tier table. The options were a hardcoded rate or an absent field. A hardcoded tier is wrong for every account on a different tier and wrong for all of them when the schedule changes, and it would be wrong *silently*, inside a number an agent sizes against — the failure mode this project already rejected for fair value, where a worse estimate of a published number was refused in favour of reading the real one. So the field is absent until a `userFees` read exists, audited against the live API the way the info types were on 2026-09-04. **What this costs:** an agent sizing near the minimum notional cannot see the fee drag, and must treat `notional_usd` as pre-fee. Named on the tool and in the contract rather than left to be discovered. |
| C9 | What item 15's "approve dialog" is a dialog *about* | **Operator-first: the operator names the agent, binds the container and assigns the guardrails, and the token is minted from that** | Item 15 says "first connection is an explicit approve dialog", which reads two ways. Agent-first — the agent connects, oppen raises a dialog, the operator approves — needs an endpoint an unpaired caller can reach, and item 14 with D2 requires a bearer token on *every* request. An unauthenticated intake route is the one hole the door exists to not have, and it would be reachable by the same browser page the `Origin`/`Host` checks are there to stop. Operator-first needs no such route: an agent oppen has not been told about has no credential to present, which is what default-deny means. It is also what P7's onboarding already describes — "pair first agent with the `claude mcp add` snippet". So the dialog is where a pairing is *created*, not where an inbound connection is answered. **What this gives up:** an agent cannot request pairing, so the operator must initiate every one; with one container per agent and a snippet to paste, that is the same click either way. |
| C10 | Where a tool learns which agent it is | **From the pairing token, per request, via the context** | The gateway held one agent and one account at construction, so every token got that identity and per-agent guardrails, containers and event scoping (C6) were decorative — one gateway could only ever serve one agent. The token already names the agent (C9), so the door resolves it once and puts the `Binding` in the request extensions; `rmcp` republishes the request's `http::request::Parts` into the tool's `RequestContext`, which is that crate's documented mechanism rather than a side channel. Tools read it there and nowhere else. **Why this is also the safer shape:** a tool cannot obtain an identity except from a request the door authenticated, so reaching a tool with no binding means the gateway was mounted without its guard — which fails closed as `unavailable` rather than defaulting to somebody. **What it costs:** `get_events`'s body is split from its tool method because `rmcp::service::Peer` is `pub(crate)`, so a `RequestContext` cannot be built in a unit test; the resolver is then one line and everything that depends on the answer is testable. |

## 2026-09-04 · Licensing

Until now the licence was asserted in `README.md` and on oppen.xyz and recorded
in no decision row, which made it the one load-bearing commitment in the project
that could be re-opened without anybody noticing it had been settled. It is
settled here.

| # | Decision | Choice | Why |
|---|---|---|---|
| L1 | Licensing model | **Open core.** The trusted computing base is Apache-2.0; the operator console, the quant layer and the workflow engine are commercial | The safety claims are claims about an absence — `AGENTS.md` invariants 1–4 say there is *exactly one* code path to the signer, that no private key reaches TypeScript, that no agent-reachable path edits guardrails, that no account-owner key enters the app. An absence cannot be observed from outside a binary, so a closed build reduces every one of them to *trust us*, which is the posture [threat-model.md](threat-model.md) exists to replace. The layers that carry no such claim carry no such obligation. |
| L2 | Where the line falls | **`crates/*` open, `apps/desktop` commercial** | The line is not drawn commercially, it is drawn at the boundary the threat model already draws: signing, guardrail evaluation, the ledger and the MCP surface are the code a user must read to check the invariants, and they are `oppen-hl`, `oppen-core` and `oppen-mcp`. The console renders and does not sign (invariant 2), so nothing in it can be load-bearing for a safety claim — if it ever is, the code moves down into a crate rather than the licence moving up. `apps/desktop/src-tauri` is thin commands over the crates by construction and sits above the line with the Vue app. |
| L3 | Rejected: closed source | **Rejected** | It would have to be gated to earn anything — licence keys, activation, seats — and a gate needs a backend and accounts, which contradicts the claim [specs/signals.md](specs/signals.md) §2 records as already public: "no backend, no account, and no telemetry path carrying positions". Ungated closed source gates nothing and buys nothing; gated closed source buys revenue by falsifying a shipped claim. The fee leak it would recover is small either way — see L4. |
| L4 | Rejected: source-available (BUSL, PolyForm) | **Rejected** | Its mechanism does not fit this product. BUSL 1.1 restricts use through an Additional Use Grant whose standard form blocks *offering a competing hosted service*; oppen has no hosted service to protect, so the grant would have to be drafted against individuals editing a fee constant in a desktop binary, and suing individual operators is not a revenue strategy. It also encumbers the wrong layer: routed notional is the fee base, the core is what routes it, and restricting the core to protect the fee shrinks the base the fee is charged on. **Where it does fit:** the website is a separate product with its own repository (signals.md §2.1). If it ever hosts a service, that repository is where a restrictive licence belongs. |
| L5 | Builder-fee leakage, sized rather than assumed | **Accepted as a cost of L1** | D7 in [spec.md](spec.md) says hard enforcement is impossible because the source is open, and that stays true — the fee constant lives in `oppen-hl`, which is open under L1. What D7 does not say is the size. At M1's rates an operator routing $6m a month pays roughly $60–130; nobody maintains a fork of the venue signing path against protocol drift, and forfeits the trademark and the releases, to keep that. Leakage concentrates at the top of the volume curve, which is exactly where M1 has already cut the rate to 0.1 bp, so it is self-limiting. **What this gives up:** the fee is a floor, not the equity engine — $1m of annual fee revenue needs roughly $33b of routed notional at 0.3 bp, or $100b at 0.1 bp. The commercial layer under L1, M2's 3x headroom under the signed cap, and the v2 venues are all larger levers than the licence. |
| L6 | Source opening date | **Unchanged: Q4 2026, now scoped to the open crates** | The date is already published. What changes is its object: it opens `crates/*`, not the whole tree. Narrowing a published promise is still a change to it, so it is stated in the same words everywhere it appears rather than quietly qualified in one place — `README.md` §License, `content/terms.md`, `content/answers.md` §12, `content/whitepaper.md` §License. |
| L7 | CLA | **Unchanged** | §2 already licenses contributions "under any license, including licenses other than the one the Project currently uses", so open core needs no amendment and no re-signature from anyone who has already signed. This was deliberate when the CLA was written and is the reason L1 was available at all. |

## 2026-09-04 · Operator interview, second round

| # | Decision | Choice | Why |
|---|---|---|---|
| A1 | Approval mode default | **Auto-approve inside the agent's caps; queue anything above them** | Revises spec item 28 and D-c, which set approval ON for every order. The guardrail caps already encode what the operator considers acceptable unattended; requiring a click *inside* those caps asks the same question twice and trains the operator to approve reflexively, which is worse than not asking. The boundary still fails closed: an order that would exceed any cap returns `pending_approval` with a TTL and is re-priced at approval time with drift shown. **What this gives up:** the operator no longer sees every order before it lands, so the caps become the whole of the policy and the activity stream becomes the only record — both must be right. Approval-for-everything stays available per agent. |
| A2 | Mainnet timing | **Testnet only until v1 is complete** | Revises S2, which put mainnet after P3. The execution path is still being built: `place`, the guardrail wiring into the signer, and the console's fail-closed behaviour on a stale feed do not exist yet. Real money buys nothing a mock 999 USDC does not while those are in flight, and it can lose something. The operator's 29.70 USDC on mainnet stays untouched. **The cost, stated:** real fills exercise fee tiers, funding accrual and partial-fill behaviour that testnet does not, so those paths land unproven and must be gated deliberately at the v1 boundary rather than assumed. |
| A3 | First visible milestone | **Both: the live read-only console and the full agent loop** | Not sequenced one after the other, because they share the expensive part. `get_state` (item 16) and the console's readouts (items 31-34) are the same assembled view of positions, orders, balances, feed health and guardrail utilisation, differing only in transport — Tauri command versus MCP tool. Building the assembler once in `oppen-core` serves both and keeps R1's headless rule intact; building them separately would produce two views that drift. |
| A4 | First agent's guardrails | **D-c near-zero defaults, unchanged** | Empty symbol allowlist, $25 orders, $100 positions, $25 daily loss. The first order is refused by design with a reason naming the limit to raise. The refusal is the onboarding, and it is also the demonstration that the guardrail path is real rather than decorative. |

## 2026-09-04 · P4 gateway dependencies

`AGENTS.md` leanness rule 9: a new crate gets a line saying what was rejected.

| # | Decision | Choice | Why, and what was rejected |
|---|---|---|---|
| G1 | Token digest | **`sha3` (already a direct dependency of `oppen-hl` and `oppen-core`)** | `sha2` would have been the conventional pick for a token digest and adds a crate to buy nothing: SHA3-256 is not weaker here, and the tree already compiles `sha3` for the L1 action hash. A known-answer test against FIPS 202 pins the algorithm so a stand-in that truncates cannot pass. |
| G2 | Constant-time compare | **`subtle`** | Already in the tree via `k256`. Rejected: hand-rolled `==` on digests, which leaks a prefix-match through response timing; and comparing raw tokens rather than digests, which keeps a live credential in the store. |
| G3 | Token entropy | **`getrandom`** | Rejected: repeating `oppen-core`'s `/dev/urandom` read, which that module's own doc calls "a real gap, not a design choice" because it returns `EntropyUnavailable` on Windows. Fine for an HMAC key the operator can supply by hand; not fine for a bearer token the gateway must mint itself. `rand` was rejected as strictly more crate for a single `fill` call. **This does not fix `oppen-core`'s gap** — that is a separate change with its own trace (leanness rule 8). |
| G4 | HTTP framework and MCP protocol | **`axum` + `rmcp` 0.5, the official Rust SDK** | Hand-rolling JSON-RPC framing, version negotiation, session ids and SSE is a few hundred lines that must match a spec oppen does not own, and a mismatch shows up as "Claude Code will not connect" rather than as a test failure. The "no SDK" precedent from signing does not transfer: there the point was byte-exact control verified against 40 official vectors, and here the SDK *is* the specification. `axum` builds only on `hyper`, `http` and `tower`, all already compiled. **Pinned to `rmcp` 3, not 0.5.** 0.5 was written first, from a stale index entry, and `cargo deny` refused it: **RUSTSEC-2026-0189** — "the `rmcp` crate's Streamable HTTP server transport did not validate the incoming `Host` header", allowing exactly the DNS-rebinding attack described above, fixed in 1.4.0. oppen's own guard already blocked it, which is the argument for keeping the door in oppen rather than delegating it to the SDK — but a mitigated advisory is still an advisory, and the upstream fix is free. The upgrade also drops the unmaintained `paste` crate (rmcp 3 uses the maintained `pastey` fork), which was the gate's other error, and negotiates protocol 2025-06-18 instead of 2025-03-26. oppen keeps the door regardless: the `Host`/`Origin`/bearer guard runs as an axum middleware *in front of* the transport, so an unauthenticated request never reaches the protocol layer, and the SDK's validation is defence in depth rather than the only line. |
| G5 | Revocation mechanism | **Mark the record, never remove it** | Removing drops the `watch::Sender`, which closes live sessions as a side effect of deallocation rather than as a decision, and makes `AuthError::Revoked` unconstructible — so a revoked pairing would be indistinguishable from one that never existed. Uses `send_replace`, not `send`: `send` returns `Err` and **leaves the value unchanged** when no receiver is alive, so a pairing with no session open at that instant would silently stay live. Caught by a red-check, not by review. |

## 2026-09-04 · Venue containers

A venue audit against the official Hyperliquid, Aster and Lighter documentation, run on
2026-09-04, found that the primitive D1 is built on is not available to a new Hyperliquid
user. D1 is revised rather than abandoned: the unit of isolation is one venue **account**
per agent, and which venue primitive provides that account is a venue detail. This is the
first revision of a settled architecture decision — [spec.md](spec.md) carries the new D1
wording, this section carries the reasoning. Per-venue evidence:
[specs/venue-containers.md](specs/venue-containers.md). The ceremony it implies:
[specs/onboarding.md](specs/onboarding.md).

### The container model

| # | Decision | Chosen | Why |
|---|---|---|---|
| V1 | D1's unit of isolation | **One venue account per agent** — a sub-account where the venue grants one, a top-level account where it does not | The requirement was never "sub-account". It is a position book, a margin pool, an order set and a fill stream that belong to exactly one agent, with the segregation enforced by the venue rather than by oppen's bookkeeping. Hyperliquid supplies that with a top-level account, Aster and Lighter with a sub-account. Naming the requirement instead of the primitive is what lets one registry, one guardrail scope and one PnL attribution serve three venues. The word for it in the code and the docs is **container**. |
| V2 | What v1 uses on Hyperliquid | **One top-level account per agent** | Sub-accounts are gated: "Up to 10 sub-accounts can be created after reaching $100,000 in volume. Every additional $100M in volume enables the ability to create 1 additional sub-account, up to a maximum of 50 sub-accounts" ([HL docs, sub-accounts](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/sub-accounts)). The gate is protocol-enforced, not a UI guard, and it was **observed**, not only read: the attempt from the project's own testnet wallet on 2026-09-04 returned `Cannot create sub-accounts until enough volume traded. Required: $100000. Traded: $0` ([runbooks/testnet-provisioning.md](runbooks/testnet-provisioning.md)). An integration test in the `nktkas/hyperliquid` SDK asserts the same rejection, and it runs against testnet, where the threshold is identical to mainnet. A new user gets zero sub-accounts on either network, so v1 provisions a top-level account per agent. Cost per agent — **including the first** — is **three** signatures in the user's own wallet under D7's default: `approveAgent` and `approveBuilderFee` signed by the container, `usdSend` signed by the funding account. Two if the builder code is declined. **None of the three is once-per-user**, because every container is its own Hyperliquid user and `approveBuilderFee` is per account (O7); and creating the container costs nothing at all, since deriving account *n* in a wallet is a local key derivation with no signature and no gas. An API wallet signs for its master or that master's sub-accounts and never for an unrelated top-level account, which is why every container needs its own `approveAgent`, and which preserves spec item 7's one-agent-wallet-per-signer nonce isolation exactly as before. D5 holds completely: no master key path appears anywhere. What is lost is a shared fee tier, because volume no longer aggregates across the operator's accounts. At tier 0 that is worth nothing. |
| V3 | The shared-address model — several agents on one account | **Rejected** | Two agents on opposite sides of one asset net to zero at the venue, so per-agent margin, liquidation price and funding accrual are all fiction, and funding in particular is unrecoverable because the cash never moved: the venue pays nothing, so there is nothing to attribute. It fails on every venue that nets per account, which is Hyperliquid and Lighter. Recorded at length below so it is not re-litigated. |
| V4 | Aster and Lighter | **Promoted into D1 as first-class venues** | Neither gates sub-accounts. Aster's requirement is "All VIP levels", 10 rising to 50 at market-maker tier (docs.asterdex.com). Lighter's is tier-capped, not volume-gated: 4 on the free Standard tier, 16 on Plus, 64 on Premium (docs.lighter.xyz), and creation is an L2 transaction (`L2CreateSubAccount`) signed by the API key, with no Ethereum private key in the loop at all. **On oppen's core architecture Hyperliquid is the worst of the three**, and that is worth recording even though it does not change v1: `oppen-hl` is written, vector-tested against the official SDK and adversarially reviewed, and the other two crates do not exist. v1 ships on the venue that is built, not on the venue that fits best. |
| V5 | The sub-account upgrade path | **Designed for now, required never** | The registry stores the container's kind alongside its venue and address, so a container can change kind without the agent, its guardrails, its journal or its ledger history changing. When an operator's volume clears $100,000, migration is: create the sub-account, `usdSend` the collateral across, `approveAgent` for the new container, re-point the registry row. It requires a flat container — Hyperliquid documents collateral transfers between accounts and no way to move an open position — and nothing in v1 blocks on it. Designing the column now is free; adding it after the ledger has rows referencing containers is not, which is the same argument as R2. |
| V6 | Cross-venue netting | **There is none. Aggregate exposure is the application's job** | No venue can see the others. Three venues means three margin pools, three liquidation prices and no cross-margin. An economically flat book — long 5 BTC on one venue, short 5 BTC on another — posts full initial margin on **both** legs, and one leg can liquidate while the other survives, leaving the operator with a directional position they believed they did not have. A cross-venue exposure cap is therefore an oppen-enforced guardrail with no venue behind it: containment in the [threat model](threat-model.md)'s sense, not a boundary. It must be labelled that way wherever it is shown. |
| V7 | May the funding account itself be a container? | **No, by default.** The funding account funds containers, signs ceremonies and may carry the operator's own manual trading. No agent is bound to it unless the operator takes an explicit, labelled downgrade | [specs/onboarding.md](specs/onboarding.md) §3.2 and [specs/venue-containers.md](specs/venue-containers.md) §2.1 recommended opposite things; this settles it in onboarding's favour, and answers the concern venue-containers was raising rather than dismissing it. The threat model's bound is "an agent's worst case is that account's balance". Bind an agent to the funding account and its worst case becomes the balance that has not yet been distributed to the other containers — by construction the largest account in the fleet, and the one whose loss also stops every future agent from being funded. The saving is one `usdSend`. That is not a trade worth making by default. The downgrade stays available, because a single-agent operator may reasonably want it: it is off by default, is recorded on the registry row so the roster and risk console can label the container as shared with the treasury, and its confirmation states the worst case **in dollars, not in adjectives**. **The cost of the default, stated plainly:** the $100,000 sub-account gate meters one account's volume, so an account that only funds never clears it. v1 never depends on clearing it (V5). When it does matter, the master of a future sub-account tree is whichever account has actually traded — an agent container, or the funding account if the operator's own manual trading ran through it. Rooting *other* agents' sub-accounts under an **agent's** container is blocked until [specs/venue-containers.md](specs/venue-containers.md) §6 question 1 is closed: if an API wallet approved on a master signs for that master's sub-accounts, that arrangement puts every sibling container inside one agent key's signing scope and deletes the isolation the container model exists to provide. |
| V8 | Aster position mode | **Set at provisioning, one-way by default, treated as fixed for the container's life** | Aster's hedge mode is account-wide via `POST /fapi/v3/positionSide/dual` and **cannot be changed while any position or open order exists**. That makes it a provisioning-time attribute or it is a trap: an operator who wants to switch a live container must first flatten it. oppen therefore writes the mode onto the container row at provisioning, before the container is funded, and the roster displays it. The default is **one-way** for two reasons. D1 already establishes that hedge mode changes sign bookkeeping *inside* a container and does not make a container shareable, so nothing in oppen's model needs it. And one-way is the only mode Hyperliquid has and, on the data-model reading, the only one Lighter has, so one-way keeps a single position-sign model across all three venues instead of a per-venue special case in every PnL, guardrail and liquidation path. Changing the mode later is an operator action on a flat container with no open orders, behind a confirmation, and is never agent-reachable (D3) — leverage and margin mode are already operator-set, and this joins them. Whether Aster caps the number of agents is still unconfirmed and is unrelated to this row. |

### The container's own key

This follows from V2 and D5 and is stated separately because it is the easiest thing here
to lose in implementation. A top-level container is an ordinary Hyperliquid account, and
its owner key **can withdraw**. That key is a second account in the user's own wallet, not
a key oppen generates. oppen holds agent wallets and nothing else.

If oppen generated container keys and stored them in the OS keychain, then on Windows and
Linux — where any process running as the same OS user can read the keychain, per the
[threat model](threat-model.md) — a compromised machine could withdraw from every agent
container. "The agent wallet cannot withdraw" is described in that document as the only
containment property that holds against a fully compromised machine. Generating container
keys in the app would delete it.

### Why the shared address is rejected

One address, two agents, agent A long 5 BTC and agent B short 5 BTC at $60,000. The venue
sees one account with a position of zero.

1. **Margin.** As two containers the legs post initial margin separately — at 10× that is
   $30,000 each, $60,000 held. Shared, the address posts approximately nothing for a
   $600,000 gross book. The safety property is not degraded; it is absent.
2. **Liquidation.** Neither agent has a liquidation price, because neither has a position.
   Worse, agent B closing its short leaves the address instantly long 5 BTC against
   collateral that was never sized for it. **One agent's exit is another agent's risk
   event**, with no mechanism that could have warned either of them.
3. **Funding.** Funding is charged on the net position, so at net zero **no cash moves**.
   At 10% annualised on $300,000 of notional, honest per-agent books would debit A $82.19
   a day and credit B the same. Both numbers would be invented, they reconcile against no
   venue record — `userFunding` shows nothing — and the error compounds daily and silently.
   This is the row that kills the model: sizes and prices can be reconstructed from fills,
   and funding that never moved cannot be reconstructed from anything.
4. **Reduce-only.** The venue computes reduce-only against the address's net position, so
   agent A's reduce-only sell is an *increase* of the address's short. Guardrail 24's
   reduce-only mode would become local bookkeeping asserting a venue-enforced word, which
   is precisely the kind of claim [AGENTS.md](../AGENTS.md) invariant 1 exists to prevent.
5. **Attribution.** Fills arrive per address. Attribution survives only for orders oppen
   itself placed with a `cloid`; anything signed outside oppen on that address belongs to
   no agent, and `manual · external` becomes the only honest bucket for a growing share of
   the account.

The guards written for a shared address are not withdrawn. They are carried into
[spec.md](spec.md) D1 and stay binding for the one sharing case the container model leaves
standing — an agent's container also carrying the operator's own manual actions:

- an agent may cancel or modify only order ids it opened;
- the venue's aggregate position book is never sliced per agent;
- `close_position` and flatten are refused while two or more agents share an address;
- account equity and margin are shown once for the address and never mirrored per agent.

One escape route is worth closing before someone finds it. Aster supports hedge mode, which
would keep two opposite sides distinct at the venue and so avoid the netting above. It does
not rescue the shared-address model: hedge mode is account-wide rather than per agent, and
Aster grants sub-accounts to every VIP level anyway, so the venue where the rescue exists is
the venue that never needed it. Hyperliquid, where the model was proposed, has no hedge mode
at all — "this parameter won't have any effect until hedge mode is introduced".

### What the three venues actually grant

Read on 2026-09-04. Every cell is a documented limit rather than an observed one, with a
single exception: Hyperliquid's gate was observed refusing a real `createSubAccount` from
the project wallet that same day (V2). No Aster or Lighter call has been executed at all.

| | Hyperliquid | Aster | Lighter |
|---|---|---|---|
| Sub-account gate | **$100,000 traded volume**, protocol-enforced, same on testnet | **None** — "VIP level requirement: All VIP levels" | **None** — tier-capped, not volume-gated |
| Cap | 10, then +1 per $100M to 50 | 10 (VIP 1–2) to 50 (MM tier 3) | 4 Standard (free) · 16 Plus · 64 Premium |
| Creation signature | Master EOA (and the gate above) | **Dual**: the child key plus the master's EIP-712, `chainId` 1666 | The API key (`L2CreateSubAccount`) — no Ethereum key |
| Hedge mode | **No.** "this parameter won't have any effect until hedge mode is introduced" | **Yes**, account-wide via `POST /fapi/v3/positionSide/dual`; cannot change with open positions or orders; not supported in Shield Mode | **No** — inferred from one position per `(account_index, market_id)` with a single sign field; the docs never say so |
| Can a trade-only key withdraw? | **No.** API wallets cannot withdraw | **No.** Sub-account API withdrawal is "Permanently disabled"; agent keys carry `canWithdraw: false` | **Partly.** Keys process "secure" withdrawals, which "can only be sent to the same L1 address that created the account". Fast Withdrawals and Transfers need the L1 key |
| Key budget | 3 per master, +2 per sub-account | 30 master / 10 per sub; "Sub-accounts only support V3 API" | ~253 per account index, indices 0–1 reserved (three official pages disagree: 253 / 254 / 256) |

Three consequences the table does not make obvious:

- **A container is created once and never destroyed.** Aster: "Deleting not supported", and
  nesting is not supported either. Lighter: "Sub accounts cannot be deleted." A Hyperliquid
  address cannot be deleted in any meaningful sense. Retiring an agent means emptying and
  de-registering its container, never removing it, and the roster has to say so.
- **Lighter's key model is the weakest of the three** and the only one where a stolen
  trading key can move funds at all. It moves them to the owner's own L1 address, which is
  a strong mitigation, but it is not the flat "cannot withdraw" that holds for Hyperliquid
  and Aster, which is why [threat-model.md](threat-model.md) now carries a per-venue key
  table instead of one sentence.
- **Lighter's tier change is not free.** It requires no open positions, no open orders, and
  at least 24 hours since the last change, so an operator who fills their 4 free containers
  cannot expand mid-session.

### Operational facts that constrain the product

| # | Fact | Consequence |
|---|---|---|
| O1 | `scheduleCancel` requires a time at least 5 seconds ahead and allows a **maximum of 10 triggers per day per address, resetting at 00:00 UTC** | Spec item 27 assumes a dead-man's switch that is always available. It is a daily budget instead. The budget is per address, so N containers carry N independent budgets and N arming duties. Re-arming after every reconnect would exhaust one inside a bad hour, so the arming policy has to be deliberate and the risk console has to show the remaining count per container. Whether `scheduleCancel` is itself volume-gated could not be confirmed — a reverse-engineered binary string suggests it and the official docs are silent — which if true means a brand-new account has no dead-man at all. |
| O2 | The testnet faucet pays 1,000 mock USDC and only to an address that has **previously deposited on mainnet** | D4's testnet default does not remove a mainnet prerequisite for the *funding account*; the project wallet already satisfies it ([runbooks/testnet-provisioning.md](runbooks/testnet-provisioning.md) §0). Containers are funded from the funding account with `usdSend`, so only that account needs the faucet. P7's "fresh machine to a testnet trade in 10 minutes" gate is measured from a wallet that already holds testnet USDC; the deposit that makes the faucet work is outside oppen and outside the ten minutes, and the gate text should say so. |
| O3 | **Corrected 2026-09-04.** `userRateLimit` **does** return `cumVlm`, documented as "Cumulative volume" — verified live that day (`{"cumVlm":"188908641154.22","nRequestsUsed":…}`) and modelled in `crates/oppen-hl` as `UserRateLimit::cum_vlm`. What is **not** documented anywhere read is whether the sub-account gate meters that counter, or whether the counter is lifetime or windowed | The earlier version of this row read "no info endpoint exposes cumulative volume". That was **false**; it is superseded here and in [spec.md](spec.md) D1, and in [ROADMAP.md](../ROADMAP.md). The conclusion is unchanged and now rests on a true premise. oppen **may** show distance to $100,000 computed from `cumVlm`, provided it is labelled on screen as an estimate the venue does not confirm; it **must not** branch on it. Onboarding still attempts creation and classifies the refusal, because only the venue's answer is authoritative. `Required:` and `Traded:` may be parsed out of the refusal **for display only**; program logic never branches on venue message text (invariant 8: the typed error is oppen's, the string is the venue's). This is also why V5's upgrade is offered as an attempt rather than unlocked by a counter. |
| O4 | Re-approving the same `agentName` **replaces** the prior agent | A silent revocation: the old agent wallet stops signing with no event anywhere. One distinct name per container, forever, and rotation never reuses a name — the same rule the provisioning runbook already applies to addresses, for the same reason. |
| O5 | `valid_until` caps at 180 days | D-b's 90-day expiry with a warning from day 14 sits inside the cap and needs no change. Nothing to do; recorded so nobody proposes a one-year agent. |
| O6 | API wallets start "at 3 for all master accounts and increase by 2 per sub-account" | Each top-level container is its own master, so it carries its own baseline of 3 agent wallets instead of drawing on the operator's. The container model relieves the agent-wallet budget rather than consuming it — the opposite of what the old D1 assumed, where four sub-accounts bought 11 wallets shared across everything. |
| O7 | `approveBuilderFee` is a user-signed approval **per account**; max 10 active builder approvals per user; the builder must hold ≥ 100 USDC in its perps account; the perp maximum is 0.1% | A top-level container is a different Hyperliquid user from the funding account, so it must approve the builder fee itself for D7's builder code to be attached to its orders. That makes the per-agent ceremony three signatures, not two, for **every** agent including the first: there is no once-per-user signature left on Hyperliquid under the container model. Whether a *sub-account* inherits its master's approval was not confirmed, so V5's upgrade may or may not need a fresh approval, and no signature count is quoted for that path until it is settled. M2's 1 bp signed cap remains far inside the venue's 0.1% ceiling. |

### What this revises elsewhere

Stated plainly, because each of these is a sentence that is now false:

- **[spec.md](spec.md) D1**: already rewritten in the same batch. It was titled "One Hyperliquid sub-account per agent" and said "The roster maps 1:1 to sub-accounts"; it now reads "One venue account per agent" and the roster maps 1:1 to containers.
- **[README.md](../README.md)**: "**One Hyperliquid sub-account per agent.** Attribution and capital segregation enforced by the venue, not by bookkeeping." The second sentence stays true; the first names the wrong primitive.
- **[threat-model.md](threat-model.md)**: already rewritten in the same batch — the boundary now reads "One venue account per agent" and a new "multi-account model" section carries the per-venue key table and the blast radius. It now states the plural correctly: under V2 there are N container-owner keys, one per agent, all withdrawal-capable and all in the user's own wallet rather than in oppen.
- **[runbooks/testnet-provisioning.md](runbooks/testnet-provisioning.md)**: already rewritten the same day. Its old §2, "Create four sub-accounts", is what produced the refusal quoted in V2, and its old agent-wallet arithmetic — "Four sub-accounts therefore allows 11" — is superseded by O6, which gives each top-level container its own baseline of 3.
- **[spec.md](spec.md) D5**: retitled from "The master wallet key never enters the app" to "**No account-owner key ever enters the app**", and re-derived for N containers in the same batch. The old title named a singular master the revised D1 deleted. The guarantee is unchanged and the derivation is now the honest one: there are N account-owner keys, every one of them can withdraw, and not one of them is oppen's.
- **[spec.md](spec.md) items 10 and 27**: both were written as though there were one address. Re-derived in the same batch. `userRateLimit` is per address, so N containers is N budgets that are additive but not fungible; `scheduleCancel` is per address with 10 triggers a day, so N containers is N arming duties and N trigger budgets, and no container is covered by another's arming.
- **[ROADMAP.md](../ROADMAP.md)**: carried the same false premise and was corrected in this pass. The attempt-then-classify behaviour it describes was always right; only its premise was wrong.
- **R3** (2026-09-03), "oppen discovers every sub-account under the master": **rewritten in place**, because there is nothing to discover for a top-level container — no info endpoint links it to the funding account. The recovery problem that exposed is now its own row, **R7**. Opt-in recording still applies to genuine sub-accounts and to accounts the operator adds by address, and that second path is now the recovery route rather than a convenience.
- **R2**: unchanged in substance. The row gains a container kind and a venue; whether a workflow owns a container or binds to an agent's is still open.
- **M3**: **rewritten in place.** "Aggregates across sub-accounts by construction" was a property of one master with sub-accounts under it, which is not the shape the container model has. The re-derivation is stronger, not weaker — on Hyperliquid the venue no longer aggregates an operator's volume at all, so the local ledger is the only place a fee tier could be computed — and it adds the two conditions the old sentence left implicit: only oppen-routed notional counts, and the sum is per network (R4).
- **[specs/onboarding.md](specs/onboarding.md) §3.2 and [specs/venue-containers.md](specs/venue-containers.md) §2.1** recommended **opposite** things about whether the funding account may host the first agent. Settled by **V7** in onboarding's favour: no by default, an explicit labelled downgrade otherwise. venue-containers §2.1's "the first agent trades in the account that will later be the master" is superseded as a recommendation; the volume-metering concern behind it is answered inside V7 rather than dropped.
- **S7**: the mobile companion reads "the account address" — now N container addresses per venue, which is a polling-cost question for [specs/mobile.md](specs/mobile.md), not a design change.

### Not confirmed

None of the following are settled, and no decision above depends on any of them being
true. They are listed so a later reader does not mistake silence for verification.

- Whether the sub-account gate meters `userRateLimit`'s `cumVlm`. The field exists and
  returns a number (O3); no source read links it to the gate. This is why oppen may
  display distance-to-gate and may never branch on it.
- Whether `cumVlm` is lifetime-cumulative or windowed. Everything points to cumulative;
  Hyperliquid has never stated it. If it is windowed, the upgrade stops being a one-way
  ratchet and an interrupted volume run decays.
- Whether a Hyperliquid API wallet may sign `createSubAccount` or `subAccountTransfer`
  ([hl-signing.md](hl-signing.md) open question 3). If it may, an agent key can move USDC
  between a master and its sub-accounts and no guardrail models that. A testnet negative
  test closes it, and it must be closed before the V5 upgrade path ships.
- Whether reading an account's approved-agent list back is a documented info endpoint.
  [specs/onboarding.md](specs/onboarding.md) §3.3 step 7 assumes it is; R7's recovery
  story depends on it.
- Whether `scheduleCancel` is itself volume-gated (O1). If it is, a freshly provisioned
  container has no dead-man's switch at all — which under the container model means
  *every* new container starts without one, not just the first.
- Whether a `usdSend` to an address with no prior Hyperliquid activity creates that account,
  or whether every container must first be touched some other way. This sets the real
  per-agent onboarding cost and should be answered on testnet before onboarding is built.
- Whether a sub-account inherits its master's builder-fee approval (O7).
- Whether Aster caps the number of agents. The docs do not say.
- Whether a never-traded Aster wallet, below VIP1, gets exactly 10 sub-accounts. The
  requirement reads "All VIP levels" but the table's lowest row is VIP1.
- Whether Aster sub-accounts work in Shield Mode / 1001×.
- Lighter's exact API key count per account index — three official pages disagree.
- Whether Lighter truly has no hedge mode. It is read off the data model, not the docs.
- Whether Lighter gates account creation by invitation. There is no documented gate today,
  and no affirmative statement that it is open to all either.

---

## 2026-09-03 · Interview

Thirty-three decisions taken in one sitting, after the six feature specs in
[specs/](specs/) were written.

### Sequencing

| # | Decision | Chosen | Why |
|---|---|---|---|
| S1 | What to build after P1 | **P2 as planned** — WS pool, reconcile, hash-chained ledger | Everything queues behind it. History is a ledger projection, workflow run state lives in it, charts need the trades and candles feeds, and the fair value sampler needs a fixed clock over live components. |
| S2 | Mainnet readiness | **After P3** — guardrails, loss breaker, kill switch, dead-man, with the no-bypass property proven by a test | The whitepaper's central claim is only true of a build that has P3. Going to mainnet before it means the claim is not true of the build being used. P4 and P5 are not required; the CLI example can drive real money once the limits are real. |
| S3 | Public release gate | **All of P7** — fresh machine to a testnet trade in ten minutes | The repo is already public and anyone can build from source. A released binary with a checksum is a promise, and for a product whose pitch is safety the first binary is the promise that matters. |
| S4 | Approval mode | **Stays in v1, built last** | It is what makes "new agents start with approval mode ON" true, and that sentence is in the spec, the README and the pairing flow. It is also what makes oppen demonstrable to someone who does not trust it yet. |
| S5 | Paper trading | **No paper broker** — testnet is the paper mode | Testnet gives real fills, real rejections, real latency and real funding, and exercises the real signing path. A paper fill model is an assumption that would have to be maintained and would be optimistic in exactly the ways the assumptions are wrong. `workflows.md` §8 becomes "arm on testnet, promote to mainnet"; the execute node has one implementation. |
| S6 | Community signal layer | **Parked until after v1** | It is the only feature touching the architecture's central public claim, and it needs users before it needs code — a feed with three publishers looks abandoned. The spec stays written and costs nothing to defer. |
| S7 | Mobile companion | **v2, read-only first** | The phone reads public venue state from the account address alone: no keys, no backend, no pairing. The cancel-only panic wallet is a second signing key on a second device and has to be earned by the read-only version proving the pattern. |

### Interface

| # | Decision | Chosen | Why |
|---|---|---|---|
| U1 | Primary chart renderer | **ASCII, character grid** | One visual language, no foreign object in the shell, deterministic and snapshot-testable, ports free to mobile, and direction is encoded twice. Resolves the blocker on P5 and supersedes `lightweight-charts` in [spec.md](spec.md) item 31. |
| U2 | Canvas fallback | **Dropped entirely** | Zero chart dependencies in an auditable local-first binary. "Not enough resolution" is really "change the interval", which [charts.md](specs/charts.md) §3 already builds and which is one keystroke. An optional second renderer becomes a parity obligation for every overlay. |
| U3 | Up/down colour | **`--up: #2fbf71`, `--down: #e5484d`** | Measured: 8.3:1 and 5.0:1 on void, both clearing AA for graphics with margin. `--down` is deliberately dimmer than `--hazard` so hazard keeps its exclusive meaning. In greyscale the pair separates 5.8:1 to 4.4:1, and the glyph (`+` / `:`) already carries direction independently. |
| U4 | Quantoppen placement | **Its own tab** | Fourteen columns across fifty symbols needs full width, and it is the surface left open on a second monitor. Trade is about one symbol; Quantoppen is about many. |
| U5 | Workflows placement | **The existing Builder tab** | The design mock's Builder (source, policy, instructions, testnet run, arm) is already a workflow definition without a graph in it. No new tab, no navigation change, and the design work is largely done. |

### Money

| # | Decision | Chosen | Why |
|---|---|---|---|
| M1 | Builder fee schedule | **Volume-tiered: 0.3 / 0.2 / 0.1 bp** | Breaks at $1m and $25m of routed notional. `f` = 3, 2, 1 in tenths of a basis point. Most users never leave the first tier, which is why 0.3 is the number that matters. |
| M2 | Signed fee cap | **1 bp** (`maxFeeRate: "0.01%"`) | Roughly three times the entry rate: enough headroom to adjust without asking anyone to re-sign, small enough that the gap between the cap a user approves and the rate they pay stays defensible. The venue's own maximum for perps is 0.1%, so this is well inside it. |
| M3 | Volume source for tiering | **The local ledger** — the sum of the notional oppen actually routed, across containers, on the network the fee is charged on | Re-derived 2026-09-04 for N containers. The original reason — "aggregates across sub-accounts by construction" — described one master with sub-accounts beneath it, and the container model does not have that shape. The conclusion gets stronger rather than weaker: on Hyperliquid, fee-tier volume no longer aggregates across an operator's top-level accounts at the venue (V2), so **no venue-side counter of the fleet's routed notional exists anywhere**, and the local ledger is not merely the cheap option but the only place the number can be computed at all. Three properties to build to, each of which the old wording left implicit. It sums **only what oppen routed**: anything signed outside oppen, `manual · external` included, never enters the ledger — which is the correct base anyway, since the builder code is only ever attached to orders oppen routes. It sums **across containers and across venues**, because the fee is oppen's and belongs to no venue's tier table. And it is computed **per network**: R4 puts testnet and mainnet in separate database files with separate hash chains, so the tier query runs against the mainnet file and testnet notional can never lift a real tier. Editable by a determined user, which D7 already concedes: the source is open, so anyone willing to edit the database would simply fork and set `f` to zero. |
| M4 | `collateral_apr` | **4.5%** | The opportunity cost of USDC sitting as margin rather than in T-bills. Makes `carry_edge_apr` answer whether funding is worth tying up capital, not merely what funding pays. Must never default to the venue's hardcoded `0.01%/8h`, which is a mechanism constant and not a carry estimate. |

### Data and keys

| # | Decision | Chosen | Why |
|---|---|---|---|
| D-a | Master ceremony wallet | **MetaMask** | Settles an open question in [hl-signing.md](hl-signing.md): the app reads the chain id from the live WalletConnect session and supports Arbitrum One (`0xa4b1`) and Arbitrum Sepolia (`0x66eee`). |
| D-b | Agent wallet expiry | **90 days, warn from 14** | Long enough not to be a chore, short enough that an abandoned deployment stops trading within a quarter. At expiry: signing fails, oppen halts the agent and cancels resting orders, and **leaves positions open** — force-closing on a calendar event is a destructive action triggered by a clock. Makes "authority decays by default" literally true. |
| D-c | Default guardrails for a new agent | **Near-zero** | Empty symbol allowlist, $25 orders, $100 positions, $25 daily loss, 5 orders per 5 minutes, approval mode on, testnet. The first order is refused with a reason naming the limit to raise: the refusal is the onboarding. Default-deny all the way down. |
| D-d | First-run history backfill | **30 days foreground, the rest in the background** | The tab is useful in seconds; the background job walks backwards until the venue serves nothing older, yields to any risk-reducing request, and is pausable. The UI states the date from which history is provably complete. |
| D-e | Retention | **Never delete records; prune recomputable inputs** | Ledger events, fills, funding payments, closed positions, refusals and approvals are kept forever, because a gap in a hash chain is indistinguishable from tampering. 1-second fair value samples (default 7 days) and locally-aggregated sub-minute bars (default 30 days) are prunable; book snapshots are not stored at all. |

### Runtime and records

| # | Decision | Chosen | Why |
|---|---|---|---|
| R1 | Where a workflow runs | **Headless-capable core, same machine** | A cron with second resolution is fiction if nothing is awake at 05:00, and `position-guardian` watches nothing on a closed laptop. The constraint starts now: `oppen-core`, `oppen-hl` and `oppen-mcp` keep zero Tauri dependencies so the core can later run as a daemon with the UI as a client. This is already true on `main`; the decision is to keep it true. Nearly free before P2 lands, expensive after. |
| R2 | Container ownership | **Schema carries an owner discriminator; the product rule stays open** | `owner_type ENUM('agent','workflow')` plus `owner_id`. The column is free today and impossible to add cleanly once the ledger has rows referencing containers. Whether a workflow gets its own container or binds to an agent's is a product question that real usage will answer better than reasoning. Revised 2026-09-04 (V1): the container is a venue account, which on Hyperliquid v1 is top-level rather than a sub-account. |
| R3 | Which accounts get recorded | **Record what oppen provisioned; discover only where the venue makes discovery possible; opt in per account** | Rewritten 2026-09-04. The original text — "oppen discovers every sub-account under the master and the operator ticks which ones it records" — is not true of the container model, because discoverability is not uniform across container kinds. A **sub-account is discoverable**: the venue links it to its master, oppen enumerates, and the operator ticks. A **top-level container is not discoverable at all** — no info endpoint links it to the funding account, and its address is an ordinary wallet account indistinguishable from any other the user holds. For those, the registry oppen writes at provisioning time is the only record that the address is a container, which agent owns it and which agent wallet is approved on it. That is a recovery problem in its own right and is R7, not a footnote here. Default off for anything oppen did not provision: it watches what it made and asks before watching you. The "add by address" path is unchanged in mechanism and changes in importance — under the container model it is the recovery route (R7), not a convenience. |
| R4 | Network isolation | **One database file and one hash chain per network** | Testnet and mainnet agents will run simultaneously. D6 makes the monotonic rowid the agent's `get_events` cursor, so a shared file means testnet row 4,812 and mainnet row 4,812 are the same cursor position. A mainnet number that is actually a testnet number is the worst bug this product can ship, and a file boundary makes it unrepresentable rather than filtered. |
| R5 | Hash chain scope | **Record of record chained, over content hashes** | Intents, decisions, refusals, fills and operator actions are chained. Candles, book snapshots, sub-minute bars and projections sit in unchained side tables with a disk budget. The chain commits to `hash(payload)` rather than the payload, so a row can be tombstoned without breaking verification — narrowing a chain later means rehashing, which destroys the property it exists for. |
| R6 | Decision-time market context | **Snapshot by reference** | Orders, fills and approvals store a nullable `snapshot_id` and `snapshot_hash`; the hash goes in the chained row and the body lives in a prunable `book_snapshots` table. Roughly forty lines at P2, no capture policy decided yet. The book at the moment an agent decided is the one class of data that cannot be backfilled, so the plumbing has to exist before the policy does. |
| R7 | Losing the container registry | **The registry is a recovery artefact, not only an index. oppen writes a plain-text container manifest beside the database on every provisioning and names it in onboarding** | New 2026-09-04, and it exists because R3 stopped being able to hide the problem: a top-level container is undiscoverable, so oppen's SQLite registry is the only place the agent → container → agent-wallet binding lives. Losing the database is not merely losing history. What survives and what does not, separately, because they are usually confused. **The container keys survive**: they are accounts derived in the user's own wallet from a seed oppen never held. **The agent-wallet keys do not**: they are in that machine's OS keychain and nowhere else. **The binding does not**: nothing on-chain says which address was `agent-alpha` or which of the user's twelve wallet accounts were containers at all. Recovery without the registry, in order: enumerate the wallet's own accounts; query `userRole` and `clearinghouseState` per address to find the funded ones; re-add each through R3's add-by-address path; read each account's approved-agent list back from the venue to see what is still authorised; then approve **fresh** agent wallets under **fresh** names, never a reused one, because re-approval under an existing name silently replaces it (O4). That is slow but complete, and it is available only while the user still has the wallet seed. The manifest exists to make it cheap: venue, container address, container kind, agent name, agent-wallet address, `valid_until`, created-at, network — no secret in it, one line per container, rewritten on every provisioning, and onboarding tells the user to keep it with their wallet backup. **Not confirmed:** whether reading an account's approved agents back is a documented info endpoint. [specs/onboarding.md](specs/onboarding.md) §3.3 step 7 assumes it exists and reconciles against it; if it does not, that step of the recovery is lost and the manifest becomes the only record of which agent wallet was ever approved where. |

### Scope and risk posture

| # | Decision | Chosen | Why |
|---|---|---|---|
| P1 | v1 cut line | **P0–P7, console minimal** | P5's gate changes from "parity with the design" to a named list: activity stream, rejection explainability, roster with last-seen, staleness overlay, kill switches. Chart annotations, OS notifications and full parity move to v1.1. P6 quant stays, because it is what makes the MCP surface worth connecting to. Parity is a judgement, not a test, and an ungated judgement resolves as drift. |
| P2 | Unattended live trading | **Earned, after a clean record** | A workflow definition unlocks live-unattended execution only after N clean testnet runs with no guardrail trips. Editing the definition resets the counter, because a fork is a new definition. The unlock is a query against the ledger that already exists, not a new subsystem. |
| P3 | Template prompts | **Responsibilities, never thresholds** | "Try to falsify this thesis" ships. "Enter when annualised funding exceeds 12% for three consecutive intervals" does not. The design mock's Builder currently displays exactly that rule and must be rewritten before it goes public, because it contradicts both `specs/workflows.md` §3 and the whitepaper's "no alpha, no signals, no default agent behaviour". |
| P4 | Chart annotations | **Fill marks on the chart, reasons in the stream** | Uranium marks carry the fact; the stream carries the words, already labelled agent-authored and inert. On a character grid an untrusted `reason` string is made of the same characters as the chart itself, so escaping HTML does not stop an agent writing a label that reads as a price row or an axis rule. Follow-agent mode becomes the v1.1 upgrade. |
| P5 | Builder fee custody | **A dedicated hardware-wallet EOA** | Used for nothing else. The address is compiled into official builds, so changing it later silently stops revenue until every user re-signs; its custody has to be what you would choose at ten times the revenue. A Safe or any smart-contract wallet is disqualified: fees are withdrawn by that address signing for itself, and Hyperliquid cannot verify ERC-1271. |

### Fair value scope — decided by evidence, not preference

| # | Decision | Chosen |
|---|---|---|
| F1 | Engine scope | **Carry, basis and micro components with the full combination engine and all five MCP tools. Mark is consumed, not replicated.** |

An audit of the live Hyperliquid API settled this. See [specs/fair-value.md](specs/fair-value.md)
§14 for the evidence and the twenty-four corrections it produced. The short version:

- Every input the carry, basis and micro components need is live today from
  Hyperliquid alone, and §3.1's premium formula reproduces the published
  `premium` to under 1e-9 on 177 of 177 live mainnet assets.
- The funding constants hold across 4,627 mainnet records with zero dead-zone
  violations on either edge.
- **Mark replication is unreachable at any configuration**, so §10.1's gate is
  replaced rather than deferred. Even granting perfect clock alignment and a
  perfect component selector, the residual is 3.09 bp at p99 on HYPE. The venue's
  sampling instants are unobservable, and 52 of 233 assets have a single mark
  tick wider than 1 bp.
- The five centralised-exchange feeds would break the local-first posture to buy
  a worse estimate of a number Hyperliquid already publishes for free.

### Still open

Everything listed under "Open decisions" in each spec that is not resolved above.
The near-term ones: how many watched symbols Quantoppen actually supports given
the depth measurement problem in §14, and whether the sub-account owner rule
(R2) lands on agent or workflow.
