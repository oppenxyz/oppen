import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import type { ChartBinding, ChartProjection } from "../src/lib/bridge";

test("chart projection renders retained bars beside errors with independent trade and exact provenance", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)), configFile: false,
    plugins: [vue(), { name: "static-chart-decoration", enforce: "pre", load(id) {
      if (id.endsWith("/src/lib/clock.ts")) return 'import { ref } from "vue"; export const useClock = () => ref(0);';
    } }], server: { middlewareMode: true, hmr: false },
  });
  try {
    const { default: Trade } = await server.ssrLoadModule("/src/views/TradeView.vue");
    const { chartObservation, createChartObservations, select } = await server.ssrLoadModule("/src/stores/market.ts");
    await select("BTC");
    const binding: ChartBinding = { network: "testnet", generation: "1", selection_id: "1", symbol: "BTC", interval: "1h" };
    const projection: ChartProjection = {
      selection_id: "1", revision: "1", symbol: "BTC", interval: "1h", interval_ms: 3600000, price_decimals: null,
      closed: [{ time_ms: 0, open: "99", high: "102", low: "98", close: "100", volume: "2.123400",
        source: "observed_trades", partial: true, open_close_ambiguous: true, received_at_ms: 2000 }],
      forming: { time_ms: 3600000, open: "100", high: "110", low: "90", close: "101", volume: "12.5000",
        source: "venue", partial: false, open_close_ambiguous: false, received_at_ms: 3000 },
      latest_trade: { time_ms: 3600001, price: "125.00000", price_ambiguous: true },
      history_error: "<script>history failed</script>", observation_error: "Conflicting print identity",
      last_observation_received_at_ms: 3000, tape_status: "invalid_observation",
    };
    const fixture = createChartObservations(async () => projection);
    fixture.bind(binding); fixture.accept(projection); fixture.retain();
    Object.assign(toRaw(chartObservation), toRaw(fixture.state));
    const html = await renderToString(createSSRApp(Trade));
    expect(html).toContain('data-chart="history-error"');
    expect(html).toContain("&lt;script&gt;history failed&lt;/script&gt;");
    expect(html).not.toContain("<script>history failed</script>");
    expect(html).toContain('role="img"');
    expect(html).toContain("Last candle close 101");
    expect(html).toContain("Latest observed trade 125.00000");
    expect(html).toContain("Ambiguous");
    expect(html).toContain("Tape frozen · invalid observation");
    expect(html).toContain("Retained chart · not live");
    expect(html).toContain("Venue 1 · Observed tape 1 · Partial 1 · Ambiguous O/C 1");
    expect(html).toContain("Observed trades only");
    expect(html).toContain("Partial coverage");
    expect(html).toContain("Volume 12.5000 BTC");
    expect(html).toContain("display fallback: 6 decimals");
    expect(html).toContain("Host observed 1970-01-01T00:00:03.000Z");
    fixture.fail({ binding, detail: "<script>consumer terminated</script>" });
    Object.assign(toRaw(chartObservation), toRaw(fixture.state));
    const stopped = await renderToString(createSSRApp(Trade));
    expect(stopped).toContain('data-chart="consumer-error"');
    expect(stopped).toContain("Selected market consumer stopped");
    expect(stopped).toContain("&lt;script&gt;consumer terminated&lt;/script&gt;");
    expect(stopped).not.toContain("<script>consumer terminated</script>");
    expect(stopped).toContain("Last candle close 101");
    fixture.invalidate(true); fixture.bind({ ...binding, selection_id: "2" });
    fixture.accept({ ...projection, selection_id: "2", revision: "2", forming: null, closed: [] });
    Object.assign(toRaw(chartObservation), toRaw(fixture.state));
    expect(await renderToString(createSSRApp(Trade))).toContain("Latest observed trade 125.00000");
  } finally { await server.close(); }
});
