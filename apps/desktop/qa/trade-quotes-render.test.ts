import { expect, spyOn, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";

test("TRADE renders live quotes without REST features, then unavailable sides and retained timestamps", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)), configFile: false,
    plugins: [vue(), {
      name: "static-decorative-clock",
      enforce: "pre",
      load(id) {
        if (id.endsWith("/src/lib/clock.ts")) return 'import { ref } from "vue"; export const useClock = () => ref(0);';
      },
    }],
    server: { middlewareMode: true, hmr: false },
  });
  let restoreClock: (() => void) | undefined;
  try {
    const { default: Trade } = await server.ssrLoadModule("/src/views/TradeView.vue");
    const { select, applyFeed, market, quotes } = await server.ssrLoadModule("/src/stores/market.ts");
    const clock = spyOn(Date, "now").mockReturnValue(10000);
    restoreClock = () => clock.mockRestore();
    const render = () => renderToString(createSSRApp(Trade));
    await select("BTC");
    applyFeed({ kind: "bbo", coin: "BTC", at_ms: 1000,
      bid: { px: "78575.000", sz: "1", n: 1 }, ask: { px: "78581", sz: "1", n: 1 } });
    const live = await render();
    expect(live).toContain('data-quote="bid"');
    expect(live).toContain('data-quote="ask"');
    expect(live).toContain("78575.000");
    expect(live).toMatch(/<div class="label"[^>]*>BTC<\/div>/);
    expect(live).toContain("Latest observed approx. spread");
    expect(live).not.toContain("Current approx. spread");
    expect(live).toContain("0.76 bp");
    expect(live).toContain("Latest observed BBO touch");
    expect(live).toContain("1970-01-01T00:00:01.000Z");
    expect(live).toContain("Observed by UI");
    expect(live).toContain("Depth unavailable");
    expect(live).toContain("Host request started Not read");
    expect(live).toContain("0s ago");
    const row = { symbol: "BTC", mark_px: "78580", funding_1h_bps: "0", open_interest: "1", day_volume_usd: "1", has_book: true };
    toRaw(market).rows = [row];
    clock.mockReturnValue(25000);
    applyFeed({ kind: "ctx", row: { ...row, mark_px: "78582" } });
    expect(market.rows[0].mark_px).toBe("78582");
    expect(quotes.touch.observedMs).toBe(10000);
    const quiet = await render();
    expect(quiet).toContain("15s ago");
    expect(quiet).toContain("1970-01-01T00:00:01.000Z");
    expect(quiet).toContain("Latest observed approx. spread");
    expect(quiet).not.toContain("Current approx. spread");
    clock.mockReturnValue(75000);
    applyFeed({ kind: "ctx", row: { ...row, mark_px: "78583" } });
    expect(await render()).toContain("65s ago");
    clock.mockReturnValue(75500);
    applyFeed({ kind: "bbo", coin: "BTC", at_ms: 1001 });
    const empty = await render();
    expect(empty).toContain("0s ago");
    expect(empty).not.toContain("UI clock moved backward");
    expect(empty).not.toContain("78575.000");
    expect(empty).not.toContain("0.76 bp");
    expect(empty).toContain("Best bid");
    expect(empty).toContain("Best ask");
    expect(empty).toContain("Unavailable");
    clock.mockReturnValue(74000);
    expect(await render()).toContain("UI clock moved backward");
    applyFeed({ kind: "status", connected: false });
    const retained = await render();
    expect(retained).toContain("Retained touch · not live");
    expect(retained).toContain("1970-01-01T00:00:01.001Z");
    applyFeed({ kind: "bbo", coin: "BTC", at_ms: 9000000000000000 });
    expect(await render()).toContain("Venue Unavailable");
  } finally { restoreClock?.(); await server.close(); }
});
