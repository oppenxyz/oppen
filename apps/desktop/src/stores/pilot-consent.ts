import { reactive, readonly, toRaw, type DeepReadonly } from "vue";
import { fetchPilotConsentStatus, reviewPilotConsent, confirmPilotConsent, discardPilotConsent, reconcilePilotConsent, inTauri,
  type PilotConsentStatus, type PilotConsentOperation, type PilotConsentAttestations } from "../lib/bridge";

function after(callback: () => void, ms: number) { const timer = setTimeout(callback, ms); return () => clearTimeout(timer); }
function detail(error: unknown): string {
  return error && typeof error === "object" && "detail" in error && typeof error.detail === "string" ? error.detail : error instanceof Error ? error.message : String(error);
}
function sameOperation(a: PilotConsentOperation | null, b: PilotConsentOperation | null) {
  return a && b ? a.kind === b.kind && (a.kind === "review" || (b.kind !== "review" && a.review_id === b.review_id)) : a === b;
}
export function consentOwnsContext(state: { command: string | null; outcomeUnknown: boolean; status: DeepReadonly<PilotConsentStatus> | null }): boolean {
  return !!state.command || state.outcomeUnknown || ["reviewing", "review_ready", "confirming", "uncertain", "recovery_required"].includes(state.status?.phase ?? "");
}
export function consentCanEditIdentity(state: { command: string | null; outcomeUnknown: boolean; status: DeepReadonly<PilotConsentStatus> | null }): boolean {
  if (state.command || state.outcomeUnknown) return false;
  const status = state.status;
  return !status || (["refused", "closed"].includes(status.phase) && !status.existing && !status.receipt
    && status.error?.status !== "uncertain" && status.resolution?.status !== "committed" && status.resolution?.status !== "unknown");
}
export function consentReviewBlocker(status: DeepReadonly<PilotConsentStatus> | null, now = Date.now()): string | null {
  const display = status?.review?.display;
  if (!display) return "A retained native consent review is required.";
  const route = display.correlation.route;
  if (display.network !== "testnet" || route.network !== "testnet" || display.account.network !== "testnet"
    || route.binding.agent !== status.agent || route.binding.container.toLowerCase() !== status.account.toLowerCase()
    || display.account.address.toLowerCase() !== status.account.toLowerCase()
    || display.coverage.account.toLowerCase() !== status.account.toLowerCase() || display.coverage.network !== "testnet") return "Consent evidence does not match the reviewed TESTNET identity.";
  if (!display.coverage.terminated_by_short_page || display.coverage.pages < 1) return "Complete observation pagination is required.";
  if (now < display.observed_at_ms || now >= display.expires_at_ms) return "Consent review expired or clock changed. A fresh review is required.";
  if (display.account.orders.length || display.account.positions.some(position => !/^[-+]?0*(?:\.0*)?$/.test(position.size))) return "Fresh flat positions and no resting orders are required.";
  return null;
}
export function createPilotConsent(transport: {
  status: typeof fetchPilotConsentStatus; review: typeof reviewPilotConsent; confirm: typeof confirmPilotConsent;
  discard: typeof discardPilotConsent; reconcile: typeof reconcilePilotConsent;
}, available = () => true, schedule = after) {
  const state = reactive<{
    status: PilotConsentStatus | null; observed: boolean; reading: boolean; readError: string | null;
    command: PilotConsentOperation["kind"] | null; commandError: string | null; outcomeUnknown: boolean;
    priorUnknown: string[]; retainedReviewId: string | null; context: { network: "testnet" | "mainnet"; blocked: string | null };
  }>({ status: null, observed: false, reading: false, readError: null, command: null, commandError: null,
    outcomeUnknown: false, priorUnknown: [], retainedReviewId: null, context: { network: "testnet", blocked: "Read setup prerequisites first." } });
  const draft = reactive({ agent: "", account: "" });
  let active = false, generation = 0, version = 0, observations = 0;
  let token: symbol | null = null, cancelPoll: (() => void) | null = null;
  let readToken: symbol | null = null;
  let pending: { owner: string | null; seq: number; operation: PilotConsentOperation; agent: string; account: string } | null = null;
  let rejectedAdmission: { owner: string | null; seq: number } | null = null;
  const retired = new Set<string>();
  function accept(status: PilotConsentStatus | null): boolean {
    if (!status) {
      if (pending && rejectedAdmission?.owner === null && !state.status) {
        pending = null; rejectedAdmission = null; token = null; state.command = null; state.outcomeUnknown = false;
      }
      if (state.status || pending) { state.readError = "No current consent owner observed; retained outcomes remain unverified."; return false; }
      state.observed = true; observations++; return true;
    }
    if (!status.owner_id || !status.agent || !/^0x[0-9a-fA-F]{40}$/.test(status.account)
      || !Number.isSafeInteger(status.operation_seq) || status.operation_seq < 0) throw new Error("Consent owner identity or operation sequence is unavailable.");
    if (retired.has(status.owner_id)) return false;
    const previous = state.status;
    const unchangedRejectedOwner = !!pending && rejectedAdmission?.owner === status.owner_id
      && rejectedAdmission.seq === status.operation_seq && previous?.owner_id === status.owner_id
      && previous.operation_seq === status.operation_seq && previous.agent === status.agent
      && previous.account.toLowerCase() === status.account.toLowerCase()
      && sameOperation(previous.last_operation, status.last_operation);
    if (pending && rejectedAdmission?.owner && rejectedAdmission.owner !== status.owner_id) return false;
    if (pending && !unchangedRejectedOwner && (pending.agent !== status.agent || pending.account.toLowerCase() !== status.account.toLowerCase())) throw new Error("Consent status does not match the requested identity.");
    if (previous?.owner_id === status.owner_id) {
      if (previous.agent !== status.agent || previous.account.toLowerCase() !== status.account.toLowerCase()) return false;
      const degradation = status.phase === "recovery_required" || (status.phase === "uncertain" && previous.phase !== "recovery_required");
      if (previous.phase === "recovery_required" && status.phase !== "recovery_required") return false;
      if (previous.phase === "uncertain" && status.operation_seq === previous.operation_seq && !degradation) return false;
      if (status.operation_seq < previous.operation_seq || (previous.phase === "closed" && status.phase !== "closed" && !degradation)) return false;
      if (status.operation_seq === previous.operation_seq && (!sameOperation(previous.last_operation, status.last_operation)
        || (!degradation && !["reviewing", "confirming"].includes(previous.phase) && previous.phase !== status.phase && status.phase !== "closed"))) return false;
      if (degradation && !status.receipt && previous.receipt) status = { ...status, receipt: structuredClone(toRaw(previous.receipt)) };
    } else if (previous) retired.add(previous.owner_id);
    if (pending?.owner && pending.owner !== status.owner_id) {
      state.priorUnknown.push(`Consent owner ${pending.owner}, ${pending.operation.kind} operation ${pending.seq}: previous outcome remains unverified.`);
      pending = null; token = null; state.command = null; state.outcomeUnknown = false;
    }
    state.status = status; state.observed = true; observations++;
    if (unchangedRejectedOwner) {
      pending = null; rejectedAdmission = null; token = null; state.command = null; state.outcomeUnknown = false;
    }
    if (previous?.owner_id !== status.owner_id) { draft.agent = status.agent; draft.account = status.account; }
    if (status.review) state.retainedReviewId = status.review.id;
    if (pending && (!pending.owner || pending.owner === status.owner_id) && pending.seq === status.operation_seq
      && sameOperation(pending.operation, status.last_operation)) {
      const kind = pending.operation.kind;
      if (["refused", "uncertain", "recovery_required", "existing"].includes(status.phase) || (kind === "review" && status.phase === "review_ready")
        || (kind === "confirm" && status.phase === "authorized") || (kind === "discard" && ["idle", "closed"].includes(status.phase))
        || (status.phase === "closed" && status.error?.status === "refused")
        || (kind === "reconcile" && status.resolution !== null)) {
        pending = null; token = null; state.command = null; state.outcomeUnknown = false;
      }
    }
    return true;
  }
  async function refresh() {
    if (!active || state.reading || !available()) return;
    state.reading = true; const owner = generation, started = version;
    const attempt = Symbol("consent-status"); readToken = attempt;
    const cancel = schedule(() => {
      if (readToken !== attempt) return;
      readToken = null; state.reading = false;
      if (active) {
        if (owner === generation && started === version) state.readError = "Consent status has not responded within 5 seconds. Evidence retained.";
        void refresh();
      }
    }, 5000);
    try { const status = await transport.status(); if (readToken === attempt && active && owner === generation && started === version && accept(status)) state.readError = null; }
    catch (error) { if (readToken === attempt && active && owner === generation && started === version) state.readError = detail(error); }
    finally {
      cancel();
      if (readToken === attempt) {
        readToken = null; state.reading = false;
        if (active && (owner !== generation || started !== version)) void refresh();
      }
    }
  }
  function poll() { cancelPoll = schedule(() => { if (active) { void refresh(); poll(); } }, 1000); }
  function startPolling() { if (!active && available()) { active = true; generation++; void refresh(); poll(); } }
  function stopPolling() { active = false; generation++; cancelPoll?.(); state.readError = "Consent polling stopped."; }
  function setContext(context: typeof state.context) {
    if (JSON.stringify(context) === JSON.stringify(state.context)) return;
    state.context = context; generation++; version++; state.readError = "Read current consent status after the context change."; void refresh();
  }
  function blocker(reconcile = false): string | null {
    if (state.status?.phase === "recovery_required") return "Consent recovery required. This owner cannot reconcile. Controlled recovery and verified reconciliation are required; restart alone does not verify publication.";
    if (!available() || state.context.network !== "testnet") return "Initial pilot consent requires the TESTNET desktop runtime.";
    if (state.context.blocked) return state.context.blocked;
    if (!state.observed || state.readError) return "Read current consent status before continuing.";
    if (state.command || ["reviewing", "confirming"].includes(state.status?.phase ?? "")) return "Native consent work is in progress.";
    if (!reconcile && (state.outcomeUnknown || state.status?.phase === "uncertain")) return "Consent outcome unknown. Read-only reconciliation only; no authorization retry.";
    return null;
  }
  async function run(operation: PilotConsentOperation, agent: string, account: string, invoke: () => Promise<PilotConsentStatus>) {
    const current = state.status;
    const seq = operation.kind === "review" ? 1 : (current?.operation_seq ?? 0) + 1;
    if (!Number.isSafeInteger(seq)) { state.commandError = "Consent operation sequence exhausted."; return; }
    const commandToken = Symbol(operation.kind); token = commandToken;
    rejectedAdmission = null;
    pending = { owner: operation.kind === "review" ? null : current?.owner_id ?? null, seq, operation, agent, account };
    state.command = operation.kind; state.commandError = null; state.outcomeUnknown = true; version++;
    const owner = generation, observed = observations;
    try { const status = await invoke(); if (token === commandToken && owner === generation && observed === observations) accept(status); }
    catch (error) {
      if (token === commandToken && owner === generation) {
        state.commandError = detail(error);
        // Native command errors are pre-admission; worker refusals arrive in cached status.
        if (error && typeof error === "object" && "status" in error && error.status === "refused" && "detail" in error && typeof error.detail === "string") {
          rejectedAdmission = { owner: current?.owner_id ?? null, seq: current?.operation_seq ?? 0 };
        }
        state.readError = "Consent reply unavailable. Await current native evidence.";
      }
    }
    finally { if (token === commandToken) { token = null; version++; state.command = null; void refresh(); } else if (!token) void refresh(); }
  }
  async function review() {
    const blocked = blocker(); if (blocked) { state.commandError = blocked; return; }
    if (!consentCanEditIdentity(state)) { state.commandError = "Existing consent is inspect-only; unresolved work cannot be replaced or reset."; return; }
    const agent = draft.agent.trim(), account = draft.account.trim();
    if (!agent || !/^0x[0-9a-fA-F]{40}$/.test(account)) { state.commandError = "Enter the existing agent and full TESTNET account."; return; }
    await run({ kind: "review" }, agent, account, () => transport.review(agent, account));
  }
  async function confirm(owner: string, id: string, attestations: PilotConsentAttestations) {
    const blocked = blocker(); if (blocked) { state.commandError = blocked; return; }
    const status = state.status;
    if (!status || status.owner_id !== owner || status.review?.id !== id || status.phase !== "review_ready"
      || !/^0x[0-9a-fA-F]{40}$/.test(attestations.typed_account) || attestations.typed_account.toLowerCase() !== status.account.toLowerCase()
      || !attestations.never_used_for_in_scope_trading || !attestations.dedicated_account_exclusive_use || !attestations.original_baseline_and_no_reset_confirmed) {
      state.commandError = "Type the exact reviewed account and independently confirm every statement."; return;
    }
    const error = consentReviewBlocker(status); if (error) { state.commandError = error; return; }
    const snapshot = { ...attestations };
    await run({ kind: "confirm", review_id: id }, status.agent, status.account, () => transport.confirm(owner, id, snapshot));
  }
  async function retained(kind: "discard" | "reconcile", owner: string, id: string) {
    const blocked = blocker(kind === "reconcile"); if (blocked) { state.commandError = blocked; return; }
    const status = state.status;
    if (!status || status.owner_id !== owner || (kind === "discard" ? status.phase !== "review_ready" || status.review?.id !== id : state.retainedReviewId !== id)) {
      state.commandError = "A matching retained owner and review ID are required."; return;
    }
    await run({ kind, review_id: id }, status.agent, status.account, () => transport[kind](owner, id));
  }
  return { state: readonly(state), draft, setContext, blocker, refresh, startPolling, stopPolling, review, confirm,
    discard: (owner: string, id: string) => retained("discard", owner, id), reconcile: (owner: string, id: string) => retained("reconcile", owner, id) };
}
export const pilotConsent = createPilotConsent({ status: fetchPilotConsentStatus, review: reviewPilotConsent,
  confirm: confirmPilotConsent, discard: discardPilotConsent, reconcile: reconcilePilotConsent }, inTauri);
