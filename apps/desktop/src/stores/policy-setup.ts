import { reactive, readonly } from "vue";
import {
  discardPolicySetup, fetchPolicySetupStatus, inTauri, isConsoleError, persistPolicySetup, reviewPolicySetup,
  type PolicySetupEdits, type PolicySetupStatus, type ReviewedAgentPolicy,
} from "../lib/bridge";
import { shell } from "./shell";
import { supervision } from "./supervision";
import { pilotConsent, consentOwnsContext } from "./pilot-consent";

function after(callback: () => void, ms: number): () => void {
  const timer = setTimeout(callback, ms);
  return () => clearTimeout(timer);
}
function detail(error: unknown): string {
  return isConsoleError(error) ? error.detail : error instanceof Error ? error.message : String(error);
}

export function policyEdits(policy?: Readonly<ReviewedAgentPolicy>): PolicySetupEdits {
  return {
    symbols: policy ? [...policy.symbols] : [],
    max_order_usd: policy?.max_order_usd ?? "15",
    max_position_usd: policy?.max_position_usd ?? "25",
    max_open_exposure_usd: policy ? policy.risk.max_open_exposure_usd ?? "" : "25",
    max_leverage: policy?.risk.max_leverage ?? 1,
    approval_required: policy?.approval_required ?? true,
  };
}

export function policySetupOwnsContext(state: {
  command: string | null; outcomeUnknown: boolean;
  status: { phase: PolicySetupStatus["phase"]; review: unknown } | null;
}): boolean {
  return state.command !== null || state.outcomeUnknown || state.status?.phase === "reviewing"
    || state.status?.phase === "persisting" || state.status?.phase === "uncertain" || state.status?.phase === "recovery_required"
    || (!!state.status?.review && !["saved", "stopped"].includes(state.status.phase));
}

export function createPolicySetup(
  transport: { status: typeof fetchPolicySetupStatus; review: typeof reviewPolicySetup; persist: typeof persistPolicySetup; discard: typeof discardPolicySetup },
  contextBlocker: () => string | null = () => null,
  available: () => boolean = () => true,
  schedule: typeof after = after,
) {
  const state = reactive<{
    status: PolicySetupStatus | null; reading: boolean; readError: string | null; checkedAt: number | null;
    command: "review" | "persist" | "discard" | null; commandError: string | null; outcomeUnknown: boolean;
  }>({ status: null, reading: false, readError: null, checkedAt: null, command: null, commandError: null, outcomeUnknown: false });
  // Drafts belong to the singleton, not the Settings component's mount.
  const draft = reactive({ agent: "", account: "", writersStopped: false, emptySourceConfirmed: false });
  const edits = reactive(policyEdits());
  let unknownCommand: "review" | "persist" | "discard" | null = null;
  let active = false;
  let generation = 0;
  let version = 0;
  let observations = 0;
  let cancelPoll: (() => void) | null = null;

  function accept(status: PolicySetupStatus): void {
    if (state.status?.phase === "recovery_required" && status.phase !== "recovery_required") return;
    if (state.status?.phase === "stopped" && !["stopped", "recovery_required"].includes(status.phase)) return;
    state.status = status;
    observations += 1;
    state.checkedAt = Date.now();
  }

  async function refresh(): Promise<void> {
    if (!active || state.reading || !available()) return;
    state.reading = true;
    const owner = generation;
    const started = version;
    let expired = false;
    const cancelDeadline = schedule(() => {
      expired = true;
      if (active && owner === generation && started === version) state.readError = "Policy setup status has not responded within 5 seconds. Last observed result retained.";
    }, 5_000);
    try {
      const status = await transport.status();
      if (!active || owner !== generation || started !== version || expired) return;
      accept(status);
      state.readError = null;
      if (["saved", "failed", "uncertain", "recovery_required", "stopped"].includes(status.phase)
        || (unknownCommand === "review" && status.phase === "review_ready")
        || (unknownCommand === "discard" && status.phase === "idle")) {
        state.outcomeUnknown = false;
        unknownCommand = null;
      }
    } catch (error) {
      if (active && owner === generation && started === version && !expired) state.readError = detail(error);
    } finally {
      cancelDeadline();
      state.reading = false;
      if (active && (owner !== generation || started !== version || expired)) void refresh();
    }
  }

  function poll(): void {
    const owner = generation;
    cancelPoll = schedule(() => {
      if (!active || owner !== generation) return;
      void refresh();
      poll();
    }, 1_000);
  }
  function startPolling(): void {
    if (active || !available()) return;
    active = true;
    generation += 1;
    void refresh();
    poll();
  }
  function stopPolling(): void {
    active = false;
    generation += 1;
    cancelPoll?.();
    state.readError = "Policy setup status polling is stopped.";
  }

  function blocker(): string | null {
    if (state.status?.phase === "recovery_required") return "Outcome uncertain. Controlled restart and reconciliation required; restart alone does not verify publication.";
    if (!available()) return "Policy setup requires the desktop app and existing TESTNET authority.";
    const blocked = contextBlocker();
    if (blocked) return blocked;
    if (state.command) return "A policy setup command is still awaiting its reply.";
    if (state.outcomeUnknown) return "The command outcome is unknown. Await native status; no new policy operation has been submitted.";
    if (!state.status || state.readError) return "Read current policy setup status before continuing.";
    if (state.status.phase === "stopped") return "Runtime admission is closed. Restart the app before policy setup.";
    if (["reviewing", "persisting"].includes(state.status.phase)) return "Native policy work is in progress.";
    return null;
  }

  async function command(kind: NonNullable<typeof state.command>, invoke: () => Promise<PolicySetupStatus>): Promise<void> {
    state.command = kind;
    state.commandError = null;
    version += 1;
    const observed = observations;
    try {
      const status = await invoke();
      if (observations === observed) accept(status);
    } catch (error) {
      state.commandError = detail(error);
      // Typed synchronous refusals are not unknown transport outcomes.
      state.outcomeUnknown = !isConsoleError(error) || !["prerequisite", "conflict", "validation"].includes(error.kind);
      unknownCommand = state.outcomeUnknown ? kind : null;
      state.readError = "Command reply failed. Awaiting a fresh native status reading.";
    } finally {
      version += 1;
      state.command = null;
      void refresh();
    }
  }

  async function review(candidateEdits: PolicySetupEdits = edits): Promise<void> {
    const blocked = blocker();
    if (blocked) { state.commandError = blocked; return; }
    if (state.status?.review || !["idle", "failed"].includes(state.status!.phase)) {
      state.commandError = "Discard the retained review before requesting another."; return;
    }
    if (!draft.agent.trim() || !/^0x[0-9a-fA-F]{40}$/.test(draft.account.trim()) || !draft.writersStopped) {
      state.commandError = "Enter the existing agent/account and confirm all other policy writers are stopped."; return;
    }
    const candidate: PolicySetupEdits = { ...candidateEdits, symbols: [...candidateEdits.symbols] };
    await command("review", () => transport.review(draft.agent.trim(), draft.account.trim(), candidate, draft.emptySourceConfirmed, draft.writersStopped));
  }

  async function persist(reviewId: number): Promise<void> {
    const blocked = blocker();
    if (blocked) { state.commandError = blocked; return; }
    if (!Number.isSafeInteger(reviewId) || state.status?.review?.id !== reviewId
      || !["review_ready", "uncertain"].includes(state.status.phase)) {
      state.commandError = "The reviewed policy is no longer ready to persist."; return;
    }
    await command("persist", () => transport.persist(reviewId));
  }

  async function discard(reviewId: number): Promise<void> {
    const blocked = blocker();
    if (blocked) { state.commandError = blocked; return; }
    if (!Number.isSafeInteger(reviewId) || state.status?.review?.id !== reviewId
      || !["review_ready", "failed", "saved"].includes(state.status.phase)) {
      state.commandError = "Work in progress or an uncertain outcome cannot be discarded here."; return;
    }
    await command("discard", () => transport.discard(reviewId));
  }

  async function editReviewed(useExisting: boolean): Promise<void> {
    const review = state.status?.review;
    if (!review || !["review_ready", "failed"].includes(state.status!.phase)) return;
    const source = useExisting ? review.before?.guardrails[review.agent] : review.proposed.guardrails[review.agent];
    if (useExisting && !source) return;
    const next = policyEdits(source);
    await discard(review.id);
    if (state.status?.phase !== "idle" || state.outcomeUnknown) return;
    draft.agent = review.agent;
    draft.account = review.account;
    Object.assign(edits, next);
  }

  return { state: readonly(state), draft, edits, blocker, refresh, startPolling, stopPolling, review, persist, discard, editReviewed };
}

export const policySetup = createPolicySetup(
  { status: fetchPolicySetupStatus, review: reviewPolicySetup, persist: persistPolicySetup, discard: discardPolicySetup },
  () => {
    if (consentOwnsContext(pilotConsent.state)) return "Initial consent owns the setup context. Resolve or discard its idle review first.";
    if (shell.network !== "testnet") return "Paused policy setup is TESTNET only. The network is not switched automatically.";
    const mcp = supervision.state;
    if (mcp.stopRequested || mcp.command || mcp.error || mcp.status?.network !== "testnet" || mcp.status.phase !== "idle") {
      return "A current idle TESTNET MCP runtime is required. Stop is terminal; do not stop a listener to attempt setup in this runtime.";
    }
    return null;
  }, inTauri,
);
