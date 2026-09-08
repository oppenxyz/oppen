import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, reactive, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import type { AccountState } from "../src/lib/bridge";

test("account notice hides healthy observations and foreign-owner failures, retaining scoped diagnostics", async () => {
  const server = await createServer({ root: fileURLToPath(new URL("..", import.meta.url)), configFile: false,
    plugins: [vue()], optimizeDeps: { noDiscovery: true, entries: [] }, server: { middlewareMode: true, hmr: false } });
  try {
    const { default: Notice } = await server.ssrLoadModule("/src/components/AccountNotice.vue");
    const { shell, accountObservation, accountObservationLabel } = await server.ssrLoadModule("/src/stores/shell.ts");
    const account: AccountState = { contract_version: 1, network: "testnet", address: "0x0000000000000000000000000000000000000001",
      as_of_ms: 1000, feed_age_ms: 0, feed: "live", balances: { equity_usd: "1", perps_account_value_usd: "1", spot_usdc_available: "0", total_margin_used_usd: "0", withdrawable_usd: "1" }, positions: [], orders: [] };
    Object.assign(reactive(toRaw(shell)), { network: "testnet", account, accountError: null });
    const binding = { network: "testnet", generation: "2" };
    Object.assign(reactive(toRaw(accountObservation)), { binding, failure: null });
    const render = () => renderToString(createSSRApp(Notice));
    expect(await render()).not.toContain('class="account-notice"');
    Object.assign(reactive(toRaw(shell)), { account: { ...account, feed: "stale", feed_age_ms: 30000 } });
    expect(await render()).toContain("Stale observation · 30000 ms at last read");
    Object.assign(reactive(toRaw(shell)), { account });
    Object.assign(reactive(toRaw(accountObservation)), { failure: { binding, detail: "<script>Account consumer failed</script>" } });
    const failed = await render();
    expect(failed).toContain("Consumer failed");
    expect(failed).toContain("&lt;script&gt;Account consumer failed&lt;/script&gt;");
    expect(failed).not.toContain("<script>Account consumer failed</script>");
    expect(failed).toContain("<details");
    for (const retired of [{ ...binding, network: "mainnet" }, { ...binding, generation: "1" }]) {
      Object.assign(reactive(toRaw(accountObservation)), { failure: { binding: retired, detail: "Foreign owner failure" } });
      expect(await render()).not.toContain('class="account-notice"');
      expect(accountObservationLabel()).toBe("0 ms at last read");
    }
    Object.assign(reactive(toRaw(shell)), { account: null });
    expect(await render()).toContain("Observation unavailable");
  } finally { await server.close(); }
});
