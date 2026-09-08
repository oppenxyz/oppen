import { channelHealthFixture } from "../../qa/channel-health";
import { createMarketHealth, channelStatus, marketHealth } from "./market-health";
import { receiveSelectedFeed, quotes, quoteValidity, select } from "./market";
import type { ChartBinding } from "../lib/bridge";

declare const test: (name: string, body: () => void) => void;
declare const expect: (actual: unknown) => { toBe(expected: unknown): void; toEqual(expected: unknown): void };
const binding: ChartBinding = { network: "testnet", generation: "1", selection_id: "1", symbol: "BTC", interval: "1h" };
function fixture() {
  let now = 0;
  const tasks: Array<{ at: number; active: boolean; run: () => void }> = [];
  const health = createMarketHealth(() => now, (run, delay) => {
    const task = { at: now + delay, active: true, run }; tasks.push(task); return () => { task.active = false; };
  });
  health.bind(binding);
  return { health, advance(value: number) { now = value; for (const task of tasks) if (task.active && task.at <= now) { task.active = false; task.run(); } } };
}
test("null and non-increasing observations never renew the independent monotonic deadline", () => {
  const f = fixture(); const sample = channelHealthFixture(binding); f.health.accept(sample);
  for (let at = 1000; at <= 5000; at += 1000) { f.advance(at); f.health.accept(null); f.health.accept(sample); }
  expect(f.health.state.current).toBe(false);
  expect(f.health.state.snapshot?.revision).toBe("1");
  expect(f.health.accept(channelHealthFixture(binding, "2"))).toBe(true);
  expect(f.health.state.current).toBe(true);
});
test("selection ABA, generation, network and interval reject old observations without refreshing", () => {
  const f = fixture(); f.health.accept(channelHealthFixture(binding)); f.health.invalidate();
  const next = { ...binding, selection_id: "3" }; f.health.bind(next);
  for (const stale of [binding, { ...next, network: "mainnet" as const }, { ...next, generation: "2" }, { ...next, interval: "5m" }]) {
    expect(f.health.accept(channelHealthFixture(stale, "100"))).toBe(false);
  }
  expect(f.health.state.current).toBe(false);
  expect(f.health.accept(channelHealthFixture(next))).toBe(true);
  f.health.historical("Runtime stopping"); expect(f.health.state.current).toBe(false);
});
test("native age budgets, clock uncertainty and transport remain independent", () => {
  const quiet = channelHealthFixture(binding, "1", "quiet");
  expect(quiet.rows[1]!.connected).toBe(true);
  expect(channelStatus(quiet.rows[1]!)).toBe("Age budget exceeded");
  expect(channelStatus(quiet.rows[3]!)).toBe("Acknowledged");
  expect(channelStatus(channelHealthFixture(binding, "2", "clock").rows[0]!)).toBe("Clock uncertain");
  expect(channelStatus(channelHealthFixture(binding, "3", "reconnect").rows[0]!)).toBe("Awaiting ACK");
  expect(channelStatus(channelHealthFixture(binding, "4", "quarantine").rows[1]!)).toBe("Quarantined");
});
test("loss and terminal public failure stay scoped to their exact selection", () => {
  const f = fixture(); const sample = channelHealthFixture(binding);
  sample.rows[0]!.consumer_failure = "Console terminated";
  sample.rows[0]!.last_loss = { subscription_key: "activeAssetCtx:BTC", owner: "selected", channel: "context", connection_id: "1", kind: "parse_loss", received_at_ms: 10, detail: "BTC loss" };
  f.health.accept(sample); f.health.accept(channelHealthFixture(binding, "2"));
  expect(f.health.state.snapshot?.rows[0]!.last_loss?.detail).toBe("BTC loss");
  const eth = { ...binding, symbol: "ETH", selection_id: "2" }; f.health.bind(eth); f.health.accept(channelHealthFixture(eth, "3"));
  expect(f.health.state.snapshot?.rows[0]!.last_loss).toBe(null);
  expect(f.health.state.snapshot?.rows[0]!.consumer_failure).toBe(null);
});
test("context, ready status and malformed books cannot restore health or valid quotes", () => {
  void select("BTC");
  marketHealth.bind(binding); marketHealth.accept(channelHealthFixture(binding, "10", "quarantine"));
  marketHealth.historical("Expired"); const accepted = marketHealth.state.acceptedAt;
  receiveSelectedFeed({ kind: "book", coin: "BTC", at_ms: 900000, bids: [{ px: "110", sz: "1", n: 1 }], asks: [{ px: "100", sz: "1", n: 1 }] });
  const touch = quotes.touch;
  receiveSelectedFeed({ kind: "ctx", at_ms: Date.now(), row: { symbol: "BTC", mark_px: "100", funding_1h_bps: "0", open_interest: "1", day_volume_usd: "1", has_book: true } });
  receiveSelectedFeed({ kind: "status", connected: true, detail: "Ready" });
  expect(marketHealth.state.current).toBe(false);
  expect(marketHealth.state.acceptedAt).toBe(accepted);
  expect(quoteValidity.value).toBe("Invalid / crossed");
  marketHealth.accept(channelHealthFixture(binding, "11"));
  expect(quotes.touch).toEqual(touch);
  marketHealth.invalidate();
});
