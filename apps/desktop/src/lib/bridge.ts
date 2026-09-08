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
  kind: "not_configured" | "venue" | "local_status" | "halt_not_admitted";
  detail: string;
}

export function isConsoleError(value: unknown): value is ConsoleError {
  return (
    typeof value === "object" &&
    value !== null &&
    "kind" in value &&
    typeof value.kind === "string" &&
    ["not_configured", "venue", "local_status", "halt_not_admitted"].includes(value.kind) &&
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

export type SourceRead<T> = { status: "ready"; value: T } | { status: "unavailable"; detail: string };
export interface LedgerEvent {
  seq: number;
  ts_ms: number;
  kind: string;
  agent_id: string | null;
  payload: unknown;
}
export interface EventPage {
  events: LedgerEvent[];
  next_cursor: number;
  head_seq: number;
  resync_required: boolean;
}
export interface AgentPolicy {
  symbols: string[];
  max_order_usd: string;
  max_position_usd: string;
  max_slippage_bps: string;
  order_rate: { count: number; per_ms: number };
  reduce_only: boolean;
  approval_required: boolean;
  risk: { max_leverage: number; margin_mode: string; max_open_exposure_usd: string | null; max_risk_usd: string | null };
  loss: { max_daily_loss_usd: string | null; max_drawdown_usd: string | null };
}
export interface StoredPolicy {
  guardrails: Record<string, AgentPolicy>;
  account_limits: { max_daily_loss_usd: string | null; max_drawdown_usd: string | null };
  kill: { global: { engaged_at_ms: number; reason: unknown } | null; agents: Record<string, { engaged_at_ms: number; reason: unknown }> };
}
export interface OperatorRead {
  network: "testnet" | "mainnet";
  ledger: SourceRead<EventPage>;
  policy: SourceRead<PolicyInspection>;
}
export interface PolicyInspection {
  provenance: "unverified_legacy" | "unverified_ledger";
  revision: number | null;
  observed_at_ms: number;
  state: StoredPolicy;
}
export async function fetchOperatorState(network: "testnet" | "mainnet"): Promise<OperatorRead> {
  return invoke<OperatorRead>("operator_state", { network });
}

export type PilotMetric = "order_notional" | "executed_notional" | "committed_notional" | "realized_loss";

export type PilotStop =
  | { reason: "awaiting_reconciliation" }
  | { reason: "exhausted"; metric: PilotMetric; observed_usd: string; limit_usd: string }
  | { reason: "unavailable"; detail: string };

/** Local pilot evidence carries consent authentication separately from accounting. */
export type PilotStatus = {
  authentication: "unverified" | "legacy_review_required" | "verified";
  agent: string;
  account: string;
  halt: PilotStop | null;
} & (
  | { accounting: "known"; executed_usd: string; reserved_usd: string; net_realized_pnl_usd: string }
  | { accounting: "unavailable"; detail: string }
);

export async function fetchPilotStatus(network: "testnet" | "mainnet"): Promise<PilotStatus | null> {
  return invoke<PilotStatus | null>("pilot_status", { network });
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

/**
 * One bar as it crosses the boundary. Decimals arrive as strings like every
 * other price here; the chart store parses them to numbers at its own edge,
 * because a renderer that maps prices to pixels is the one consumer where an
 * f64 loses nothing anybody can see.
 */
export interface ChartBar {
  time_ms: number;
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
}

/** A symbol's bars at one interval. */
export interface ChartSeries {
  symbol: string;
  /** Canonical, so `120s` comes back `2m`. The axis is labelled from this. */
  interval: string;
  interval_ms: number;
  price_decimals: number;
  closed: ChartBar[];
  /** The bucket in progress, drawn with the forming glyph. Absent between buckets. */
  forming?: ChartBar;
}

export async function fetchChartSeries(
  network: "testnet" | "mainnet",
  coin: string,
  interval: string,
): Promise<ChartSeries> {
  return invoke<ChartSeries>("chart_series", { network, coin, interval });
}

export async function fetchMarketSnapshot(
  network: "testnet" | "mainnet",
  coin: string,
): Promise<MarketSnapshot> {
  return invoke<MarketSnapshot>("market_snapshot", { network, coin });
}

// ---------------------------------------------------------------------------
// The live feed (`docs/spec.md` items 31, 34)
// ---------------------------------------------------------------------------

/**
 * One frame off the socket, as the console draws it.
 *
 * Tagged rather than five separate Tauri channels, so an unknown `kind` is
 * ignored in one place instead of being a variant nobody subscribed to. Prices
 * are strings here for the reason they are strings everywhere on this
 * boundary: the renderer parses at its own edge and nothing between the venue
 * and the pixel rounds.
 */
export type FeedUpdate =
  | { kind: "ctx"; at_ms: number; row: MarketRow }
  | { kind: "bbo"; coin: string; at_ms: number; bid?: BookLevel; ask?: BookLevel }
  | { kind: "book"; coin: string; at_ms: number; bids: BookLevel[]; asks: BookLevel[] }
  | {
      kind: "candle";
      coin: string;
      interval: string;
      time_ms: number;
      open: string;
      high: string;
      low: string;
      close: string;
      volume: string;
    }
  | {
      kind: "trade";
      coin: string;
      at_ms: number;
      /** The last print in the frame: the bar's close. */
      px: string;
      /** The frame's own extremes. A batch can carry a spike `px` does not show. */
      high: string;
      low: string;
      /** Every print in the frame, summed. */
      sz: string;
    }
  | { kind: "status"; last_tick_ms?: number; connected: boolean; detail?: string };

export interface FeedBinding {
  network: "testnet" | "mainnet";
  generation: string;
}

export interface FeedEnvelope extends FeedBinding {
  update: FeedUpdate;
  failure?: string;
}

/** Cached desktop task lifecycle only; not execution or reconciliation readiness. */
export interface RuntimeStatus {
  phase: "running" | "replacing" | "stopping" | "stopped" | "stopped_with_error";
  binding: FeedBinding | null;
  detail: string | null;
}

export function fetchRuntimeStatus(): Promise<RuntimeStatus> {
  return invoke<RuntimeStatus>("runtime_status");
}

export interface McpStatus {
  halt: {
    phase: "idle" | "persisting" | "persisted" | "uncertain";
    cancellation: "not_requested" | "pending" | "retrying" | "acknowledged" | "unavailable";
    requested_at_ms: number | null;
    durable_revision: number | null;
    error: string | null;
    cancellation_error: string | null;
  };
  phase: "idle" | "starting" | "listening" | "stopping" | "stopped" | "failed";
  network: "testnet" | "mainnet";
  agent: string | null;
  account: string | null;
  listener: string | null;
  reconciled: boolean | null;
  account_feeds_ready: boolean | null;
  supervision_last_completed_ms: number | null;
  supervision_in_progress: boolean;
  supervision_error: string | null;
  orders_inhibited: boolean;
  detail: string | null;
}

export function fetchMcpStatus(): Promise<McpStatus> { return invoke<McpStatus>("mcp_status"); }
export function startMcp(agent: string, account: string): Promise<McpStatus> {
  return invoke<McpStatus>("start_mcp", { agent, account });
}
export function stopMcp(): Promise<RuntimeStatus> { return invoke<RuntimeStatus>("stop_mcp"); }
export function haltMcp(agent: string, account: string): Promise<McpStatus> {
  return invoke<McpStatus>("halt_mcp", { agent, account });
}

/**
 * Point the socket at one symbol and interval.
 *
 * Called on selection and on an interval change. The REST reads beside it stay
 * — they are the seed and the history a socket does not carry — but from here
 * on the strip, the ladder and the forming bar move on their own.
 */
export async function watchMarket(
  network: "testnet" | "mainnet",
  coin: string,
  interval: string,
): Promise<FeedBinding> {
  return invoke<FeedBinding>("watch_market", { network, coin, interval });
}

/**
 * Subscribe to the feed. Returns the unsubscribe.
 *
 * Outside Tauri there is no socket, so this resolves to a no-op rather than
 * throwing: the console renders in a browser for design work and must not need
 * a venue to do it.
 */
export async function onFeedUpdate(
  handler: (envelope: FeedEnvelope) => void,
): Promise<() => void> {
  if (!inTauri()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<FeedEnvelope>("feed://update", (event) => handler(event.payload));
}

/** UP2: only version/readiness cross IPC; private-release credentials stay in Rust. */
export interface UpdateInfo { current_version: string; available_version: string | null; ready: boolean }
export function checkUpdate(): Promise<UpdateInfo> { return invoke("check_update"); }
export function downloadUpdate(): Promise<UpdateInfo> { return invoke("download_update"); }
export function installUpdate(): Promise<void> { return invoke("install_update"); }
