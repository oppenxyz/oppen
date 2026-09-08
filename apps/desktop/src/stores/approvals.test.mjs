import { describe, expect, test } from "bun:test";
import { createApprovals, approvalBinding, APPROVAL_DECISIONS } from "./approvals";

const A = { agent: "alpha", account: `0x${"1".repeat(40)}` };
const B = { agent: "beta", account: `0x${"2".repeat(40)}` };
const NOW = 1000;
const proposal = (binding = A) => ({
  ...binding, id: "approval-testnet-7", symbol: "BTC", is_buy: true, px: "101.000000000001",
  sz: "0.10000000001", reduce_only: false, reason: "<img src=x onerror=alert(1)>", expires_at_ms: 100_000,
  original: { kind: { kind: "market", slippage_bps: "100" }, reference_px: "100", reference_at_ms: 999 },
});
const status = (binding = A, overrides = {}) => ({
  ...binding, owner_id: "owner-1", phase: "ready", observed_at_ms: NOW,
  pending: [proposal(binding)], decision: null, error: null, ...overrides,
});
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function settle() { for (let i = 0; i < 6; i++) await Promise.resolve(); }
function fixture(overrides = {}) {
  const calls = [];
  const timers = [];
  const handlers = { status: async () => status(), refresh: async () => status(), reject: async () => status(), ...overrides };
  const store = createApprovals(Object.fromEntries(Object.keys(handlers).map(kind => [kind, (...args) => {
    calls.push([kind, ...args]); return handlers[kind](...args);
  }])), () => true, (run, ms) => {
    const timer = { run, ms, canceled: false }; timers.push(timer);
    return () => { timer.canceled = true; };
  }, () => NOW);
  store.setBinding(A);
  store.start();
  return { store, calls, handlers, timers,
    fire(ms) { const timer = timers.find(t => !t.canceled && t.ms === ms); expect(timer).toBeDefined(); timer.canceled = true; timer.run(); },
  };
}

describe("runtime-bound approval queue", () => {
  test("derives selection only from a current listening TESTNET runtime", () => {
    const input = { network: "testnet", supervision: { ...A, network: "testnet", phase: "listening" },
      runtime: { phase: "running", binding: null, detail: null }, supervisionError: null, runtimeError: null, stopping: false };
    expect(approvalBinding(input)).toEqual(A);
    for (const changed of [
      { network: "mainnet" }, { supervision: null }, { supervision: { ...input.supervision, phase: "starting" } },
      { supervision: { ...input.supervision, account: null } }, { runtime: null },
      { runtime: { ...input.runtime, phase: "replacing" } }, { stopping: true },
      { runtimeError: "stale runtime" }, { supervisionError: "stale binding" },
      { runtime: { ...input.runtime, binding: { network: "mainnet" } } },
    ]) expect(approvalBinding({ ...input, ...changed })).toBeNull();
  });

  test("polls cached status only; refresh is explicit and single flight", async () => {
    const hung = deferred();
    const f = fixture({ status: async () => status(A, { phase: "idle", observed_at_ms: null, pending: [] }), refresh: () => hung.promise });
    await settle();
    expect(f.store.state.status.phase).toBe("idle");
    expect(f.store.canRefresh()).toBe(true);
    f.fire(1000); await settle();
    expect(f.calls.map(c => c[0])).toEqual(["status", "status"]);
    const reading = f.store.refresh();
    await f.store.refresh(); await f.store.readStatus();
    expect(f.calls.map(c => c[0])).toEqual(["status", "status", "refresh"]);
    hung.resolve(status()); await reading;
    expect(f.store.state.status.pending[0].px).toBe("101.000000000001");
    f.store.stop();
  });

  test("read failure preserves last evidence but disables rejection", async () => {
    const f = fixture(); await settle();
    f.handlers.status = async () => { throw new Error("ledger unavailable"); };
    await f.store.readStatus();
    expect(f.store.state.error).toBe("ledger unavailable");
    expect(f.store.state.status.pending).toEqual([proposal()]);
    expect(f.store.canReject(proposal().id)).toBe(false);
    f.store.stop();
  });

  test("clears selection immediately and rejects A-B-A late replies", async () => {
    const old = deferred();
    const f = fixture({ status: () => old.promise });
    f.store.setBinding(B); f.store.setBinding(null); f.store.setBinding(A);
    expect(f.store.state.status).toBeNull();
    expect(f.calls.length).toBe(1);
    f.handlers.status = async () => status(A, { owner_id: "new-owner" });
    old.resolve(status()); await settle();
    expect(f.store.state.status.owner_id).toBe("new-owner");
    expect(f.calls.length).toBe(2);
    f.store.stop();
  });

  test("bounds hung IPC across rapid changes and ignores timed-out replies", async () => {
    const old = deferred();
    const next = deferred();
    const f = fixture({ status: () => old.promise });
    for (let i = 0; i < 30; i++) { f.store.setBinding(i % 2 ? A : B); await f.store.readStatus(); }
    f.fire(5000);
    expect(f.calls.length).toBe(1);
    expect(f.store.state.status).toBeNull();
    expect(f.store.state.error).toContain("earlier approval request");
    f.handlers.status = () => next.promise;
    old.resolve(status()); await settle();
    expect(f.calls.length).toBe(2);
    expect(f.store.state.status).toBeNull();
    f.store.stop(); next.resolve(status()); await settle();
    expect(f.store.state.status).toBeNull();
  });

  test("validates echoed binding on the status and every pending proposal", async () => {
    for (const reply of [status(B), status(A, { pending: [proposal(B)] }), status(A, { owner_id: "" })]) {
      const f = fixture({ status: async () => reply }); await settle();
      expect(f.store.state.status).toBeNull();
      expect(f.store.state.error).toContain("identity");
      expect(f.store.canReject(proposal().id)).toBe(false);
      f.store.stop();
    }
  });

  test("owner changes clear old confirmation and displayed decisions", async () => {
    const f = fixture(); await settle();
    f.store.prepareReject(proposal().id);
    expect(f.store.state.confirmation.owner_id).toBe("owner-1");
    f.handlers.status = async () => status(A, { owner_id: "owner-2", pending: [] });
    await f.store.readStatus(); await settle();
    expect(f.store.state.confirmation).toBeNull();
    expect(f.store.state.status.owner_id).toBe("owner-2");
    expect(f.store.state.status.pending).toEqual([]);
    await f.store.reject();
    expect(f.calls.filter(c => c[0] === "reject")).toEqual([]);
    f.store.stop();
  });

  test("reject requires confirmation and sends its exact binding, owner and proposal once", async () => {
    const hung = deferred();
    const f = fixture({ reject: () => hung.promise }); await settle();
    await f.store.reject();
    expect(f.calls.length).toBe(1);
    f.store.prepareReject(proposal().id);
    expect(f.store.state.confirmation.proposal.reason).toBe(proposal().reason);
    const rejecting = f.store.reject(); await f.store.reject();
    expect(f.calls[1]).toEqual(["reject", A.agent, A.account, "owner-1", proposal().id]);
    hung.resolve(status(A, { pending: [], decision: { proposal_id: proposal().id, outcome: "not_pending", at_ms: NOW, error: null } }));
    await rejecting;
    expect(APPROVAL_DECISIONS[f.store.state.decision.outcome]).toBe("Not pending; no rejection recorded");
    f.store.prepareReject(proposal().id); await f.store.reject();
    expect(f.calls.filter(c => c[0] === "reject").length).toBe(1);
    f.store.stop();
  });

  test("transport failure is uncertain, never success or an implicit rejection retry", async () => {
    const f = fixture({ reject: async () => { throw new Error("response lost"); } }); await settle();
    f.store.prepareReject(proposal().id); await f.store.reject();
    expect(f.store.state.decision.outcome).toBe("uncertain");
    expect(f.store.state.error).toBe("response lost");
    await f.store.readStatus();
    expect(f.store.state.decision.outcome).toBe("uncertain");
    expect(f.store.canReject(proposal().id)).toBe(false);
    f.store.prepareReject(proposal().id); await f.store.reject();
    f.fire(1000); await settle();
    expect(f.calls.filter(c => c[0] === "reject").length).toBe(1);
    f.store.stop();
  });

  test("cached success for A cannot overwrite unresolved B; matching B evidence resolves it", async () => {
    const a = proposal();
    const b = { ...proposal(), id: "approval-testnet-8" };
    const decisionA = { proposal_id: a.id, outcome: "rejected", at_ms: NOW, error: null };
    const afterA = status(A, { pending: [b], decision: decisionA });
    const f = fixture({ status: async () => status(A, { pending: [a, b] }), reject: async () => afterA });
    await settle();
    f.store.prepareReject(a.id); await f.store.reject();
    expect(f.store.state.decision).toEqual(decisionA);
    f.handlers.reject = async () => { throw new Error("B reply lost"); };
    f.store.prepareReject(b.id); await f.store.reject();
    const uncertainB = { ...f.store.state.decision };
    expect(uncertainB.proposal_id).toBe(b.id);
    expect(uncertainB.outcome).toBe("uncertain");
    f.handlers.status = async () => afterA;
    await f.store.readStatus(); await f.store.readStatus();
    expect(f.store.state.decision).toEqual(uncertainB);
    expect(f.store.canReject(b.id)).toBe(false);
    expect(f.calls.filter(c => c[0] === "reject").length).toBe(2);
    const decisionB = { proposal_id: b.id, outcome: "rejected", at_ms: NOW + 1, error: null };
    f.handlers.status = async () => status(A, { pending: [], decision: decisionB });
    await f.store.readStatus();
    expect(f.store.state.decision).toEqual(decisionB);
    f.store.stop();
  });

  test("timed-out rejection retains its real slot and observes completion only through cached status", async () => {
    const hung = deferred();
    const f = fixture({ reject: () => hung.promise }); await settle();
    f.store.prepareReject(proposal().id); const rejecting = f.store.reject();
    f.fire(5000);
    expect(f.store.state.pending).toBe("reject");
    expect(f.store.state.decision.outcome).toBe("uncertain");
    await f.store.readStatus(); await f.store.refresh(); await f.store.reject();
    expect(f.calls.length).toBe(2);
    hung.resolve(status(A, { pending: [], decision: { proposal_id: proposal().id, outcome: "rejected", at_ms: NOW, error: null } }));
    await rejecting; await settle();
    expect(f.store.state.decision.outcome).toBe("uncertain");
    expect(f.calls.filter(c => c[0] === "reject").length).toBe(1);
    f.handlers.status = async () => status(A, { pending: [], decision: { proposal_id: proposal().id, outcome: "rejected", at_ms: NOW, error: null } });
    await f.store.readStatus();
    expect(f.store.state.decision.outcome).toBe("rejected");
    f.store.stop();
  });

  test("only a new successful explicit refresh can enable another explicit rejection", async () => {
    const f = fixture({ reject: async () => { throw new Error("native busy or response lost"); } }); await settle();
    f.store.prepareReject(proposal().id); await f.store.reject();
    f.handlers.status = async () => status(A, { observed_at_ms: NOW + 1 });
    await f.store.readStatus();
    expect(f.store.canReject(proposal().id)).toBe(false);
    f.handlers.refresh = async () => { throw new Error("refresh unavailable"); };
    await f.store.refresh();
    expect(f.store.canReject(proposal().id)).toBe(false);
    f.handlers.refresh = async () => status(A, { phase: "refreshing", observed_at_ms: NOW + 1 });
    await f.store.refresh();
    expect(f.store.canReject(proposal().id)).toBe(false);
    await f.store.readStatus();
    expect(f.store.canReject(proposal().id)).toBe(false); // Same observation is not proof.
    f.handlers.status = async () => status(A, { observed_at_ms: NOW + 2 });
    await f.store.readStatus();
    expect(f.store.state.error).toBeNull();
    expect(f.store.canReject(proposal().id)).toBe(true);
    expect(f.calls.filter(c => c[0] === "reject").length).toBe(1);
    f.store.prepareReject(proposal().id);
    expect(f.store.state.confirmation).not.toBeNull();
    await f.store.reject();
    expect(f.calls.filter(c => c[0] === "reject").length).toBe(2);
    f.store.stop();
  });

  test("unmount and remount do not cancel, replay, or accept a late rejection reply", async () => {
    const hung = deferred();
    const f = fixture({ reject: () => hung.promise }); await settle();
    f.store.prepareReject(proposal().id); const rejecting = f.store.reject();
    f.store.stop(); f.store.setBinding(A); f.store.start();
    expect(f.calls.length).toBe(2);
    expect(f.store.state.status).toBeNull();
    hung.resolve(status(A, { pending: [], decision: { proposal_id: proposal().id, outcome: "rejected", at_ms: NOW, error: null } }));
    await rejecting; await settle();
    expect(f.store.state.decision).toBeNull();
    expect(f.store.canReject(proposal().id)).toBe(false);
    expect(f.calls.filter(c => c[0] === "reject").length).toBe(1);
    f.store.stop();
  });

  test("late command owner changes cannot repopulate the old queue", async () => {
    const f = fixture({ reject: async () => status(A, { owner_id: "owner-2" }) }); await settle();
    f.handlers.status = async () => status(A, { owner_id: "owner-2", pending: [] });
    f.store.prepareReject(proposal().id); await f.store.reject(); await settle();
    expect(f.store.state.status.owner_id).toBe("owner-2");
    expect(f.store.state.status.pending).toEqual([]);
    expect(f.store.state.confirmation).toBeNull();
    f.store.stop();
  });

  test("closed, recovery and unavailable phases never enable rejection", async () => {
    for (const phase of ["idle", "refreshing", "rejecting", "unavailable", "recovery_required", "closed"]) {
      const f = fixture({ status: async () => status(A, { phase }) }); await settle();
      expect(f.store.canReject(proposal().id)).toBe(false);
      if (phase === "closed" || phase === "recovery_required") expect(f.store.canRefresh()).toBe(false);
      f.store.stop();
    }
  });
});
