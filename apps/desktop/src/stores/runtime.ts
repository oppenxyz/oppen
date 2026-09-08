import { reactive, readonly } from "vue";
import { fetchRuntimeStatus, inTauri, isConsoleError, type RuntimeStatus } from "../lib/bridge";

interface RuntimeReading {
  status: RuntimeStatus | null;
  error: string | null;
  checkedAt: number | null;
  pending: boolean;
}

function after(callback: () => void, delayMs: number): () => void {
  const timer = setTimeout(callback, delayMs);
  return () => clearTimeout(timer);
}

/** Injected read and clock keep stalled IPC and remount proofs entirely local. */
export function createRuntimeMonitor(
  read: () => Promise<RuntimeStatus>,
  schedule: typeof after = after,
) {
  const state = reactive<RuntimeReading>({ status: null, error: null, checkedAt: null, pending: false });
  let active = false;
  let generation = 0;
  let reading = false;
  let expiredRead = false;
  let cancelPoll: (() => void) | null = null;
  let cancelDeadline: (() => void) | null = null;

  function watchDeadline(): void {
    cancelDeadline?.();
    const current = generation;
    cancelDeadline = schedule(() => {
      if (active && current === generation) {
        expiredRead = true;
        state.error = "Desktop task status has not responded within 5 seconds.";
      }
    }, 5_000);
  }

  async function refresh(): Promise<void> {
    if (!active || reading) return;
    const current = generation;
    reading = true;
    expiredRead = false;
    state.pending = true;
    watchDeadline();
    try {
      const status = await read();
      if (!active || current !== generation || expiredRead) return;
      state.status = status;
      state.error = null;
      state.checkedAt = Date.now();
    } catch (error) {
      if (!active || current !== generation || expiredRead) return;
      state.error = isConsoleError(error) ? error.detail : error instanceof Error ? error.message : String(error);
    } finally {
      reading = false;
      state.pending = false;
      cancelDeadline?.();
      cancelDeadline = null;
      // A remount needs its own observation, but only after the old IPC finishes.
      if (active && (current !== generation || expiredRead)) void refresh();
    }
  }

  function poll(): void {
    const current = generation;
    cancelPoll = schedule(() => {
      if (!active || current !== generation) return;
      void refresh();
      poll();
    }, 1_000);
  }

  function start(): void {
    if (active) return;
    active = true;
    generation += 1;
    if (reading) watchDeadline();
    else void refresh();
    poll();
  }

  function stop(): void {
    active = false;
    generation += 1;
    cancelPoll?.();
    cancelPoll = null;
    cancelDeadline?.();
    cancelDeadline = null;
    // Do not clear the last known stop or pretend the actual IPC was canceled.
    state.error = "Desktop task status polling is stopped.";
  }

  return { state: readonly(state), start, stop, refresh };
}

const monitor = createRuntimeMonitor(fetchRuntimeStatus);
export const runtime = monitor.state;

export function startRuntimePolling(): void {
  if (inTauri()) monitor.start();
}

export function stopRuntimePolling(): void {
  monitor.stop();
}

const PHASES: Record<RuntimeStatus["phase"], string> = {
  running: "Unavailable",
  replacing: "Changing feed",
  stopping: "Stopping",
  stopped: "Stopped",
  stopped_with_error: "Stopped with errors",
};

export function runtimeNotice(reading: Readonly<RuntimeReading>): { title: string; detail: string | null; error: string | null } | null {
  if (reading.status === null) {
    return { title: "Unavailable", detail: reading.error ?? "Desktop task status has not been read.", error: null };
  }
  if (reading.status.phase === "running" && reading.status.detail === null && reading.error === null) return null;
  return { title: PHASES[reading.status.phase], detail: reading.status.detail, error: reading.error };
}
