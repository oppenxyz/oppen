import { reactive, readonly } from "vue";
import { fetchMcpStatus, haltMcp, inTauri, isConsoleError, startMcp, stopMcp, type McpStatus, type RuntimeStatus } from "../lib/bridge";

function detail(error: unknown): string {
  if (typeof error === "object" && error !== null && "detail" in error && typeof error.detail === "string") return error.detail;
  return error instanceof Error ? error.message : String(error);
}

function after(callback: () => void, ms: number): () => void {
  const timer = setTimeout(callback, ms);
  return () => clearTimeout(timer);
}

function terminal(status: Readonly<McpStatus> | null): boolean {
  return status?.phase === "failed" || status?.phase === "stopping" || status?.phase === "stopped";
}
export function haltReleased(status: Readonly<McpStatus> | null): boolean {
  if (!status?.agent || status.network !== "testnet" || status.phase !== "listening") return false;
  const marker = status.halt.released_stop_generation;
  const releasedEngine = status.halt.released_engine_stop_generation;
  const currentEngine = status.policy_status?.stop_generation;
  return marker != null && marker === status.halt.stop_generation
    && releasedEngine != null && currentEngine != null && Number.isSafeInteger(releasedEngine)
    && Number.isSafeInteger(currentEngine) && releasedEngine >= 0 && releasedEngine <= currentEngine
    && status.cached_effective_kill != null && status.cached_effective_kill.global === null
    && !status.cached_effective_kill.agents[status.agent];
}

export function supervisionInputError(network: McpStatus["network"], agent: string, account: string): string | null {
  if (network !== "testnet") return "Supervision can only start from TESTNET. The network will not be switched automatically.";
  if (!agent.trim()) return "Enter the agent ID from the existing pilot authorization.";
  if (!/^0x[0-9a-fA-F]{40}$/.test(account.trim())) return "Enter the full 0x account address from the existing pilot authorization.";
  return null;
}

/** Read-only polling never starts supervision. Explicit commands retain ownership across remounts. */
export function createSupervision(
  transport: { status: typeof fetchMcpStatus; start: typeof startMcp; stop: typeof stopMcp; halt: typeof haltMcp },
  available: () => boolean = () => true,
  schedule: typeof after = after,
) {
  const state = reactive<{
    status: McpStatus | null; error: string | null; commandError: string | null;
    command: "start" | "stop" | null; reading: boolean; checkedAt: number | null;
    stopRequested: boolean; runtime: RuntimeStatus | null;
    haltPending: boolean; haltRequested: boolean; haltNotAdmitted: boolean; haltError: string | null;
    releaseActive: boolean;
  }>({ status: null, error: null, commandError: null, command: null, reading: false, checkedAt: null, stopRequested: false, runtime: null, haltPending: false, haltRequested: false, haltNotAdmitted: false, haltError: null, releaseActive: false });
  let active = false;
  let generation = 0;
  let version = 0;
  let observations = 0;
  let rejectedHalt: { agent: string; account: string; owner: string; generation: number } | null = null;
  let starting = false;
  let stopping = false;
  let haltToken: symbol | null = null;
  let haltFence: { owner: string; minimum: number } | null = null;
  const retiredOwners = new Set<string>();
  function acceptStatus(status: McpStatus): boolean {
    const previous = state.status;
    if (retiredOwners.has(status.halt.owner_id)) return false;
    if (previous?.halt.owner_id && previous.halt.owner_id === status.halt.owner_id) {
      if (status.halt.stop_generation < previous.halt.stop_generation) return false;
      if (previous.policy_status && status.policy_status && status.policy_status.stop_generation < previous.policy_status.stop_generation) return false;
      if (terminal(previous) && !terminal(status)) return false;
    } else if (previous?.halt.owner_id) retiredOwners.add(previous.halt.owner_id);
    state.status = status;
    if (haltReleased(status) && (!haltFence || (haltFence.owner === status.halt.owner_id && status.halt.stop_generation >= haltFence.minimum))) {
      state.haltRequested = false; state.haltPending = false; state.haltError = null; state.haltNotAdmitted = false;
      haltToken = null; haltFence = null; rejectedHalt = null;
    }
    return true;
  }
  let cancelPoll: (() => void) | null = null;
  let cancelDeadline: (() => void) | null = null;

  async function refresh(): Promise<void> {
    if (!active || state.reading || !available()) return;
    const owner = generation;
    const observed = version;
    let expired = false;
    state.reading = true;
    cancelDeadline = schedule(() => {
      expired = true;
      if (active && owner === generation && observed === version) state.error = "Supervision status has not responded within 5 seconds.";
    }, 5_000);
    try {
      const status = await transport.status();
      if (!active || owner !== generation || observed !== version || expired) return;
      if (!acceptStatus(status)) return;
      observations += 1;
      state.error = null;
      state.checkedAt = Date.now();
      if (rejectedHalt !== null && !state.stopRequested && state.command !== "start"
        && status.network === "testnet" && status.phase === "listening" && (status.halt.phase === "idle" || haltReleased(status))
        && status.halt.owner_id === rejectedHalt.owner && status.halt.stop_generation === rejectedHalt.generation
        && status.agent === rejectedHalt.agent && status.account === rejectedHalt.account) {
        state.haltRequested = false;
        haltFence = null;
        rejectedHalt = null;
      }
    } catch (error) {
      if (active && owner === generation && observed === version && !expired) state.error = detail(error);
    } finally {
      state.reading = false;
      cancelDeadline?.();
      cancelDeadline = null;
      if (active && (owner !== generation || observed !== version || expired)) void refresh();
    }
  }

  function poll(): void {
    const owner = generation;
    cancelPoll = schedule(() => {
      if (!active || owner !== generation) return;
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
    cancelPoll = null;
    // Keep the deadline and actual read owned until completion, like commands.
    state.error = "Supervision status polling is stopped.";
  }

  async function start(network: McpStatus["network"], agent: string, account: string): Promise<void> {
    if (state.command !== null) return;
    state.commandError = supervisionInputError(network, agent, account);
    if (state.commandError !== null) return;
    if (!available()) { state.commandError = "Supervision is available only in the desktop app."; return; }
    if (state.stopRequested || terminal(state.status)) {
      state.commandError = "Runtime shutdown is terminal. Restart the app before starting supervision.";
      return;
    }
    if (state.status === null || state.error !== null || state.status.phase !== "idle") {
      state.commandError = "Read the current supervision status before starting; an existing start or listener cannot be replaced here.";
      return;
    }
    state.command = "start";
    starting = true;
    version += 1;
    try {
      const status = await transport.start(agent.trim(), account.trim());
      // Polling may have observed terminal admission closure while start awaited.
      if (!state.stopRequested && !terminal(state.status)) {
        state.status = status;
        state.error = null;
        state.checkedAt = Date.now();
      }
    } catch (error) {
      if (!state.stopRequested && !terminal(state.status)) {
        state.commandError = detail(error);
        state.error = "Start failed. Awaiting current runtime status; terminal failure requires restarting the app.";
      }
    }
    finally { version += 1; starting = false; state.command = stopping ? "stop" : null; void refresh(); }
  }

  async function stop(): Promise<void> {
    if (stopping || state.stopRequested) return;
    if (!available()) { state.commandError = "Runtime shutdown is available only in the desktop app."; return; }
    state.stopRequested = true;
    state.command = "stop";
    stopping = true;
    state.commandError = null;
    version += 1;
    try { state.runtime = await transport.stop(); }
    catch (error) { state.commandError = detail(error); }
    finally { version += 1; stopping = false; state.command = starting ? "start" : null; void refresh(); }
  }

  function haltBlocker(network: McpStatus["network"]): string | null {
    if (!available()) return "Agent halt is available only in the desktop app.";
    if (network !== "testnet" || state.status?.network !== "testnet") return "Agent halt requires the bound TESTNET runtime.";
    if (state.stopRequested || terminal(state.status)) return "Runtime admission is closed; agent halt is unavailable.";
    if (state.command === "start") return "Runtime startup has not completed; agent halt is not yet available.";
    if (state.status?.phase !== "listening" || !state.status.agent || !state.status.account) return "No listening runtime agent/account binding has been read.";
    if (state.error !== null) return "Current runtime binding is unavailable; halt has not been submitted.";
    if (!state.releaseActive && (state.haltPending || state.haltRequested || (state.status.halt?.phase !== "idle" && !haltReleased(state.status)))) return "A halt has already been requested; inspect persistence and cancellation evidence.";
    return null;
  }

  async function halt(network: McpStatus["network"], confirmed: { agent: string; account: string }): Promise<void> {
    if (!state.releaseActive && (state.haltPending || state.haltRequested)) return;
    state.haltError = haltBlocker(network);
    if (state.haltError !== null) return;
    const binding = state.status!;
    if (confirmed.agent !== binding.agent || confirmed.account !== binding.account) {
      state.haltError = "The runtime binding changed. Confirm the current agent and account before halting.";
      return;
    }
    const retrySafe = !state.haltPending && !state.haltRequested;
    state.haltPending = true;
    haltFence = { owner: binding.halt.owner_id,
      minimum: Math.max(binding.halt.stop_generation, haltFence?.owner === binding.halt.owner_id ? haltFence.minimum : 0) + 1 };
    const token = Symbol("halt");
    haltToken = token;
    state.haltRequested = true;
    state.haltNotAdmitted = false;
    rejectedHalt = null;
    version += 1;
    const observed = observations;
    try {
      const status = await transport.halt(binding.agent!, binding.account!);
      // Admission replies can lag newer poll evidence, including a terminal stop.
      if (haltToken === token && observations === observed && !state.stopRequested && !terminal(state.status)
        && state.status?.agent === binding.agent && state.status.account === binding.account
        && status.agent === binding.agent && status.account === binding.account && status.network === "testnet") {
        if (!acceptStatus(status)) return;
        state.checkedAt = Date.now();
        version += 1;
      }
    } catch (error) {
      if (haltToken !== token) return;
      state.haltError = detail(error);
      if (isConsoleError(error) && error.kind === "halt_not_admitted") {
        state.haltNotAdmitted = true;
        if (retrySafe) rejectedHalt = { agent: binding.agent!, account: binding.account!, owner: binding.halt.owner_id, generation: binding.halt.stop_generation };
      }
      // Only reads admitted after this rejection may unlock an explicit retry.
      version += 1;
    }
    finally { if (haltToken === token) { haltToken = null; state.haltPending = false; void refresh(); } }
  }

  return { state: readonly(state), refresh, startPolling, stopPolling, start, stop, halt, haltBlocker,
    setReleaseActive: (active: boolean) => { state.releaseActive = active; } };
}

export const supervision = createSupervision({ status: fetchMcpStatus, start: startMcp, stop: stopMcp, halt: haltMcp }, inTauri);

export const MCP_PHASES: Record<McpStatus["phase"], string> = {
  idle: "Not started", starting: "Starting supervision", listening: "Listening",
  stopping: "Stopping", stopped: "Stopped", failed: "Failed",
};

export function pauseSweepLabel(status: Readonly<McpStatus> | null): string {
  if (status === null) return "Unknown";
  if (status.supervision_in_progress) return "In progress";
  if (status.supervision_error != null) return "Last attempt failed";
  return status.supervision_last_completed_ms == null ? "Not observed" : "Completed";
}

export function haltNotice(reading: {
  status: Readonly<McpStatus> | null; haltPending: boolean; haltRequested: boolean; haltNotAdmitted?: boolean; haltError: string | null;
}) {
  const halt = reading.status?.halt;
  if ((!halt || (halt.phase === "idle" && halt.cancellation === "not_requested"))
    && !reading.haltPending && !reading.haltRequested && reading.haltError === null) return null;
  const phases = {
    idle: "Halt requested; persistence unconfirmed", persisting: "Halt persistence pending",
    persisted: "Agent pause persisted", uncertain: "Halt durability uncertain",
  };
  const cancellations = {
    not_requested: "Cancellation not requested", pending: "Cancellation pending", retrying: "Cancellation retrying",
    acknowledged: "Cancellation acknowledged", unavailable: "Cancellation unavailable",
  };
  return {
    title: !reading.haltRequested && haltReleased(reading.status) ? "Previous agent halt released; activation required"
      : halt?.phase === "idle" && reading.haltNotAdmitted ? "Halt not admitted" : halt?.phase === "idle" && !reading.haltRequested ? "Halt not submitted" : phases[halt?.phase ?? "idle"],
    cancellation: reading.haltRequested && !reading.haltNotAdmitted && (!halt || halt.phase === "idle") ? "Cancellation unconfirmed" : cancellations[halt?.cancellation ?? "not_requested"],
    revision: halt?.durable_revision ?? null,
  };
}
