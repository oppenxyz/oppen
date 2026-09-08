import { policyDiff, policyFields } from "./policy-review";

declare const it: (name: string, body: () => void) => void;
declare const expect: (value: unknown) => { toBe(expected: unknown): void };

it("shows retained risk, freshness, unrelated policies and stop reasons without rounding", () => {
  const before = {
    guardrails: { other: { freshness: { max_market_age_ms: 1234 }, risk: { max_risk_usd: "0.000000000000000001" } } },
    kill: { agents: { other: { reason: "<img src=x onerror=alert(1)>" } } },
  };
  const rows = policyDiff(before, before);
  expect(rows.length).toBe(3);
  expect(rows.every(row => !row.changed)).toBe(true);
  expect(rows[1].proposed).toBe("0.000000000000000001");
  expect(rows[2].proposed).toBe("<img src=x onerror=alert(1)>");
});

it("distinguishes removed fields, null, empty maps and empty lists", () => {
  const rows = policyDiff({ missing: "value", limit: null, symbols: [] }, { limit: "0", symbols: [], agents: {} });
  expect(rows.find(row => row.path[0] === "missing")?.proposed).toBe("Not present");
  expect(rows.find(row => row.path[0] === "limit")?.before).toBe("None / unset");
  expect(rows.find(row => row.path[0] === "symbols")?.proposed).toBe("Empty list");
  expect(rows.find(row => row.path[0] === "agents")?.proposed).toBe("Empty map");
});

it("keeps arbitrary agent and reason paths distinct", () => {
  const rows = policyFields({ "a/b": { c: true }, a: { "b/c": false } });
  expect(JSON.stringify(rows[0].path) === JSON.stringify(rows[1].path)).toBe(false);
});
