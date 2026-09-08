import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp } from "vue";
import { renderToString } from "vue/server-renderer";
import { channelHealthFixture } from "./channel-health";

test("channel readout separates connected age exhaustion, escaped failure and retained diagnostics", async () => {
  const server = await createServer({ root: fileURLToPath(new URL("..", import.meta.url)), configFile: false,
    plugins: [vue()], server: { middlewareMode: true, hmr: false } });
  const { marketHealth } = await server.ssrLoadModule("/src/stores/market-health.ts");
  try {
    const { default: Panel } = await server.ssrLoadModule("/src/components/MarketChannelHealth.vue");
    const binding = { network: "testnet" as const, generation: "1", selection_id: "1", symbol: "BTC", interval: "1h" };
    marketHealth.bind(binding); marketHealth.accept(channelHealthFixture(binding, "1", "quiet"));
    const quiet = await renderToString(createSSRApp(Panel));
    expect(quiet).toContain("Transport · Connected");
    expect(quiet).toContain("Age budget exceeded");
    expect(quiet).toContain("Event-driven");
    const evicted = channelHealthFixture(binding, "2");
    evicted.pool_last_losses = [{ owner: "console", connection_id: "17", subscription_key: null, channel: null,
      kind: "parse_loss", received_at_ms: 1788998300000, detail: "<script>Malformed connection frame</script>" }];
    evicted.diagnostics = Array.from({ length: 16 }, (_, index) => ({ owner: "console", connection_id: "17",
      subscription_key: null, channel: null, kind: "reconnect", received_at_ms: 1788998400000 + index, detail: "Reconnected" }));
    evicted.omitted_diagnostics = 1;
    marketHealth.accept(evicted);
    const retainedLoss = await renderToString(createSSRApp(Panel));
    expect(retainedLoss).toContain("Channels · Acknowledged");
    expect(retainedLoss).toContain("Retained pool losses 1");
    expect(retainedLoss).toContain("Retained pool loss · console / 17 · Connection-scoped · Transport · parse_loss");
    expect(retainedLoss).toContain("&lt;script&gt;Malformed connection frame&lt;/script&gt;");
    expect(retainedLoss).not.toContain("<script>Malformed connection frame</script>");
    expect(evicted.rows.every(row => row.last_loss === null)).toBe(true);
    expect(evicted.diagnostics.some(diagnostic => diagnostic.kind === "parse_loss")).toBe(false);
    const absent = channelHealthFixture(binding, "3");
    absent.rows[1]!.subscribed = false; absent.rows[1]!.threshold_ms = null;
    marketHealth.accept(absent);
    expect(await renderToString(createSSRApp(Panel))).toContain("Budget unavailable");
    marketHealth.accept(channelHealthFixture(binding, "4", "failure"));
    marketHealth.historical("Health observation expired");
    const failed = await renderToString(createSSRApp(Panel));
    expect(failed).toContain("Historical");
    expect(failed).toContain("Omitted 4");
    expect(failed).toContain("&lt;script&gt;not executable&lt;/script&gt;");
    expect(failed).not.toContain("<script>not executable</script>");
    expect(failed).toContain('data-health="candles"');
  } finally { marketHealth.invalidate(); await server.close(); }
});
