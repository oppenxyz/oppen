# UI refinement implementation status

Goal: complete the recommendations in [the review](ui-review-2026-09-07.md).
Branch: `design/app-ui-refinement`, isolated worktree `oppen-app-ui` from `28fbf36`.

## Implemented, verification in progress

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
- 85 Bun tests pass, including chart width, exact rounding, account failures, out-of-order market snapshots and independent derived-feature freshness and book-level deduplication. Network-save failure preserves the existing session; decorative pause/reduced-motion/visibility leave independent data timers running; operator actions are not attributed to agents.
- Core suite after merging current main: 508 passed, 5 ignored; 10 desktop Rust tests passed, including disconnect/reconcile and the new read-only operator tests.
- `cargo clippy -p oppen-desktop -p oppen-core --all-targets -- -D warnings` passes.
- Browser checks at 1280×720: Setup scroll layout, unclipped shell, readable walkthrough spotlight, Enter advances exactly once, Tab cycles within modal, Escape restores Setup focus.

- The isolated native review bundle renders live testnet data; complete right price scale and forming label verified. No account was configured. Initial empty webview observation resolved on the next inspection.

- The rebuilt native app reads an isolated, explicitly labelled fixture through the real Rust/Tauri bridge. Agents displays exact stored caps, a refusal with observed/limit values, and literal HTML-like text without rendering markup. Builder displays the same recorded activity. Settings distinguishes stored halts from live cancel completion. Setup renders the complete terrain and framed wordmark with decorative motion paused.
- Fixture reproduction: `cargo run -p oppen-core --example ui_fixture -- /tmp/NEW-DIRECTORY`; launch the isolated review bundle with `OPPEN_DATA_DIR` pointing there. The example refuses an existing directory and never creates an engine, loads keys or contacts a venue. Public market reads in the review app remain separate. Fixtures are not testnet execution evidence.

- Integrated merged main `2ea98ab` (durable pilot budgets and stop supervision). Preserved the new banner/polling, independent operator reads, network-specific account selection, and tour inert behavior. Both local read failures use main's `local_status` error variant.

## Remaining work

- Native visual verification of live chart/book, every screen, minimum/default window geometry, and decorative motion behavior.
- Complete UI regression checks for persisted network behavior, accessible controls and populated account views.
- The gateway's in-memory pairing registry and live policy/approval/kill/dead-man controls are not yet connected to the console. Existing ledger and persisted policy reads are now implemented. Honest unavailable states and the current development setup path are implemented, but they do **not** satisfy the recommendation to wire those operator workflows. Continue with existing core/gateway read models, preserving the signing and operator-only boundaries.
- Review all changed code, remove any newly orphaned styling, document actual verification evidence and commit coherent milestones. Do not mark the overall goal complete while the workflow recommendation remains open.

## Build recovery

Initial native packaging exhausted disk space in this worktree's generated `target` cache. The cache was removed with `cargo clean`; native packaging succeeded with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=false`. No user data or other worktrees were removed.
