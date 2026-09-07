import { decimal, eventText, sumDecimals } from "./display";
declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void) => void;
declare const expect: (value: unknown) => { toBe(expected: unknown): void };

describe("display decimals", () => {
  it("separates absent and invalid readings from real zero", () => {
    for (const value of [null, undefined, "", "null", "NaN", "1e10"]) expect(decimal(value)).toBe("—");
    expect(decimal("0")).toBe("0.00");
  });
  it("rounds without losing exact large integer digits", () => {
    expect(decimal("9007199254740993.995")).toBe("9,007,199,254,740,994.00");
    expect(decimal("1.0117745260468704549191210000", 2, " bp")).toBe("1.01 bp");
    expect(decimal("-0.856850000", 2, " bp")).toBe("−0.86 bp");
    expect(decimal("-0.00001")).toBe("0.00");
    expect(decimal("999.6", 0)).toBe("1,000");
  });
});

it("event claims accept only text and preserve it without interpretation", () => {
  expect(eventText({ reason: "<b>claim</b>" }, "reason")).toBe("<b>claim</b>");
  expect(eventText({ reason: { html: "claim" } }, "reason")).toBe(null);
  expect(eventText(["claim"], "reason")).toBe(null);
  expect(eventText(null, "reason")).toBe(null);
});

// Agent IDs also appear on operator actions; do not attribute those reasons to agents.
it('does not label an operator action as an agent decision', async () => {
  const { isAgentDecision } = await import('./display');
  expect(isAgentDecision({ kind: 'operator_action', agent_id: 'alpha' })).toBe(false);
  expect(isAgentDecision({ kind: 'refusal', agent_id: 'alpha' })).toBe(true);
  expect(isAgentDecision({ kind: 'refusal', agent_id: null })).toBe(false);
});

describe("portfolio decimal totals", () => {
  it("keeps cents beyond the safe integer range and rounds only the total", () => {
    expect(sumDecimals(["9007199254740993.99", "0.02"])).toBe("9007199254740994.01");
    expect(decimal(sumDecimals(["0.004", "0.004"]))).toBe("0.01");
  });
  it("supports signed PnL and absolute gross exposure", () => {
    expect(sumDecimals(["24.56789", "-12.345"])).toBe("12.22289");
    expect(sumDecimals(["-20.1", "10.01"], true)).toBe("30.11");
    expect(sumDecimals(["-2", "0.1"])).toBe("-1.9");
  });
  it("distinguishes invalid values from an empty total", () => {
    expect(sumDecimals([])).toBe("0");
    expect(sumDecimals(["1", "NaN"])).toBe(null);
  });
});
