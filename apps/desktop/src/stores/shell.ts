/**
 * Shell state: which view is showing, which network, feed health, equity.
 * A plain reactive module — no router, no Pinia. Later phases bind these
 * fields to Tauri commands; the shapes stay.
 */

import { reactive, readonly } from "vue";
import { fetchAccountState, inTauri, isConsoleError, type AccountState } from "../lib/bridge";

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
    state.feeds = {
      wsMarket: next.feed === "live" ? "ok" : next.feed === "stale" ? "stale" : "unknown",
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
