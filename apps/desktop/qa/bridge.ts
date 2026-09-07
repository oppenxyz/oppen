// UI fixture transport only. Every read is local; no invoke, venue, key or signer call.
import type { AccountState, ChartSeries, MarketSnapshot, OperatorRead } from "../src/lib/bridge";
export type * from "../src/lib/bridge";
export { isConsoleError } from "../src/lib/bridge";
export let failedRead = false;
export function failNextReads(): void { failedRead = true; }
const scenario = new URLSearchParams(location.search).get("state") ?? "positions";
const symbols = ["BTC", "ETH", "kPEPE", "SOL", "AVAX", "HYPE", "DOGE", "ARB", "LONG-MARKET-NAME", "SUI", "TIA", "PENDLE"];
const account: AccountState = {
  contract_version: 1, network: "testnet", address: "0x0000000000000000000000000000000000000001",
  as_of_ms: Date.now(), feed_age_ms: 0, feed: "live",
  balances: { equity_usd: "123456.78", perps_account_value_usd: "123000", spot_usdc_available: "456.78", total_margin_used_usd: "2400", withdrawable_usd: "120600" },
  positions: symbols.map((symbol, index) => ({
    symbol, size: index % 2 ? "-0.12345678" : "12345.678901",
    entry_px: index === 2 ? "0.0000012345" : "1234.56789",
    position_value_usd: `${(index + 1) * 1000}.12`, unrealized_pnl_usd: index % 2 ? "-12.345" : "24.56789",
    margin_used_usd: "200", liquidation_px: index === 0 ? null : "987.654321",
    liq_distance_frac: index === 0 ? null : "0.123456789", max_leverage: 20,
  })),
  orders: symbols.map((symbol, index) => ({ symbol, oid: 1000 + index, cloid: null, is_buy: index % 2 === 0,
    limit_px: "1234.56789", size: "0.123456789", original_size: "0.123456789", reduce_only: index % 2 === 1,
    is_trigger: false, trigger_px: null, placed_ts_ms: Date.now() })),
};
export const inTauri = () => true;
export async function fetchAccountState(): Promise<AccountState> {
  if (failedRead || scenario === "unavailable") throw { kind: "venue", detail: "UI fixture: account read failed. This intentionally long diagnostic must remain readable and must not hide the last successful positions or change them into zero balances." };
  return { ...account, as_of_ms: Date.now(), ...(scenario === "empty" ? { positions: [], orders: [] } : {}) };
}
export async function fetchKeychainStatus() { return { reachable: false, detail: "UI fixture: no keychain is accessed." }; }
export async function fetchPilotStatus() { return null; }
export async function fetchOperatorState(): Promise<OperatorRead> {
  if (scenario === "loading") await new Promise(resolve => setTimeout(resolve, 10000));
  return { network: "testnet", policy: { status: "unavailable", detail: "UI fixture: no live operator connection." },
    ledger: { status: "ready", value: { events: [], head_seq: 0, next_cursor: 0, resync_required: false } } };
}
export async function fetchMarkets() { return []; }
export async function fetchChartSeries(): Promise<ChartSeries> { throw new Error("UI fixture: chart not supplied."); }
export async function fetchMarketSnapshot(): Promise<MarketSnapshot> { throw new Error("UI fixture: market snapshot not supplied."); }
export async function watchMarket() {}
export async function onFeedUpdate() { return () => {}; }

export async function checkUpdate() { throw { kind: "unavailable", detail: "Updates are unavailable in the UI fixture." }; }
export async function downloadUpdate() { throw new Error("UI fixture: no updates."); }
export async function installUpdate() { throw new Error("UI fixture: no installation."); }
