import { reactive, toRaw } from "vue";
import { accountObservation, accountObservationLabel, bindAccountObservation, receiveAccountStatus, reportAccountFailure, shell } from "./shell";
import { reportSelectedFailure, select } from "./market";
import type { AccountState } from "../lib/bridge";

declare const test: (name: string, body: () => void) => void;
declare const expect: (actual: unknown) => { toBe(expected: unknown): void };

test("account failure survives selected replacement and status recovery but not a new account owner", () => {
  const prior = { ...toRaw(accountObservation) };
  try {
    Object.assign(reactive(toRaw(accountObservation)), { binding: null, failure: null, detail: null });
    const binding = { network: shell.network, generation: "100" };
    bindAccountObservation(binding);
    receiveAccountStatus({ scope: "account", binding, update: { kind: "status", connected: false, last_tick_ms: null, detail: null }, failure: "Account application failed" });
    void select("ETH"); bindAccountObservation(binding);
    receiveAccountStatus({ scope: "account", binding, update: { kind: "status", connected: true, last_tick_ms: null, detail: null } });
    reportSelectedFailure({ binding: { ...binding, selection_id: "9", symbol: "ETH", interval: "1h" }, detail: "Selected failed" });
    expect(accountObservation.failure?.detail).toBe("Account application failed");
    expect(accountObservationLabel()).toBe("Consumer failed");
    bindAccountObservation({ ...binding, generation: "101" });
    expect(accountObservation.failure).toBe(null);
    reportAccountFailure({ binding, detail: "Retired account failed" });
    bindAccountObservation(binding);
    expect(accountObservation.binding?.generation).toBe("101");
    expect(accountObservation.failure).toBe(null);
  } finally { Object.assign(reactive(toRaw(accountObservation)), prior); }
});

test("account display names observation age at last read without socket readiness or a zero for unknown", () => {
  const prior = { account: shell.account, accountError: shell.accountError };
  const owner = { ...toRaw(accountObservation) };
  try {
    Object.assign(reactive(toRaw(accountObservation)), { binding: null, failure: null });
    const account: AccountState = { contract_version: 1, network: shell.network, address: "0x0000000000000000000000000000000000000001",
      as_of_ms: 1000, feed_age_ms: 500, feed: "live", balances: { equity_usd: "1", perps_account_value_usd: "1", spot_usdc_available: "0", total_margin_used_usd: "0", withdrawable_usd: "1" }, positions: [], orders: [] };
    Object.assign(reactive(toRaw(shell)), { account, accountError: null });
    expect(accountObservationLabel()).toBe("500 ms at last read");
    Object.assign(reactive(toRaw(shell)), { account: { ...account, feed: "stale", feed_age_ms: 30000 } });
    expect(accountObservationLabel()).toBe("Stale observation · 30000 ms at last read");
    Object.assign(reactive(toRaw(shell)), { account: { ...account, feed_age_ms: null } });
    expect(accountObservationLabel()).toBe("Observation unavailable");
  } finally { Object.assign(reactive(toRaw(shell)), prior); Object.assign(reactive(toRaw(accountObservation)), owner); }
});
