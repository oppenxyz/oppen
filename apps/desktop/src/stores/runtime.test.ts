import { createSSRApp, reactive, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import type { ChartBinding, ChartProjection, RuntimeStatus } from "../lib/bridge";
import { createRuntimeMonitor, runtimeNotice, observeRuntimeStatus } from "./runtime";
import { createChartObservations, createMarketFeed } from "./market";
import { accountObservation, bindAccountObservation, receiveAccountStatus, shell } from "./shell";
import type { WatchMarketReply } from "../lib/bridge";
import { createMarketHealth } from "./market-health";
import { channelHealthFixture } from "../../qa/channel-health";

interface Assertions {
  toBe(expected: unknown): void;
  toEqual(expected: unknown): void;
  toContain(expected: string): void;
}
declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (actual: unknown) => Assertions & { not: Assertions };

const RUNNING: RuntimeStatus = { phase: "running", binding: null, detail: null, channel_health: null, account_failure: null, selected_failure: null };
const STOPPING: RuntimeStatus = {
  phase: "stopping", binding: { network: "testnet", generation: "9007199254740993" },
  detail: "Waiting for retained desktop work to drain.",
  channel_health: null, account_failure: null, selected_failure: null,
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

async function settle(): Promise<void> {
  for (let step = 0; step < 8; step += 1) await Promise.resolve();
}

function fixture(observe: (status: RuntimeStatus) => void = () => {}) {
  const requests: Array<ReturnType<typeof deferred<RuntimeStatus>>> = [];
  const timers: Array<{ callback: () => void; delayMs: number; active: boolean }> = [];
  const monitor = createRuntimeMonitor(
    () => {
      const request = deferred<RuntimeStatus>();
      requests.push(request);
      return request.promise;
    },
    (callback, delayMs) => {
      const timer = { callback, delayMs, active: true };
      timers.push(timer);
      return () => { timer.active = false; };
    },
    observe,
  );
  function fire(delayMs: number): void {
    const timer = timers.find(candidate => candidate.active && candidate.delayMs === delayMs);
    if (!timer) throw new Error(`No active ${delayMs}ms timer`);
    timer.active = false;
    timer.callback();
  }
  return { monitor, requests, timers, fire };
}

describe("desktop runtime polling", () => {
  it("recovers a missed bound account failure before the first watch acknowledges, without allowing event ownership or regression", async () => {
    const prior = { ...toRaw(accountObservation) };
    Object.assign(reactive(toRaw(accountObservation)), { binding: null, failure: null, detail: null });
    const binding = { network: shell.network, generation: "100" };
    const watch = deferred<WatchMarketReply>();
    const selected = createMarketFeed(() => ({ network: shell.network, coin: "BTC", interval: "1h" }),
      { watch: () => watch.promise, listen: async () => () => {} },
      { update: () => {}, invalidate: () => {}, failure: () => {}, error: () => {},
        chartBinding: bindAccountObservation, accountBinding: () => accountObservation.binding, account: receiveAccountStatus });
    const f = fixture(observeRuntimeStatus);
    try {
      await selected.start();
      receiveAccountStatus({ scope: "account", binding, update: { kind: "status", connected: false, detail: null, last_tick_ms: null }, failure: "Unestablished event" });
      expect(accountObservation.binding).toBe(null);
      expect(accountObservation.failure).toBe(null);
      f.monitor.start();
      const failure = { binding, detail: "Account consumer failed before watch completion" };
      f.requests[0]!.resolve({ ...STOPPING, binding: null, account_failure: failure }); await settle();
      expect(accountObservation.binding).toEqual(binding);
      expect(accountObservation.failure).toEqual(failure);
      watch.reject(new Error("Selected watch rejected")); await settle();
      expect(accountObservation.failure).toEqual(failure);
      const newer = { ...binding, generation: "101" };
      bindAccountObservation(newer);
      f.fire(1000); f.requests[1]!.resolve({ ...STOPPING, binding: null, account_failure: failure }); await settle();
      expect(accountObservation.binding).toEqual(newer);
      expect(accountObservation.failure).toBe(null);
      f.fire(1000); f.requests[2]!.resolve({ ...STOPPING, binding: null, account_failure: {
        binding: { network: shell.network === "testnet" ? "mainnet" : "testnet", generation: "102" }, detail: "Foreign failure",
      } }); await settle();
      expect(accountObservation.binding).toEqual(newer);
      expect(accountObservation.failure).toBe(null);
    } finally {
      selected.stop(); f.monitor.stop(); watch.reject(new Error("Fixture stopped")); await settle();
      Object.assign(reactive(toRaw(accountObservation)), prior);
    }
  });
  it("successful null polls cannot renew channel health and stale remount replies cannot restore it", async () => {
    const binding: ChartBinding = { network: "testnet", generation: "1", selection_id: "1", symbol: "BTC", interval: "1h" };
    let clock = 0;
    let expire = () => {};
    const health = createMarketHealth(() => clock, callback => { expire = callback; return () => {}; });
    health.bind(binding);
    const f = fixture(status => { if (status.phase === "running") health.accept(status.channel_health); else health.historical("Runtime stopping"); });
    f.monitor.start(); f.requests[0]!.resolve({ ...RUNNING, channel_health: channelHealthFixture(binding) }); await settle();
    expect(health.state.current).toBe(true);
    for (let second = 1; second <= 5; second++) {
      clock = second * 1000; f.fire(1000); f.requests[second]!.resolve(RUNNING); await settle();
    }
    expire(); expect(health.state.current).toBe(false);
    f.fire(1000); f.monitor.stop(); f.monitor.start();
    const next = { ...binding, selection_id: "3" }; health.bind(next);
    f.requests[6]!.resolve({ ...RUNNING, channel_health: channelHealthFixture(binding, "99") }); await settle();
    expect(health.state.current).toBe(false);
    f.requests[7]!.resolve({ ...RUNNING, channel_health: channelHealthFixture(next) }); await settle();
    expect(health.state.current).toBe(true);
    f.fire(1000); f.requests[8]!.resolve(STOPPING); await settle();
    expect(health.state.current).toBe(false); f.monitor.stop();
  });
  it("delivers exact-bound chart failure during ordinary polling and latches it against later projections", async () => {
    const binding: ChartBinding = { network: "testnet", generation: "1", selection_id: "10", symbol: "BTC", interval: "1h" };
    const projection = (owner: ChartBinding, revision: string): ChartProjection => ({
      selection_id: owner.selection_id, revision, symbol: owner.symbol, interval: owner.interval,
      interval_ms: 3600000, price_decimals: null, closed: [],
      forming: { time_ms: 0, open: "100", high: "101", low: "99", close: "100.5", volume: "1",
        source: "venue", partial: false, open_close_ambiguous: false, received_at_ms: 1000 },
      latest_trade: null, history_error: null, observation_error: null,
      last_observation_received_at_ms: 1000, tape_status: "observing",
    });
    const chart = createChartObservations(async () => projection(binding, "1"));
    chart.bind(binding); chart.accept(projection(binding, "1"));
    const f = fixture(status => { if (status.selected_failure) chart.fail(status.selected_failure); });
    try {
      f.monitor.start();
      const replacement = { ...binding, selection_id: "11" };
      chart.invalidate(true); chart.bind(replacement); chart.accept(projection(replacement, "1"));
      f.requests[0]!.resolve({ ...RUNNING, selected_failure: { binding, detail: "Old consumer failed" } });
      await settle();
      expect(chart.state.failure).toBe(null);
      expect(chart.state.retained).toBe(false);
      f.fire(1000);
      f.requests[1]!.resolve({ ...RUNNING, selected_failure: { binding: replacement, detail: "Consumer terminated" } });
      await settle();
      expect(chart.state.failure?.detail).toBe("Consumer terminated");
      expect(chart.state.retained).toBe(true);
      expect(chart.state.data?.forming?.close).toBe(100.5);
      expect(chart.accept(projection(replacement, "99"))).toBe(false);
      f.fire(1000); f.requests[2]!.resolve(RUNNING); await settle();
      expect(chart.state.failure?.detail).toBe("Consumer terminated");
      expect(chart.state.projection?.revision).toBe("1");
      f.fire(1000); f.fire(5000);
      expect(chart.state.failure?.detail).toBe("Consumer terminated");
      chart.invalidate(); chart.bind(replacement);
      expect(chart.accept(projection(replacement, "100"))).toBe(false);
      const next = { ...replacement, selection_id: "12" };
      chart.invalidate(true); chart.bind(next);
      expect(chart.state.failure).toBe(null);
      expect(chart.accept(projection(next, "1"))).toBe(true);
    } finally { f.monitor.stop(); }
  });

  it("does not deliver a chart failure from a stopped runtime read after remount", async () => {
    const delivered: RuntimeStatus[] = [];
    const f = fixture(status => delivered.push(status));
    try {
      f.monitor.start(); f.monitor.stop(); f.monitor.start();
      f.requests[0]!.resolve({ ...RUNNING, selected_failure: {
        binding: { network: "testnet", generation: "1", selection_id: "1", symbol: "BTC", interval: "1h" }, detail: "Retired read",
      } });
      await settle();
      expect(delivered.length).toBe(0);
      f.requests[1]!.resolve(RUNNING); await settle();
      expect(delivered).toEqual([RUNNING]);
    } finally { f.monitor.stop(); }
  });

  it("polls cached typed status once a second and hides ordinary running without a readiness claim", async () => {
    const f = fixture();
    try {
      expect(runtimeNotice(f.monitor.state)?.title).toBe("Unavailable");
      f.monitor.start();
      f.monitor.start();
      expect(f.requests.length).toBe(1);
      f.requests[0]!.resolve(RUNNING);
      await settle();
      expect(runtimeNotice(f.monitor.state)).toBe(null);
      f.fire(1_000);
      expect(f.requests.length).toBe(2);
      f.requests[1]!.resolve(STOPPING);
      await settle();
      expect(f.monitor.state.status).toEqual(STOPPING);
      expect(runtimeNotice(f.monitor.state)?.title).toBe("Stopping");
    } finally { f.monitor.stop(); }
  });

  it("keeps one actual IPC through repeated ticks and a deadline without losing known stopping", async () => {
    const f = fixture();
    try {
      f.monitor.start();
      f.requests[0]!.resolve(STOPPING);
      await settle();
      const checked = f.monitor.state.checkedAt;
      f.fire(1_000);
      for (let tick = 0; tick < 10; tick += 1) f.fire(1_000);
      await f.monitor.refresh();
      expect(f.requests.length).toBe(2);
      f.fire(5_000);
      expect(f.monitor.state.pending).toBe(true);
      expect(f.monitor.state.status).toEqual(STOPPING);
      expect(f.monitor.state.checkedAt).toBe(checked);
      expect(runtimeNotice(f.monitor.state)?.title).toBe("Stopping");
      expect(f.monitor.state.error).toContain("5 seconds");
      f.requests[1]!.resolve(RUNNING);
      await settle();
      expect(f.requests.length).toBe(3);
      expect(f.monitor.state.status).toEqual(STOPPING);
      f.requests[2]!.resolve({ ...STOPPING, phase: "stopped" });
      await settle();
      expect(runtimeNotice(f.monitor.state)?.title).toBe("Stopped");
      expect(f.monitor.state.error).toBe(null);
    } finally { f.monitor.stop(); }
  });

  it("retains last observed stop and its detail on a typed read error", async () => {
    const f = fixture();
    try {
      f.monitor.start();
      f.requests[0]!.resolve(STOPPING);
      await settle();
      f.fire(1_000);
      f.requests[1]!.reject({ kind: "local_status", detail: "Cached runtime status unavailable" });
      await settle();
      expect(f.monitor.state.status).toEqual(STOPPING);
      expect(runtimeNotice(f.monitor.state)).toEqual({
        title: "Stopping", detail: STOPPING.detail, error: "Cached runtime status unavailable",
      });
    } finally { f.monitor.stop(); }
  });

  it("retains outstanding IPC across stop/remount and refuses its old running response", async () => {
    const f = fixture();
    try {
      f.monitor.start();
      f.requests[0]!.resolve(STOPPING);
      await settle();
      f.fire(1_000);
      f.monitor.stop();
      expect(f.monitor.state.pending).toBe(true);
      expect(f.timers.filter(timer => timer.active).length).toBe(0);
      f.monitor.start();
      f.monitor.start();
      f.fire(1_000);
      expect(f.requests.length).toBe(2);
      f.requests[1]!.resolve(RUNNING);
      await settle();
      expect(f.requests.length).toBe(3);
      expect(f.monitor.state.status).toEqual(STOPPING);
      expect(f.monitor.state.pending).toBe(true);
      f.requests[2]!.resolve({ ...STOPPING, phase: "stopped_with_error", detail: "Drain failed" });
      await settle();
      expect(runtimeNotice(f.monitor.state)?.title).toBe("Stopped with errors");
      expect(f.monitor.state.error).toBe(null);
    } finally { f.monitor.stop(); }
  });

  it("ignores stopped read failures and does not restart polling until remount", async () => {
    const f = fixture();
    f.monitor.start();
    f.monitor.stop();
    f.requests[0]!.reject(new Error("old IPC failed"));
    await settle();
    expect(f.monitor.state.pending).toBe(false);
    expect(f.monitor.state.error).toBe("Desktop task status polling is stopped.");
    expect(f.requests.length).toBe(1);
    expect(f.timers.filter(timer => timer.active).length).toBe(0);
  });

  it("names actual lifecycle phases and shows unavailable initial or running errors", () => {
    for (const [phase, title] of [
      ["replacing", "Changing feed"], ["stopping", "Stopping"],
      ["stopped", "Stopped"], ["stopped_with_error", "Stopped with errors"],
    ] as const) {
      expect(runtimeNotice({ status: { ...STOPPING, phase }, error: null, checkedAt: 100, pending: false })?.title).toBe(title);
    }
    expect(runtimeNotice({ status: { ...RUNNING, detail: "Owner event loop failed" }, error: null, checkedAt: 100, pending: false })?.title).toBe("Unavailable");
    expect(runtimeNotice({ status: RUNNING, error: "Read failed", checkedAt: 100, pending: false })?.title).toBe("Unavailable");
  });

  it("renders the actual banner with escaped diagnostics and no normal-running banner", async () => {
    const { createServer } = await import("vite");
    const { default: vue } = await import("@vitejs/plugin-vue");
    const server = await createServer({
      root: decodeURIComponent(new URL("../../", import.meta.url).pathname),
      configFile: false,
      plugins: [vue()],
      server: { middlewareMode: true, hmr: { host: "127.0.0.1", port: 0 } },
    });
    try {
      const { runtime } = await server.ssrLoadModule("/src/stores/runtime.ts");
      const { default: Banner } = await server.ssrLoadModule("/src/components/shell/DesktopRuntimeBanner.vue");
      const { default: source } = await server.ssrLoadModule("/src/components/shell/DesktopRuntimeBanner.vue?raw");
      // The app retains its desktop minimum; this safety band must not inherit it.
      expect(source).toContain("width: min(100%, 100vw)");
      expect(source).toContain("max-width: 100vw");
      expect(source).toContain("box-sizing: border-box");
      expect(source).toContain("overflow-wrap: anywhere");
      const raw = toRaw(runtime);
      raw.status = { ...STOPPING, detail: "<img src=x onerror=alert(1)>" + "long-diagnostic/".repeat(40) };
      raw.error = "IPC observation stalled";
      raw.checkedAt = 100;
      const html = await renderToString(createSSRApp(Banner));
      expect(html).toContain("Desktop tasks");
      expect(html).toContain("Stopping");
      expect(html).toContain("Last observed");
      expect(html).toContain("&lt;img");
      expect(html).not.toContain("<img");
      expect(html).toContain("IPC observation stalled");
      raw.status = RUNNING;
      raw.error = null;
      expect(await renderToString(createSSRApp(Banner))).not.toContain("Desktop tasks");
    } finally { await server.close(); }
  });
});
