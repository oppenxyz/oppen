import type { ActivationDisplay, ActivationStatus, McpStatus } from "../lib/bridge";
import { activationContext, activationReviewBlocker, createActivation, type ActivationContext } from "./activation";
import { ref, watch } from "vue";
import { activationConsentKey, activationSections } from "../lib/activation-review";

interface Assertions {
  toBe(expected: unknown): void; toEqual(expected: unknown): void;
  toHaveLength(expected: number): void; toBeNull(): void; toContain(expected: string): void;
}
declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (value: unknown) => Assertions;

const context: ActivationContext = { network: "testnet", agent: "alpha", account: "0x1111111111111111111111111111111111111111", blocked: null };
const idle: ActivationStatus = { owner_id: "owner-1", agent: context.agent, account: context.account, phase: "idle", review: null, receipt: null, error: null,
  operation_seq: 0, last_operation: null,
  policy_status: { cached_revision: 2, acknowledgment: null, stop_generation: 3, admission_inhibited: true } };
const at = Date.now();
const display: ActivationDisplay = {
  route: { network: "testnet", binding_seq: 1, binding: { agent: context.agent, container: context.account, vault_address: null,
    wallet: { generation: 1, address: "0x2222222222222222222222222222222222222222", approved_at_ms: at, valid_until_ms: at + 60_000 } } },
  policy_revision: 2, stop_generation: 3,
  policy: { symbols: ["TEST"], max_order_usd: "15", max_position_usd: "25", max_slippage_bps: "50", order_rate: { count: 10, per_ms: 60_000 },
    reduce_only: false, approval_required: true, risk: { max_leverage: 1, margin_mode: "cross", max_open_exposure_usd: "25", max_risk_usd: null },
    loss: { max_daily_loss_usd: "10", max_drawdown_usd: null }, freshness: { max_market_age_ms: 5000, max_account_age_ms: 5000 },
    max_mark_divergence_bps: "100", mark_divergence_window_ms: 5000 },
  pilot: { agent: context.agent, account: context.account, authorized_at_ms: at, baseline: { seq: 1, hash: "test-only-baseline" },
    executed_usd: "10", reserved_usd: "5", net_realized_pnl_usd: "-1", halt: null },
  account: { contract_version: 0, network: "testnet", address: context.account, as_of_ms: at, feed_age_ms: 0, feed: "live",
    balances: { equity_usd: "100", perps_account_value_usd: "100", spot_usdc_available: "0", total_margin_used_usd: "5", withdrawable_usd: "95" }, positions: [], orders: [] },
  wallet_approval: { name: "fixture", address: "0x2222222222222222222222222222222222222222", validUntil: at + 60_000 },
  observed_at_ms: at, expires_at_ms: at + 60_000, gross_exposure_usd: "5", remaining_committed_usd: "135",
};
const ready: ActivationStatus = { ...idle, operation_seq: 1, last_operation: { kind: "review" }, phase: "review_ready", review: { id: "review-1", display } };
const confirmation = { operation_seq: 2, last_operation: { kind: "confirm" as const, review_id: "review-1" } };
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function settle() { for (let i = 0; i < 10; i++) await Promise.resolve(); }
function fixture() {
  const reads: ReturnType<typeof deferred<ActivationStatus>>[] = [];
  const writes: { kind: string; args: string[]; reply: ReturnType<typeof deferred<ActivationStatus>> }[] = [];
  const timers: { callback: () => void; ms: number; active: boolean }[] = [];
  function write(kind: string, ...args: string[]) {
    const reply = deferred<ActivationStatus>(); writes.push({ kind, args, reply }); return reply.promise;
  }
  const store = createActivation({
    status: () => { const reply = deferred<ActivationStatus>(); reads.push(reply); return reply.promise; },
    review: (...args) => write("review", ...args),
    confirm: (...args) => write("confirm", ...args),
    discard: (...args) => write("discard", ...args),
  }, () => true, (callback, ms) => {
    const timer = { callback, ms, active: true }; timers.push(timer); return () => { timer.active = false; };
  });
  store.setContext(context);
  async function open(status = idle) { store.startPolling(); reads[0]!.resolve(status); await settle(); }
  return { store, reads, writes, timers, open };
}

describe("operator activation", () => {
  it("continues review polling and explicit confirmation after inhibition advances the engine generation", async () => {
    const mcp: McpStatus = { phase: "listening", network: "testnet", agent: context.agent, account: context.account,
      listener: "127.0.0.1:7433", reconciled: true, account_feeds_ready: true, supervision_last_completed_ms: null,
      supervision_in_progress: false, supervision_error: null, orders_inhibited: true, detail: null,
      policy_status: { cached_revision: 2, acknowledgment: null, stop_generation: 7, admission_inhibited: true },
      cached_effective_kill: { global: null, agents: {} },
      halt: { owner_id: "halt-owner", stop_generation: 1, released_stop_generation: 1, released_engine_stop_generation: 7,
        previous: null, phase: "persisted", cancellation: "acknowledged", requested_at_ms: at, durable_revision: null, error: null, cancellation_error: null } };
    const observation = { status: mcp, command: null, error: null, stopRequested: false, haltRequested: false };
    const f = fixture(); await f.open();
    f.store.setContext(activationContext("testnet", observation));
    const command = f.store.review();
    f.writes[0]!.reply.resolve({ ...ready, phase: "reviewing", review: null }); await command;
    mcp.policy_status = { ...mcp.policy_status!, stop_generation: 8 };
    expect(activationContext("testnet", observation)?.blocked).toBe(null);
    f.store.setContext(activationContext("testnet", observation));
    f.reads[1]!.resolve({ ...ready, review: { ...ready.review!, display: { ...display, stop_generation: 8 } } }); await settle();
    const confirming = f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(2);
    expect(f.store.state.status?.receipt).toBeNull();
    f.writes[1]!.reply.resolve({ ...ready, ...confirmation, phase: "acknowledged", review: null,
      receipt: { route: display.route, policy_revision: 2, stop_generation: 8, acknowledged_at_ms: at, audit_seq: 10, audit_hash: "native-receipt" } }); await confirming;
    expect(f.store.state.status?.receipt?.stop_generation).toBe(8);
    observation.haltRequested = true;
    expect(activationContext("testnet", observation)?.blocked).toContain("HALT");
    f.store.stopPolling();
  });
  it("unlocks a hung confirmation only on correlated terminal evidence and fences its late reply from a new review", async () => {
    const f = fixture(); await f.open(ready);
    const hanging = f.store.confirm("owner-1", "review-1", true);
    const reading = f.store.refresh();
    const receipt = { route: display.route, policy_revision: 2, stop_generation: 3, acknowledged_at_ms: at, audit_seq: 9, audit_hash: "native-fixture-receipt" };
    f.reads[1]!.resolve({ ...ready, ...confirmation, phase: "acknowledged", review: null, receipt }); await reading;
    expect(f.store.state.command).toBeNull();
    expect(f.store.state.status?.receipt).toEqual(receipt);
    expect(f.writes).toHaveLength(1);
    const next = f.store.review();
    expect(f.writes).toHaveLength(2); expect(f.store.state.command).toBe("review");
    f.writes[0]!.reply.resolve({ ...ready, ...confirmation, phase: "confirming", review: null }); await hanging;
    expect(f.store.state.command).toBe("review"); expect(f.store.state.outcomeUnknown).toBe(true);
    expect(f.store.state.status?.phase).toBe("acknowledged");
    f.writes[1]!.reply.resolve({ ...ready, operation_seq: 3, last_operation: { kind: "review" },
      review: { ...ready.review!, id: "review-2" } }); await next;
    expect(f.store.state.command).toBeNull();
    expect(f.store.state.status?.phase).toBe("review_ready");
    expect(f.store.state.status?.receipt).toBeNull();
    expect(f.writes).toHaveLength(2); f.store.stopPolling();
  });
  it("retains an old owner's unknown outcome informationally while allowing explicit work for a verified replacement", async () => {
    const f = fixture(); await f.open(ready);
    const command = f.store.confirm("owner-1", "review-1", true);
    f.writes[0]!.reply.reject(new Error("Lost IPC")); await command;
    f.reads[1]!.resolve({ ...idle, owner_id: "owner-2" }); await settle();
    expect(f.store.state.outcomeUnknown).toBe(false);
    expect(f.store.state.previousUnknown).toHaveLength(1);
    expect(f.store.state.previousUnknown[0]).toContain("owner-1");
    expect(f.store.blocker()).toBeNull();
    expect(f.writes).toHaveLength(1);
    const reviewing = f.store.review();
    f.writes[1]!.reply.resolve({ ...ready, owner_id: "owner-2" }); await reviewing;
    expect(f.store.state.status?.owner_id).toBe("owner-2");
    f.reads[2]!.resolve({ ...ready, ...confirmation, phase: "acknowledged" }); await settle();
    expect(f.store.state.status?.owner_id).toBe("owner-2");
    expect(f.store.state.previousUnknown).toHaveLength(1);
    f.store.stopPolling();
  });

  it("never clears newer unknown work from old terminal status or mismatched operation evidence", async () => {
    for (const kind of ["review", "confirm"] as const) {
      const f = fixture();
      const start: ActivationStatus = kind === "review" ? { ...ready, phase: "acknowledged", review: null } : ready;
      await f.open(start);
      const command = kind === "review" ? f.store.review() : f.store.confirm("owner-1", "review-1", true);
      f.writes[0]!.reply.reject(new Error("Lost IPC")); await command;
      f.reads[1]!.resolve({ ...start, phase: "acknowledged" }); await settle();
      expect(f.store.state.outcomeUnknown).toBe(true);
      const reading = f.store.refresh();
      f.reads[2]!.resolve({ ...ready, operation_seq: 2, last_operation: kind === "review"
        ? { kind: "confirm", review_id: "other" } : { kind: "review" }, phase: "refused" }); await reading;
      expect(f.store.state.outcomeUnknown).toBe(true);
      expect(f.writes).toHaveLength(1);
      f.store.stopPolling();
    }
  });

  it("correlates completion and rejects a lower sequence after cached policy inhibition changes", async () => {
    const f = fixture(); await f.open(ready);
    const command = f.store.confirm("owner-1", "review-1", true);
    f.writes[0]!.reply.reject(new Error("Lost IPC")); await command;
    f.reads[1]!.resolve({ ...ready, ...confirmation, phase: "acknowledged", review: null,
      policy_status: { ...idle.policy_status, admission_inhibited: false, acknowledgment: { revision: 2, stop_generation: 3 } } }); await settle();
    expect(f.store.state.outcomeUnknown).toBe(false);
    expect(f.store.state.status?.policy_status.admission_inhibited).toBe(false);
    const reading = f.store.refresh(); f.reads[2]!.resolve(ready); await reading;
    expect(f.store.state.status?.phase).toBe("acknowledged");
    expect(f.store.state.status?.policy_status.admission_inhibited).toBe(false);
    f.store.stopPolling();
  });
  it("renders every review section and preserves consent only for identical evidence", () => {
    const sections = activationSections(display);
    expect(sections.map(section => section.title)).toEqual(["Registry identity", "Policy limits and approval", "Pilot authorization and accounting", "Venue wallet approval", "Account observation"]);
    expect(sections[1]!.fields.some(row => row.path.join(".") === "freshness.max_market_age_ms")).toBe(true);
    expect(sections[2]!.fields.some(row => row.path.join(".") === "baseline.hash")).toBe(true);
    const status = ref(ready);
    let consent = true;
    const stop = watch(() => activationConsentKey(status.value, JSON.stringify(context), false), () => { consent = false; }, { flush: "sync" });
    status.value = structuredClone(ready);
    expect(consent).toBe(true);
    status.value = { ...ready, review: { ...ready.review!, id: "review-2" } };
    expect(consent).toBe(false);
    consent = true;
    status.value = { ...status.value, phase: "confirming" };
    expect(consent).toBe(false);
    consent = true;
    status.value = { ...status.value, review: { ...ready.review!, display: { ...display, remaining_committed_usd: "0" } } };
    expect(consent).toBe(false);
    stop();
  });

  it("completes only explicit review then confirmation, with a native receipt", async () => {
    const f = fixture(); await f.open();
    const reviewing = f.store.review();
    expect(f.writes[0]!.kind).toBe("review");
    f.writes[0]!.reply.resolve({ ...ready, phase: "reviewing", review: null }); await reviewing;
    f.reads[1]!.resolve(ready); await settle();
    expect(f.writes).toHaveLength(1);
    const confirming = f.store.confirm("owner-1", "review-1", true);
    const receipt = { route: display.route, policy_revision: 2, stop_generation: 3, acknowledged_at_ms: at, audit_seq: 9, audit_hash: "fixture-audit-hash" };
    f.writes[1]!.reply.resolve({ ...ready, ...confirmation, phase: "acknowledged", review: null, receipt }); await confirming;
    expect(f.store.state.status?.receipt).toEqual(receipt);
    expect(f.writes).toHaveLength(2);
    f.store.stopPolling();
  });

  it("allows a new explicit review after acknowledgment with no retained review", async () => {
    const f = fixture(); await f.open({ ...idle, phase: "acknowledged" });
    const command = f.store.review();
    expect(f.writes[0]!.kind).toBe("review");
    f.writes[0]!.reply.resolve({ ...ready, phase: "reviewing", review: null }); await command;
    expect(f.store.state.status?.phase).toBe("reviewing");
    f.reads[1]!.resolve({ ...ready, review: { ...ready.review!, id: "review-2" } }); await settle();
    expect(f.store.state.status?.review?.id).toBe("review-2");
    expect(f.writes).toHaveLength(1);
    f.store.stopPolling();
  });

  it("keeps discard explicit and never confirms expired evidence", async () => {
    const f = fixture(); await f.open({ ...ready, review: { ...ready.review!, display: { ...display, expires_at_ms: at - 1 } } });
    await f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(0);
    const discarding = f.store.discard("owner-1", "review-1");
    expect(f.writes[0]!.kind).toBe("discard");
    f.writes[0]!.reply.resolve({ ...idle, operation_seq: 2, last_operation: { kind: "discard", review_id: "review-1" } }); await discarding;
    expect(f.store.state.status?.review).toBeNull();
    expect(f.writes).toHaveLength(1);
    f.store.stopPolling();
  });
  it("requires matching complete identity and unexpired evidence", () => {
    expect(activationReviewBlocker(ready, at)).toBe(null);
    expect(activationReviewBlocker(ready, at + 60_000)).toContain("expired");
    expect(activationReviewBlocker(ready, at - 1)).toContain("clock");
    expect(activationReviewBlocker({ ...ready, account: "0x3333333333333333333333333333333333333333" }, at)).toContain("identity");
  });
  it("polls without implicit review or confirmation and requires exact explicit confirmation", async () => {
    const f = fixture(); await f.open(ready);
    await f.store.confirm("old-owner", "review-1", true);
    await f.store.confirm("owner-1", "old-review", true);
    await f.store.confirm("owner-1", "review-1", false);
    expect(f.writes).toHaveLength(0);
    const command = f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(1);
    expect(f.writes[0]!.args).toEqual([context.agent, context.account, "owner-1", "review-1"]);
    await f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(1);
    f.writes[0]!.reply.resolve({ ...ready, ...confirmation, phase: "confirming" }); await command;
    f.store.stopPolling();
  });

  it("fences an old A read across A-B-A context changes without overlapping reads", async () => {
    const f = fixture(); f.store.startPolling();
    f.store.setContext({ ...context, agent: "beta" });
    f.store.setContext(context);
    expect(f.reads).toHaveLength(1);
    f.reads[0]!.resolve(ready); await settle();
    expect(f.store.state.status).toBeNull();
    expect(f.reads).toHaveLength(2);
    f.reads[1]!.resolve({ ...idle, owner_id: "owner-2" }); await settle();
    await f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(0);
    f.store.stopPolling();
  });

  it("preserves a newer terminal poll across a late confirmation reply and stalled read", async () => {
    const f = fixture(); await f.open(ready);
    const command = f.store.confirm("owner-1", "review-1", true);
    const reading = f.store.refresh();
    f.reads[1]!.resolve({ ...ready, phase: "closed", error: { kind: "worker", detail: "Runtime stopped" } }); await reading;
    f.writes[0]!.reply.resolve({ ...ready, ...confirmation, phase: "confirming" }); await command;
    expect(f.store.state.status?.phase).toBe("closed");
    expect(f.store.state.status?.error?.detail).toBe("Runtime stopped");
    expect(f.reads).toHaveLength(3);
    await f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(1);
    f.store.stopPolling();
  });

  it("retains unknown confirmation across remount and a cached ready poll, with no retry", async () => {
    const f = fixture(); await f.open(ready);
    const command = f.store.confirm("owner-1", "review-1", true);
    f.store.stopPolling(); f.store.startPolling();
    f.writes[0]!.reply.reject(new Error("IPC connection lost")); await command;
    f.reads[1]!.resolve(ready); await settle();
    f.reads[2]!.resolve(ready); await settle();
    expect(f.store.state.outcomeUnknown).toBe(true);
    await f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(1);
    f.store.stopPolling();
  });

  it("does not turn a failed confirmation into acknowledgment", async () => {
    const f = fixture(); await f.open(ready);
    const command = f.store.confirm("owner-1", "review-1", true);
    f.writes[0]!.reply.reject({ kind: "local_status", detail: "Admission unavailable" }); await command;
    expect(f.store.state.outcomeUnknown).toBe(true);
    expect(f.store.state.status?.phase).toBe("review_ready");
    f.reads[1]!.resolve({ ...ready, ...confirmation, phase: "refused", error: { kind: "refusal", detail: "Approval expired" } }); await settle();
    expect(f.store.state.status?.phase).toBe("refused");
    expect(f.store.state.status?.receipt).toBeNull();
    await f.store.confirm("owner-1", "review-1", true);
    expect(f.writes).toHaveLength(1);
    f.store.stopPolling();
  });

  it("blocks mainnet, halt, shutdown and mismatched status identities", async () => {
    for (const next of [null, { ...context, network: "mainnet" as const }, { ...context, blocked: "HALT requested" }, { ...context, blocked: "Runtime stopping" }]) {
      const f = fixture(); await f.open(ready); f.store.setContext(next);
      await f.store.review(); await f.store.confirm("owner-1", "review-1", true);
      expect(f.writes).toHaveLength(0); f.store.stopPolling();
    }
    const f = fixture(); await f.open({ ...ready, agent: "beta" });
    expect(f.store.state.status).toBeNull();
    expect(f.store.state.readError).toContain("does not match");
    f.store.stopPolling();
  });

  it("invalidates pre-command polls and keeps stalled reads singleflight across remount", async () => {
    const f = fixture(); await f.open();
    const reading = f.store.refresh();
    const command = f.store.review();
    f.writes[0]!.reply.resolve(ready); await command;
    f.reads[1]!.resolve(idle); await reading;
    expect(f.store.state.status?.phase).toBe("review_ready");
    f.store.stopPolling(); f.store.startPolling();
    await f.store.refresh();
    expect(f.reads).toHaveLength(3);
    expect(f.writes).toHaveLength(1);
    f.store.stopPolling();
  });
});
