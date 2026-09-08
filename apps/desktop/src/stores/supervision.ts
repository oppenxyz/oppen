import { reactive, readonly } from "vue";
import { fetchMcpStatus, inTauri, startMcp, stopMcp, type McpStatus, type RuntimeStatus } from "../lib/bridge";

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

export function supervisionInputError(network: McpStatus["network"], agent: string, account: string): string | null {
  if (network !== "testnet") return "Supervision can only start from TESTNET. The network will not be switched automatically.";
  if (!agent.trim()) return "Enter the agent ID from the existing pilot authorization.";
  if (!/^0x[0-9a-fA-F]{40}$/.test(account.trim())) return "Enter the full 0x account address from the existing pilot authorization.";
  return null;
}

/** Read-only polling never starts supervision. Explicit commands retain ownership across remounts. */
export function createSupervision(
  transport: { status: typeof fetchMcpStatus; start: typeof startMcp; stop: typeof stopMcp },
  available: () => boolean = () => true,
  schedule: typeof after = after,
) {
  const state = reactive<{
    status: McpStatus | null; error: string | null; commandError: string | null;
    command: "start" | "stop" | null; reading: boolean; checkedAt: number | null;
    stopRequested: boolean; runtime: RuntimeStatus | null;
  }>({ status: null, error: null, commandError: null, command: null, reading: false, checkedAt: null, stopRequested: false, runtime: null });
  let active = false;
  let generation = 0;
  let version = 0;
  let starting = false;
  let stopping = false;
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
      state.status = status;
      state.error = null;
      state.checkedAt = Date.now();
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

  return { state: readonly(state), refresh, startPolling, stopPolling, start, stop };
}

export const supervision = createSupervision({ status: fetchMcpStatus, start: startMcp, stop: stopMcp }, inTauri);

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
