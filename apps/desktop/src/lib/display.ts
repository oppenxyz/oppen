/** Display only (spec F). Exact decimal strings remain untouched in the bridge. */
export function decimal(value: string | null | undefined, digits = 2, suffix = ""): string {
  if (value == null || !/^[+-]?\d+(?:\.\d+)?$/.test(value)) return "—";
  const negative = value.startsWith("-");
  const [whole = "0", fraction = ""] = value.replace(/^[+-]/, "").split(".");
  const scale = 10n ** BigInt(digits);
  let rounded = BigInt(whole) * scale + BigInt(fraction.slice(0, digits).padEnd(digits, "0") || "0");
  if (Number(fraction[digits] ?? "0") >= 5) rounded += 1n;
  const integer = (rounded / scale).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const tail = digits ? `.${(rounded % scale).toString().padStart(digits, "0")}` : "";
  return `${negative && rounded !== 0n ? "−" : ""}${integer}${tail}${suffix}`;
}

/** Event bodies are heterogeneous and untrusted. Only a string can become a text claim. */
export function eventText(payload: unknown, field: string): string | null {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) return null;
  const value = (payload as Record<string, unknown>)[field];
  return typeof value === "string" ? value : null;
}

/** An operator action may name an agent; that does not make its text agent-authored. */
export function isAgentDecision(event: { kind: string; agent_id: string | null }): boolean {
  return event.agent_id !== null && ["agent_decision", "order_intent", "refusal"].includes(event.kind);
}
