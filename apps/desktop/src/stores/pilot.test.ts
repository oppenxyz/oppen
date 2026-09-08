import type { PilotStatus } from "../lib/bridge";
import { shell } from "./shell";
import { createPilotMonitor, pilotAuthentication, pilotNotice } from "./pilot";

interface Assertions {
  toBe(expected: unknown): void;
  toEqual(expected: unknown): void;
  toContain(expected: string): void;
}
declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (actual: unknown) => Assertions & { not: Assertions };

const STOP: PilotStatus = {
  authentication: "unverified",
  agent: "pilot-alpha",
  account: "0x1111111111111111111111111111111111111111",
  halt: { reason: "exhausted", metric: "realized_loss", observed_usd: "5.01", limit_usd: "5" },
  accounting: "known",
  executed_usd: "23.000000000000000001",
  reserved_usd: "2",
  net_realized_pnl_usd: "-5.01",
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

describe("pilot local polling", () => {
  it("keeps a verified stop and its timestamp during a local outage, then recovers", async () => {
    let fail = false;
    const monitor = createPilotMonitor(async () => {
      if (fail) throw { kind: "local_status", detail: "Ledger chain could not be verified" };
      return STOP;
    });
    monitor.setNetwork("testnet");
    await monitor.refresh();
    const checked = monitor.state.checkedAt;
    fail = true;
    await monitor.refresh();
    expect(monitor.state.status).toEqual(STOP);
    expect(monitor.state.checkedAt).toBe(checked);
    expect(monitor.state.error).toBe("Ledger chain could not be verified");
    expect(pilotNotice(monitor.state)?.title).toBe("Pilot stopped");
    fail = false;
    await monitor.refresh();
    expect(monitor.state.error).toBe(null);
    expect(monitor.state.status).toEqual(STOP);
  });

  it("clears immediately on a network switch and discards a late old-network success", async () => {
    const old = deferred<PilotStatus | null>();
    const monitor = createPilotMonitor((network) => network === "testnet" ? old.promise : Promise.resolve(null));
    monitor.setNetwork("testnet");
    const first = monitor.refresh();
    monitor.setNetwork("mainnet");
    expect(monitor.state.status).toBe(null);
    expect(monitor.state.checkedAt).toBe(null);
    await monitor.refresh();
    old.resolve(STOP);
    await first;
    await monitor.refresh();
    expect(monitor.state.network).toBe("mainnet");
    expect(monitor.state.status).toBe(null);
    expect(monitor.state.error).toBe(null);
  });

  it("uses a generation, so testnet-mainnet-testnet cannot revive an old response", async () => {
    const old = deferred<PilotStatus | null>();
    let calls = 0;
    const monitor = createPilotMonitor(() => ++calls === 1 ? old.promise : Promise.resolve(null));
    monitor.setNetwork("testnet");
    const first = monitor.refresh();
    monitor.setNetwork("mainnet");
    monitor.setNetwork("testnet");
    await monitor.refresh();
    old.resolve(STOP);
    await first;
    await monitor.refresh();
    expect(monitor.state.status).toBe(null);
    expect(monitor.state.error).toBe(null);
  });

  it("drops a late old-context error and then reads the current context", async () => {
    const old = deferred<PilotStatus | null>();
    const monitor = createPilotMonitor((network) => network === "testnet" ? old.promise : Promise.resolve(STOP));
    monitor.setNetwork("testnet");
    const first = monitor.refresh();
    monitor.setNetwork("mainnet");
    await monitor.refresh();
    old.reject(new Error("old ledger unavailable"));
    await first;
    expect(monitor.state.error).toBe(null);
    await monitor.refresh();
    expect(monitor.state.status).toEqual(STOP);
    expect(monitor.state.error).toBe(null);
  });

  it("clears a previously verified stop and its error when the context changes", async () => {
    const monitor = createPilotMonitor(async () => STOP);
    monitor.setNetwork("testnet");
    await monitor.refresh();
    monitor.setNetwork("mainnet");
    expect(monitor.state.status).toBe(null);
    expect(monitor.state.checkedAt).toBe(null);
    expect(monitor.state.error).toBe(null);
    expect(pilotNotice(monitor.state)).toBe(null);
  });

  it("does not overlap local reads and rejects responses after polling stops", async () => {
    const pending = deferred<PilotStatus | null>();
    let calls = 0;
    const monitor = createPilotMonitor(() => { calls += 1; return pending.promise; });
    monitor.setNetwork("testnet");
    const first = monitor.refresh();
    await monitor.refresh();
    expect(calls).toBe(1);
    monitor.invalidate();
    pending.resolve(STOP);
    await first;
    expect(monitor.state.status).toBe(null);
    expect(monitor.state.pending).toBe(false);
  });

  it("reports first-read failure without inventing a pilot identity or zero totals", async () => {
    const monitor = createPilotMonitor(async () => { throw new Error("Missing local ledger"); });
    monitor.setNetwork("testnet");
    await monitor.refresh();
    expect(monitor.state.status).toBe(null);
    expect(monitor.state.checkedAt).toBe(null);
    expect(pilotNotice(monitor.state)?.title).toBe("Pilot status unavailable");
  });

  it("marks a hung refresh stale while retaining the stop and the single-flight slot", async () => {
    let expired = () => {};
    let calls = 0;
    const hung = deferred<PilotStatus | null>();
    const monitor = createPilotMonitor(
      () => ++calls === 1 ? Promise.resolve(STOP) : hung.promise,
      (callback) => { expired = callback; return () => {}; },
    );
    monitor.setNetwork("testnet");
    await monitor.refresh();
    const checked = monitor.state.checkedAt;
    const refresh = monitor.refresh();
    expired();
    expect(monitor.state.status).toEqual(STOP);
    expect(monitor.state.checkedAt).toBe(checked);
    expect(monitor.state.error).toBe("Local pilot status has not responded within 5 seconds.");
    expect(monitor.state.pending).toBe(true);
    expect(pilotNotice(monitor.state)?.title).toBe("Pilot stopped");
    await monitor.refresh();
    expect(calls).toBe(2);
    hung.resolve(STOP);
    await refresh;
    expect(monitor.state.error).toBe(null);
    expect(monitor.state.pending).toBe(false);
  });

  it("bounds IPC jobs across rapid switches and shows unavailable while the old read hangs", async () => {
    let expired = () => {};
    let calls = 0;
    const hung = deferred<PilotStatus | null>();
    const monitor = createPilotMonitor(
      () => { calls += 1; return hung.promise; },
      (callback) => { expired = callback; return () => {}; },
    );
    monitor.setNetwork("testnet");
    const first = monitor.refresh();
    for (let i = 0; i < 20; i += 1) {
      monitor.setNetwork(i % 2 ? "testnet" : "mainnet");
      await monitor.refresh();
    }
    expect(calls).toBe(1);
    expired();
    expect(pilotNotice(monitor.state)?.title).toBe("Pilot status unavailable");
    expect(monitor.state.status).toBe(null);
    expect(monitor.state.pending).toBe(true);
    monitor.invalidate();
    await monitor.refresh();
    expect(calls).toBe(1);
    hung.resolve(STOP);
    await first;
    expect(monitor.state.status).toBe(null);
  });

  it("updates local evidence without changing any venue-feed freshness", async () => {
    const before = JSON.stringify({ feeds: shell.feeds, accountError: shell.accountError });
    const monitor = createPilotMonitor(async () => STOP);
    monitor.setNetwork("testnet");
    await monitor.refresh();
    expect(monitor.state.status).toEqual(STOP);
    expect(JSON.stringify({ feeds: shell.feeds, accountError: shell.accountError })).toBe(before);
  });
});

describe("pilot banner states", () => {
  async function notice(status: PilotStatus | null) {
    const monitor = createPilotMonitor(async () => status);
    monitor.setNetwork("testnet");
    await monitor.refresh();
    return { monitor, notice: pilotNotice(monitor.state) };
  }

  it("keeps consent authentication independent of stop and unavailable accounting priority", async () => {
    for (const authentication of ["unverified", "legacy_review_required", "verified"] as const) {
      const stopped = await notice({ ...STOP, authentication });
      expect(stopped.notice?.title).toBe("Pilot stopped");
      expect(stopped.monitor.state.status).toEqual({ ...STOP, authentication });
      expect(pilotAuthentication(stopped.monitor.state.status)).toBe(`Consent authentication: ${authentication.split("_").join(" ")}`);
      const unavailable = await notice({ authentication, agent: STOP.agent, account: STOP.account, halt: null, accounting: "unavailable", detail: "Missing accounting evidence" });
      expect(unavailable.notice?.title).toBe("Pilot accounting unavailable");
      const reconciling = await notice({ ...STOP, authentication, halt: { reason: "awaiting_reconciliation" } });
      expect(reconciling.notice?.title).toBe("Pilot reconciling");
    }
  });

  it("surfaces unverified or legacy consent without inventing a budget stop or enabling trading", async () => {
    expect((await notice({ ...STOP, halt: null })).notice?.title).toBe("Pilot consent unverified");
    expect((await notice({ ...STOP, authentication: "legacy_review_required", halt: null })).notice?.title).toBe("Pilot consent review required");
    expect((await notice({ ...STOP, authentication: "verified", halt: null })).notice).toBe(null);
  });

  it("retains authentication and exact totals beside a stale stop and untrusted read error", async () => {
    let fail = false;
    const monitor = createPilotMonitor(async () => {
      if (fail) throw { kind: "local_status", detail: "<img src=x onerror=alert(1)>" };
      return { ...STOP, authentication: "legacy_review_required" as const };
    });
    monitor.setNetwork("testnet");
    await monitor.refresh();
    const before = monitor.state.status;
    const checked = monitor.state.checkedAt;
    fail = true;
    await monitor.refresh();
    expect(monitor.state.status).toEqual(before);
    expect(monitor.state.checkedAt).toBe(checked);
    expect(monitor.state.error).toBe("<img src=x onerror=alert(1)>");
    expect(pilotNotice(monitor.state)?.title).toBe("Pilot stopped");
    expect(pilotAuthentication(monitor.state.status)).toBe("Consent authentication: legacy review required");
  });

  it("renders authentication as subordinate evidence with escaped errors and last-observed wording", async () => {
    const { createServer } = await import("vite");
    const { default: vue } = await import("@vitejs/plugin-vue");
    const { createSSRApp, toRaw } = await import("vue");
    const { renderToString } = await import("vue/server-renderer");
    const server = await createServer({
      root: decodeURIComponent(new URL("../../", import.meta.url).pathname), configFile: false,
      plugins: [vue()], server: { middlewareMode: true, hmr: false },
    });
    try {
      const { pilot } = await server.ssrLoadModule("/src/stores/pilot.ts");
      const { default: Banner } = await server.ssrLoadModule("/src/components/shell/PilotStatusBanner.vue");
      const raw = toRaw(pilot);
      raw.network = "testnet";
      raw.checkedAt = 100;
      raw.error = "<img src=x onerror=alert(1)>";
      for (const authentication of ["unverified", "legacy_review_required", "verified"] as const) {
        raw.status = { ...STOP, authentication };
        const html = await renderToString(createSSRApp(Banner));
        expect(html).toContain("Pilot stopped");
        expect(html).toContain(pilotAuthentication(raw.status));
        expect(html).toContain("Authentication does not enable trading");
        expect(html).toContain("last observed");
        expect(html).not.toContain("last verified");
        expect(html).toContain(STOP.executed_usd);
        expect(html).toContain("&lt;img");
        expect(html).not.toContain("<img");
      }
    } finally { await server.close(); }
  });

  it("shows the exhausted metric with exact observed and limit strings", async () => {
    const result = await notice(STOP);
    expect(result.notice).toEqual({ title: "Pilot stopped", detail: "Realized loss: 5.01 USD observed; 5 USD limit." });
    expect(result.monitor.state.status).toEqual(STOP);
  });

  it("distinguishes transient reconciliation from a permanent stop", async () => {
    expect((await notice({ ...STOP, halt: { reason: "awaiting_reconciliation" } })).notice?.title).toBe("Pilot reconciling");
  });

  it("keeps the verified stop when accounting is unavailable, without old totals", async () => {
    const unavailable: PilotStatus = { authentication: "unverified", agent: STOP.agent, account: STOP.account, halt: STOP.halt, accounting: "unavailable", detail: "Contradictory fill evidence" };
    let next: PilotStatus = STOP;
    const monitor = createPilotMonitor(async () => next);
    monitor.setNetwork("testnet");
    await monitor.refresh();
    next = unavailable;
    await monitor.refresh();
    expect(monitor.state.status).toEqual(unavailable);
    expect(pilotNotice(monitor.state)?.title).toBe("Pilot stopped");
    expect("executed_usd" in monitor.state.status!).toBe(false);
  });

  it("shows unavailable accounting even without a persisted halt", async () => {
    const result = await notice({ authentication: "unverified", agent: STOP.agent, account: STOP.account, halt: null, accounting: "unavailable", detail: "Unmatched submission evidence" });
    expect(result.notice).toEqual({ title: "Pilot accounting unavailable", detail: "Unmatched submission evidence" });
  });

  it("shows permanent unavailable evidence as stopped", async () => {
    expect((await notice({ ...STOP, halt: { reason: "unavailable", detail: "Required evidence missing" } })).notice)
      .toEqual({ title: "Pilot stopped", detail: "Required evidence missing" });
  });

  it("does not show a stop for no pilot or verified non-halted accounting", async () => {
    expect((await notice(null)).notice).toBe(null);
    expect((await notice({ ...STOP, authentication: "verified", halt: null })).notice).toBe(null);
  });
});
