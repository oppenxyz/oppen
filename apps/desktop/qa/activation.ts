// Local visual fixtures only. No native invocation, account request or authority mutation.
import type { ActivationDisplay, ActivationStatus, PolicyStatus } from "../src/lib/bridge";

export const activationAccount = "0x0000000000000000000000000000000000000001";
export const activationAgent = "fixture-agent";

interface ActivationRuntimeFixture {
  read(): { policy: PolicyStatus; blocked: boolean };
  publish(policy: PolicyStatus): void;
}
let runtimeFixture: ActivationRuntimeFixture | undefined;
export function connectActivationFixture(runtime: ActivationRuntimeFixture) { runtimeFixture = runtime; }

export function createActivationFixture(scenario = "idle", clock = Date.now, runtime?: ActivationRuntimeFixture) {
  let serial = 0;
  let next: ActivationStatus | null = null;
  let status: ActivationStatus = {
    operation_seq: 0, last_operation: null,
    owner_id: "fixture-activation-1", agent: activationAgent, account: activationAccount,
    phase: "idle", review: null, receipt: null, error: null,
    policy_status: { cached_revision: 42, acknowledgment: null, stop_generation: 7, admission_inhibited: true },
  };
  const copy = () => structuredClone(status);
  function display(): ActivationDisplay {
    const now = clock();
    const policy = runtime?.read().policy ?? status.policy_status;
    const signer = "0x1234567890abcdef1234567890abcdef12345678";
    return {
      route: { network: "testnet", binding_seq: 2, binding: { agent: activationAgent, container: activationAccount, vault_address: null,
        wallet: { generation: 1, address: signer, approved_at_ms: now - 86_400_000, valid_until_ms: now + 86_400_000 } } },
      policy_revision: policy.cached_revision ?? 42, stop_generation: policy.stop_generation,
      policy: { symbols: ["BTC", "ETH"], max_order_usd: "15", max_position_usd: "25", max_slippage_bps: "50",
        order_rate: { count: 3, per_ms: 60_000 }, reduce_only: false, approval_required: true,
        risk: { max_leverage: 1, margin_mode: "cross", max_open_exposure_usd: "25", max_risk_usd: null },
        loss: { max_daily_loss_usd: "5", max_drawdown_usd: "10" }, freshness: { max_market_age_ms: 5000, max_account_age_ms: 10000 },
        max_mark_divergence_bps: "50", mark_divergence_window_ms: 30000 },
      pilot: { agent: activationAgent, account: activationAccount, authorized_at_ms: now - 60000,
        baseline: { seq: 3, hash: "a".repeat(64) }, executed_usd: "12.123456789012", reserved_usd: "3",
        net_realized_pnl_usd: "-1.25", halt: null },
      account: { contract_version: 1, network: "testnet", address: activationAccount, as_of_ms: now, feed_age_ms: 0, feed: "live",
        balances: { equity_usd: "100", perps_account_value_usd: "100", spot_usdc_available: "0", total_margin_used_usd: "5", withdrawable_usd: "95" },
        positions: [], orders: [] },
      wallet_approval: { name: "Synthetic operator wallet <script>inert</script>", address: signer, validUntil: now + 86_400_000 },
      observed_at_ms: now, expires_at_ms: now + 60_000, gross_exposure_usd: "5", remaining_committed_usd: "134.876543210988",
    };
  }
  function ready(): ActivationStatus {
    return { ...copy(), operation_seq: status.operation_seq + 1, last_operation: { kind: "review" }, phase: "review_ready", receipt: null, error: null,
      review: { id: String(++serial), display: display() },
      policy_status: { ...(runtime?.read().policy ?? status.policy_status), acknowledgment: null, admission_inhibited: true } };
  }
  function outcome(evidence: ActivationDisplay, phase: "acknowledged" | "refused" | "uncertain"): ActivationStatus {
    return { ...copy(), operation_seq: status.operation_seq + 1, last_operation: { kind: "confirm", review_id: status.review!.id }, phase, review: null,
      receipt: phase === "acknowledged" ? { route: evidence.route, policy_revision: evidence.policy_revision,
        stop_generation: evidence.stop_generation, acknowledged_at_ms: clock(), audit_seq: 12, audit_hash: "b".repeat(64) } : null,
      error: phase === "acknowledged" ? null : { kind: phase === "refused" ? "refusal" : "worker",
        detail: `Synthetic ${phase} confirmation. <img src=x> ` + "retained-activation-diagnostic/".repeat(12) },
      policy_status: { ...status.policy_status, admission_inhibited: phase !== "acknowledged",
        acknowledgment: phase === "acknowledged" ? { revision: evidence.policy_revision, stop_generation: evidence.stop_generation } : null } };
  }
  function scope(agent: string, account: string) {
    if (agent !== status.agent || account !== status.account) throw new Error("Synthetic activation binding mismatch.");
  }
  function reviewScope(agent: string, account: string, ownerId: string, reviewId: string) {
    scope(agent, account);
    if (ownerId !== status.owner_id || reviewId !== status.review?.id || status.phase !== "review_ready") {
      throw new Error("Synthetic stale owner or review.");
    }
  }
  if (["review_ready", "stale", "refused", "uncertain", "acknowledged", "confirming", "unknown"].includes(scenario)) {
    status = ready();
    if (scenario === "stale" && status.review) {
      status.review.display.observed_at_ms = clock() - 60_001;
      status.review.display.expires_at_ms = clock() - 1;
    }
    if (scenario === "refused" || scenario === "uncertain" || scenario === "acknowledged") status = outcome(status.review!.display, scenario);
    if (scenario === "confirming") status = { ...outcome(status.review!.display, "acknowledged"), phase: "confirming", receipt: null,
      policy_status: { ...status.policy_status, admission_inhibited: true } };
  }
  if (scenario === "closed" || scenario === "reviewing") status.phase = scenario;
  return {
    async fetchActivationStatus(agent: string, account: string) {
      scope(agent, account);
      if (scenario === "error") throw new Error("Synthetic status unavailable. " + "activation-read-diagnostic/".repeat(12));
      if (next) {
        status = next; next = null;
        if (status.phase === "acknowledged" && status.receipt) {
          const current = runtime?.read();
          if (current && (current.blocked || current.policy.stop_generation !== status.receipt.stop_generation || current.policy.cached_revision !== status.receipt.policy_revision)) {
            status = { ...status, phase: "refused", receipt: null, error: { kind: "refusal", detail: "Synthetic authority changed before completion." } };
          } else runtime?.publish(status.policy_status);
        }
      }
      if (runtime) status.policy_status = structuredClone(runtime.read().policy);
      return copy();
    },
    async reviewActivation(agent: string, account: string) {
      scope(agent, account);
      if (!["idle", "acknowledged", "refused"].includes(status.phase)) throw new Error("Synthetic review admission refused.");
      if (runtime?.read().blocked) throw new Error("Synthetic activation refused: an effective stop remains.");
      if (runtime) {
        const policy = runtime.read().policy;
        runtime.publish({ ...policy, stop_generation: policy.stop_generation + 1, acknowledgment: null, admission_inhibited: true });
      }
      next = ready();
      runtime?.publish(next.policy_status);
      status = { ...next, phase: "reviewing", review: null, receipt: null, error: null,
        policy_status: { ...next.policy_status } };
      return copy();
    },
    async confirmActivation(agent: string, account: string, ownerId: string, reviewId: string) {
      reviewScope(agent, account, ownerId, reviewId);
      const evidence = status.review!.display;
      const current = runtime?.read();
      const result = evidence.expires_at_ms <= clock() || scenario === "refused"
        || (current && (current.blocked || current.policy.stop_generation !== evidence.stop_generation || current.policy.cached_revision !== evidence.policy_revision)) ? "refused"
        : scenario === "unknown" || scenario === "uncertain" ? "uncertain" : "acknowledged";
      next = outcome(evidence, result);
      status = { ...status, operation_seq: next.operation_seq, last_operation: next.last_operation, phase: "confirming", review: null, receipt: null, error: null };
      if (scenario === "unknown") throw new Error("Synthetic lost confirmation IPC reply; outcome unknown.");
      return copy();
    },
    async discardActivation(agent: string, account: string, ownerId: string, reviewId: string) {
      reviewScope(agent, account, ownerId, reviewId);
      status = { ...status, operation_seq: status.operation_seq + 1, last_operation: { kind: "discard", review_id: reviewId }, phase: "idle", review: null, receipt: null, error: null };
      return copy();
    },
  };
}

let fixture: ReturnType<typeof createActivationFixture> | undefined;
function local() { return fixture ??= createActivationFixture(new URLSearchParams(location.search).get("activation") ?? "idle", Date.now, runtimeFixture); }
export const fetchActivationStatus = (...args: Parameters<ReturnType<typeof createActivationFixture>["fetchActivationStatus"]>) => local().fetchActivationStatus(...args);
export const reviewActivation = (...args: Parameters<ReturnType<typeof createActivationFixture>["reviewActivation"]>) => local().reviewActivation(...args);
export const confirmActivation = (...args: Parameters<ReturnType<typeof createActivationFixture>["confirmActivation"]>) => local().confirmActivation(...args);
export const discardActivation = (...args: Parameters<ReturnType<typeof createActivationFixture>["discardActivation"]>) => local().discardActivation(...args);
