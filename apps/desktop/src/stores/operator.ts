import { computed, reactive, readonly } from "vue";
import { fetchOperatorState, inTauri, isConsoleError, type EventPage, type PolicyInspection } from "../lib/bridge";
import { shell } from "./shell";

const state = reactive<{
  ledger: EventPage | null; policy: PolicyInspection | null;
  ledgerError: string | null; policyError: string | null; error: string | null;
  ledgerReadMs: number | null; policyReadMs: number | null; reading: boolean;
}>({ ledger: null, policy: null, ledgerError: null, policyError: null, error: null, ledgerReadMs: null, policyReadMs: null, reading: false });
export const operator = readonly(state);
export const events = computed(() => state.ledger?.events ?? []);
export const storedPolicy = computed(() => state.policy?.state ?? null);
export const policySourceLabel = computed(() => {
  switch (state.policy?.provenance) {
    case "unverified_legacy": return "Unverified legacy policy";
    case "unverified_ledger": return "Unverified ledger policy";
    default: return "Policy source not read";
  }
});
export const recordedAgents = computed(() => [...new Set([
  ...Object.keys(storedPolicy.value?.guardrails ?? {}),
  ...events.value.flatMap(event => event.agent_id ? [event.agent_id] : []),
])].sort());
export async function refreshOperator(): Promise<void> {
  if (state.reading) return;
  if (!inTauri()) { state.error = "Gateway data is available in the desktop app."; return; }
  state.reading = true;
  const network = shell.network;
  try {
    const read = await fetchOperatorState(network);
    if (network !== shell.network) return;
    if (read.ledger.status === "ready") {
      state.ledger = read.ledger.value; state.ledgerReadMs = Date.now(); state.ledgerError = null;
    } else state.ledgerError = read.ledger.detail;
    if (read.policy.status === "ready") {
      state.policy = read.policy.value; state.policyReadMs = read.policy.value.observed_at_ms; state.policyError = null;
    } else state.policyError = read.policy.detail;
    state.error = null;
  } catch (error) {
    state.error = isConsoleError(error) ? error.detail : String(error);
  } finally { state.reading = false; }
}
let poll: ReturnType<typeof setInterval> | null = null;
export function startOperatorPolling(): void {
  if (poll !== null) return;
  void refreshOperator();
  poll = setInterval(() => void refreshOperator(), 5000);
}
export function stopOperatorPolling(): void {
  if (poll !== null) clearInterval(poll);
  poll = null;
}
