import type { ChartBinding, ChartProjection, ObservedChartBar } from "../lib/bridge";
import { createChartObservations, receiveSelectedFeed } from "./market";
import { shell } from "./shell";

declare const test: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (actual: unknown) => { toBe(expected: unknown): void; toEqual(expected: unknown): void };

const binding: ChartBinding = { network: "testnet", generation: "1", selection_id: "10", symbol: "BTC", interval: "1h" };
const venue: ObservedChartBar = { time_ms: 3600000, open: "100", high: "110", low: "90", close: "101", volume: "12.5000",
  source: "venue", partial: false, open_close_ambiguous: false, received_at_ms: 1000 };
function projection(revision: string, owner = binding): ChartProjection {
  return { selection_id: owner.selection_id, revision, symbol: owner.symbol, interval: owner.interval, interval_ms: 3600000,
    price_decimals: null, closed: [], forming: { ...venue }, latest_trade: null, history_error: null, observation_error: null,
    last_observation_received_at_ms: 1000, tape_status: "observing" };
}
function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function fixture() {
  const reads: Array<{ binding: ChartBinding; reply: ReturnType<typeof deferred<ChartProjection>> }> = [];
  const owner = createChartObservations(async binding => {
    const reply = deferred<ChartProjection>(); reads.push({ binding, reply }); return reply.promise;
  });
  return { owner, reads };
}

test("chart and book payloads never promote the market summary", () => {
  const prior = { ...shell.feeds };
  receiveSelectedFeed({ kind: "chart", projection: projection("1") });
  receiveSelectedFeed({ kind: "book", coin: "BTC", at_ms: 1000, bids: [], asks: [] });
  expect(shell.feeds).toEqual(prior);
});

test("only acknowledged exact chart owners bootstrap projections without REST", () => {
  const { owner } = fixture();
  expect(owner.accept(projection("1"))).toBe(false);
  owner.bind(binding);
  expect(owner.accept(projection("1"), { ...binding, network: "mainnet" })).toBe(false);
  expect(owner.accept(projection("1"), { ...binding, generation: "2" })).toBe(false);
  expect(owner.accept({ ...projection("1"), interval: "5m" })).toBe(false);
  expect(owner.accept(projection("1"))).toBe(true);
  expect(owner.state.data?.forming?.volume).toBe(12.5);
  expect(owner.state.data?.priceDecimals).toBe(null);
  expect(owner.state.projection?.forming?.volume).toBe("12.5000");
});

test("events and history replies share increasing revisions without number precision loss", async () => {
  const { owner, reads } = fixture(); owner.bind(binding);
  const read = owner.refresh();
  expect(owner.accept(projection("9007199254740993"))).toBe(true);
  reads[0]!.reply.resolve(projection("9007199254740992")); await read;
  expect(owner.state.projection?.revision).toBe("9007199254740993");
  expect(owner.accept(projection("9007199254740993"))).toBe(false);
  const next = owner.refresh(); reads[1]!.reply.resolve(projection("9007199254740994")); await next;
  expect(owner.accept(projection("9007199254740993"))).toBe(false);
  expect(owner.state.projection?.revision).toBe("9007199254740994");
});

test("generation or network replacement cannot reuse a retained selection's revision", () => {
  const { owner } = fixture(); owner.bind(binding); owner.accept(projection("99"));
  owner.invalidate();
  const replacement = { ...binding, network: "mainnet" as const, generation: "2" };
  owner.bind(replacement);
  expect(owner.state.projection).toBe(null);
  expect(owner.accept(projection("1", replacement), binding)).toBe(false);
  expect(owner.accept(projection("1", replacement))).toBe(true);
});

test("REST success and error from an earlier A cannot affect replacement A after A-B-A", async () => {
  const { owner, reads } = fixture(); owner.bind(binding);
  const old = owner.refresh();
  owner.invalidate(true); owner.bind({ ...binding, symbol: "ETH", selection_id: "11" });
  const middle = owner.refresh();
  owner.invalidate(true); const last = { ...binding, selection_id: "12" }; owner.bind(last);
  owner.accept(projection("1", last));
  reads[0]!.reply.resolve(projection("100")); reads[1]!.reply.reject(new Error("old ETH read"));
  await Promise.all([old, middle]);
  expect(owner.state.projection?.selection_id).toBe("12");
  expect(owner.state.error).toBe(null);
  expect(owner.accept(projection("101"))).toBe(false);
});

test("interval round trips and concurrent reads fence late replies and errors", async () => {
  const { owner, reads } = fixture(); owner.bind(binding); const first = owner.refresh();
  owner.invalidate(true); owner.bind({ ...binding, interval: "5m", selection_id: "11" });
  owner.invalidate(true); const last = { ...binding, selection_id: "12" }; owner.bind(last);
  const second = owner.refresh(), third = owner.refresh();
  reads[2]!.reply.resolve(projection("2", last)); await third;
  reads[1]!.reply.reject(new Error("older read failed")); reads[0]!.reply.resolve(projection("999"));
  await Promise.all([first, second]);
  expect(owner.state.projection?.revision).toBe("2"); expect(owner.state.error).toBe(null);
});

test("history failure leaves live or retained data visible and late errors do not poison newer events", async () => {
  const { owner, reads } = fixture(); owner.bind(binding);
  const read = owner.refresh(); reads[0]!.reply.reject(new Error("history failed")); await read;
  expect(owner.state.error).toBe("Error: history failed");
  owner.accept({ ...projection("1"), history_error: "history unavailable" });
  expect(owner.state.data?.forming?.close).toBe(101);
  expect(owner.state.error).toBe("history unavailable");
  const retry = owner.refresh(); owner.accept(projection("2")); reads[1]!.reply.reject(new Error("late error")); await retry;
  expect(owner.state.error).toBe(null);
  owner.retain(); expect(owner.state.retained).toBe(true);
  expect(owner.state.data?.forming?.close).toBe(101);
  owner.invalidate(); expect(owner.accept(projection("3"))).toBe(false);
  expect(owner.state.data?.forming?.close).toBe(101);
});

test("independent latest trade never changes venue OHLCV and keeps frozen ambiguity", () => {
  const { owner } = fixture(); owner.bind(binding); owner.accept(projection("1"));
  const before = owner.state.data?.forming;
  owner.accept({ ...projection("2"), latest_trade: { time_ms: 3600100, price: "125.000", price_ambiguous: true },
    tape_status: "invalid_observation", observation_error: "Conflicting print identity" });
  expect(owner.state.data?.forming).toEqual(before);
  expect(owner.state.data?.latestTrade).toEqual({ timeMs: 3600100, price: 125, ambiguous: true });
  expect(owner.state.projection?.latest_trade?.price).toBe("125.000");
  expect(owner.state.projection?.observation_error).toBe("Conflicting print identity");
  expect(owner.accept(projection("1"))).toBe(false);
  expect(owner.state.projection?.tape_status).toBe("invalid_observation");
});

test("marker-only, partial tape and elapsed buckets preserve their native provenance", () => {
  const { owner } = fixture(); owner.bind(binding);
  owner.accept({ ...projection("1"), forming: null, latest_trade: { time_ms: 3600100, price: "100", price_ambiguous: false } });
  expect(owner.state.data?.closed).toEqual([]); expect(owner.state.data?.forming).toBe(null);
  owner.accept({ ...projection("2"), forming: null, closed: [{ ...venue, source: "observed_trades", partial: true, open_close_ambiguous: true }] });
  expect(owner.state.projection?.closed[0]?.partial).toBe(true);
  expect(owner.state.projection?.closed[0]?.source).toBe("observed_trades");
});
