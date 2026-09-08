/**
 * Tests for the market store's one lossy boundary.
 *
 * Everything else in this store is plumbing over the bridge, but `parseBar` is
 * where an exact decimal from the venue becomes a float for the renderer. That
 * conversion is safe *only* because its output is mapped to character cells
 * and never to an order, and the two things worth pinning are that it drops
 * what it cannot read rather than defaulting it, and that it keeps the
 * timestamp intact.
 *
 * Same `describe` / `it` / `expect` shims as `lib/candles.test.ts`, so this
 * runs unchanged under `bun test` and keeps `vue-tsc --noEmit` green.
 */

import { reactive } from "vue";
import type { FeedBinding, FeedEnvelope, FeedUpdate, MarketSnapshot } from "../lib/bridge";
import { createMarketFeed, createQuotes, quoteSpread, parseBar } from "./market";

interface Assertions {
  toBe(expected: unknown): void;
  toEqual(expected: unknown): void;
}

interface Matchers extends Assertions {
  not: Assertions;
}

declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (actual: unknown) => Matchers;

const GOOD = {
  time_ms: 1_788_544_667_000,
  open: "100.5",
  high: "110",
  low: "90.25",
  close: "101.75",
  volume: "7.5",
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

async function settle(): Promise<void> {
  for (let step = 0; step < 8; step += 1) await Promise.resolve();
}

function feedFixture() {
  const scope = reactive({ network: "testnet" as FeedBinding["network"], coin: "BTC", interval: "1h" });
  const calls: Array<{ network: FeedBinding["network"]; coin: string; interval: string; reply: ReturnType<typeof deferred<FeedBinding>> }> = [];
  const listeners: Array<{ emit: (event: FeedEnvelope) => void; ready: ReturnType<typeof deferred<() => void>>; cleaned: number }> = [];
  const updates: FeedUpdate[] = [];
  const failures: Array<string | undefined> = [];
  let healthy = false;
  let error: string | null = null;
  const controller = createMarketFeed(
    () => ({ ...scope }),
    {
      watch: (network, coin, interval) => {
        const reply = deferred<FeedBinding>();
        calls.push({ network, coin, interval, reply });
        const selection = calls.length;
        return reply.promise.then(feed => ({ feed, chart: { ...feed, selection_id: String(selection), symbol: coin, interval } }));
      },
      listen: (emit) => {
        const ready = deferred<() => void>();
        listeners.push({ emit, ready, cleaned: 0 });
        return ready.promise;
      },
    },
    {
      invalidate: () => { healthy = false; },
      failure: () => { healthy = false; },
      error: (detail) => { error = detail; },
      chartBinding: () => {},
      update: (update, failure) => {
        healthy = failure === undefined && (update.kind !== "status" || update.connected);
        updates.push(update);
        failures.push(failure);
      },
    },
  );
  function listen(index: number): void {
    const listener = listeners[index]!;
    listener.ready.resolve(() => { listener.cleaned += 1; });
  }
  function emit(generation: string, network: FeedBinding["network"] = "testnet", index = 0, update: FeedUpdate = { kind: "status", connected: true }): void {
    listeners[index]!.emit({ network, generation, update });
  }
  return { scope, calls, listeners, updates, failures, controller, listen, emit, get healthy() { return healthy; }, get error() { return error; } };
}

describe("scoped market feed ownership", () => {
  it("filters chart selection incarnations even when the market feed generation is reused", async () => {
    const f = feedFixture();
    const chart = (selection_id: string, interval = "1h"): FeedUpdate => ({ kind: "chart", projection: {
      selection_id, revision: "1", symbol: "BTC", interval, interval_ms: 3600000, price_decimals: null,
      closed: [], forming: null, latest_trade: null, history_error: null, observation_error: null,
      last_observation_received_at_ms: null, tape_status: "observing",
    } });
    try {
      const started = f.controller.start(); f.listen(0); await started;
      f.emit("1", "testnet", 0, chart("1"));
      expect(f.updates.length).toBe(0);
      f.calls[0]!.reply.resolve({ network: "testnet", generation: "1" }); await settle();
      f.emit("1", "testnet", 0, chart("1")); expect(f.updates.length).toBe(1);
      f.scope.interval = "5m";
      f.calls[1]!.reply.resolve({ network: "testnet", generation: "1" }); await settle();
      f.scope.interval = "1h";
      f.calls[2]!.reply.resolve({ network: "testnet", generation: "1" }); await settle();
      f.emit("1", "testnet", 0, chart("1"));
      f.emit("1", "testnet", 0, chart("2", "5m"));
      expect(f.updates.length).toBe(1);
      f.emit("1", "testnet", 0, chart("3")); expect(f.updates.length).toBe(2);
    } finally { f.controller.stop(); }
  });

  it("keeps the first owner failure through later payloads and same-owner watches until a new binding", async () => {
    const f = feedFixture();
    try {
      const started = f.controller.start();
      f.listen(0);
      await started;
      f.calls[0]!.reply.resolve({ network: "testnet", generation: "9007199254740993" });
      await settle();
      const payload: FeedUpdate = { kind: "book", coin: "BTC", at_ms: 100, bids: [], asks: [] };
      f.listeners[0]!.emit({ network: "testnet", generation: "9007199254740993", update: payload, failure: "ledger append failed" });
      expect(f.updates[0]).toEqual(payload);
      expect(f.healthy).toBe(false);
      f.emit("9007199254740993", "testnet", 0, payload);
      f.emit("9007199254740993");
      f.listeners[0]!.emit({ network: "testnet", generation: "9007199254740993", update: payload, failure: "later failure" });
      expect(f.failures).toEqual(Array(4).fill("ledger append failed"));
      expect(f.healthy).toBe(false);
      f.scope.interval = "5m";
      f.calls[1]!.reply.resolve({ network: "testnet", generation: "9007199254740993" });
      await settle();
      f.emit("9007199254740993");
      expect(f.healthy).toBe(false);
      expect(f.failures[4]).toBe("ledger append failed");
      f.scope.network = "mainnet";
      f.calls[2]!.reply.resolve({ network: "mainnet", generation: "9007199254740994" });
      await settle();
      expect(f.healthy).toBe(false);
      f.emit("9007199254740994", "mainnet");
      expect(f.healthy).toBe(true);
      expect(f.failures[5]).toBe(undefined);
    } finally { f.controller.stop(); }
  });

  it("serializes A-B-A and coalesces to the latest symbol and interval without accepting the first A", async () => {
    const f = feedFixture();
    try {
      const started = f.controller.start();
      f.listen(0);
      await started;
      f.scope.network = "mainnet";
      f.scope.network = "testnet";
      f.scope.coin = "ETH";
      f.scope.coin = "SOL";
      f.scope.interval = "5m";
      expect(f.calls.length).toBe(1);
      f.calls[0]!.reply.resolve({ network: "testnet", generation: "1" });
      await settle();
      expect(f.calls.length).toBe(2);
      expect([f.calls[1]!.network, f.calls[1]!.coin, f.calls[1]!.interval]).toEqual(["testnet", "SOL", "5m"]);
      f.emit("1");
      expect(f.healthy).toBe(false);
      // The intermediate network never reached IPC, so the owner may be reused.
      f.calls[1]!.reply.resolve({ network: "testnet", generation: "1" });
      await settle();
      expect(f.healthy).toBe(false);
      f.emit("0");
      f.emit("1", "mainnet");
      f.emit("1", "testnet", 0, { kind: "book", coin: "BTC", at_ms: 100, bids: [], asks: [] });
      expect(f.updates.length).toBe(0);
      f.emit("1");
      expect(f.healthy).toBe(true);
      expect(f.updates.length).toBe(1);
    } finally { f.controller.stop(); }
  });

  it("discards off-selection payloads without discarding an accepted owner's failure", async () => {
    const f = feedFixture();
    try {
      const started = f.controller.start();
      f.listen(0);
      await started;
      f.calls[0]!.reply.resolve({ network: "testnet", generation: "50" });
      await settle();
      f.emit("50");
      expect(f.healthy).toBe(true);
      f.listeners[0]!.emit({
        network: "testnet", generation: "50", failure: "owner ledger failed",
        update: { kind: "book", coin: "ETH", at_ms: 100, bids: [], asks: [] },
      });
      expect(f.updates.length).toBe(1);
      expect(f.healthy).toBe(false);
      f.emit("50");
      expect(f.failures[1]).toBe("owner ledger failed");
      expect(f.healthy).toBe(false);
    } finally { f.controller.stop(); }
  });

  it("invalidates synchronously and refuses old events before the replacement IPC acknowledges", async () => {
    const f = feedFixture();
    try {
      const started = f.controller.start();
      f.listen(0);
      await started;
      f.calls[0]!.reply.resolve({ network: "testnet", generation: "10" });
      await settle();
      f.emit("10");
      expect(f.healthy).toBe(true);
      f.scope.network = "mainnet";
      expect(f.healthy).toBe(false);
      f.emit("10");
      expect(f.healthy).toBe(false);
      f.scope.network = "testnet";
      f.calls[1]!.reply.resolve({ network: "mainnet", generation: "11" });
      await settle();
      expect(f.calls.length).toBe(3);
      f.calls[2]!.reply.resolve({ network: "testnet", generation: "12" });
      await settle();
      f.emit("10");
      f.emit("11", "mainnet");
      expect(f.healthy).toBe(false);
      f.emit("12");
      expect(f.healthy).toBe(true);
    } finally { f.controller.stop(); }
  });

  it("ignores a late failed acknowledgment and validates the current acknowledgment network", async () => {
    const f = feedFixture();
    try {
      const started = f.controller.start();
      f.listen(0);
      await started;
      f.scope.network = "mainnet";
      f.calls[0]!.reply.reject(new Error("old watch failed"));
      await settle();
      expect(f.error).toBe(null);
      f.calls[1]!.reply.resolve({ network: "testnet", generation: "20" });
      await settle();
      expect(f.error).toBe("Error: Market feed acknowledgment does not match the requested scope.");
      f.emit("20");
      expect(f.healthy).toBe(false);
      const retry = f.controller.request();
      f.calls[2]!.reply.resolve({ network: "mainnet", generation: "21" });
      await retry;
      expect(f.error).toBe(null);
      expect(f.healthy).toBe(false);
      f.emit("21", "mainnet");
      expect(f.healthy).toBe(true);
    } finally { f.controller.stop(); }
  });

  it("cleans up a late listener after remount while retaining the actual outstanding watch", async () => {
    const f = feedFixture();
    try {
      const oldStart = f.controller.start();
      f.controller.stop();
      f.scope.network = "mainnet";
      const newStart = f.controller.start();
      expect(f.calls.length).toBe(1);
      f.listen(0);
      await oldStart;
      expect(f.listeners[0]!.cleaned).toBe(1);
      f.listen(1);
      await newStart;
      f.calls[0]!.reply.reject(new Error("stopped watch"));
      await settle();
      expect(f.calls.length).toBe(2);
      expect(f.calls[1]!.network).toBe("mainnet");
      expect(f.error).toBe(null);
      f.calls[1]!.reply.resolve({ network: "mainnet", generation: "31" });
      await settle();
      f.emit("31", "mainnet", 0);
      expect(f.healthy).toBe(false);
      f.emit("31", "mainnet", 1);
      expect(f.healthy).toBe(true);
      f.controller.stop();
      expect(f.listeners[1]!.cleaned).toBe(1);
      f.emit("31", "mainnet", 1);
      expect(f.healthy).toBe(false);
    } finally { f.controller.stop(); }
  });

  it("drops stopped pending desires and ignores late listener failure", async () => {
    const f = feedFixture();
    const started = f.controller.start();
    f.scope.network = "mainnet";
    f.controller.stop();
    f.listeners[0]!.ready.reject(new Error("stopped listener"));
    await started;
    f.calls[0]!.reply.resolve({ network: "testnet", generation: "40" });
    await settle();
    expect(f.calls.length).toBe(1);
    expect(f.error).toBe(null);
    expect(f.healthy).toBe(false);
  });
});

describe("parseBar", () => {
  it("carries every field across, timestamp included", () => {
    expect(parseBar(GOOD)).toEqual({
      time: 1_788_544_667_000,
      open: 100.5,
      high: 110,
      low: 90.25,
      close: 101.75,
      volume: 7.5,
    });
  });

  /**
   * The failure this exists to prevent. `Number("")` is `0` and
   * `Number(undefined)` is `NaN`, so a boundary that trusted the cast would
   * draw a candle at the floor of the chart — a price that looks deliberate
   * and never happened. Dropping the bar hands the renderer a shorter window,
   * which it reports rather than invents.
   */
  it("drops a bar it cannot read rather than defaulting it to zero", () => {
    expect(parseBar({ ...GOOD, low: "" })).toBe(null);
    expect(parseBar({ ...GOOD, close: "not a price" })).toBe(null);
    expect(parseBar({ ...GOOD, volume: "Infinity" })).toBe(null);
    expect(parseBar({ ...GOOD, high: "   " })).toBe(null);
  });

  /**
   * A zero is a real reading and must survive: a bucket in which nothing
   * traded has zero volume, and treating that as unreadable would drop bars
   * from every quiet market.
   */
  it("keeps a genuine zero", () => {
    const quiet = parseBar({ ...GOOD, volume: "0" });
    expect(quiet === null).toBe(false);
    expect(quiet?.volume).toBe(0);
  });
});

/**
 * The live feed's two decisions (`docs/spec.md` items 31, 34).
 *
 * `applyFeed` itself is dispatch over module state, but these two are where a
 * frame can be folded *wrong* rather than merely dropped, so they are the two
 * the socket path pins.
 */
describe("independent quote observations", () => {
  const level = (px: string) => ({ px, sz: "1.000", n: 1 });
  const book = (at_ms: number): Extract<FeedUpdate, { kind: "book" }> => ({ kind: "book", coin: "BTC", at_ms,
    bids: [level("100"), level("99")], asks: [level("102"), level("103")] });
  const bbo = (at_ms: number): Extract<FeedUpdate, { kind: "bbo" }> => ({ kind: "bbo", coin: "BTC", at_ms,
    bid: level("100.5000"), ask: level("101.5000") });
  const snapshot = { bids: [level("10")], asks: [level("12")], as_of_ms: 99999999 } as MarketSnapshot;

  it("bootstraps BBO without REST or invented depth and preserves exact strings and separate clocks", () => {
    const q = createQuotes();
    q.update(bbo(100), 900);
    expect(q.quotes.touch?.bid?.px).toBe("100.5000");
    expect(q.quotes.touch?.venueMs).toBe(100);
    expect(q.quotes.touch?.observedMs).toBe(900);
    expect(q.quotes.depth).toBe(null);
    expect(q.quotes.touch?.live).toBe(true);
    q.invalidate();
    expect(q.quotes.touch?.live).toBe(false);
    expect(q.quotes.touch?.bid?.px).toBe("100.5000");
  });

  it("orders both channels by venue time with BBO winning equal timestamps in either arrival order", () => {
    for (const firstBbo of [false, true]) {
      const q = createQuotes();
      q.update(firstBbo ? bbo(100) : book(100), 900);
      q.update(firstBbo ? book(100) : bbo(100), 901);
      expect(q.quotes.touch?.source).toBe("bbo");
      expect(q.quotes.touch?.bid?.px).toBe("100.5000");
      expect(q.quotes.depth?.bids[0]?.px).toBe("100");
      expect(q.quotes.depth?.live).toBe(false);
      q.update(book(99), 999);
      q.update(bbo(99), 1000);
      expect(q.quotes.touch?.venueMs).toBe(100);
      expect(q.quotes.depth?.venueMs).toBe(100);
      q.update(book(101), 1001);
      expect(q.quotes.touch?.source).toBe("book");
      expect(q.quotes.depth?.live).toBe(true);
    }
  });

  it("authoritative missing sides clear liquidity and cannot be resurrected by older or tied L2", () => {
    const q = createQuotes();
    q.update(book(100), 900);
    q.update({ kind: "bbo", coin: "BTC", at_ms: 101, bid: level("101") }, 901);
    expect(q.quotes.touch?.ask).toBe(null);
    expect(q.quotes.depth?.asks).toEqual([]);
    q.update(book(100), 902);
    q.update(book(101), 903);
    expect(q.quotes.touch?.ask).toBe(null);
    expect(q.quotes.depth?.asks).toEqual([]);
    expect(quoteSpread(q.quotes.touch?.bid?.px, q.quotes.touch?.ask?.px)).toBe(null);
    q.update({ kind: "bbo", coin: "BTC", at_ms: 102 }, 904);
    expect(q.quotes.touch?.bid).toBe(null);
    expect(q.quotes.depth?.bids).toEqual([]);
    q.update(book(103), 905);
    expect(q.quotes.touch?.ask?.px).toBe("102");
  });

  it("identical observations refresh UI receipt and liveness after invalidation, but equal-time conflicts do not", () => {
    for (const frame of [bbo(100), book(100)]) {
      const q = createQuotes();
      q.update(frame, 900);
      q.invalidate();
      const conflict = frame.kind === "bbo" ? { ...frame, bid: level("98") }
        : { ...frame, bids: [level("98")] };
      q.update(conflict, 901);
      expect(q.quotes.touch?.observedMs).toBe(900);
      expect(q.quotes.touch?.live).toBe(false);
      q.update(frame, 902);
      expect(q.quotes.touch?.observedMs).toBe(902);
      expect(q.quotes.touch?.live).toBe(true);
      if (frame.kind === "book") {
        expect(q.quotes.depth?.observedMs).toBe(902);
        expect(q.quotes.depth?.live).toBe(true);
      }
    }
  });

  it("newer depth advances on its own clock behind a newer BBO without reviving a cleared side", () => {
    const q = createQuotes();
    q.update(book(100), 900);
    q.update({ ...bbo(110), ask: undefined }, 901);
    q.update({ ...book(105), bids: [level("99.5")] }, 902);
    expect(q.quotes.touch?.venueMs).toBe(110);
    expect(q.quotes.touch?.observedMs).toBe(901);
    expect(q.quotes.touch?.bid?.px).toBe("100.5000");
    expect(q.quotes.touch?.ask).toBe(null);
    expect(q.quotes.depth?.venueMs).toBe(105);
    expect(q.quotes.depth?.observedMs).toBe(902);
    expect(q.quotes.depth?.bids[0]?.px).toBe("99.5");
    expect(q.quotes.depth?.asks).toEqual([]);
    expect(q.quotes.depth?.live).toBe(false);
  });

  it("REST seeds only absent live ownership, never comparing host time with venue time", () => {
    const q = createQuotes();
    q.seed(snapshot, q.revision, 800);
    expect(q.quotes.touch?.venueMs).toBe(null);
    expect(q.quotes.touch?.observedMs).toBe(800);
    expect(q.quotes.touch?.live).toBe(false);
    const pending = q.revision;
    q.update(bbo(1), 900);
    q.seed(snapshot, pending, 901);
    expect(q.quotes.touch?.bid?.px).toBe("100.5000");
    q.invalidate();
    q.seed(snapshot, q.revision, 902);
    expect(q.quotes.touch?.bid?.px).toBe("100.5000");
    const oldScope = q.revision;
    q.reset();
    q.seed(snapshot, oldScope, 903);
    expect(q.quotes.touch).toBe(null);
    q.seed(snapshot, q.revision, 904);
    expect(q.quotes.touch?.source).toBe("rest");
  });

  it("does not compute spreads from missing, invalid, nonpositive or crossed prices", () => {
    for (const bad of [undefined, null, "", " ", "Infinity", "NaN", "-1", "0", "0x64", "1e2"]) {
      expect(quoteSpread(bad, "102")).toBe(null);
      expect(quoteSpread("100", bad)).toBe(null);
    }
    expect(quoteSpread("103", "102")).toBe(null);
    expect(quoteSpread("100", "100")).toBe(0);
    expect(quoteSpread("99", "101")).toBe(200);
  });
});
