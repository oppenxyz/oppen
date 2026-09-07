/**
 * The guided walkthrough and the setup tracker (`docs/spec.md` item 4).
 *
 * Two things that look like one. The **walkthrough** points at real elements
 * in the real console and says what each is for; the **tracker** says how far
 * the operator has got toward a first agent placing a first order.
 *
 * They are here rather than inside a component because both outlive any one
 * view: the walkthrough switches views as it goes, and the tracker is read
 * from the onboarding screen and the header alike.
 *
 * **The tracker never claims a step is done on its own say-so.** Every
 * milestone is a predicate over state the console can actually observe, and a
 * milestone whose evidence does not exist yet reports `unverifiable` rather
 * than `pending` — those are different facts, and a checklist that quietly
 * shows an unbuilt step as merely incomplete is one an operator will wait on
 * forever.
 */

import { computed, reactive, readonly } from "vue";

import type { KeychainStatus } from "../lib/bridge";
import { shell, setView, type View } from "./shell";

/** Where a callout sits relative to the element it points at. */
export type Placement = "top" | "bottom" | "left" | "right";

export interface TourStep {
  /** Matched against `[data-tour="…"]`. */
  target: string;
  /** Switched to before the step is shown, so the target exists to point at. */
  view: View;
  title: string;
  body: string;
  placement: Placement;
  /**
   * Shown in the callout when the thing being pointed at is not finished.
   * A tour that presents a disabled control as working is a tour that teaches
   * the operator to distrust it.
   */
  caveat?: string;
}

/**
 * The tour, in the order an operator meets the product.
 *
 * Deliberately one pass over every surface rather than a short "here are the
 * three things" tour: item 4's gate is a fresh machine to a testnet trade, and
 * the surfaces an operator does not know about are the ones they will not use
 * when it matters — the kill switch above all.
 */
export const TOUR: readonly TourStep[] = [
  {
    target: "nav",
    view: "trade",
    title: "Five screens, one machine",
    body: "Trade, Agents, Builder, Portfolio, Settings. The console and execution engine run locally. External agents connect through MCP and may use their own model providers.",
    placement: "bottom",
  },
  {
    target: "network",
    view: "trade",
    title: "Testnet until you say otherwise",
    body: "The badge is always visible because the difference matters. Mainnet is an explicit switch, never a default and never inferred from anything else.",
    placement: "bottom",
  },
  {
    target: "feeds",
    view: "trade",
    title: "Feed health, per feed",
    body: "Market socket, user socket, REST — each reported separately. When a feed is stale the console says so and the engine refuses to size an order against it, rather than quietly using the last price it saw.",
    placement: "top",
  },
  {
    target: "chart",
    view: "trade",
    title: "The market, drawn in characters",
    body: "Price history for the selected symbol. Direction is encoded by glyph as well as colour, so it survives a greyscale screenshot and a colour-blind reader.",
    placement: "right",
  },
  {
    target: "ticket",
    view: "trade",
    title: "The manual ticket",
    body: "Your own orders, signed by the same engine and recorded in the same ledger as an agent's. A trade you made by hand is not a special case.",
    placement: "left",
    caveat: "The manual ticket is not connected to the execution engine in this console yet.",
  },
  {
    target: "positions",
    view: "trade",
    title: "Positions, orders and fills",
    body: "Positions and open orders come from the configured account read. Unknown account state is distinct from a successful empty result.",
    placement: "top",
    caveat: "Fill history and agent attribution are not connected to this console.",
  },
  {
    target: "agents",
    view: "agents",
    title: "One agent, one container, one set of limits",
    body: "Every agent gets its own venue account and its own guardrails. A new one starts near zero — $25 an order, $100 a position, $25 of daily loss — and you raise the limits deliberately.",
    placement: "right",
  },
  {
    target: "builder",
    view: "builder",
    title: "Connect an external agent",
    body: "Builder explains the current testnet development path: configure the account, start the gateway, pair your MCP client and verify state and preflight.",
    placement: "right",
    caveat: "Saving a draft and arming an agent arrive with the agent registry and the pairing flow.",
  },
  {
    target: "portfolio",
    view: "portfolio",
    title: "What actually happened",
    body: "Equity, unrealised PnL, gross exposure, margin and nearest liquidation for the configured account. Exposure bars show each market’s share. Other containers do not share its margin.",
    placement: "right",
    caveat: "The execution report — slippage against the price each order was decided at, split by whether you crossed — is answered to agents through the gateway and is not on this screen yet.",
  },
  {
    target: "settings",
    view: "settings",
    title: "Guardrails, and who may reach them",
    caveat: "Active limits are not read or editable in this console yet.",
    body: "Risk limits are operator owned and enforced in Rust before signing. Settings identifies which controls are connected. External agents cannot change risk parameters through MCP.",
    placement: "right",
  },
  {
    target: "kill",
    view: "settings",
    title: "The kill switch",
    body: "Stops an agent, or all of them, and cancels their resting orders. Risk-reducing actions still go through while it is engaged — it stops new exposure, it never traps you in a position.",
    placement: "left",
    caveat: "The gateway halt control and per-container dead-man coverage are not connected to this console.",
  },
  {
    // The status bar is shell furniture, on screen whatever view is open, so
    // this stop stays where the previous one left the operator rather than
    // sending them back to Trade for something that was never hidden.
    target: "statusbar",
    view: "settings",
    title: "The last thing an agent did, and why",
    caveat: "The decision log is not connected to this console. Feed health and account errors are available.",
    body: "Every decision an agent takes is written to an append-only, hash-chained ledger with the agent's own stated reason. The reason is the agent's claim, shown as plain text and never acted on.",
    placement: "top",
  },
];

/** How a milestone stands. */
export type MilestoneState = "done" | "pending" | "unverifiable";

export interface Milestone {
  id: string;
  label: string;
  /** What the operator does, in one line. */
  detail: string;
  state: MilestoneState;
  /** Present on `unverifiable`: what is missing, named rather than implied. */
  blocked?: string;
}

interface TourState {
  /** `-1` when the walkthrough is closed. */
  step: number;
  /** Set once the operator finishes or skips, so it does not reopen itself. */
  seen: boolean;
}

const SEEN_KEY = "oppen.walkthrough.seen";

function loadSeen(): boolean {
  try {
    return localStorage.getItem(SEEN_KEY) === "1";
  } catch {
    // A browser with storage blocked still gets a working console; it just
    // offers the walkthrough again next launch, which is the harmless
    // direction to fail in.
    return false;
  }
}

const state = reactive<TourState>({ step: -1, seen: loadSeen() });

export const tour = readonly(state);

export const isOpen = computed(() => state.step >= 0);
export const currentStep = computed<TourStep | null>(() =>
  state.step >= 0 && state.step < TOUR.length ? TOUR[state.step] : null,
);

function show(index: number): void {
  state.step = index;
  const step = TOUR[index];
  if (step) setView(step.view);
}

export function startTour(): void {
  show(0);
}

export function nextStep(): void {
  if (state.step < 0) return;
  if (state.step + 1 >= TOUR.length) {
    endTour();
    return;
  }
  show(state.step + 1);
}

export function prevStep(): void {
  if (state.step > 0) show(state.step - 1);
}

/** Closes the walkthrough and remembers that it was offered. */
export function endTour(): void {
  state.step = -1;
  state.seen = true;
  try {
    localStorage.setItem(SEEN_KEY, "1");
  } catch {
    // Nothing to do: the flag is a convenience, not state anything depends on.
  }
}

/**
 * The setup tracker: how far this machine is from a first agent trading.
 *
 * Derived from `shell` on every read rather than stored, so it cannot drift
 * from what the console is actually showing — a checklist that remembers being
 * satisfied is one that keeps saying so after the thing stops being true.
 */
/**
 * A keychain nobody has asked about is not an unreachable one.
 *
 * `null` means the question has not been put — outside Tauri there is no store
 * to ask. Collapsing that into `pending` would tell an operator to go and fix
 * something that is not broken.
 */
export function keychainState(status: KeychainStatus | null): MilestoneState {
  if (status === null) return "unverifiable";
  return status.reachable ? "done" : "pending";
}

export function keychainDetail(status: KeychainStatus | null): string {
  if (status === null) return "The OS keychain that holds agent keys and the guardrail HMAC key.";
  if (status.reachable) return "The store answered. Nothing need be in it yet.";
  // The store's own words, because "the keychain is locked" is actionable
  // where "unreachable" is not.
  return `The store did not answer: ${status.detail ?? "no reason given"}`;
}

export const milestones = computed<Milestone[]>(() => {
  const account = shell.account;
  // Equity crosses the bridge as a decimal *string* — the workspace never puts
  // a decimal in JSON as a number, because a float is the wrong container for
  // money. So it is parsed here, and anything unparseable counts as not
  // funded: a milestone that ticked on `NaN` would be worse than one that
  // stayed unticked.
  const equity = account === null ? Number.NaN : Number(account.balances.equity_usd);
  const funded = Number.isFinite(equity) && equity > 0;

  return [
    {
      id: "runtime",
      label: "Console open",
      detail: "This interface is open. Engine and feed connectivity are checked separately.",
      // Reaching this code at all means the runtime served the page.
      state: "done",
    },
    {
      id: "network",
      label: "Network chosen",
      detail: `Currently ${shell.network}. Mainnet is an explicit switch.`,
      state: "done",
    },
    {
      id: "venue",
      label: "Account read",
      detail: "A successful account read is separate from the public market feed.",
      state: account !== null ? "done" : "pending",
    },
    {
      id: "funded",
      label: "Container funded",
      detail: funded
        ? "Equity is positive, so an order has something to be sized against."
        : (account ? `Fund the ${shell.network} container address, then refresh.` : "Configure an account before checking its funding."),
      state: funded ? "done" : "pending",
    },
    {
      id: "keychain",
      label: "Keychain reachable",
      detail: keychainDetail(shell.keychain),
      state: keychainState(shell.keychain),
      blocked:
        shell.keychain === null
          ? "Only askable inside the desktop app; a browser dev server has no OS keychain."
          : undefined,
    },
    {
      id: "wallet",
      label: "Agent wallet generated",
      detail: "A key for the agent to sign with, held in the OS keychain.",
      state: "unverifiable",
      // Corrected: `oppen-core::keys` implements the whole store — records,
      // rotation, address retirement. What is missing is narrower and worth
      // naming precisely, because the vague version sent me looking in the
      // wrong crate.
      blocked:
        "The keystore exists, but the console has no agent registry to enumerate, so it does not know which agent to ask about.",
    },
    {
      id: "approve",
      label: "Approvals signed",
      detail: "Three signatures in your own wallet: fund, authorise the agent, approve the builder fee.",
      state: "unverifiable",
      blocked:
        "The venue publishes no read that confirms an `approveAgent` landed, so this can only be inferred from a signature the console did not witness.",
    },
    {
      id: "paired",
      label: "First agent paired",
      detail: "The `claude mcp add` snippet, and a connection test that proves the token works.",
      state: "unverifiable",
      blocked: "Pairing is built in the gateway; the console has no read of the pairing store yet.",
    },
  ];
});

/** Milestones the console can actually check, and how many are done. */
export const progress = computed(() => {
  const list = milestones.value;
  const verifiable = list.filter((m) => m.state !== "unverifiable");
  const done = verifiable.filter((m) => m.state === "done").length;
  return {
    done,
    /** Deliberately the count of *checkable* steps, not of all of them. */
    total: verifiable.length,
    unverifiable: list.length - verifiable.length,
    fraction: verifiable.length === 0 ? 0 : done / verifiable.length,
  };
});
