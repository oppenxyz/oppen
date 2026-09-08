import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp } from "vue";
import { renderToString } from "vue/server-renderer";
import type { CancelTarget } from "../src/lib/bridge";

test("cancellation review distinguishes observed and unknown TIF without interpreting venue text", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)), configFile: false, plugins: [vue()],
    server: { middlewareMode: true, hmr: false },
  });
  try {
    const { default: Targets } = await server.ssrLoadModule("/src/components/ApprovalCancelTargets.vue");
    const target: CancelTarget = {
      symbol: "TEST", asset_index: 0, oid: 501, cloid: null, is_buy: true,
      limit_px: "100", sz: "0.12", orig_sz: "0.15", timestamp: 100,
      order_type: "Limit", tif: "Gtc", reduce_only: false, is_trigger: false,
      trigger_px: "0", trigger_condition: "N/A", is_position_tpsl: false,
    };
    const render = (row: CancelTarget) => renderToString(createSSRApp(Targets, { targets: [row] }));
    expect(await render(target)).toContain("Time in force: GTC");
    const { tif: _tif, ...historical } = target;
    expect(await render(historical)).toContain("Time in force: Unknown");
    expect(await render({ ...target, tif: null })).toContain("Time in force: Unknown");
    const trigger = await render({ ...historical, is_trigger: true, order_type: "Stop Market",
      trigger_px: "105", trigger_condition: "<script>untrusted</script>" });
    expect(trigger).not.toContain("Time in force:");
    expect(trigger).toContain("&lt;script&gt;");
    expect(trigger).not.toContain("<script>");
    expect(trigger).toContain("Original size 0.15");
    expect(trigger).toContain("0.12 @ 100");
  } finally { await server.close(); }
});
