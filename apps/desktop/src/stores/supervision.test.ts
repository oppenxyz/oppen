import type { McpStatus, RuntimeStatus } from "../lib/bridge";
import { createSupervision, haltNotice, pauseSweepLabel, supervisionInputError } from "./supervision";

interface Assertions { toBe(expected: unknown): void; toEqual(expected: unknown): void; toContain(expected: string): void }
declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (value: unknown) => Assertions;

const ACCOUNT = "0x1111111111111111111111111111111111111111";
const IDLE: McpStatus = {
  halt: { phase: "idle", cancellation: "not_requested", requested_at_ms: null, durable_revision: null, error: null, cancellation_error: null },
  phase: "idle", network: "testnet", agent: null, account: null, listener: null,
  reconciled: null, orders_inhibited: true, detail: null, account_feeds_ready: null,
  supervision_last_completed_ms: null, supervision_in_progress: false, supervision_error: null,
};
const LISTENING: McpStatus = { ...IDLE, phase: "listening", agent: "alpha", account: ACCOUNT, listener: "127.0.0.1:7433" };
const ADMITTED: McpStatus = { ...LISTENING, halt: { ...IDLE.halt, phase: "persisting", cancellation: "pending", requested_at_ms: 100 } };
const HALTED: McpStatus = { ...LISTENING, halt: { ...ADMITTED.halt, phase: "persisted", durable_revision: 42, cancellation: "retrying", cancellation_error: "Cancellation attempt failed" } };

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
  const halts: Array<{ agent: string; account: string; reply: ReturnType<typeof deferred<McpStatus>> }> = [];
  const timers: Array<{ ms: number; callback: () => void; active: boolean }> = [];
  const monitor = createSupervision({
    status: () => { const reply = deferred<McpStatus>(); reads.push(reply); return reply.promise; },
    start: (agent, account) => { const reply = deferred<McpStatus>(); starts.push({ agent, account, reply }); return reply.promise; },
    stop: () => { const reply = deferred<RuntimeStatus>(); stops.push(reply); return reply.promise; },
    halt: (agent, account) => { const reply = deferred<McpStatus>(); halts.push({ agent, account, reply }); return reply.promise; },
  }, () => true, (callback, ms) => {
    const timer = { callback, ms, active: true }; timers.push(timer);
    return () => { timer.active = false; };
  });
  async function opened(status = IDLE): Promise<void> { monitor.startPolling(); reads[0]!.resolve(status); await settle(); }
  function fire(ms: number): void {
    const timer = timers.find(timer => timer.active && timer.ms === ms);
    if (!timer) throw new Error("Missing timer");
    timer.active = false; timer.callback();
  }
  return { monitor, reads, starts, stops, halts, opened, fire };
}

describe("explicit TESTNET supervision", () => {
  it("halts only explicitly using the confirmed runtime binding, without stopping supervision", async () => {
    const f = fixture();
    try {
      await f.opened(LISTENING);
      expect(f.halts.length).toBe(0);
      await f.monitor.halt("testnet", { agent: "roster-agent", account: ACCOUNT });
      expect(f.halts.length).toBe(0);
      expect(f.monitor.state.haltRequested).toBe(false);
      const command = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(1);
      expect([f.halts[0]!.agent, f.halts[0]!.account]).toEqual(["alpha", ACCOUNT]);
      expect(haltNotice(f.monitor.state)?.cancellation).toBe("Cancellation unconfirmed");
      f.monitor.stopPolling();
      f.monitor.startPolling();
      await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(1);
      f.halts[0]!.reply.resolve(ADMITTED);
      await command;
      expect(f.monitor.state.status?.halt.phase).toBe("persisting");
      expect(f.stops.length).toBe(0);
      expect(f.starts.length).toBe(0);
    } finally { f.monitor.stopPolling(); }
  });

  it("blocks missing, terminal and mainnet bindings without submitting halt", async () => {
    for (const status of [IDLE, { ...LISTENING, agent: null }, { ...LISTENING, account: null },
      { ...LISTENING, phase: "failed" as const }, { ...LISTENING, phase: "stopping" as const },
      { ...LISTENING, phase: "stopped" as const }, { ...LISTENING, network: "mainnet" as const }]) {
      const f = fixture();
      try {
        await f.opened(status);
        await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
        expect(f.halts.length).toBe(0);
        expect(f.monitor.state.haltRequested).toBe(false);
      } finally { f.monitor.stopPolling(); }
    }
    const f = fixture();
    try {
      await f.opened(LISTENING);
      await f.monitor.halt("mainnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(0);
    } finally { f.monitor.stopPolling(); }
  });

  it("blocks halt during start even after a poll briefly reports listening", async () => {
    const f = fixture();
    try {
      await f.opened();
      const start = f.monitor.start("testnet", "alpha", ACCOUNT);
      const read = f.monitor.refresh();
      f.reads[1]!.resolve(LISTENING);
      await read;
      await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(0);
      expect(f.monitor.state.haltRequested).toBe(false);
      f.starts[0]!.reply.resolve(LISTENING);
      await start;
      expect(f.monitor.haltBlocker("testnet")).toBe(null);
    } finally { f.monitor.stopPolling(); }
  });

  it("does not let an older inflight poll erase a halt admission", async () => {
    const f = fixture();
    try {
      await f.opened(LISTENING);
      const oldRead = f.monitor.refresh();
      const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      f.halts[0]!.reply.resolve(ADMITTED);
      await halt;
      f.reads[1]!.resolve(LISTENING);
      await oldRead;
      expect(f.monitor.state.status).toEqual(ADMITTED);
      expect(f.halts.length).toBe(1);
    } finally { f.monitor.stopPolling(); }
  });

  for (const phase of ["listening", "failed", "stopping", "stopped"] as const) {
    it(`retains newer polled halt progress in ${phase} through a late admission reply and outage`, async () => {
      const f = fixture();
      try {
        await f.opened(LISTENING);
        const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
        const read = f.monitor.refresh();
        const newer = { ...HALTED, phase };
        f.reads[1]!.resolve(newer);
        await read;
        const checkedAt = f.monitor.state.checkedAt;
        const failedRead = f.monitor.refresh();
        f.reads[2]!.reject({ detail: "Status unavailable" });
        await failedRead;
        f.halts[0]!.reply.resolve(ADMITTED);
        await halt;
        expect(f.monitor.state.status).toEqual(newer);
        expect(f.monitor.state.error).toBe("Status unavailable");
        expect(f.monitor.state.checkedAt).toBe(checkedAt);
        f.fire(5_000);
        expect(f.monitor.state.status).toEqual(newer);
        expect(f.monitor.state.error).toContain("5 seconds");
        expect(f.halts.length).toBe(1);
      } finally { f.monitor.stopPolling(); }
    });
  }

  it("retains unknown halt outcome without automatic retry and permits explicit runtime stop", async () => {
    const f = fixture();
    try {
      await f.opened(LISTENING);
      const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      f.halts[0]!.reply.reject({ detail: "Halt IPC interrupted" });
      await halt;
      expect(haltNotice(f.monitor.state)?.cancellation).toBe("Cancellation unconfirmed");
      f.fire(1_000);
      await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(1);
      const stop = f.monitor.stop();
      f.stops[0]!.resolve({ phase: "stopped", binding: null, detail: null });
      await stop;
      expect(f.monitor.state.haltRequested).toBe(true);
      expect(f.monitor.state.runtime?.phase).toBe("stopped");
    } finally { f.monitor.stopPolling(); }
  });

  it("does not interpret generic local_status or Busy display text as proof of rejected admission", async () => {
    for (const error of [
      { kind: "local_status", detail: "desktop runtime is replacing its feed or a read is busy" },
      { kind: "local_status", detail: "halt reply unavailable" },
    ]) {
      const f = fixture();
      try {
        await f.opened(LISTENING);
        const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
        const preErrorRead = f.monitor.refresh();
        f.halts[0]!.reply.reject(error);
        await halt;
        const checkedAt = f.monitor.state.checkedAt;
        f.reads[1]!.resolve(LISTENING);
        await preErrorRead;
        expect(f.monitor.state.checkedAt).toBe(checkedAt);
        expect(f.reads.length).toBe(3);
        // Even a matching idle snapshot requested after the rejected IPC does
        // not recover the lost native error discriminator.
        f.reads[2]!.resolve(LISTENING);
        await settle();
        expect(f.monitor.state.error).toBe(null);
        expect(f.monitor.state.status?.halt.phase).toBe("idle");
        await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
        expect(f.halts.length).toBe(1);
        expect(f.monitor.state.haltRequested).toBe(true);
        expect(f.monitor.state.haltError).toBe(error.detail);
        expect(haltNotice(f.monitor.state)?.cancellation).toBe("Cancellation unconfirmed");
      } finally { f.monitor.stopPolling(); }
    }
  });

  it("unlocks only an explicit retry after typed refusal and a fresh matching idle read", async () => {
    const f = fixture();
    try {
      await f.opened(LISTENING);
      const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      const oldRead = f.monitor.refresh();
      f.halts[0]!.reply.reject({ kind: "halt_not_admitted", detail: "Runtime ownership is busy" });
      await halt;
      expect(haltNotice(f.monitor.state)?.title).toBe("Halt not admitted");
      expect(f.monitor.state.haltRequested).toBe(true);
      await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(1);
      f.reads[1]!.resolve(LISTENING);
      await oldRead;
      expect(f.monitor.state.haltRequested).toBe(true);
      expect(f.reads.length).toBe(3);
      f.reads[2]!.resolve(LISTENING);
      await settle();
      expect(f.monitor.state.haltRequested).toBe(false);
      expect(f.monitor.haltBlocker("testnet")).toBe(null);
      expect(f.halts.length).toBe(1);
      const retry = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(2);
      expect(f.monitor.state.haltNotAdmitted).toBe(false);
      f.halts[1]!.reply.resolve(ADMITTED);
      await retry;
      expect(f.monitor.state.status?.halt.phase).toBe("persisting");
    } finally { f.monitor.stopPolling(); }
  });

  it("does not unlock definitive-refusal retry from a changed binding, terminal phase or admitted halt", async () => {
    for (const status of [
      { ...LISTENING, network: "mainnet" as const }, { ...LISTENING, agent: "another-agent" },
      { ...LISTENING, account: "0x2222222222222222222222222222222222222222" },
      { ...LISTENING, phase: "stopping" as const }, { ...LISTENING, phase: "failed" as const }, ADMITTED,
    ]) {
      const f = fixture();
      try {
        await f.opened(LISTENING);
        const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
        f.halts[0]!.reply.reject({ kind: "halt_not_admitted", detail: "Busy before admission" });
        await halt;
        f.reads[1]!.resolve(status);
        await settle();
        expect(f.monitor.state.haltRequested).toBe(true);
        await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
        expect(f.halts.length).toBe(1);
      } finally { f.monitor.stopPolling(); }
    }
  });

  it("retains retry gating through a fresh-read timeout and concurrent terminal stop", async () => {
    const f = fixture();
    try {
      await f.opened(LISTENING);
      const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      f.halts[0]!.reply.reject({ kind: "halt_not_admitted", detail: "Busy before admission" });
      await halt;
      f.fire(5_000);
      f.reads[1]!.resolve(LISTENING);
      await settle();
      expect(f.monitor.state.haltRequested).toBe(true);
      const stop = f.monitor.stop();
      f.stops[0]!.resolve({ phase: "stopped", binding: null, detail: null });
      await stop;
      f.reads[2]!.resolve(LISTENING);
      await settle();
      f.reads[3]!.resolve(LISTENING);
      await settle();
      expect(f.monitor.state.haltRequested).toBe(true);
      await f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      expect(f.halts.length).toBe(1);
    } finally { f.monitor.stopPolling(); }
  });

  it("does not allow late halt admission to overwrite a concurrent terminal stop", async () => {
    const f = fixture();
    try {
      await f.opened(LISTENING);
      const halt = f.monitor.halt("testnet", { agent: "alpha", account: ACCOUNT });
      const stop = f.monitor.stop();
      f.halts[0]!.reply.resolve(ADMITTED);
      await halt;
      expect(f.monitor.state.status).toEqual(LISTENING);
      expect(f.monitor.state.command).toBe("stop");
      f.stops[0]!.resolve({ phase: "stopped", binding: null, detail: null });
      await stop;
      expect(f.monitor.state.runtime?.phase).toBe("stopped");
    } finally { f.monitor.stopPolling(); }
  });

  it("keeps uncertain durability and cancellation retry distinct from sweep completion or flatness", () => {
    const reading = { status: { ...HALTED, halt: { ...HALTED.halt, phase: "uncertain" as const, durable_revision: null } }, haltPending: false, haltRequested: true, haltError: null };
    expect(haltNotice(reading)).toEqual({ title: "Halt durability uncertain", cancellation: "Cancellation retrying", revision: null });
    expect(haltNotice({ ...reading, status: { ...HALTED, halt: { ...HALTED.halt, cancellation: "acknowledged" } } })).toEqual({ title: "Agent pause persisted", cancellation: "Cancellation acknowledged", revision: 42 });
    expect(haltNotice({ ...reading, haltRequested: false, status: { ...LISTENING, supervision_last_completed_ms: 100 } })).toBe(null);
  });

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
