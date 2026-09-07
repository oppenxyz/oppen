# UI refinement implementation status

Goal: complete the recommendations in [the review](ui-review-2026-09-07.md).
Branch: `design/app-ui-refinement`, isolated worktree `oppen-app-ui` from `28fbf36`.

## Implemented

- Chart host width includes dynamic price gutter; font-load remeasurement; consistent chart glyph font; accessible chart summary.
- Exact decimal display formatting, null handling, compact feature units, original values in tooltips.
- Account unread/empty/failed states; last good data retained; real positions/orders in Trade, shared position table with Portfolio.
- Stale/disconnected chart notice, original feature-read timestamp separate from book ticks, selection-race protection, non-destructive feature refresh.
- Searchable markets, labelled book columns, measured visible-level depth gauges and market exposure bars.
- Readable working text, compact two-row shell at small widths, full account error disclosure.
- Truthful capability wording in manual ticket, shell, Agents, Builder, Settings and tour.
- Actual settings navigation, explicit persisted network selection and network-specific account environment variable.
- Setup overflow correction, actionable next step, explained progress denominator, expandable diagnostics.
- Terrain V2 + framed black wordmark plate on Setup; detailed static apertures; local-machine schematic.
- Decorative pause setting, reduced-motion handling, hidden-document clock gating; live data clocks remain separate.
- Walkthrough spotlight mask, focus trap/return, single Enter activation, measured callout height.
- Builder offers the existing testnet gateway setup path with a read-only operator briefing; unavailable controls are explicit.

- Read-only operator projection now connects existing gateway ledger and stored policy through `OPPEN_DATA_DIR`; it creates/migrates neither database. Trade Activity/Fills, recorded-agent roster, stored limits and stored halt state consume this source. Live pairings/approvals remain explicitly distinct.

## Verification completed

- Desktop TypeScript/Vite build passes.
- 88 Bun tests pass, including chart width, exact rounding, account failures, out-of-order market snapshots and independent derived-feature freshness and book-level deduplication. Network-save failure preserves the existing session; decorative pause/reduced-motion/visibility leave independent data timers running; operator actions are not attributed to agents.
- Core suite after merging current main: 508 passed, 5 ignored; 10 desktop Rust tests passed, including disconnect/reconcile and the new read-only operator tests.
- `cargo clippy -p oppen-desktop -p oppen-core --all-targets -- -D warnings` passes.
- Browser checks at 1280×720: Setup scroll layout, unclipped shell, readable walkthrough spotlight, Enter advances exactly once, Tab cycles within modal, Escape restores Setup focus.

- The isolated native review bundle renders live testnet data; complete right price scale and forming label verified. No account was configured. Initial empty webview observation resolved on the next inspection.

- The rebuilt native app reads an isolated, explicitly labelled fixture through the real Rust/Tauri bridge. Agents displays exact stored caps, a refusal with observed/limit values, and literal HTML-like text without rendering markup. Builder displays the same recorded activity. Settings distinguishes stored halts from live cancel completion. Setup renders the complete terrain and framed wordmark with decorative motion paused.
- Fixture reproduction: `cargo run -p oppen-core --example ui_fixture -- /tmp/NEW-DIRECTORY`; launch the isolated review bundle with `OPPEN_DATA_DIR` pointing there. The example refuses an existing directory and never creates an engine, loads keys or contacts a venue. Public market reads in the review app remain separate. Fixtures are not testnet execution evidence.

- Integrated merged main `2ea98ab` (durable pilot budgets and stop supervision). Preserved the new banner/polling, independent operator reads, network-specific account selection, and tour inert behavior. Both local read failures use main's `local_status` error variant.

## Compact-window verification — 7 September, later pass

- Merged current main `2ea98ab` into this branch as `5d8301b`; preserved durable pilot supervision and its tests.
- Native 1280×720 pass with the supervision banner caught shrinking auto rows in Trade, Agents and Settings. The book, policy and settings stacks now use content-sized rows and bounded scrolling. Verified the complete book, expanded feature definitions, all policy caps and Settings refresh control after rebuilding. Portfolio's side stack uses the same content-sizing correction for longer exposure lists.
- All six views inspected at minimum size. Builder instructions/activity and Setup's lower walkthrough action remain scrollable and separated. Default 1440×900 native Trade, Portfolio and Setup also inspected.
- Corrected “volume ratio” to **volatility ratio** from the core definition. A keyboard-accessible disclosure gives definitions, sample counts and exact feature values.
- Native network confirmation opens without persisting a switch. Focus enters Keep Testnet; Escape cancels and returns to the original choice. Separate isolated startup checks proved saved mainnet restoration, invalid-value testnet fallback and unavailable-storage testnet fallback. The native session stayed on testnet.
- Decorative pause survived native restarts while public market updates continued. Contrast calculations on panel background: labels 4.87:1, body 8.67:1, primary values 15.77:1.
- Native walkthrough: focus enters Skip; Tab wraps; Enter advances exactly one step. A WebKit mouse-focus case originally returned to body; fixed and reverified that Escape returns to Setup.
- A fixed QA address (`0x111…111`, not an operator-selected account) returned a public testnet balance and no positions/orders. Verified successful account values and genuine zero position counts, distinct from the previously inspected unknown account state. No key, order, cancellation, or funding action was involved; the QA session was closed after inspection. “Account has equity” now describes only the observed account balance, without implying a paired container or trading permission.
- Empty recorded-agent views now distinguish an actual initial read with the sweep pattern and busy text. Once read, empty apertures remain static; unavailable sources retain explicit text.

## Populated and enlarged-text verification

- Separate development-only preview uses the real Vue views/stores with twelve local positions and orders, long market names, small prices and full decimal inputs. Run `bun run dev -- --config vite.review.config.ts` in `apps/desktop`, then open `http://127.0.0.1:1431/review.html`. An iframe supplies actual 1280×720 and 1440×900 CSS viewports. The toolbar labels every scenario as a fixture without a venue/execution connection.
- Inspected populated Portfolio at 1280×720 with normal and 150% working text; table overflow remains scrollable. The larger text exposed clipped gauge endpoints, now corrected by measuring glyph width and reducing cells to the available panel width.
- Forced an account-read failure after a successful populated read: twelve positions and balances remain, with the last-success timestamp, complete wrapped diagnostic and Retry action. Inspected populated Trade positions and orders at 1440×900.
- Inspected Settings and Setup at 1280×720 with 150% working text. Sections stay separated and the checklist scrolls independently of the complete framed terrain.
- Verified the initial recorded-agent read displays its busy text and ASCII read pattern; completed empty reads use the static state.
- Portfolio aggregate PnL and gross exposure now sum decimal strings exactly before rounding. Regression tests cover large balances beyond the safe integer range, sub-cent accumulation, signed PnL, absolute exposure and invalid inputs.
- Final isolated native bundle rebuilt and launched successfully: public testnet chart, full price axis, book/gauge endpoints, recorded fixture activity and unknown account state render correctly.
- Production TypeScript/Vite build and separate fixture typecheck pass; 88 frontend tests pass. The fixture config deliberately rejects builds, and the normal production output contains no fixture transport/entry markers.

## Review acceptance and scope

| Review recommendation | Result |
|---|---|
| 1. Complete chart housing | Dynamic gutter/font measurement, geometry tests, native live chart proof |
| 2. Display precision | Exact source-string formatting and totals, missing values, definitions and book headings |
| 3. Unknown/empty/stale/unavailable | Independent read state, last-good retention, timestamps/errors and actual recorded activity |
| 4. Capability language/navigation | Factual status, explicit unavailable actions, working settings and persisted network confirmation |
| 5. Compact window/walkthrough | Both supported sizes, enlarged text, bounded scrolling, spotlight and native keyboard proof |
| Operator surfaces | Existing ledger/policy data connected read-only; actionable External MCP setup; current controls labelled by actual capability |
| Brand/ASCII/motion | Terrain arrival, framed wordmark, quantitative gauges, state-specific apertures, chart semantics preserved, decorative pause independent of feeds |

The review explicitly conditions operator workflows on existing backend capabilities and says “once wired” for live controls. The UI implementation is accepted against that scope. A clarification about expanding into a new runtime lifecycle received no answer; this is not treated as authorization for such expansion. Live pairing, approval execution and kill/cancel commands remain unavailable in the console, with explicit explanations. The earlier remaining-work wording incorrectly elevated those backend capabilities into unconditional UI acceptance requirements; this table corrects that interpretation.

The branch integrates main through `2ea98ab`. Main subsequently advanced to `4966e50` (persistent pairing authority); that independent backend change is not removed or overwritten by this branch. Reconcile it at merge time. No app merge, push or production release is part of this UI review handoff.

## Build recovery

Initial native packaging exhausted disk space in this worktree's generated `target` cache. The cache was removed with `cargo clean`; native packaging succeeded with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=false`. No user data or other worktrees were removed.
