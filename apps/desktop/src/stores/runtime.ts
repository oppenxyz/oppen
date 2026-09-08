import { reactive, readonly } from "vue";
import { fetchRuntimeStatus, inTauri, isConsoleError, type RuntimeStatus } from "../lib/bridge";
import { reportSelectedFailure } from "./market";
import { marketHealth } from "./market-health";
import { bindAccountObservation, reportAccountFailure } from "./shell";

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
  observe: (status: RuntimeStatus) => void = () => {},
  unavailable: (detail: string) => void = () => {},
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
        unavailable(state.error);
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
      observe(status);
    } catch (error) {
      if (!active || current !== generation || expiredRead) return;
      state.error = isConsoleError(error) ? error.detail : error instanceof Error ? error.message : String(error);
      unavailable(state.error);
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
    unavailable(state.error);
  }

  return { state: readonly(state), start, stop, refresh };
}

/** Only an accepted runtime read may establish an owner from its cached failure. */
export function observeRuntimeStatus(status: RuntimeStatus): void {
  if (status.binding) bindAccountObservation(status.binding);
  if (status.account_failure) {
    bindAccountObservation(status.account_failure.binding);
    reportAccountFailure(status.account_failure);
  }
  if (status.selected_failure) reportSelectedFailure(status.selected_failure);
  if (status.phase !== "running") marketHealth.historical("Runtime " + status.phase);
  else marketHealth.accept(status.channel_health);
}
const monitor = createRuntimeMonitor(fetchRuntimeStatus, after, observeRuntimeStatus, detail => marketHealth.historical(detail));
export const runtime = monitor.state;

export function startRuntimePolling(): void {
  if (inTauri()) monitor.start();
}

export function stopRuntimePolling(): void {
  marketHealth.historical("Status polling stopped");
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
