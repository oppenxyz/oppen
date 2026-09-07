/**
 * The one place the renderer talks to Rust.
 *
 * Every field here mirrors `oppen_core::state::AccountState`. Decimals cross
 * as strings and stay strings until the moment they are formatted: parsing
 * them into JS numbers would round a size or a price that the venue validates
 * exactly, and a rounded price is a rejected order.
 */

import { invoke } from "@tauri-apps/api/core";

export type Freshness = "live" | "stale" | "never_connected";

export interface PositionView {
  symbol: string;
  size: string;
  entry_px: string | null;
  position_value_usd: string;
  unrealized_pnl_usd: string;
  margin_used_usd: string;
  liquidation_px: string | null;
  liq_distance_frac: string | null;
  max_leverage: number;
}

export interface OrderView {
  symbol: string;
  oid: number;
  cloid: string | null;
  is_buy: boolean;
  limit_px: string;
  size: string;
  original_size: string;
  reduce_only: boolean;
  is_trigger: boolean;
  trigger_px: string | null;
  placed_ts_ms: number;
}

export interface Balances {
  /** Perps collateral plus spot. Not the venue's `accountValue`, which reads 0 here. */
  equity_usd: string;
  perps_account_value_usd: string;
  spot_usdc_available: string;
  total_margin_used_usd: string;
  withdrawable_usd: string;
}

export interface AccountState {
  contract_version: number;
  network: "testnet" | "mainnet";
  address: string;
  as_of_ms: number;
  feed_age_ms: number | null;
  feed: Freshness;
  balances: Balances;
  positions: PositionView[];
  orders: OrderView[];
}

/** What the Rust side returns instead of a state. */
export interface ConsoleError {
  kind: "not_configured" | "venue";
  detail: string;
}

export function isConsoleError(value: unknown): value is ConsoleError {
  return (
    typeof value === "object" &&
    value !== null &&
    "kind" in value &&
    typeof (value as ConsoleError).detail === "string"
  );
}

/** Whether we are running inside Tauri at all, or in a plain browser dev server. */
export function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export async function fetchAccountState(network: "testnet" | "mainnet"): Promise<AccountState> {
  return invoke<AccountState>("account_state", { network });
}

/** What the keychain probe answers. */
export interface KeychainStatus {
  reachable: boolean;
  /** Absent when reachable; the store's own message when not. */
  detail?: string;
}

/**
 * Whether this machine's keychain answers at all.
 *
 * Read-only on the Rust side, and deliberately the only key-related command
 * the console has: it reports that the store responded, never what is in it.
 */
export async function fetchKeychainStatus(
  network: "testnet" | "mainnet",
): Promise<KeychainStatus> {
  return invoke<KeychainStatus>("keychain_status", { network });
}

/** One row of the markets rail. Decimals arrive as strings. */
export interface MarketRow {
  symbol: string;
  mark_px: string;
  /** Absent on an asset the venue has stopped quoting. There is no substitute. */
  mid_px?: string;
  /** Absent when the previous close was zero. */
  change_24h_pct?: string;
  funding_1h_bps: string;
  open_interest: string;
  day_volume_usd: string;
  has_book: boolean;
}

export interface BookLevel {
  px: string;
  sz: string;
  n: number;
}

export interface DepthBand {
  band_bps: number;
  bid_usd: string;
  ask_usd: string;
  /** False when the ladder stopped short of the band — a floor, not the depth. */
  covers_band: boolean;
}

export interface BookFeatures {
  spread_bps?: string;
  book_imbalance?: string;
  /** Always absent from the console: it needs `bbo`, and there is no socket here. */
  micro_tilt_bps?: string;
  bid_reach_bps?: string;
  ask_reach_bps?: string;
  depth: DepthBand[];
}

export interface FundingFeatures {
  hour_to_date_bps: string;
  apr_pct: string;
  predicted_apr_pct?: string;
  next_funding_s: number;
  basis_bps?: string;
}

export interface VolFeatures {
  rv_1h_bps?: string;
  rv_24h_bps?: string;
  vol_ratio?: string;
  bars_1h: number;
  bars_24h: number;
}

/** One symbol in depth. Ages on its own clock — see `as_of_ms`. */
export interface MarketSnapshot {
  symbol: string;
  as_of_ms: number;
  bids: BookLevel[];
  asks: BookLevel[];
  book: BookFeatures;
  funding: FundingFeatures;
  vol: VolFeatures;
}

export async function fetchMarkets(network: "testnet" | "mainnet"): Promise<MarketRow[]> {
  return invoke<MarketRow[]>("markets", { network });
}

export async function fetchMarketSnapshot(
  network: "testnet" | "mainnet",
  coin: string,
): Promise<MarketSnapshot> {
  return invoke<MarketSnapshot>("market_snapshot", { network, coin });
}
