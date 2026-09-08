// UI fixture transport only. Every read is local; no invoke, venue, key or signer call.
import type { AccountState, ChartSeries, FeedBinding, MarketSnapshot, McpStatus, OperatorRead, RuntimeStatus } from "../src/lib/bridge";
export type * from "../src/lib/bridge";
export { isConsoleError } from "../src/lib/bridge";
export { fetchPolicySetupStatus, reviewPolicySetup, persistPolicySetup, discardPolicySetup } from "./policy-setup";
export { fetchApprovalQueueStatus, refreshApprovalQueue, rejectApprovalProposal, prepareApprovalReview, confirmApprovalReview, discardApprovalReview } from "./approvals";
export { fetchActivationStatus, reviewActivation, confirmActivation, discardActivation } from "./activation";
export { fetchKillReleaseStatus, reviewKillRelease, confirmKillRelease, discardKillRelease, reconcileKillRelease } from "./release";
import { connectReleaseFixture } from "./release";
import { connectActivationFixture } from "./activation";
export let failedRead = false;
export function failNextReads(): void { failedRead = true; }
const scenario = new URLSearchParams(location.search).get("state") ?? "positions";
const symbols = ["BTC", "ETH", "kPEPE", "SOL", "AVAX", "HYPE", "DOGE", "ARB", "LONG-MARKET-NAME", "SUI", "TIA", "PENDLE"];
const account: AccountState = {
  contract_version: 1, network: "testnet", address: "0x0000000000000000000000000000000000000001",
  as_of_ms: Date.now(), feed_age_ms: 0, feed: "live",
  balances: { equity_usd: "123456.78", perps_account_value_usd: "123000", spot_usdc_available: "456.78", total_margin_used_usd: "2400", withdrawable_usd: "120600" },
  positions: symbols.map((symbol, index) => ({
    symbol, size: index % 2 ? "-0.12345678" : "12345.678901",
    entry_px: index === 2 ? "0.0000012345" : "1234.56789",
    position_value_usd: `${(index + 1) * 1000}.12`, unrealized_pnl_usd: index % 2 ? "-12.345" : "24.56789",
    margin_used_usd: "200", liquidation_px: index === 0 ? null : "987.654321",
    liq_distance_frac: index === 0 ? null : "0.123456789", max_leverage: 20,
  })),
  orders: symbols.map((symbol, index) => ({ symbol, oid: 1000 + index, cloid: null, is_buy: index % 2 === 0,
    limit_px: "1234.56789", size: "0.123456789", original_size: "0.123456789", reduce_only: index % 2 === 1,
    is_trigger: false, trigger_px: null, placed_ts_ms: Date.now() })),
};
export const inTauri = () => true;
export async function fetchAccountState(): Promise<AccountState> {
  if (failedRead || scenario === "unavailable") throw { kind: "venue", detail: "UI fixture: account read failed. This intentionally long diagnostic must remain readable and must not hide the last successful positions or change them into zero balances." };
  return { ...account, as_of_ms: Date.now(), ...(scenario === "empty" ? { positions: [], orders: [] } : {}) };
}
export async function fetchKeychainStatus() { return { reachable: false, detail: "UI fixture: no keychain is accessed." }; }
export async function fetchPilotStatus() { return null; }
export async function fetchOperatorState(): Promise<OperatorRead> {
  if (scenario === "loading") await new Promise(resolve => setTimeout(resolve, 10000));
  return { network: "testnet", policy: { status: "unavailable", detail: "UI fixture: no live operator connection." },
    ledger: { status: "ready", value: { events: [], head_seq: 0, next_cursor: 0, resync_required: false } } };
}
export async function fetchMarkets() { return []; }
export async function fetchChartSeries(): Promise<ChartSeries> { throw new Error("UI fixture: chart not supplied."); }
export async function fetchMarketSnapshot(): Promise<MarketSnapshot> { throw new Error("UI fixture: market snapshot not supplied."); }
export async function watchMarket(network: FeedBinding["network"]): Promise<FeedBinding> { return { network, generation: "1" }; }
export async function onFeedUpdate() { return () => {}; }

let stoppedRuntime: RuntimeStatus | null = null;
export async function fetchRuntimeStatus(): Promise<RuntimeStatus> {
  if (stoppedRuntime) return { ...stoppedRuntime };
  const requested = new URLSearchParams(location.search).get("runtime");
  const phase: RuntimeStatus["phase"] = requested === "stopping" || requested === "stopped_with_error" || requested === "replacing" || requested === "stopped" ? requested : "running";
  const detail = phase === "stopped_with_error"
    ? "UI fixture: all desktop tasks finished, but a feed consumer reported an error. Update installation was not started. Diagnostic="
    : "UI fixture: waiting for retained desktop work to finish before shutdown or replacement. Update installation has not started. Diagnostic=";
  return {
    phase,
    binding: phase === "running" ? { network: "testnet", generation: "1" } : null,
    detail: phase === "running" || phase === "stopped" ? null : detail + "retained-task-context/".repeat(24),
  };
}

const haltScenario = new URLSearchParams(location.search).get("halt");
const releaseScenario = new URLSearchParams(location.search).get("release");
const mcpScenario = new URLSearchParams(location.search).get("mcp") ?? (haltScenario || releaseScenario ? "listening" : "idle");
let mcpStatus: McpStatus = {
  halt: { owner_id: "fixture-halt-1", stop_generation: 0, released_stop_generation: null, released_engine_stop_generation: null, previous: null, phase: "idle", cancellation: "not_requested", requested_at_ms: null, durable_revision: null, error: null, cancellation_error: null },
  policy_status: null, cached_effective_kill: null,
  phase: mcpScenario === "listening" || mcpScenario === "sweep_error" ? "listening" : mcpScenario === "failed" ? "failed" : "idle",
  network: "testnet", agent: null, account: null, listener: null,
  reconciled: null, account_feeds_ready: null, orders_inhibited: true,
  supervision_last_completed_ms: null, supervision_in_progress: false, supervision_error: null,
  detail: mcpScenario === "failed" ? "UI fixture: existing pilot authorization is missing. No authority or keys were created." : null,
};
if (mcpStatus.phase === "listening") {
  mcpStatus = { ...mcpStatus, agent: "fixture-agent", account: account.address, listener: "127.0.0.1:7433", reconciled: true, account_feeds_ready: true };
}
if (mcpScenario === "sweep_error") {
  mcpStatus.supervision_last_completed_ms = 1_788_998_400_000;
  mcpStatus.supervision_error = "UI fixture: pause cancellation failed; next sweep will retry. " + "retained-cancel-error/".repeat(20);
}
function copyMcpStatus(): McpStatus { return { ...mcpStatus, halt: { ...mcpStatus.halt } }; }
export async function fetchMcpStatus(): Promise<McpStatus> { return copyMcpStatus(); }
export async function startMcp(agent: string, requestedAccount: string): Promise<McpStatus> {
  if (stoppedRuntime) throw { detail: "UI fixture: runtime shutdown is terminal. Restart required." };
  if (mcpScenario === "failed" || requestedAccount !== account.address) throw { detail: "UI fixture: existing authorized pilot/account setup does not match. No setup was created." };
  mcpStatus = { ...mcpStatus, phase: "listening", agent, account: requestedAccount, listener: "127.0.0.1:7433", orders_inhibited: true };
  return copyMcpStatus();
}
export async function stopMcp(): Promise<RuntimeStatus> {
  mcpStatus = { ...mcpStatus, phase: "stopped", listener: null, reconciled: null, account_feeds_ready: null, supervision_in_progress: false };
  stoppedRuntime = { phase: "stopped", binding: null, detail: "UI fixture: desktop tasks stopped. Restart required." };
  return { ...stoppedRuntime };
}

/** Local visual-test control only; no native or venue call. */
export function setHaltFixture(phase: "persisting" | "persisted" | "uncertain" | "retrying" | "acknowledged"): void {
  mcpStatus.halt = {
    owner_id: mcpStatus.halt.owner_id, stop_generation: mcpStatus.halt.stop_generation || 1,
    released_stop_generation: null, released_engine_stop_generation: null, previous: mcpStatus.halt.previous,
    phase: phase === "retrying" || phase === "acknowledged" ? "persisted" : phase,
    cancellation: phase === "retrying" ? "retrying" : phase === "acknowledged" ? "acknowledged" : "pending",
    requested_at_ms: 1_788_998_400_000,
    durable_revision: phase === "persisting" || phase === "uncertain" ? null : 42,
    error: phase === "uncertain" ? "UI fixture: policy commit outcome is uncertain. " + "retained-authority-error/".repeat(16) : null,
    cancellation_error: phase === "retrying" ? "UI fixture: cancellation failed; supervision will retry. " + "cancel-evidence/".repeat(16) : null,
  };
  mcpStatus.orders_inhibited = true;
  mcpStatus.policy_status = { cached_revision: mcpStatus.policy_status?.cached_revision ?? 42, acknowledgment: null, stop_generation: mcpStatus.policy_status?.stop_generation ?? 7, admission_inhibited: true };
  mcpStatus.cached_effective_kill = { global: null, agents: { "fixture-agent": { engaged_at_ms: 1_788_998_400_000, reason: { reason: "operator" } } } };
}
if (haltScenario === "persisting" || haltScenario === "persisted" || haltScenario === "uncertain" || haltScenario === "retrying" || haltScenario === "acknowledged") setHaltFixture(haltScenario);
if (releaseScenario && releaseScenario !== "idle") {
  setHaltFixture("acknowledged");
  if (releaseScenario !== "rearm") mcpStatus.cached_effective_kill!.global = { engaged_at_ms: 1_788_998_399_000, reason: { reason: "operator" } };
}

export async function haltMcp(agent: string, requestedAccount: string): Promise<McpStatus> {
  if (stoppedRuntime || mcpStatus.phase !== "listening" || agent !== mcpStatus.agent || requestedAccount !== mcpStatus.account) {
    throw { detail: "UI fixture: no matching listening runtime binding." };
  }
  const { owner_id: _owner, durable_revision: _revision, previous: _previous, released_stop_generation: _released, released_engine_stop_generation: _engineReleased, ...previous } = mcpStatus.halt;
  mcpStatus.halt.previous = previous;
  mcpStatus.halt.stop_generation += 1;
  if (mcpStatus.policy_status) mcpStatus.policy_status.stop_generation += 1;
  setHaltFixture("persisted");
  return copyMcpStatus();
}
connectReleaseFixture(() => ({ generation: mcpStatus.policy_status?.stop_generation ?? 0, kill: structuredClone(mcpStatus.cached_effective_kill ?? { global: null, agents: {} }) }), status => {
  const receipt = status.receipt;
  if (!receipt || receipt.reviewed_stop_generation !== mcpStatus.policy_status?.stop_generation) return;
  mcpStatus.policy_status = structuredClone(status.policy_status);
  mcpStatus.cached_effective_kill = structuredClone(status.cached_effective_kill);
  if (!status.cached_effective_kill.global && Object.keys(status.cached_effective_kill.agents).length === 0) {
    mcpStatus.halt.released_stop_generation = mcpStatus.halt.stop_generation;
    mcpStatus.halt.released_engine_stop_generation = receipt.reviewed_stop_generation;
  }
});
connectActivationFixture({
  read: () => ({ policy: structuredClone(mcpStatus.policy_status ?? { cached_revision: 42, acknowledgment: null,
    stop_generation: mcpStatus.halt.stop_generation, admission_inhibited: true }),
    blocked: mcpStatus.phase !== "listening" || !!mcpStatus.cached_effective_kill?.global || !!mcpStatus.cached_effective_kill?.agents[mcpStatus.agent ?? ""] }),
  publish: policy => { mcpStatus.policy_status = structuredClone(policy); mcpStatus.orders_inhibited = policy.admission_inhibited; },
});

export async function checkUpdate() { throw { kind: "unavailable", detail: "Updates are unavailable in the UI fixture." }; }
export async function downloadUpdate() { throw new Error("UI fixture: no updates."); }
export async function installUpdate() { throw new Error("UI fixture: no installation."); }
