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
    body: "Trade, Agents, Builder, Portfolio, Settings. Everything runs locally: your keys, your agents, the execution engine. Nothing here calls home.",
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
    caveat: "Wired to the guardrail engine; live signing arrives with the keychain phase.",
  },
  {
    target: "positions",
    view: "trade",
    title: "Positions, orders and fills",
    body: "Three ledgers behind one set of tabs, and each position carries its liquidation price and how far the mark is from it. Agent-held and manual rows sit together: a trade you made by hand is not a special case.",
    placement: "top",
    caveat: "The same distance in daily standard deviations — the figure that compares across symbols — is answered to agents through the gateway but is not on this screen yet.",
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
    title: "Build an agent, then prove it before arming it",
    body: "Source, the hard limits the runtime enforces, and the instructions the model actually sees — then a testnet run whose trades, PnL and refusals you read before anything is armed. The order of the panels is the order of the ceremony.",
    placement: "right",
    caveat: "Saving a draft and arming an agent arrive with the agent registry and the pairing flow.",
  },
  {
    target: "portfolio",
    view: "portfolio",
    title: "What actually happened",
    body: "Equity, unrealised PnL, exposure, margin used and the nearest liquidation across every position — then the same broken out by agent and by sub-account, so a number you do not like has somewhere to be traced to.",
    placement: "right",
    caveat: "The execution report — slippage against the price each order was decided at, split by whether you crossed — is answered to agents through the gateway and is not on this screen yet.",
  },
  {
    target: "settings",
    view: "settings",
    title: "Guardrails, and who may reach them",
    body: "Limits are set here and nowhere else. No agent can read this screen or change what is on it — the one path from an agent to the signer runs through the guardrail engine, and it has no door onto this page.",
    placement: "right",
  },
  {
    target: "kill",
    view: "settings",
    title: "The kill switch",
    body: "Stops an agent, or all of them, and cancels their resting orders. Risk-reducing actions still go through while it is engaged — it stops new exposure, it never traps you in a position.",
    placement: "left",
    caveat: "The engine enforces this today; the console control is wired up in the guardrail phase.",
  },
  {
    // The status bar is shell furniture, on screen whatever view is open, so
    // this stop stays where the previous one left the operator rather than
    // sending them back to Trade for something that was never hidden.
    target: "statusbar",
    view: "settings",
    title: "The last thing an agent did, and why",
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
      label: "Local runtime",
      detail: "The console is running and talking to its own engine.",
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
      label: "Venue reachable",
      detail: "A read of your account returned, so the venue is answering.",
      state: account !== null ? "done" : "pending",
    },
    {
      id: "funded",
      label: "Container funded",
      detail: funded
        ? "Equity is positive, so an order has something to be sized against."
        : "Send test USDC to the container address, then refresh.",
      state: funded ? "done" : "pending",
    },
    {
      id: "wallet",
      label: "Agent wallet generated",
      detail: "A key for the agent to sign with, held in the OS keychain.",
      state: "unverifiable",
      blocked: "The keychain phase is not built, so the console cannot see whether a key exists.",
    },
    {
      id: "approve",
      label: "Approvals signed",
      detail: "Three signatures in your own wallet: fund, authorise the agent, approve the builder fee.",
      state: "unverifiable",
      blocked: "Needs the keychain phase and a live `approveAgent` read to confirm against.",
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
