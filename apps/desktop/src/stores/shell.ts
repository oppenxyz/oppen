/**
 * Shell state: which view is showing, which network, feed health, equity.
 * A plain reactive module — no router, no Pinia. Later phases bind these
 * fields to Tauri commands; the shapes stay.
 */

import { reactive, readonly } from "vue";

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
