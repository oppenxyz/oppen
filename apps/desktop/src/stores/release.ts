import { reactive, readonly, type DeepReadonly } from "vue";
import type { KillScope, ReleaseOperation, ReleaseStatus } from "../lib/bridge";
import { fetchKillReleaseStatus, reviewKillRelease, confirmKillRelease, discardKillRelease, reconcileKillRelease, inTauri } from "../lib/bridge";
import type { ActivationContext } from "./activation";

export interface ReleaseTransport {
  status(agent: string, account: string): Promise<ReleaseStatus>;
  review(agent: string, account: string, scope: KillScope): Promise<ReleaseStatus>;
  confirm(agent: string, account: string, ownerId: string, reviewId: string): Promise<ReleaseStatus>;
  discard(agent: string, account: string, ownerId: string, reviewId: string): Promise<ReleaseStatus>;
  reconcile(agent: string, account: string, ownerId: string, operationId: string): Promise<ReleaseStatus>;
}
const sameScope = (a: KillScope, b: KillScope) => a.scope === b.scope && (a.scope === "global" || (b.scope === "agent" && a.agent === b.agent));
function sameOperation(a: ReleaseOperation | null, b: ReleaseOperation | null): boolean {
  if (!a || !b) return a === b;
  if (a.kind !== b.kind) return false;
  if (a.kind === "review" && b.kind === "review") return sameScope(a.scope, b.scope);
  if (a.kind === "reconcile" && b.kind === "reconcile") return a.operation_id === b.operation_id;
  return "review_id" in a && "review_id" in b && a.review_id === b.review_id;
}
const busy = (phase: ReleaseStatus["phase"]) => ["reviewing", "confirming", "reconciling"].includes(phase);
function message(error: unknown): string {
  if (typeof error === "object" && error && "detail" in error && typeof error.detail === "string") return error.detail;
  return error instanceof Error ? error.message : String(error);
}
function after(callback: () => void, ms: number) { const timer = setTimeout(callback, ms); return () => clearTimeout(timer); }

export function releaseReviewBlocker(status: DeepReadonly<ReleaseStatus> | null, now = Date.now()): string | null {
  const display = status?.review?.display;
  if (!display) return "A current native release review is required.";
  if (display.network !== "testnet" || !display.operation_id || !display.affected.length
    || (display.scope.scope === "agent" && display.scope.agent !== status.agent)) return "Release scope or affected membership is unresolved.";
  const members = new Set<string>();
  for (const member of display.affected) {
    const binding = member.route.binding;
    if (member.route.network !== "testnet" || members.has(binding.agent) || member.pilot.agent !== binding.agent
      || member.pilot.account.toLowerCase() !== (binding.vault_address ?? binding.container).toLowerCase()
      || (display.scope.scope === "agent" && binding.agent !== status.agent)) return "Release membership evidence is inconsistent.";
    members.add(binding.agent);
  }
  if (now < display.reviewed_at_ms || now >= display.expires_at_ms) return "Release review expired or clock changed. Discard and review again.";
  return null;
}

/** Retained IPC observers only. A release outcome never changes supervision or activation state. */
export function createRelease(transport: ReleaseTransport, schedule = after) {
  const state = reactive<{
    context: ActivationContext | null; status: ReleaseStatus | null; reading: boolean; readError: string | null;
    command: ReleaseOperation["kind"] | null; commandError: string | null; outcomeUnknown: boolean;
    previousUnknown: string[]; unresolvedOperationId: string | null;
  }>({ context: null, status: null, reading: false, readError: null, command: null, commandError: null,
    outcomeUnknown: false, previousUnknown: [], unresolvedOperationId: null });
  let active = false, generation = 0, version = 0, observations = 0;
  let pending: { owner: string; seq: number; operation: ReleaseOperation } | null = null;
  let commandToken: symbol | null = null;
  let cancelPoll: (() => void) | null = null;
  const retired = new Set<string>();
  function accept(status: ReleaseStatus): boolean {
    const context = state.context;
    if (!context || context.network !== "testnet" || status.agent !== context.agent
      || status.account.toLowerCase() !== context.account.toLowerCase() || !status.owner_id
      || !Number.isSafeInteger(status.operation_seq) || status.operation_seq < 0) throw new Error("Release status does not match the current TESTNET owner.");
    if (retired.has(status.owner_id)) return false;
    const previous = state.status;
    if (previous?.owner_id === status.owner_id) {
      if (status.operation_seq < previous.operation_seq || (previous.phase === "closed" && status.phase !== "closed")) return false;
      if (status.operation_seq === previous.operation_seq && (!sameOperation(previous.last_operation, status.last_operation)
        || (!busy(previous.phase) && busy(status.phase)))) return false;
    } else if (previous) retired.add(previous.owner_id);
    if (pending && pending.owner !== status.owner_id) {
      state.previousUnknown.push(`Owner ${pending.owner}, operation ${pending.seq}: ${pending.operation.kind} outcome remains unknown. Replacement does not verify completion.`);
      pending = null; commandToken = null; state.command = null; state.outcomeUnknown = false; state.commandError = null;
    }
    state.status = status; observations++;
    if (status.error?.status === "uncertain" && status.error.operation_id) state.unresolvedOperationId = status.error.operation_id;
    if (pending?.owner === status.owner_id && pending.seq === status.operation_seq && sameOperation(pending.operation, status.last_operation)) {
      const kind = pending.operation.kind;
      if (["refused", "uncertain"].includes(status.phase) || (kind === "review" && status.phase === "review_ready")
        || (kind === "confirm" && status.phase === "released") || (kind === "discard" && status.phase === "idle")
        || (kind === "reconcile" && status.resolution !== null)) {
        pending = null; commandToken = null; state.command = null; state.outcomeUnknown = false;
      }
    }
    return true;
  }
  async function refresh(): Promise<void> {
    const context = state.context;
    if (!active || state.reading || !context || context.network !== "testnet" || context.blocked) return;
    state.reading = true;
    const owner = generation, start = version;
    let expired = false;
    const cancel = schedule(() => { expired = true; if (active && owner === generation && start === version) state.readError = "Release status has not responded within 5 seconds. Evidence retained."; }, 5000);
    try {
      const status = await transport.status(context.agent, context.account);
      if (active && owner === generation && start === version && !expired && accept(status)) state.readError = null;
    } catch (error) { if (active && owner === generation && start === version && !expired) state.readError = message(error); }
    finally { cancel(); state.reading = false; if (active && (owner !== generation || start !== version || expired)) void refresh(); }
  }
  function setContext(context: ActivationContext | null) {
    if (JSON.stringify(context) === JSON.stringify(state.context)) return;
    state.context = context; generation++; version++; state.readError = "Read the current release owner before continuing."; void refresh();
  }
  function poll() { cancelPoll = schedule(() => { if (active) { void refresh(); poll(); } }, 1000); }
  function startPolling() { if (!active) { active = true; generation++; void refresh(); poll(); } }
  function stopPolling() { active = false; generation++; cancelPoll?.(); state.readError = "Release polling stopped."; }
  function blocker(reconcile = false): string | null {
    if (!state.context || state.context.network !== "testnet") return "A supervised TESTNET owner is required.";
    if (state.context.blocked) return state.context.blocked;
    if (!state.status || state.readError) return "Read current release status before continuing.";
    if (state.command || busy(state.status.phase)) return "Release work is still observed in progress. HALT remains separate.";
    if (state.status.phase === "closed") return "Release owner closed.";
    if (!reconcile && (state.outcomeUnknown || state.status.phase === "uncertain")) return "Release outcome unresolved. Only read-only reconciliation is available.";
    return null;
  }
  async function run(operation: ReleaseOperation, invoke: () => Promise<ReleaseStatus>) {
    const current = state.status!;
    if (!Number.isSafeInteger(current.operation_seq + 1)) { state.commandError = "Release sequence exhausted."; return; }
    const owner = generation, observed = observations;
    const token = Symbol(operation.kind);
    commandToken = token;
    pending = { owner: current.owner_id, seq: current.operation_seq + 1, operation };
    state.command = operation.kind; state.outcomeUnknown = true; state.commandError = null; version++;
    try { const status = await invoke(); if (commandToken === token && owner === generation && observed === observations && status.owner_id === current.owner_id) accept(status); }
    catch (error) { if (commandToken === token && owner === generation) { state.commandError = message(error); if (pending) state.readError = "Command reply unavailable; read current native status."; } }
    finally {
      if (commandToken === token) { commandToken = null; version++; state.command = null; void refresh(); }
      else if (commandToken === null) void refresh();
    }
  }
  async function review(scope: KillScope) {
    const blocked = blocker();
    if (blocked) { state.commandError = blocked; return; }
    if (state.status!.review || (scope.scope === "agent" && scope.agent !== state.context!.agent)) { state.commandError = "Discard the retained review; scope must match the supervised agent or global."; return; }
    const { agent, account } = state.context!;
    await run({ kind: "review", scope }, () => transport.review(agent, account, scope));
  }
  async function reviewed(kind: "confirm" | "discard", ownerId: string, reviewId: string, confirmed: boolean) {
    const blocked = blocker();
    if (blocked) { state.commandError = blocked; return; }
    const status = state.status!;
    if (status.owner_id !== ownerId || status.review?.id !== reviewId || status.phase !== "review_ready" || (kind === "confirm" && !confirmed)) { state.commandError = "Confirm the exact current scope, owner and review."; return; }
    if (kind === "confirm") {
      const error = releaseReviewBlocker(status); if (error) { state.commandError = error; return; }
      state.unresolvedOperationId = status.review.display.operation_id;
    }
    const { agent, account } = state.context!;
    await run({ kind, review_id: reviewId }, () => transport[kind](agent, account, ownerId, reviewId));
  }
  async function reconcile(operationId: string) {
    const blocked = blocker(true);
    if (blocked) { state.commandError = blocked; return; }
    if (!operationId.trim() || state.status!.review) { state.commandError = "An exact operation ID and no retained review are required."; return; }
    const { agent, account } = state.context!, owner = state.status!.owner_id;
    await run({ kind: "reconcile", operation_id: operationId }, () => transport.reconcile(agent, account, owner, operationId));
  }
  return { state: readonly(state), setContext, startPolling, stopPolling, refresh, blocker, review, reconcile,
    confirm: (owner: string, review: string, confirmed: boolean) => reviewed("confirm", owner, review, confirmed),
    discard: (owner: string, review: string) => reviewed("discard", owner, review, false) };
}
export type ReleaseController = ReturnType<typeof createRelease>;
export const release = createRelease({ status: fetchKillReleaseStatus, review: reviewKillRelease,
  confirm: confirmKillRelease, discard: discardKillRelease, reconcile: reconcileKillRelease });
export function releaseContext(network: ActivationContext["network"], mcp: {
  status: { network: ActivationContext["network"]; agent: string | null; account: string | null; phase: string } | null;
  command: string | null; stopRequested: boolean; error: string | null;
}): ActivationContext | null {
  const status = mcp.status;
  if (!status?.agent || !status.account) return null;
  return { network, agent: status.agent, account: status.account, blocked: !inTauri() ? "Release requires the desktop runtime."
    : status.network !== "testnet" || status.phase !== "listening" || mcp.stopRequested || mcp.command
      ? "A listening TESTNET runtime is required." : mcp.error ? "Current supervision identity is unavailable." : null };
}
