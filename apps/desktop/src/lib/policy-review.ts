/** Flatten for inert display only. Missing, null and empty collections stay distinct. */
export function policyFields(value: unknown, path: string[] = []): { path: string[]; value: string }[] {
  if (value === null) return [{ path, value: "None / unset" }];
  if (Array.isArray(value)) {
    return value.length ? value.flatMap((item, index) => policyFields(item, [...path, String(index + 1)])) : [{ path, value: "Empty list" }];
  }
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>);
    return entries.length ? entries.flatMap(([key, item]) => policyFields(item, [...path, key])) : [{ path, value: "Empty map" }];
  }
  return [{ path, value: typeof value === "boolean" ? (value ? "Yes" : "No") : String(value) }];
}

export function policyDiff(before: unknown, proposed: unknown) {
  const old = new Map(policyFields(before).map(row => [JSON.stringify(row.path), row]));
  const next = new Map(policyFields(proposed).map(row => [JSON.stringify(row.path), row]));
  return [...new Set([...old.keys(), ...next.keys()])].map(key => {
    const previous = old.get(key);
    const current = next.get(key);
    return {
      key, path: (current ?? previous)!.path,
      before: previous?.value ?? "Not present", proposed: current?.value ?? "Not present",
      changed: previous?.value !== current?.value,
    };
  });
}

export function policyFieldLabel(path: string[]): string {
  return path.map((part, index) => ["guardrails", "agents"].includes(path[index - 1] ?? "")
    ? part : part.split("_").join(" ")).join(" / ") || "Snapshot";
}
