declare const describe: (name: string, body: () => void) => void;
declare const test: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (value: unknown) => {
  toBe(value: unknown): void; toEqual(value: unknown): void; toHaveLength(value: number): void;
  toBeNull(): void; toBeDefined(): void; toContain(value: string): void;
};
import type { ReleaseStatus, ReleaseOperation } from "../lib/bridge";
import { createRelease, releaseReviewBlocker } from "./release";

const account = `0x${"1".repeat(40)}`;
const context = { network: "testnet" as const, agent: "alpha", account, blocked: null };
const idle: ReleaseStatus = { owner_id: "1", agent: "alpha", account, operation_seq: 0, last_operation: null,
  phase: "idle", review: null, receipt: null, resolution: null, error: null,
  policy_status: { cached_revision: 4, acknowledgment: null, stop_generation: 7, admission_inhibited: true },
  cached_effective_kill: { global: { engaged_at_ms: 1, reason: { reason: "operator" } }, agents: {} } };
const ready: ReleaseStatus = { ...idle, phase: "review_ready", operation_seq: 1, last_operation: { kind: "review", scope: { scope: "global" } },
  review: { id: "1", display: { operation_id: "a".repeat(64), network: "testnet", scope: { scope: "global" },
    persisted_engagement: idle.cached_effective_kill.global, local_engagement: null, policy_revision: 4, stop_generation: 7,
    affected: ["alpha", "beta"].map(agent => ({
      route: { network: "testnet", binding_seq: 2, binding: { agent, container: account, vault_address: null,
        wallet: { generation: 1, address: `0x${"2".repeat(40)}`, approved_at_ms: 1, valid_until_ms: Date.now() + 86400000 } } },
      pilot: { agent, account, authorized_at_ms: 1, baseline: { seq: 2, hash: "baseline" }, executed_usd: "12.123456789",
        reserved_usd: "3", net_realized_pnl_usd: "-1", halt: null },
    })), remaining_kill: { global: null, agents: { beta: { engaged_at_ms: 2, reason: { reason: "operator" } } } },
    reviewed_at_ms: Date.now(), expires_at_ms: Date.now() + 60000 } } };
function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; }); return { resolve, reject, promise };
}
async function settle() { for (let i = 0; i < 10; i++) await Promise.resolve(); }
function fixture() {
  const reads: ReturnType<typeof deferred<ReleaseStatus>>[] = [];
  const writes: { operation: ReleaseOperation; reply: ReturnType<typeof deferred<ReleaseStatus>> }[] = [];
  function write(operation: ReleaseOperation) { const reply = deferred<ReleaseStatus>(); writes.push({ operation, reply }); return reply.promise; }
  const store = createRelease({ status: () => { const read = deferred<ReleaseStatus>(); reads.push(read); return read.promise; },
    review: (_a, _b, scope) => write({ kind: "review", scope }),
    confirm: (_a, _b, _owner, review_id) => write({ kind: "confirm", review_id }),
    discard: (_a, _b, _owner, review_id) => write({ kind: "discard", review_id }),
    reconcile: (_a, _b, _owner, operation_id) => write({ kind: "reconcile", operation_id }),
  }, () => () => {});
  store.setContext(context);
  return { store, reads, writes, async open(status = idle) { store.startPolling(); reads[0]!.resolve(status); await settle(); } };
}
describe("reviewed kill release", () => {
  test("terminal poll unlocks a hung observer and its late finally cannot clear newer work", async () => {
    const f = fixture(); await f.open(ready);
    const hanging = f.store.confirm("1", "1", true);
    const poll = f.store.refresh();
    f.reads[1]!.resolve({ ...ready, operation_seq: 2, last_operation: { kind: "confirm", review_id: "1" }, phase: "uncertain", review: null,
      error: { status: "uncertain", operation_id: ready.review!.display.operation_id, detail: "Publication unknown" } }); await poll;
    expect(f.store.state.command).toBeNull();
    const next = f.store.reconcile(ready.review!.display.operation_id);
    expect(f.writes).toHaveLength(2); expect(f.store.state.command).toBe("reconcile");
    f.writes[0]!.reply.resolve({ ...ready, operation_seq: 2, last_operation: { kind: "confirm", review_id: "1" }, phase: "released" }); await hanging;
    expect(f.store.state.command).toBe("reconcile"); expect(f.store.state.outcomeUnknown).toBe(true);
    expect(f.store.state.status?.phase).toBe("uncertain");
    f.writes[1]!.reply.resolve({ ...idle, operation_seq: 3, last_operation: f.writes[1]!.operation,
      resolution: { status: "not_committed", operation_id: ready.review!.display.operation_id, proof: "worker_terminal" } }); await next;
    expect(f.store.state.command).toBeNull(); f.store.stopPolling();
  });
  test("requires explicit scoped review and full confirmation without changing pilot budgets", async () => {
    const f = fixture(); await f.open(); expect(f.writes).toHaveLength(0);
    const review = f.store.review({ scope: "global" }); f.writes[0]!.reply.resolve(ready); await review;
    f.reads[1]!.resolve(ready); await settle();
    await f.store.confirm("old", "1", true); await f.store.confirm("1", "1", false);
    expect(f.writes).toHaveLength(1);
    const confirm = f.store.confirm("1", "1", true);
    expect(f.writes[1]!.operation).toEqual({ kind: "confirm", review_id: "1" });
    f.writes[1]!.reply.resolve({ ...ready, operation_seq: 2, last_operation: { kind: "confirm", review_id: "1" }, phase: "released", review: null }); await confirm;
    expect(f.store.state.status?.policy_status.acknowledgment).toBeNull();
    expect(ready.review!.display.affected[0]!.pilot.executed_usd).toBe("12.123456789");
    expect(ready.review!.display.remaining_kill.agents.beta).toBeDefined(); f.store.stopPolling();
  });
  test("retains unknown confirmation across old receipts and reconciles explicitly without another confirm", async () => {
    const f = fixture(); await f.open(ready);
    const command = f.store.confirm("1", "1", true); f.writes[0]!.reply.reject(new Error("Lost IPC")); await command;
    f.reads[1]!.resolve({ ...ready, phase: "released" }); await settle();
    expect(f.store.state.outcomeUnknown).toBe(true);
    await f.store.review({ scope: "global" }); expect(f.writes).toHaveLength(1);
    const reconcile = f.store.reconcile(ready.review!.display.operation_id);
    // Retained review must be absent before native reconciliation can be requested.
    await reconcile; expect(f.writes).toHaveLength(1);
    const read = f.store.refresh();
    f.reads[2]!.resolve({ ...ready, operation_seq: 2, last_operation: { kind: "confirm", review_id: "1" }, phase: "uncertain", review: null,
      error: { status: "uncertain", operation_id: ready.review!.display.operation_id, detail: "Publication unknown" } }); await read;
    const resolving = f.store.reconcile(ready.review!.display.operation_id);
    expect(f.writes[1]!.operation.kind).toBe("reconcile");
    f.writes[1]!.reply.resolve({ ...idle, operation_seq: 3, last_operation: f.writes[1]!.operation,
      resolution: { status: "not_committed", operation_id: ready.review!.display.operation_id, proof: "worker_terminal" } }); await resolving;
    expect(f.store.state.outcomeUnknown).toBe(false); expect(f.writes.filter(row => row.operation.kind === "confirm")).toHaveLength(1);
    f.store.stopPolling();
  });
  test("fences context replacement and rejects expiry or incomplete global membership", async () => {
    expect(releaseReviewBlocker(ready)).toBeNull();
    expect(releaseReviewBlocker({ ...ready, review: { ...ready.review!, display: { ...ready.review!.display, affected: [] } } })).toContain("membership");
    expect(releaseReviewBlocker(ready, ready.review!.display.expires_at_ms)).toContain("expired");
    const f = fixture(); f.store.startPolling(); f.store.setContext({ ...context, agent: "beta" }); f.store.setContext(context);
    f.reads[0]!.resolve(ready); await settle(); expect(f.store.state.status).toBeNull();
    f.reads[1]!.resolve(idle); await settle(); expect(f.store.state.status?.phase).toBe("idle"); f.store.stopPolling();
  });
});
