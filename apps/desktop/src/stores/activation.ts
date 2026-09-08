import { reactive, readonly, type DeepReadonly } from "vue";
import {
  confirmActivation, discardActivation, fetchActivationStatus, inTauri, isConsoleError, reviewActivation,
  type ActivationStatus, type McpStatus,
} from "../lib/bridge";

export interface ActivationContext {
  network: "testnet" | "mainnet";
  agent: string;
  account: string;
  blocked: string | null;
}
type Command = "review" | "confirm" | "discard";
function after(callback: () => void, ms: number): () => void {
  const timer = setTimeout(callback, ms);
  return () => clearTimeout(timer);
}
function detail(error: unknown): string {
  return isConsoleError(error) ? error.detail : error instanceof Error ? error.message : String(error);
}
const terminal = (phase: ActivationStatus["phase"]) => ["acknowledged", "refused", "uncertain", "closed"].includes(phase);
function sameOperation(a: ActivationStatus["last_operation"], b: ActivationStatus["last_operation"]): boolean {
  return a === null || b === null ? a === b : a.kind === b.kind
    && (a.kind === "review" || (b.kind !== "review" && a.review_id === b.review_id));
}

export function activationContext(network: "testnet" | "mainnet", mcp: {
  status: Readonly<McpStatus> | null; command: string | null; error: string | null;
  stopRequested: boolean; haltRequested: boolean;
}): ActivationContext | null {
  const status = mcp.status;
  if (!status?.agent || !status.account) return null;
  return { network, agent: status.agent, account: status.account,
    blocked: status.network !== "testnet" || status.phase !== "listening" || mcp.stopRequested
      ? "A listening TESTNET runtime is required; stop and startup failure require restart."
      : mcp.haltRequested || status.halt.phase !== "idle" ? "HALT has been requested. Activation does not release stops."
      : mcp.command ? "A supervision command is in progress."
      : mcp.error ? "Current supervision status is unavailable." : null };
}

export function activationReviewBlocker(status: DeepReadonly<ActivationStatus> | null, now = Date.now()): string | null {
  const display = status?.review?.display;
  if (!display) return "A complete native activation review is required.";
  const binding = display.route.binding;
  if (display.route.network !== "testnet" || display.account.network !== "testnet"
    || binding.agent !== status.agent || display.pilot.agent !== status.agent
    || [binding.container, display.account.address, display.pilot.account].some(account => account.toLowerCase() !== status.account.toLowerCase())
    || binding.wallet.address.toLowerCase() !== display.wallet_approval.address.toLowerCase()) {
    return "Review evidence does not match the supervised identity.";
  }
  if (now < display.observed_at_ms || now >= display.expires_at_ms || now >= display.wallet_approval.validUntil
    || now >= binding.wallet.valid_until_ms) return "Review expired or clock changed. A fresh native review is required.";
  return null;
}

export function createActivation(
  transport: { status: typeof fetchActivationStatus; review: typeof reviewActivation; confirm: typeof confirmActivation; discard: typeof discardActivation },
  available: () => boolean = () => true,
  schedule: typeof after = after,
) {
  const state = reactive<{
    context: ActivationContext | null; status: ActivationStatus | null;
    reading: boolean; readError: string | null; checkedAt: number | null;
    command: Command | null; commandError: string | null; outcomeUnknown: boolean;
    previousUnknown: string[];
  }>({ context: null, status: null, reading: false, readError: null, checkedAt: null,
    command: null, commandError: null, outcomeUnknown: false, previousUnknown: [] });
  let active = false;
  let generation = 0;
  let version = 0;
  let observations = 0;
  let cancelPoll: (() => void) | null = null;
  let pending: { owner: string; seq: number; operation: NonNullable<ActivationStatus["last_operation"]> } | null = null;
  const retiredOwners = new Set<string>();

  function setContext(context: ActivationContext | null): void {
    if (JSON.stringify(state.context) === JSON.stringify(context)) return;
    state.context = context ? { ...context } : null;
    generation += 1;
    version += 1;
    state.readError = "Current runtime binding must be observed again.";
    void refresh();
  }
  function matches(status: ActivationStatus): boolean {
    return !!state.context && state.context.network === "testnet"
      && status.agent === state.context.agent && status.account.toLowerCase() === state.context.account.toLowerCase()
      && status.owner_id.length > 0;
  }
  function accept(status: ActivationStatus): boolean {
    if (!matches(status)) throw new Error("Activation status does not match the supervised TESTNET binding.");
    if (!Number.isSafeInteger(status.operation_seq) || status.operation_seq < 0) throw new Error("Activation operation sequence is unavailable.");
    if (retiredOwners.has(status.owner_id)) return false;
    const previous = state.status;
    if (previous?.owner_id === status.owner_id) {
      if (status.operation_seq < previous.operation_seq) return false;
      if (previous.phase === "closed" && status.phase !== "closed") return false;
      if (status.operation_seq === previous.operation_seq) {
        if (!sameOperation(previous.last_operation, status.last_operation)) return false;
        if (terminal(previous.phase) && status.phase !== previous.phase && status.phase !== "closed") return false;
        if (previous.phase === "review_ready" && status.phase === "reviewing") return false;
      }
    } else if (previous) {
      retiredOwners.add(previous.owner_id);
    }
    if (pending && pending.owner !== status.owner_id) {
      state.previousUnknown.push(`Previous owner ${pending.owner}: ${pending.operation.kind} operation ${pending.seq} outcome remains unknown. Replacement does not verify its completion.`);
      pending = null;
      state.outcomeUnknown = false;
      state.commandError = null;
    }
    state.status = status;
    state.checkedAt = Date.now();
    observations += 1;
    if (pending && pending.owner === status.owner_id && pending.seq === status.operation_seq
      && sameOperation(pending.operation, status.last_operation)
      && (["refused", "uncertain"].includes(status.phase)
        || (pending.operation.kind === "review" && status.phase === "review_ready")
        || (pending.operation.kind === "confirm" && status.phase === "acknowledged")
        || (pending.operation.kind === "discard" && status.phase === "idle"))) {
      pending = null;
      state.outcomeUnknown = false;
    }
    return true;
  }
  async function refresh(): Promise<void> {
    const context = state.context;
    if (!active || state.reading || !available() || !context || context.network !== "testnet" || context.blocked) return;
    state.reading = true;
    const owner = generation;
    const started = version;
    let expired = false;
    const cancelDeadline = schedule(() => {
      expired = true;
      if (active && owner === generation && started === version) state.readError = "Activation status has not responded within 5 seconds. Last observed evidence retained.";
    }, 5_000);
    try {
      const status = await transport.status(context.agent, context.account);
      if (!active || owner !== generation || started !== version || expired) return;
      if (!accept(status)) return;
      state.readError = null;
    } catch (error) {
      if (active && owner === generation && started === version && !expired) state.readError = detail(error);
    } finally {
      cancelDeadline();
      state.reading = false;
      if (active && (owner !== generation || started !== version || expired)) void refresh();
    }
  }
  function poll(): void {
    cancelPoll = schedule(() => {
      if (!active) return;
      void refresh();
      poll();
    }, 1_000);
  }
  function startPolling(): void {
    if (active || !available()) return;
    active = true;
    generation += 1;
    void refresh();
    poll();
  }
  function stopPolling(): void {
    active = false;
    generation += 1;
    cancelPoll?.();
    state.readError = "Activation status polling is stopped.";
  }
  function blocker(): string | null {
    if (!available()) return "Activation requires the desktop runtime.";
    if (!state.context || state.context.network !== "testnet") return "A supervised TESTNET agent/account is required.";
    if (state.context.blocked) return state.context.blocked;
    if (state.command) return "An activation command is awaiting its reply.";
    if (state.outcomeUnknown) return "Command outcome unknown. Await native status; no automatic retry was submitted.";
    if (!state.status || !matches(state.status) || state.readError) return "Read current activation status before continuing.";
    if (state.status.phase === "closed") return "Activation owner closed. Runtime restart is required.";
    if (state.status.phase === "uncertain") return "Activation outcome uncertain. Retained evidence does not confirm completion.";
    if (["reviewing", "confirming"].includes(state.status.phase)) return "Native activation work is in progress.";
    return null;
  }
  async function command(kind: Command, invoke: () => Promise<ActivationStatus>): Promise<void> {
    const current = state.status!;
    if (!Number.isSafeInteger(current.operation_seq + 1)) { state.commandError = "Activation operation sequence exhausted."; return; }
    state.command = kind;
    state.commandError = null;
    state.outcomeUnknown = true;
    pending = { owner: current.owner_id, seq: current.operation_seq + 1,
      operation: kind === "review" ? { kind } : { kind, review_id: current.review!.id } };
    version += 1;
    const owner = generation;
    const observed = observations;
    try {
      const status = await invoke();
      if (owner === generation && observations === observed && status.owner_id === current.owner_id) {
        accept(status);
      }
    } catch (error) {
      if (owner === generation) {
        state.commandError = detail(error);
        if (pending?.owner === current.owner_id) state.readError = "Command reply failed. Awaiting fresh native status.";
      }
    } finally {
      version += 1;
      state.command = null;
      void refresh();
    }
  }
  async function review(): Promise<void> {
    const blocked = blocker();
    if (blocked) { state.commandError = blocked; return; }
    if (!state.status || !["idle", "refused", "acknowledged"].includes(state.status.phase)) {
      state.commandError = "A fresh activation review is not available in the current state."; return;
    }
    const context = state.context!;
    await command("review", () => transport.review(context.agent, context.account));
  }
  async function reviewedCommand(kind: "confirm" | "discard", ownerId: string, reviewId: string, accountConfirmed: boolean): Promise<void> {
    const blocked = blocker();
    if (blocked) { state.commandError = blocked; return; }
    const status = state.status!;
    if (status.phase !== "review_ready" || status.owner_id !== ownerId || status.review?.id !== reviewId
      || (kind === "confirm" && !accountConfirmed)) {
      state.commandError = "The current owner, review and explicit account confirmation are required."; return;
    }
    if (kind === "confirm") {
      const evidenceError = activationReviewBlocker(status);
      if (evidenceError) { state.commandError = evidenceError; return; }
    }
    const context = state.context!;
    await command(kind, () => transport[kind](context.agent, context.account, ownerId, reviewId));
  }
  return { state: readonly(state), setContext, refresh, startPolling, stopPolling, blocker, review,
    confirm: (ownerId: string, reviewId: string, accountConfirmed: boolean) => reviewedCommand("confirm", ownerId, reviewId, accountConfirmed),
    discard: (ownerId: string, reviewId: string) => reviewedCommand("discard", ownerId, reviewId, false) };
}

export const activation = createActivation({ status: fetchActivationStatus, review: reviewActivation, confirm: confirmActivation, discard: discardActivation }, inTauri);
