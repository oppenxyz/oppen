// Synthetic controller only. No native, signer, ledger or venue calls.
import type { KillScope, ReleaseStatus } from "../src/lib/bridge";
import { activationAgent as agent, activationAccount as account, createActivationFixture } from "./activation";
let runtimeSnapshot: (() => { generation: number; kill: ReleaseStatus["cached_effective_kill"] }) | null = null;
let publishRelease: ((status: ReleaseStatus) => void) | null = null;
export function connectReleaseFixture(snapshot: NonNullable<typeof runtimeSnapshot>, publish: NonNullable<typeof publishRelease>) {
  const previousSnapshot = runtimeSnapshot, previousPublish = publishRelease;
  runtimeSnapshot = snapshot; publishRelease = publish;
  return () => { runtimeSnapshot = previousSnapshot; publishRelease = previousPublish; };
}

export function createReleaseFixture(scenario = "idle", clock = Date.now, renderStress = false) {
  let status: ReleaseStatus = { owner_id: "fixture-release-1", agent, account, operation_seq: 0, last_operation: null,
    phase: "idle", review: null, receipt: null, resolution: null, error: null,
    policy_status: { cached_revision: 42, acknowledgment: null, stop_generation: 7, admission_inhibited: true },
    cached_effective_kill: { global: { engaged_at_ms: clock() - 60000, reason: { reason: "operator" } },
      agents: { [agent]: { engaged_at_ms: clock() - 30000, reason: { reason: "operator" } } } } };
  const copy = () => structuredClone(status);
  function scoped(a: string, b: string) { if (a !== agent || b !== account) throw new Error("Fixture release binding mismatch."); }
  function retained(owner: string, id: string) {
    if (owner !== status.owner_id || id !== status.review?.id || status.phase !== "review_ready") throw new Error("Fixture release review changed.");
  }
  const api = {
    async fetchKillReleaseStatus(a: string, b: string) { scoped(a, b); return copy(); },
    async reviewKillRelease(a: string, b: string, scope: KillScope) {
      scoped(a, b);
      if (status.review || status.phase === "uncertain") throw new Error("Fixture retained release unresolved.");
      if (scope.scope === "agent" && scope.agent !== agent) throw new Error("Fixture agent scope mismatch.");
      const source = await createActivationFixture("review_ready", clock).fetchActivationStatus(agent, account);
      const display = source.review!.display;
      const current = runtimeSnapshot?.();
      if (current) { status.cached_effective_kill = current.kill; status.policy_status.stop_generation = current.generation; }
      const engagement = scope.scope === "global" ? status.cached_effective_kill.global : status.cached_effective_kill.agents[scope.agent];
      if (!engagement) throw new Error("Synthetic release refused: requested scope is not engaged.");
      const affected = [{ route: display.route, pilot: display.pilot }];
      // Explicit render stress only: not a valid single-ledger authority fixture.
      if (renderStress && scope.scope === "global") affected.push({
        route: { ...display.route, binding: { ...display.route.binding, agent: "second-affected-agent" } },
        pilot: { ...display.pilot, agent: "second-affected-agent" },
      });
      const remaining = structuredClone(status.cached_effective_kill);
      if (scope.scope === "global") remaining.global = null; else delete remaining.agents[scope.agent];
      status = { ...status, phase: "review_ready", operation_seq: status.operation_seq + 1, last_operation: { kind: "review", scope }, receipt: null, resolution: null,
        review: { id: String(status.operation_seq + 1), display: { operation_id: "c".repeat(64), network: "testnet", scope,
          persisted_engagement: engagement,
          local_engagement: null, policy_revision: 42, stop_generation: status.policy_status.stop_generation, affected, remaining_kill: remaining,
          reviewed_at_ms: clock(), expires_at_ms: clock() + (scenario === "stale" ? -1 : 60000) } } };
      return copy();
    },
    async confirmKillRelease(a: string, b: string, owner: string, id: string) {
      scoped(a, b); retained(owner, id); const display = status.review!.display;
      status = { ...status, operation_seq: status.operation_seq + 1, last_operation: { kind: "confirm", review_id: id }, review: null };
      if (scenario === "unknown" || scenario === "uncertain") {
        status.phase = "uncertain"; status.error = { status: "uncertain", operation_id: display.operation_id,
          detail: "Synthetic release publication unknown <script>inert</script> " + "retained-release-diagnostic/".repeat(10) };
        if (scenario === "unknown") throw new Error("Synthetic lost confirmation reply.");
      } else if (clock() >= display.expires_at_ms || scenario === "refused" || (runtimeSnapshot && runtimeSnapshot().generation !== display.stop_generation)) {
        status.phase = "refused"; status.error = { status: "refused", detail: "Synthetic release refused; reviewed evidence changed." };
      } else {
        status.phase = "released"; status.cached_effective_kill = display.remaining_kill;
        status.receipt = { operation_id: display.operation_id, network: "testnet", scope: display.scope,
          reviewed_stop_generation: display.stop_generation, policy_revision: 43, recorded_at_ms: clock(), seq: 14, hash: "d".repeat(64), remaining_kill: display.remaining_kill };
        status.policy_status.cached_revision = 43;
        publishRelease?.(copy());
      }
      return copy();
    },
    async discardKillRelease(a: string, b: string, owner: string, id: string) {
      scoped(a, b); retained(owner, id);
      status = { ...status, operation_seq: status.operation_seq + 1, last_operation: { kind: "discard", review_id: id }, phase: "idle", review: null }; return copy();
    },
    async reconcileKillRelease(a: string, b: string, owner: string, operationId: string) {
      scoped(a, b); if (owner !== status.owner_id || status.review) throw new Error("Fixture reconciliation owner unavailable.");
      status = { ...status, operation_seq: status.operation_seq + 1, last_operation: { kind: "reconcile", operation_id: operationId },
        phase: "uncertain", resolution: { status: "unknown", operation_id: operationId, detail: "Synthetic read-only reconciliation remains unknown. No release write issued." } }; return copy();
    },
  };
  return api;
}
let fixture: Promise<ReturnType<typeof createReleaseFixture>> | undefined;
function local() {
  return fixture ??= (async () => {
    const scenario = new URLSearchParams(location.search).get("release") ?? "idle";
    const api = createReleaseFixture(scenario);
    if (scenario !== "idle") {
      const ready = await api.reviewKillRelease(agent, account, scenario === "rearm" ? { scope: "agent", agent } : { scope: "global" });
      if (["uncertain", "released", "refused"].includes(scenario)) await api.confirmKillRelease(agent, account, ready.owner_id, ready.review!.id);
    }
    return api;
  })();
}
export const fetchKillReleaseStatus = async (...args: Parameters<ReturnType<typeof createReleaseFixture>["fetchKillReleaseStatus"]>) => (await local()).fetchKillReleaseStatus(...args);
export const reviewKillRelease = async (...args: Parameters<ReturnType<typeof createReleaseFixture>["reviewKillRelease"]>) => (await local()).reviewKillRelease(...args);
export const confirmKillRelease = async (...args: Parameters<ReturnType<typeof createReleaseFixture>["confirmKillRelease"]>) => (await local()).confirmKillRelease(...args);
export const discardKillRelease = async (...args: Parameters<ReturnType<typeof createReleaseFixture>["discardKillRelease"]>) => (await local()).discardKillRelease(...args);
export const reconcileKillRelease = async (...args: Parameters<ReturnType<typeof createReleaseFixture>["reconcileKillRelease"]>) => (await local()).reconcileKillRelease(...args);
