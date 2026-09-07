/**
 * Tests for the walkthrough and the setup tracker.
 *
 * The tracker's whole value is that it does not overstate progress, so what is
 * pinned here is the three rules that keep it honest: an unverifiable step is
 * never counted as pending, the denominator is the checkable steps only, and a
 * milestone reads from live state rather than remembering.
 *
 * Written against the same `describe` / `it` / `expect` globals as
 * `lib/candles.test.ts`, with the same local declarations, so it runs unchanged
 * under `bun test` and keeps `vue-tsc --noEmit` green.
 */

import { keychainDetail, keychainState, milestones, progress, TOUR } from "./tour";

interface Assertions {
  toBe(expected: unknown): void;
  toEqual(expected: unknown): void;
  toBeGreaterThan(expected: number): void;
  toContain(expected: string): void;
}

interface Matchers extends Assertions {
  not: Assertions;
}

declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void) => void;
declare const expect: (actual: unknown) => Matchers;

/**
 * The slice of the filesystem this file uses, declared rather than pulled in
 * with `@types/node`.
 *
 * Same reasoning as the `describe` / `it` / `expect` shims above and in
 * `lib/candles.test.ts`: the runner supplies these at run time, and a types
 * package added for three functions in one test would be a dependency decision
 * (`AGENTS.md` leanness rule 9) taken for a very small convenience.
 */
declare const require: (id: string) => {
  readdirSync(path: string): string[];
  readFileSync(path: string, encoding: string): string;
  statSync(path: string): { isDirectory(): boolean };
};

describe("the walkthrough", () => {
  /**
   * The tour exists so an operator meets every surface. A step pointing at an
   * anchor nobody put in the markup would silently show an unhighlighted
   * callout, which is the failure mode this catches at build time instead.
   */
  it("names a placement and a view for every stop", () => {
    for (const step of TOUR) {
      expect(step.target.length).toBeGreaterThan(0);
      expect(["top", "bottom", "left", "right"]).toContain(step.placement);
      expect(["trade", "agents", "builder", "portfolio", "settings", "onboarding"]).toContain(
        step.view,
      );
      // Every stop says something. A stop with a title and no body is a stop
      // that wasted the operator's click.
      expect(step.body.length).toBeGreaterThan(40);
    }
  });

  /**
   * The tour walks each view in turn. Bouncing between screens makes the
   * product feel larger and less coherent than it is, and it was the first
   * ordering this file had — `kill` sat under Agents when the control is in
   * Settings.
   */
  it("visits each view in one run rather than bouncing between them", () => {
    const runs: string[] = [];
    for (const step of TOUR) {
      if (runs[runs.length - 1] !== step.view) runs.push(step.view);
    }
    const seen = new Set(runs);
    expect(runs.length).toBe(seen.size);
  });

  /**
   * **The one that catches a silent break.** A step naming a target nobody put
   * a `data-tour` on still renders its callout — centred, with nothing lit up —
   * so the tour keeps its step count and quietly stops pointing at anything.
   * Nothing at runtime complains, which is why this is checked against the
   * markup here.
   */
  it("points only at anchors that exist in the markup", () => {
    const fs = require("node:fs");
    const anchors = new Set<string>();
    const walk = (dir: string): void => {
      for (const entry of fs.readdirSync(dir)) {
        const path = `${dir}/${entry}`;
        if (fs.statSync(path).isDirectory()) {
          walk(path);
        } else if (path.endsWith(".vue")) {
          for (const match of fs.readFileSync(path, "utf8").matchAll(/data-tour="([^"$]+)"/g)) {
            anchors.add(match[1] as string);
          }
        }
      }
    };
    // Relative to `apps/desktop`, which is where both `bun test` and the CI
    // job that runs it start from.
    walk("src");

    for (const step of TOUR) {
      expect(anchors.has(step.target)).toBe(true);
    }
  });

  it("covers every screen the operator can navigate to", () => {
    const views = new Set(TOUR.map((step) => step.view));
    for (const view of ["trade", "agents", "builder", "portfolio", "settings"]) {
      expect(views.has(view as never)).toBe(true);
    }
  });
});

describe("the setup tracker", () => {
  /**
   * **The rule the whole panel exists for.** A step whose evidence is not
   * built reads `unverifiable` and names what is missing; it is never shown as
   * merely `pending`, because an operator waiting on a box nothing will tick
   * is worse served than one told the check does not exist.
   */
  it("never files an unbuilt check as pending", () => {
    for (const milestone of milestones.value) {
      if (milestone.state === "unverifiable") {
        expect(typeof milestone.blocked).toBe("string");
        expect((milestone.blocked ?? "").length).toBeGreaterThan(20);
      } else {
        expect(milestone.blocked).toBe(undefined);
      }
    }
  });

  /**
   * The denominator is the checkable steps only. Counting the unverifiable
   * ones would make a fully-configured machine read "2 / 7" forever, which
   * says the operator has failed at something they cannot do.
   */
  it("counts only what it can check, and says how many it cannot", () => {
    const list = milestones.value;
    const checkable = list.filter((m) => m.state !== "unverifiable");
    expect(progress.value.total).toBe(checkable.length);
    expect(progress.value.unverifiable).toBe(list.length - checkable.length);
    expect(progress.value.done).toBe(checkable.filter((m) => m.state === "done").length);
  });

  /** Every milestone carries a line the operator can act on. */
  it("says what to do, not only what is missing", () => {
    for (const milestone of milestones.value) {
      expect(milestone.detail.length).toBeGreaterThan(20);
      expect(milestone.label.length).toBeGreaterThan(0);
    }
  });

  /**
   * **Three answers, not two.** A keychain nobody has asked about is not an
   * unreachable one: outside the desktop app there is no store to ask, and
   * collapsing that into `pending` would send an operator to fix something
   * that is not broken. Only a store that answered "no" is pending.
   */
  it("tells an unasked keychain from an unreachable one", () => {
    expect(keychainState(null)).toBe("unverifiable");
    expect(keychainState({ reachable: true })).toBe("done");
    expect(keychainState({ reachable: false, detail: "locked" })).toBe("pending");
  });

  /**
   * A store that refused says why, in its own words — "the keychain is locked"
   * is actionable where "unreachable" is not.
   */
  it("passes the store's own reason through", () => {
    expect(keychainDetail({ reachable: false, detail: "the keychain is locked" })).toContain(
      "the keychain is locked",
    );
    // And a refusal with no message still reads as a refusal rather than as
    // an empty string appended to a sentence.
    expect(keychainDetail({ reachable: false })).toContain("no reason given");
  });

  /**
   * With no account read yet, "venue reachable" and "container funded" are
   * pending rather than done — the tracker starts from what is true, not from
   * an optimistic default.
   */
  it("starts pending rather than optimistic", () => {
    const byId = new Map(milestones.value.map((m) => [m.id, m]));
    expect(byId.get("venue")?.state).toBe("pending");
    expect(byId.get("funded")?.state).toBe("pending");
    expect(byId.get("runtime")?.state).toBe("done");
  });
});
