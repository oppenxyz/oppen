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

/** Sum source decimals before rounding; Number would lose cents on large accounts. */
export function sumDecimals(values: string[], absolute = false): string | null {
  if (values.some(value => !/^[+-]?\d+(?:\.\d+)?$/.test(value))) return null;
  const digits = Math.max(0, ...values.map(value => value.split(".")[1]?.length ?? 0));
  const scale = 10n ** BigInt(digits);
  const total = values.reduce((sum, value) => {
    const [whole, fraction = ""] = value.replace(/^[+-]/, "").split(".");
    const magnitude = BigInt(whole!) * scale + BigInt(fraction.padEnd(digits, "0") || "0");
    return sum + (!absolute && value.startsWith("-") ? -magnitude : magnitude);
  }, 0n);
  const magnitude = total < 0n ? -total : total;
  return `${total < 0n ? "-" : ""}${magnitude / scale}${digits ? `.${(magnitude % scale).toString().padStart(digits, "0")}` : ""}`;
}
