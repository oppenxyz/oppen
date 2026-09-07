import { reactive, readonly, watch } from "vue";
import { fetchPilotStatus, inTauri, isConsoleError, type PilotMetric, type PilotStatus } from "../lib/bridge";
import { shell, type Network } from "./shell";

interface PilotReading {
  network: Network | null;
  status: PilotStatus | null;
  error: string | null;
  checkedAt: number | null;
  pending: boolean;
}

function localDeadline(expired: () => void): () => void {
  const timer = setTimeout(expired, 5_000);
  return () => clearTimeout(timer);
}

/** Read and deadline seams keep race/outage tests independent of Tauri and wall time. */
export function createPilotMonitor(
  read: (network: Network) => Promise<PilotStatus | null>,
  deadline: (expired: () => void) => () => void = localDeadline,
) {
  const state = reactive<PilotReading>({
    network: null, status: null, error: null, checkedAt: null, pending: false,
  });
  let generation = 0;
  let reading = false;
  let cancelDeadline: (() => void) | null = null;

  function watchDeadline(): void {
    cancelDeadline?.();
    const current = generation;
    cancelDeadline = deadline(() => {
      if (current !== generation) return;
      state.error = "Local pilot status has not responded within 5 seconds.";
    });
  }

  function invalidate(): void {
    generation += 1;
    cancelDeadline?.();
    cancelDeadline = null;
  }

  function setNetwork(network: Network): void {
    if (state.network === network) return;
    invalidate();
    state.network = network;
    state.status = null;
    state.error = null;
    state.checkedAt = null;
    if (reading) watchDeadline();
  }

  async function refresh(): Promise<void> {
    if (state.network === null) return;
    if (reading) {
      if (cancelDeadline === null) watchDeadline();
      return;
    }
    const network = state.network;
    const current = generation;
    state.pending = true;
    reading = true;
    watchDeadline();
    try {
      const status = await read(network);
      if (current !== generation || network !== state.network) return;
      state.status = status;
      state.error = null;
      state.checkedAt = Date.now();
    } catch (error) {
      if (current !== generation || network !== state.network) return;
      state.error = isConsoleError(error) ? error.detail : error instanceof Error ? error.message : String(error);
      // Preserve verified evidence for this context; an IPC failure is not a resume.
    } finally {
      reading = false;
      state.pending = false;
      if (current === generation) {
        cancelDeadline?.();
        cancelDeadline = null;
      }
    }
  }

  return { state: readonly(state), setNetwork, refresh, invalidate };
}

const monitor = createPilotMonitor(fetchPilotStatus);
export const pilot = monitor.state;
let timer: ReturnType<typeof setTimeout> | null = null;
let unwatch: (() => void) | null = null;
let pollingGeneration = 0;

export function startPilotPolling(): void {
  if (unwatch !== null || !inTauri()) return;
  const current = ++pollingGeneration;
  async function poll(): Promise<void> {
    await monitor.refresh();
    if (current !== pollingGeneration) return;
    timer = setTimeout(() => {
      timer = null;
      void poll();
    }, 2_000);
  }
  unwatch = watch(() => shell.network, (network) => {
    monitor.setNetwork(network);
    // During a read, only the displayed context changes. No second IPC job.
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
      void poll();
    }
  }, { immediate: true, flush: "sync" });
  void poll();
}

export function stopPilotPolling(): void {
  pollingGeneration += 1;
  if (timer !== null) clearTimeout(timer);
  timer = null;
  unwatch?.();
  unwatch = null;
  monitor.invalidate();
}

const METRICS: Record<PilotMetric, string> = {
  order_notional: "Order notional",
  executed_notional: "Executed notional",
  committed_notional: "Committed notional",
  realized_loss: "Realized loss",
};

/** The banner describes evidence, never cancellation delivery or flat positions. */
export function pilotNotice(reading: Readonly<PilotReading>): { title: string; detail: string } | null {
  const halt = reading.status?.halt;
  if (halt?.reason === "awaiting_reconciliation") {
    return { title: "Pilot reconciling", detail: "New orders blocked while fills await durable order reconciliation." };
  }
  if (halt?.reason === "exhausted") {
    return { title: "Pilot stopped", detail: `${METRICS[halt.metric]}: ${halt.observed_usd} USD observed; ${halt.limit_usd} USD limit.` };
  }
  if (halt?.reason === "unavailable") {
    return { title: "Pilot stopped", detail: halt.detail };
  }
  if (reading.status?.accounting === "unavailable") {
    return { title: "Pilot accounting unavailable", detail: reading.status.detail };
  }
  if (reading.error !== null) {
    return { title: "Pilot status unavailable", detail: "Current local pilot status could not be verified." };
  }
  return null;
}
