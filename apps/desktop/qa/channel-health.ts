import type { ChannelHealthSnapshot, ChartBinding } from "../src/lib/bridge";

export function channelHealthFixture(binding: ChartBinding, revision = "1", scenario = "connected"): ChannelHealthSnapshot {
  const channels = ["context", "bbo", "depth", "trades", "candles"] as const;
  const snapshot: ChannelHealthSnapshot = {
    pool_last_losses: [],
    binding: { ...binding }, revision, observed_at_ms: 1788998400000, clock_uncertain: scenario === "clock",
    rows: channels.map((channel, index) => ({
      owner: "selected", channel, connection_id: "1", subscribed: true,
      connected: scenario !== "disconnected", acked: scenario !== "reconnect", quarantined: scenario === "quarantine" && channel === "bbo",
      last_received_at_ms: 1788998399900, age_ms: 100, threshold_ms: [5000, 2000, 15000, null, null][index]!,
      age_budget_exceeded: index < 3 ? scenario === "quiet" && channel === "bbo" : null,
      clock_uncertain: scenario === "clock", last_loss: null,
      consumer_failure: scenario === "failure" ? "Synthetic consumer failed <script>not executable</script> " + "diagnostic/".repeat(20) : null,
    })), diagnostics: [], omitted_diagnostics: scenario === "failure" ? 4 : 0,
  };
  return snapshot;
}
