import type { PolicySetupStatus, PolicySetupEdits, ReviewedPolicy } from "../lib/bridge";
import { createPolicySetup, policyEdits, policySetupOwnsContext } from "./policy-setup";

declare const it: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (value: unknown) => { toBe(expected: unknown): void; toEqual(expected: unknown): void; toContain(expected: string): void };

const ACCOUNT = "0x1111111111111111111111111111111111111111";
const POLICY: ReviewedPolicy = {
  guardrails: { alpha: {
    symbols: [], max_order_usd: "15", max_position_usd: "25", approval_required: true,
    max_slippage_bps: "7.0001", order_rate: { count: 2, per_ms: 9000 }, reduce_only: true,
    risk: { max_leverage: 1, margin_mode: "cross", max_open_exposure_usd: "12", max_risk_usd: "0.01" },
    loss: { max_daily_loss_usd: "3", max_drawdown_usd: null },
    freshness: { max_market_age_ms: 1000, max_account_age_ms: 2000 }, max_mark_divergence_bps: "2", mark_divergence_window_ms: 8000,
  } }, account_limits: { max_daily_loss_usd: "5", max_drawdown_usd: null },
  kill: { global: { engaged_at_ms: 100, reason: "existing stop" }, agents: { other: { engaged_at_ms: 99, reason: "<script>inert</script>" } } },
};
const IDLE: PolicySetupStatus = { phase: "idle", review: null, receipt_revision: null, error: null };
const READY: PolicySetupStatus = {
  ...IDLE, phase: "review_ready", review: {
    id: 7, agent: "alpha", account: ACCOUNT, before: POLICY, proposed: POLICY, expected_revision: 3, legacy: null,
    route: { network: "testnet", binding_seq: 1, binding: { agent: "alpha", container: ACCOUNT, vault_address: null,
      wallet: { address: "0x2222222222222222222222222222222222222222", generation: 0, approved_at_ms: 1, valid_until_ms: 9999999999999 } } },
  },
};
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function settle() { for (let i = 0; i < 8; i++) await Promise.resolve(); }
function fixture() {
  const reads: ReturnType<typeof deferred<PolicySetupStatus>>[] = [];
  const writes: { kind: string; id?: number; edits?: PolicySetupEdits; reply: ReturnType<typeof deferred<PolicySetupStatus>> }[] = [];
  const timers: { ms: number; callback: () => void; active: boolean }[] = [];
  let context: string | null = null;
  const store = createPolicySetup({
    status: () => { const reply = deferred<PolicySetupStatus>(); reads.push(reply); return reply.promise; },
    review: (_agent, _account, edits) => { const reply = deferred<PolicySetupStatus>(); writes.push({ kind: "review", edits, reply }); return reply.promise; },
    persist: id => { const reply = deferred<PolicySetupStatus>(); writes.push({ kind: "persist", id, reply }); return reply.promise; },
    discard: id => { const reply = deferred<PolicySetupStatus>(); writes.push({ kind: "discard", id, reply }); return reply.promise; },
  }, () => context, () => true, (callback, ms) => {
    const timer = { ms, callback, active: true }; timers.push(timer);
    return () => { timer.active = false; };
  });
  async function open(status = IDLE) { store.startPolling(); reads[0].resolve(status); await settle(); }
  function fire(ms: number) { const timer = timers.find(t => t.ms === ms && t.active)!; timer.active = false; timer.callback(); }
  return { store, reads, writes, open, fire, block: (reason: string | null) => { context = reason; } };
}

it("policy polling is read-only and review sends only explicit edits, never a snapshot", async () => {
  const f = fixture();
  await f.open();
  expect(f.writes.length).toBe(0);
  await f.store.review();
  expect(f.writes.length).toBe(0);
  Object.assign(f.store.draft, { agent: "alpha", account: ACCOUNT, writersStopped: true });
  const pending = f.store.review();
  expect(f.writes[0].edits).toEqual({ symbols: [], max_order_usd: "15", max_position_usd: "25", max_open_exposure_usd: "25", max_leverage: 1, approval_required: true });
  f.store.edits.symbols.push("BTC");
  expect(f.writes[0].edits?.symbols).toEqual([]);
  f.writes[0].reply.resolve({ ...IDLE, phase: "reviewing" });
  await pending;
  expect(f.store.state.status?.phase).toBe("reviewing");
  f.store.stopPolling();
});

it("a pending review remains singleflight across remount and newer poll evidence wins its late reply", async () => {
  const f = fixture(); await f.open();
  Object.assign(f.store.draft, { agent: "alpha", account: ACCOUNT, writersStopped: true });
  const pending = f.store.review();
  f.store.stopPolling(); f.store.startPolling();
  await f.store.review();
  expect(f.writes.length).toBe(1);
  f.reads[1].resolve(READY); await settle();
  f.writes[0].reply.resolve({ ...IDLE, phase: "reviewing" }); await pending;
  expect(f.store.state.status?.phase).toBe("review_ready");
  expect(f.store.draft.agent).toBe("alpha");
  f.fire(5000);
  expect(f.store.state.status?.review?.id).toBe(7);
  expect(f.store.state.readError).toContain("5 seconds");
  f.store.stopPolling();
});

it("old in-flight polls cannot regress persistence, and polling never overlaps after remount", async () => {
  const f = fixture(); await f.open(READY);
  void f.store.refresh();
  const pending = f.store.persist(7);
  f.store.stopPolling(); f.store.startPolling();
  expect(f.reads.length).toBe(2);
  f.writes[0].reply.resolve({ ...READY, phase: "persisting" }); await pending;
  f.reads[1].resolve(READY); await settle();
  expect(f.store.state.status?.phase).toBe("persisting");
  expect(f.reads.length).toBe(3);
  f.reads[2].resolve({ ...READY, phase: "saved", receipt_revision: 4 }); await settle();
  expect(f.store.state.status?.receipt_revision).toBe(4);
  f.store.stopPolling();
});

it("uncertain persistence permits only an explicit retry of the same review ID", async () => {
  const f = fixture(); await f.open({ ...READY, phase: "uncertain", error: { kind: "uncertain", detail: "anchor unavailable" } });
  await f.store.discard(7); await f.store.review(); await f.store.persist(8);
  expect(f.writes.length).toBe(0);
  const pending = f.store.persist(7);
  expect(f.writes[0].id).toBe(7);
  f.writes[0].reply.resolve({ ...READY, phase: "persisting" }); await pending;
  expect(f.writes.length).toBe(1);
  f.store.stopPolling();
});

it("unknown IPC and local_status errors never assert no change or unlock an idle old candidate", async () => {
  for (const error of [new Error("transport lost"), { kind: "local_status", detail: "admission rejected" }]) {
    const f = fixture(); await f.open(READY);
    const pending = f.store.persist(7);
    f.writes[0].reply.reject(error); await pending;
    expect(f.store.state.outcomeUnknown).toBe(true);
    f.reads[1].resolve(READY); await settle();
    await f.store.persist(7); await f.store.discard(7);
    expect(f.writes.length).toBe(1);
    void f.store.refresh();
    f.reads[2].resolve({ ...READY, phase: "uncertain", error: { kind: "uncertain", detail: "may have committed" } }); await settle();
    expect(f.store.state.outcomeUnknown).toBe(false);
    expect(f.store.state.status?.error?.detail).toBe("may have committed");
    f.store.stopPolling();
  }
});

it("terminal and newer failed evidence survives late admission replies and stalled reads", async () => {
  for (const phase of ["stopped", "failed", "recovery_required"] as const) {
    const f = fixture(); await f.open(READY);
    const pending = f.store.persist(7);
    void f.store.refresh();
    f.reads[1].resolve({ ...READY, phase, error: { kind: phase === "recovery_required" ? "uncertain" : "unavailable", detail: "native diagnostic" } }); await settle();
    f.writes[0].reply.resolve({ ...READY, phase: "persisting" }); await pending;
    f.fire(5000);
    expect(f.store.state.status?.phase).toBe(phase);
    expect(f.store.state.status?.error?.detail).toBe("native diagnostic");
    f.store.stopPolling();
  }
});

it("recovery requires reconciliation, retains evidence across remount and refuses every policy mutation", async () => {
  const f = fixture();
  const recovery: PolicySetupStatus = { ...READY, phase: "recovery_required", error: { kind: "uncertain", detail: "publication unknown; owner poisoned" } };
  await f.open(recovery);
  expect(policySetupOwnsContext(f.store.state)).toBe(true);
  expect(f.store.blocker()).toContain("restart alone does not verify publication");
  for (const next of [READY, { ...READY, phase: "uncertain" as const }, { ...READY, phase: "saved" as const }, { ...READY, phase: "stopped" as const }]) {
    f.store.stopPolling(); f.store.startPolling();
    f.reads[f.reads.length - 1].resolve(next); await settle();
    await f.store.review(); await f.store.persist(7); await f.store.discard(7);
    await f.store.editReviewed(true); await f.store.editReviewed(false);
    expect(f.store.state.status).toEqual(recovery);
    expect(f.writes.length).toBe(0);
  }
  f.store.stopPolling();
});

it("context blockers and unsafe IDs never invoke mutations", async () => {
  const f = fixture(); await f.open(READY);
  for (const reason of ["MAINNET", "MCP listening", "MCP starting", "runtime stopped", "status stale"]) {
    f.block(reason); await f.store.persist(7); await f.store.discard(7);
  }
  f.block(null); await f.store.persist(Number.MAX_SAFE_INTEGER + 1);
  expect(f.writes.length).toBe(0);
  f.store.stopPolling();
});

it("existing editable values are copied only from returned review, preserving null and exact decimals", async () => {
  const f = fixture(); await f.open(READY);
  const pending = f.store.editReviewed(true);
  expect(f.writes[0].kind).toBe("discard");
  f.writes[0].reply.resolve(IDLE); await pending;
  expect(f.store.edits.max_open_exposure_usd).toBe("12");
  expect(f.store.draft.account).toBe(ACCOUNT);
  expect(f.writes.length).toBe(1);
  expect(policyEdits({ ...POLICY.guardrails.alpha, risk: { ...POLICY.guardrails.alpha.risk, max_open_exposure_usd: null } }).max_open_exposure_usd).toBe("");
  expect(POLICY.kill.agents.other.reason).toBe("<script>inert</script>");
  f.store.stopPolling();
});

it("saved retained displays release UI exclusions but unknown outcomes and failed retained reviews do not", () => {
  const state = { command: null, outcomeUnknown: false, status: READY };
  expect(policySetupOwnsContext(state)).toBe(true);
  expect(policySetupOwnsContext({ ...state, status: { ...READY, phase: "saved" } })).toBe(false);
  expect(policySetupOwnsContext({ ...state, status: { ...READY, phase: "failed" } })).toBe(true);
  expect(policySetupOwnsContext({ ...state, status: IDLE, outcomeUnknown: true })).toBe(true);
});

it("renders complete inert review and distinct uncertain, saved and stopped evidence", async () => {
  const { createServer } = await import("vite");
  const { default: vue } = await import("@vitejs/plugin-vue");
  const { createSSRApp, toRaw } = await import("vue");
  const { renderToString } = await import("vue/server-renderer");
  const server = await createServer({
    root: decodeURIComponent(new URL("../../", import.meta.url).pathname), configFile: false,
    plugins: [vue()], server: { middlewareMode: true, hmr: false },
  });
  try {
    const { policySetup } = await server.ssrLoadModule("/src/stores/policy-setup.ts");
    const { default: Panel } = await server.ssrLoadModule("/src/components/PolicySetupPanel.vue");
    const raw = toRaw(policySetup.state);
    raw.status = READY;
    let html = await renderToString(createSSRApp(Panel));
    expect(html).toContain("&lt;script&gt;inert&lt;/script&gt;");
    expect(html.includes("<script>inert</script>")).toBe(false);
    expect(html).toContain("freshness / max market age ms");
    expect(html).toContain("max mark divergence bps");
    expect(html).toContain("account limits / max daily loss usd");
    expect(html).toContain("Use existing limits");
    expect(html.includes("No stops were released")).toBe(false);
    raw.status = { ...READY, phase: "uncertain", error: { kind: "uncertain", detail: "<img src=x>" } };
    raw.readError = "Status stalled";
    html = await renderToString(createSSRApp(Panel));
    expect(html).toContain("Persistence outcome uncertain");
    expect(html).toContain("Retry this reviewed write");
    expect(html).toContain("&lt;img src=x&gt;");
    expect(html).toContain("Previous evidence remains visible");
    expect(html.includes("Discard review")).toBe(false);
    raw.status = { ...READY, phase: "saved", receipt_revision: 4 };
    html = await renderToString(createSSRApp(Panel));
    expect(html).toContain("Native receipt revision: 4");
    expect(html).toContain("No stops were released");
    raw.status = { ...READY, phase: "stopped", receipt_revision: null };
    html = await renderToString(createSSRApp(Panel));
    expect(html).toContain("not a new confirmation of write completion");
    expect(html.includes("Save globally paused policy")).toBe(false);
    raw.status = { ...READY, phase: "recovery_required", error: { kind: "uncertain", detail: "<img src=x> publication unknown" } };
    html = await renderToString(createSSRApp(Panel));
    expect(html).toContain("Policy recovery required");
    expect(html).toContain("Controlled restart and reconciliation required");
    expect(html).toContain("restart alone does not verify publication");
    expect(html).toContain("&lt;img src=x&gt; publication unknown");
    expect(html).toContain("freshness / max market age ms");
    for (const action of ["Retry this reviewed write", "Discard review", "Edit candidate", "Use existing limits", "Save globally paused policy"]) {
      expect(html.includes(action)).toBe(false);
    }
    raw.status = { ...raw.status, review: null };
    html = await renderToString(createSSRApp(Panel));
    expect(html.includes("Review paused policy")).toBe(false);
  } finally { await server.close(); }
});

it("panel consent survives identical review polls and resets on a new review ID or phase", async () => {
  const { createServer } = await import("vite");
  const { default: vue } = await import("@vitejs/plugin-vue");
  const { createRenderer, nextTick, reactive, ssrContextKey, toRaw } = await import("vue");
  const server = await createServer({
    root: decodeURIComponent(new URL("../../", import.meta.url).pathname), configFile: false,
    plugins: [vue()], server: { middlewareMode: true, hmr: false },
  });
  let unmount: (() => void) | undefined;
  try {
    const { policySetup } = await server.ssrLoadModule("/src/stores/policy-setup.ts");
    const { default: Panel } = await server.ssrLoadModule("/src/components/PolicySetupPanel.vue");
    const state = reactive(toRaw(policySetup.state));
    state.status = structuredClone(READY);
    let confirmed!: { value: boolean };
    // Run the actual panel setup with a live Vue scheduler, not SSR's inert watchers.
    const renderer = createRenderer<object, object>({
      createElement: () => ({}), createText: () => ({}), createComment: () => ({}),
      insert: () => {}, remove: () => {}, setText: () => {}, setElementText: () => {},
      parentNode: () => null, nextSibling: () => null, patchProp: () => {},
    });
    const app = renderer.createApp({
      setup() {
        confirmed = Panel.setup({}, { expose: () => {} }).confirmed;
        return () => null;
      },
    });
    app.provide(ssrContextKey, { modules: new Set() });
    app.mount({});
    unmount = () => app.unmount();
    confirmed.value = true;
    state.status = structuredClone(READY);
    await nextTick();
    expect(confirmed.value).toBe(true);
    state.status = structuredClone(READY);
    await nextTick();
    expect(confirmed.value).toBe(true);
    state.status = { ...structuredClone(READY), review: { ...structuredClone(READY.review!), id: 8 } };
    await nextTick();
    expect(confirmed.value).toBe(false);
    confirmed.value = true;
    state.status = { ...state.status, phase: "persisting" };
    await nextTick();
    expect(confirmed.value).toBe(false);
  } finally { unmount?.(); await server.close(); }
});
