import type { McpStatus, RuntimeStatus } from "../lib/bridge";
import { createSupervision, pauseSweepLabel, supervisionInputError } from "./supervision";

interface Assertions { toBe(expected: unknown): void; toEqual(expected: unknown): void; toContain(expected: string): void }
declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (value: unknown) => Assertions;

const ACCOUNT = "0x1111111111111111111111111111111111111111";
const IDLE: McpStatus = {
  phase: "idle", network: "testnet", agent: null, account: null, listener: null,
  reconciled: null, orders_inhibited: true, detail: null, account_feeds_ready: null,
  supervision_last_completed_ms: null, supervision_in_progress: false, supervision_error: null,
};
const LISTENING: McpStatus = { ...IDLE, phase: "listening", agent: "alpha", account: ACCOUNT, listener: "127.0.0.1:7433/mcp" };

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function settle(): Promise<void> { for (let step = 0; step < 8; step += 1) await Promise.resolve(); }

function fixture() {
  const reads: Array<ReturnType<typeof deferred<McpStatus>>> = [];
  const starts: Array<{ agent: string; account: string; reply: ReturnType<typeof deferred<McpStatus>> }> = [];
  const stops: Array<ReturnType<typeof deferred<RuntimeStatus>>> = [];
  const timers: Array<{ ms: number; callback: () => void; active: boolean }> = [];
  const monitor = createSupervision({
    status: () => { const reply = deferred<McpStatus>(); reads.push(reply); return reply.promise; },
    start: (agent, account) => { const reply = deferred<McpStatus>(); starts.push({ agent, account, reply }); return reply.promise; },
    stop: () => { const reply = deferred<RuntimeStatus>(); stops.push(reply); return reply.promise; },
  }, () => true, (callback, ms) => {
    const timer = { callback, ms, active: true }; timers.push(timer);
    return () => { timer.active = false; };
  });
  async function opened(): Promise<void> { monitor.startPolling(); reads[0]!.resolve(IDLE); await settle(); }
  function fire(ms: number): void {
    const timer = timers.find(timer => timer.active && timer.ms === ms);
    if (!timer) throw new Error("Missing timer");
    timer.active = false; timer.callback();
  }
  return { monitor, reads, starts, stops, opened, fire };
}

describe("explicit TESTNET supervision", () => {
  it("polling is read-only and a stalled status retains one actual IPC", async () => {
    const f = fixture();
    try {
      f.monitor.startPolling();
      for (let tick = 0; tick < 10; tick += 1) f.fire(1_000);
      f.fire(5_000);
      expect(f.reads.length).toBe(1);
      expect(f.starts.length).toBe(0);
      expect(f.stops.length).toBe(0);
      expect(f.monitor.state.error).toContain("5 seconds");
      f.reads[0]!.resolve(LISTENING);
      await settle();
      expect(f.monitor.state.status).toBe(null);
      expect(f.reads.length).toBe(2);
    } finally { f.monitor.stopPolling(); }
  });

  it("refuses mainnet, missing identity, and malformed account before invoking start", async () => {
    const f = fixture();
    try {
      await f.opened();
      await f.monitor.start("mainnet", "alpha", ACCOUNT);
      expect(f.monitor.state.commandError).toContain("TESTNET");
      await f.monitor.start("testnet", "", ACCOUNT);
      await f.monitor.start("testnet", "alpha", "0x123");
      expect(f.starts.length).toBe(0);
      expect(supervisionInputError("testnet", "alpha", ACCOUNT)).toBe(null);
    } finally { f.monitor.stopPolling(); }
  });

  it("starts only explicitly, retains ownership across remount, and preserves inhibited/unknown health", async () => {
    const f = fixture();
    try {
      await f.opened();
      const command = f.monitor.start("testnet", " alpha ", ` ${ACCOUNT} `);
      f.monitor.stopPolling();
      f.monitor.startPolling();
      await f.monitor.start("testnet", "alpha", ACCOUNT);
      expect(f.starts.length).toBe(1);
      expect([f.starts[0]!.agent, f.starts[0]!.account]).toEqual(["alpha", ACCOUNT]);
      f.starts[0]!.reply.resolve(LISTENING);
      await command;
      expect(f.monitor.state.status?.orders_inhibited).toBe(true);
      expect(f.monitor.state.status?.account_feeds_ready).toBe(null);
      expect(pauseSweepLabel(f.monitor.state.status)).toBe("Not observed");
      // A status read admitted before start completion cannot erase the result.
      f.reads[1]!.resolve(IDLE);
      await settle();
      expect(f.monitor.state.status?.phase).toBe("listening");
    } finally { f.monitor.stopPolling(); }
  });

  it("shows native setup blockers without provisioning or retrying start automatically", async () => {
    const f = fixture();
    try {
      await f.opened();
      const command = f.monitor.start("testnet", "alpha", ACCOUNT);
      f.starts[0]!.reply.reject({ detail: "Existing pilot authorization is missing" });
      await command;
      expect(f.monitor.state.commandError).toBe("Existing pilot authorization is missing");
      f.fire(1_000);
      expect(f.starts.length).toBe(1);
      expect(f.stops.length).toBe(0);
    } finally { f.monitor.stopPolling(); }
  });

  for (const phase of ["failed", "stopping", "stopped"] as const) {
    it(`preserves polled ${phase} and its diagnostic through late start success and stalled reads`, async () => {
      const f = fixture();
      try {
        await f.opened();
        const command = f.monitor.start("testnet", "alpha", ACCOUNT);
        const observed = { ...IDLE, phase, detail: "Runtime admission closed while startup was pending" };
        const terminalRead = f.monitor.refresh();
        f.reads[1]!.resolve(observed);
        await terminalRead;
        const checkedAt = f.monitor.state.checkedAt;
        const failedRead = f.monitor.refresh();
        f.reads[2]!.reject({ detail: "Cached status unavailable after terminal observation" });
        await failedRead;
        f.starts[0]!.reply.resolve(LISTENING);
        await command;
        expect(f.monitor.state.status).toEqual(observed);
        expect(f.monitor.state.checkedAt).toBe(checkedAt);
        expect(f.monitor.state.error).toBe("Cached status unavailable after terminal observation");
        expect(f.reads.length).toBe(4);
        expect(f.monitor.state.reading).toBe(true);
        f.fire(5_000);
        expect(f.monitor.state.status).toEqual(observed);
        expect(f.monitor.state.error).toContain("5 seconds");
        expect(f.monitor.state.checkedAt).toBe(checkedAt);
        await f.monitor.start("testnet", "alpha", ACCOUNT);
        expect(f.starts.length).toBe(1);
      } finally { f.monitor.stopPolling(); }
    });
  }

  it("permits terminal stop during start and never lets late start success reopen the UI", async () => {
    const f = fixture();
    try {
      await f.opened();
      const start = f.monitor.start("testnet", "alpha", ACCOUNT);
      const stop = f.monitor.stop();
      await f.monitor.stop();
      expect(f.stops.length).toBe(1);
      f.starts[0]!.reply.resolve(LISTENING);
      await start;
      expect(f.monitor.state.command).toBe("stop");
      expect(f.monitor.state.status?.phase).toBe("idle");
      f.stops[0]!.resolve({ phase: "stopped", binding: null, detail: null });
      await stop;
      await f.monitor.start("testnet", "alpha", ACCOUNT);
      expect(f.starts.length).toBe(1);
      expect(f.monitor.state.commandError).toContain("Restart the app");
      expect(f.monitor.state.runtime?.phase).toBe("stopped");
    } finally { f.monitor.stopPolling(); }
  });

  it("blocks retry while a failed start is being observed and after terminal failed status", async () => {
    const f = fixture();
    try {
      await f.opened();
      const command = f.monitor.start("testnet", "alpha", ACCOUNT);
      f.starts[0]!.reply.reject({ detail: "Startup authority check failed" });
      await command;
      await f.monitor.start("testnet", "alpha", ACCOUNT);
      expect(f.starts.length).toBe(1);
      f.reads[1]!.resolve({ ...IDLE, phase: "failed", detail: "Runtime admission is closed" });
      await settle();
      expect(f.monitor.state.error).toBe(null);
      await f.monitor.start("testnet", "alpha", ACCOUNT);
      expect(f.starts.length).toBe(1);
      expect(f.monitor.state.commandError).toContain("Restart the app");
      expect(f.monitor.state.status?.phase).toBe("failed");
    } finally { f.monitor.stopPolling(); }
  });

  it("retains terminal intent on stop failure and known status on polling failure", async () => {
    const f = fixture();
    try {
      await f.opened();
      const read = f.monitor.refresh();
      f.reads[1]!.reject({ detail: "Cached status unavailable" });
      await read;
      expect(f.monitor.state.status).toEqual(IDLE);
      const stop = f.monitor.stop();
      f.stops[0]!.reject({ detail: "Drain still pending" });
      await stop;
      expect(f.monitor.state.commandError).toBe("Drain still pending");
      expect(f.monitor.state.stopRequested).toBe(true);
    } finally { f.monitor.stopPolling(); }
  });

  it("reports actual pause enforcement independently of connection and reconciliation", () => {
    expect(pauseSweepLabel({ ...LISTENING, reconciled: true, account_feeds_ready: true })).toBe("Not observed");
    expect(pauseSweepLabel({ ...LISTENING, supervision_in_progress: true })).toBe("In progress");
    expect(pauseSweepLabel({ ...LISTENING, supervision_error: "Cancellation failed" })).toBe("Last attempt failed");
    expect(pauseSweepLabel({ ...LISTENING, supervision_last_completed_ms: 100 })).toBe("Completed");
    expect(pauseSweepLabel({ ...LISTENING, supervision_last_completed_ms: 100, supervision_error: "Retry failed" })).toBe("Last attempt failed");
  });
});
