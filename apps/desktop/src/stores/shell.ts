/**
 * Shell state: which view is showing, which network, feed health, equity.
 * A plain reactive module — no router, no Pinia. Later phases bind these
 * fields to Tauri commands; the shapes stay.
 */

import { reactive, readonly, watchEffect } from "vue";
import { marketHealth, marketChannels, marketTransport } from "./market-health";
import { pilotConsent, consentOwnsContext } from "./pilot-consent";
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

interface ShellState {
  view: View;
  network: Network;
  feeds: FeedsHealth;
  equityUsd: number | null;
  latencyMs: number | null;
  /** Last good reading. Kept across a failed refresh so a position stays visible. */
  account: AccountState | null;
  /** Why the last refresh failed, shown beside the stale reading. */
  accountError: string | null;
  /**
   * Whether the OS keychain answers. `null` until asked once — which is not
   * the same as unreachable, and the tracker distinguishes them.
   */
  keychain: KeychainStatus | null;
}

function savedNetwork(): Network {
  try { return localStorage.getItem("oppen.network") === "mainnet" ? "mainnet" : "testnet"; }
  catch { return "testnet"; }
}

const state = reactive<ShellState>({
  view: "trade",
  network: savedNetwork(),
  feeds: { wsMarket: "unknown", wsUser: "unknown", rest: "unknown" },
  equityUsd: null,
  latencyMs: null,
  account: null,
  accountError: null,
  keychain: null,
});

export const shell = readonly(state);
watchEffect(() => {
  state.feeds.wsMarket = !marketHealth.state.snapshot ? "unknown" : !marketHealth.state.current ? "stale"
    : marketTransport.value === "Disconnected" ? "down" : marketChannels.value === "Acknowledged" ? "ok" : "stale";
}, { flush: "sync" });

export function setView(view: View): void {
  state.view = view;
}

/** D4: persist the explicit operator choice and restart all network-scoped reads together. */
export function setNetwork(network: Network): void {
  if (network === state.network) return;
  if (consentOwnsContext(pilotConsent.state)) throw new Error("Initial consent owns the setup context.");
  // Write before changing anything. If persistence fails, keep the current network.
  localStorage.setItem("oppen.network", network);
  window.location.reload();
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
  const network = state.network;
  try {
    const next = await fetchAccountState(network);
    if (state.network !== network) return;
    state.account = next;
    state.accountError = null;
    state.equityUsd = Number(next.balances.equity_usd);
    // Public market channels are observed independently of this account read.
    state.feeds = {
      ...state.feeds,
      wsUser: next.feed === "live" ? "ok" : next.feed === "stale" ? "stale" : "unknown",
      rest: "ok",
    };
  } catch (error) {
    if (state.network !== network) return;
    const unconfigured = isConsoleError(error) && error.kind === "not_configured";
    state.feeds = { ...state.feeds, rest: unconfigured ? "unknown" : "down", wsUser: state.account ? "stale" : "unknown" };
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
