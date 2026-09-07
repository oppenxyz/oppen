/**
 * Shell state: which view is showing, which network, feed health, equity.
 * A plain reactive module — no router, no Pinia. Later phases bind these
 * fields to Tauri commands; the shapes stay.
 */

import { reactive, readonly } from "vue";
import {
  fetchAccountState,
  fetchKeychainStatus,
  inTauri,
  isConsoleError,
  type AccountState,
  type KeychainStatus,
} from "../lib/bridge";

export type View = "trade" | "agents" | "builder" | "portfolio" | "settings" | "onboarding";
export type Network = "testnet" | "mainnet";
export type FeedHealth = "ok" | "stale" | "down" | "unknown";

export interface FeedsHealth {
  wsMarket: FeedHealth;
  wsUser: FeedHealth;
  rest: FeedHealth;
}

export interface Decision {
  time: string;
  agent: string;
  /** Agent-authored. Untrusted text: render as plain text only. */
  text: string;
}

interface ShellState {
  view: View;
  network: Network;
  feeds: FeedsHealth;
  equityUsd: number | null;
  latencyMs: number | null;
  lastDecision: Decision | null;
  decisionsToday: number;
  refusedToday: number;
  /** Last good reading. Kept across a failed refresh so a position stays visible. */
  account: AccountState | null;
  /** Why the last refresh failed, shown beside the stale reading. */
  accountError: string | null;
  /**
   * Whether the OS keychain answers. `null` until asked once — which is not
   * the same as unreachable, and the tracker distinguishes them.
   */
  keychain: KeychainStatus | null;
  /**
   * Newest frame off the market socket, ms. `null` before the first one —
   * which item 34 distinguishes from having gone quiet, because there is no
   * last-good value behind the overlay.
   */
  lastMarketTickMs: number | null;
  /** The socket's own words when it dropped a feed. Display-only. */
  feedDetail: string | null;
}

const state = reactive<ShellState>({
  view: "trade",
  network: "testnet",
  feeds: { wsMarket: "unknown", wsUser: "unknown", rest: "unknown" },
  equityUsd: null,
  latencyMs: null,
  lastDecision: null,
  decisionsToday: 0,
  refusedToday: 0,
  account: null,
  accountError: null,
  keychain: null,
  lastMarketTickMs: null,
  feedDetail: null,
});

export const shell = readonly(state);

export function setView(view: View): void {
  state.view = view;
}

/** Mainnet is an explicit switch (D4). Persistence arrives with the Rust settings store. */
export function setNetwork(network: Network): void {
  state.network = network;
}

export const NAV: ReadonlyArray<{ view: View; label: string }> = [
  { view: "trade", label: "Trade" },
  { view: "agents", label: "Agents" },
  { view: "builder", label: "Builder" },
  { view: "portfolio", label: "Portfolio" },
  { view: "settings", label: "Settings" },
];

export function feedLabel(health: FeedHealth): string {
  switch (health) {
    case "ok":
      return "OK";
    case "stale":
      return "STALE";
    case "down":
      return "DOWN";
    case "unknown":
      return "—";
  }
}

/** Venue health is the worst of its feeds; unknown until every feed has reported. */
export function venueLabel(feeds: FeedsHealth): string {
  const all = [feeds.wsMarket, feeds.wsUser, feeds.rest];
  if (all.includes("down")) return "DOWN";
  if (all.includes("stale")) return "STALE";
  if (all.every((h) => h === "ok")) return "OK";
  return "—";
}

export function formatUsd(value: number | null): string {
  if (value === null) return "—";
  return value.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}

export function formatLatency(ms: number | null): string {
  return ms === null ? "—" : `${ms}MS`;
}

// ---------------------------------------------------------------------------
// Live account state (spec items 16, 31-34).
// ---------------------------------------------------------------------------

/**
 * Refresh the account readouts from Rust.
 *
 * Failure is recorded rather than thrown: the console must keep rendering with
 * an explicit problem shown, because a blank panel and a stale panel look the
 * same to an operator and only one of them is safe.
 */
export async function refreshAccount(): Promise<void> {
  if (!inTauri()) {
    state.accountError = "Not running in the desktop app — start it with `bun run tauri dev`.";
    return;
  }
  try {
    const next = await fetchAccountState(state.network);
    state.account = next;
    state.accountError = null;
    state.equityUsd = Number(next.balances.equity_usd);
    // `wsMarket` is deliberately not set here. It is the socket's own
    // indicator now and `feedTick` owns it; overwriting it on the account poll
    // would make a market feed that is streaming read as whatever the account
    // read last saw.
    state.feeds = {
      ...state.feeds,
      wsUser: next.feed === "live" ? "ok" : next.feed === "stale" ? "stale" : "unknown",
      rest: "ok",
    };
  } catch (error) {
    state.accountError = isConsoleError(error)
      ? error.detail
      : error instanceof Error
        ? error.message
        : String(error);
    // The previous reading is deliberately left in place and the error shown
    // beside it. Clearing it would hide that a position is open.
  }
}

let poll: ReturnType<typeof setInterval> | null = null;

/** Start polling. Idempotent, so a remount does not stack timers. */
/**
 * Asks the keychain once whether it answers.
 *
 * Once rather than on the polling tick: reachability changes when an operator
 * unlocks a keychain or installs a keyring, not second to second, and each ask
 * can raise an OS prompt. Polling it would train the operator to dismiss the
 * dialog that matters.
 *
 * Outside Tauri there is no keychain to ask, so the state stays `null` — the
 * tracker reads that as "not asked", not as "unreachable".
 */
export async function refreshKeychain(): Promise<void> {
  if (!inTauri()) return;
  try {
    state.keychain = await fetchKeychainStatus(state.network);
  } catch (error) {
    // A command that threw is itself an unreachable store, reported the same
    // way the Rust side reports one.
    state.keychain = { reachable: false, detail: String(error) };
  }
}

export function startAccountPolling(everyMs = 5000): void {
  if (poll !== null) return;
  void refreshAccount();
  poll = setInterval(() => void refreshAccount(), everyMs);
}

export function stopAccountPolling(): void {
  if (poll === null) return;
  clearInterval(poll);
  poll = null;
}

/**
 * How long without a frame before the market feed reads stale (item 34).
 *
 * `activeAssetCtx` is the slowest channel the console subscribes at roughly
 * one second, so five is a handful of missed frames rather than a threshold a
 * quiet market trips on its own.
 */
const MARKET_STALE_AFTER_MS = 5_000;

/**
 * Record a frame off the socket (`docs/spec.md` item 34).
 *
 * The market feed's freshness is answered here rather than by `refreshAccount`
 * because the two now have different clocks: the account is four REST reads on
 * a five-second poll, and the market is a socket. Collapsing them into one
 * indicator is what item 34 forbids — a live socket beside a failed account
 * read is a real state, and the operator has to be able to see which half is
 * down.
 */
export function feedTick(atMs: number): void {
  state.lastMarketTickMs = Math.max(state.lastMarketTickMs ?? 0, atMs);
  state.feeds = { ...state.feeds, wsMarket: "ok" };
}

/** The socket said what it is doing. A drop is not a stale feed: it is a drop. */
export function feedStatus(connected: boolean, detail?: string): void {
  state.feeds = { ...state.feeds, wsMarket: connected ? "ok" : "down" };
  state.feedDetail = detail ?? null;
}

/**
 * Age the market indicator off the clock.
 *
 * A socket that stops delivering does not announce it — that is the whole
 * shape of item 34's "stale overlay after N seconds", and without this the
 * indicator would sit on `ok` forever after the last frame.
 */
export function ageFeeds(nowMs: number): void {
  if (state.feeds.wsMarket === "down") return;
  if (state.lastMarketTickMs === null) return;
  state.feeds = {
    ...state.feeds,
    wsMarket: nowMs - state.lastMarketTickMs > MARKET_STALE_AFTER_MS ? "stale" : "ok",
  };
}
