import { computed, reactive, readonly } from "vue";
import type { ChannelHealthRow, ChannelHealthSnapshot, ChartBinding, MarketChannel } from "../lib/bridge";

export const MARKET_CHANNELS: readonly MarketChannel[] = ["context", "bbo", "depth", "trades", "candles"];
const DEADLINE_MS = 5000;
function after(callback: () => void, delay: number) {
  const timer = setTimeout(callback, delay);
  return () => clearTimeout(timer);
}
function sameBinding(a: ChartBinding, b: ChartBinding): boolean {
  return a.network === b.network && a.generation === b.generation && a.selection_id === b.selection_id
    && a.symbol === b.symbol && a.interval === b.interval;
}

/** Only accepted native observations renew this monotonic UI deadline. */
export function createMarketHealth(now = () => performance.now(), schedule = after) {
  const state = reactive<{
    binding: ChartBinding | null; snapshot: ChannelHealthSnapshot | null; current: boolean;
    acceptedAt: number | null; detail: string | null;
  }>({ binding: null, snapshot: null, current: false, acceptedAt: null, detail: null });
  let cancel: (() => void) | null = null;
  let lease = 0;
  function historical(detail: string) {
    lease++; cancel?.(); cancel = null; state.current = false; state.detail = detail;
  }
  function invalidate() { historical("Selection unavailable"); state.binding = null; }
  function bind(binding: ChartBinding) {
    historical("Awaiting channel observation"); state.binding = { ...binding };
  }
  function arm(owner: number) {
    const elapsed = now() - state.acceptedAt!;
    cancel = schedule(() => {
      if (owner !== lease) return;
      const age = now() - state.acceptedAt!;
      if (!Number.isFinite(age) || age < 0 || age >= DEADLINE_MS) historical("Health observation expired");
      else arm(owner);
    }, Math.max(0, DEADLINE_MS - elapsed));
  }
  function accept(snapshot: ChannelHealthSnapshot | null): boolean {
    if (!snapshot || !state.binding || !sameBinding(snapshot.binding, state.binding)
      || !/^\d+$/.test(snapshot.revision) || snapshot.rows.length !== MARKET_CHANNELS.length
      || snapshot.rows.some((row, i) => row.channel !== MARKET_CHANNELS[i]
        || row.owner !== (i < 3 ? "console" : "chart"))) return false;
    const prior = state.snapshot;
    if (prior && sameBinding(prior.binding, snapshot.binding) && BigInt(snapshot.revision) <= BigInt(prior.revision)) return false;
    // A later registry sample cannot erase terminal failure or known channel loss.
    const sameConsole = prior?.binding.network === snapshot.binding.network && prior.binding.generation === snapshot.binding.generation;
    const rows = snapshot.rows.map((row, i) => {
      const previous = prior?.rows[i];
      const sameOwner = sameConsole && (row.owner === "console" || prior?.binding.selection_id === snapshot.binding.selection_id);
      return sameOwner && previous ? { ...row, consumer_failure: previous.consumer_failure ?? row.consumer_failure,
        last_loss: row.last_loss ?? (sameBinding(prior!.binding, snapshot.binding) ? previous.last_loss : null) } : row;
    });
    cancel?.(); const owner = ++lease;
    state.snapshot = { ...snapshot, rows };
    state.acceptedAt = now(); state.current = true; state.detail = null;
    arm(owner); return true;
  }
  return { state: readonly(state), bind, invalidate, historical, accept };
}

export const marketHealth = createMarketHealth();

export function channelStatus(row: Readonly<ChannelHealthRow>): string {
  if (row.consumer_failure !== null) return "Consumer failed";
  if (row.quarantined) return "Quarantined";
  if (!row.subscribed) return "Not subscribed";
  if (!row.connected) return "Disconnected";
  if (!row.acked) return "Awaiting ACK";
  if (row.clock_uncertain) return "Clock uncertain";
  if (row.last_received_at_ms === null) return "No observation";
  if (row.age_budget_exceeded === true) return "Age budget exceeded";
  return "Acknowledged";
}

export const marketTransport = computed(() => {
  const { snapshot, current } = marketHealth.state;
  if (!snapshot) return "Unavailable";
  if (!current) return "Historical";
  if (snapshot.rows.every(row => row.connected)) return "Connected";
  if (snapshot.rows.some(row => row.connected)) return "Mixed";
  return "Disconnected";
});
export const marketChannels = computed(() => {
  const { snapshot, current } = marketHealth.state;
  if (!snapshot) return "Unavailable";
  if (!current) return "Historical";
  return !snapshot.clock_uncertain && snapshot.rows.every(row => channelStatus(row) === "Acknowledged") ? "Acknowledged" : "Degraded";
});
