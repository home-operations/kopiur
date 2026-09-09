import { describe, expect, it } from "vitest";

import { type VerdictInputs, overviewVerdict } from "./verdict";

const quiet: VerdictInputs = {
  repositories: { failed: 0, degraded: 0, pending: 0, unknown: 0, suspended: 0, healthy: 3 },
  stalled: 0,
  doctor: { pass: 10, warn: 0, fail: 0, other: 0, total: 10 },
  unavailable: [],
};

describe("overviewVerdict", () => {
  it("is healthy only when every source loaded and nothing is lit", () => {
    const verdict = overviewVerdict(quiet);
    expect(verdict.health).toBe("healthy");
    expect(verdict.text).toBe("All 3 repositories healthy, nothing stalled, doctor passes.");
  });

  it("counts the suspended and pending as not lit but not healthy either", () => {
    const verdict = overviewVerdict({
      ...quiet,
      repositories: { ...quiet.repositories, healthy: 2, suspended: 1, pending: 1 },
    });
    expect(verdict.health).toBe("healthy");
    expect(verdict.text).toBe(
      "2 of 4 repositories healthy (1 pending, 1 suspended), nothing stalled, doctor passes.",
    );
  });

  it("fails loudly on a failed repository, a stalled object or a failing check", () => {
    expect(
      overviewVerdict({ ...quiet, repositories: { ...quiet.repositories, failed: 1 } }),
    ).toEqual({
      health: "failed",
      text: "Needs attention: 1 repository failed.",
    });
    expect(overviewVerdict({ ...quiet, stalled: 2 })).toEqual({
      health: "failed",
      text: "Needs attention: 2 objects stalled.",
    });
    expect(
      overviewVerdict({
        ...quiet,
        repositories: { ...quiet.repositories, failed: 2, degraded: 1 },
        stalled: 1,
        doctor: { ...quiet.doctor, fail: 1, warn: 2 },
      }),
    ).toEqual({
      health: "failed",
      text: "Needs attention: 2 repositories failed, 1 degraded, 1 object stalled, 1 doctor check failing, 2 warning.",
    });
  });

  it("is degraded on a degraded or unknown repository or a doctor warning", () => {
    expect(
      overviewVerdict({ ...quiet, repositories: { ...quiet.repositories, degraded: 1 } }),
    ).toEqual({ health: "degraded", text: "Mostly healthy: 1 repository degraded." });
    expect(
      overviewVerdict({ ...quiet, repositories: { ...quiet.repositories, unknown: 1 } }),
    ).toEqual({ health: "degraded", text: "Mostly healthy: 1 repository unknown." });
    expect(overviewVerdict({ ...quiet, doctor: { ...quiet.doctor, warn: 3 } })).toEqual({
      health: "degraded",
      text: "Mostly healthy: 3 doctor checks warning.",
    });
  });

  it("never claims healthy while a source is missing", () => {
    // A green verdict over a report that did not load is the lie this
    // screen exists to avoid.
    const verdict = overviewVerdict({ ...quiet, unavailable: ["the status report"] });
    expect(verdict.health).toBe("unknown");
    expect(verdict.text).toBe("Cannot tell: the status report did not load.");
    const two = overviewVerdict({
      ...quiet,
      repositories: { ...quiet.repositories, failed: 1 },
      unavailable: ["the status report", "doctor"],
    });
    // What did load still counts, and it is worse than unknown.
    expect(two.health).toBe("failed");
    expect(two.text).toBe(
      "Needs attention: 1 repository failed. The status report and doctor did not load.",
    );
  });

  it("says when there is nothing in scope rather than calling an empty cluster healthy", () => {
    const verdict = overviewVerdict({
      ...quiet,
      repositories: { failed: 0, degraded: 0, pending: 0, unknown: 0, suspended: 0, healthy: 0 },
    });
    expect(verdict.health).toBe("unknown");
    expect(verdict.text).toBe("No repositories in scope, nothing stalled, doctor passes.");
  });
});
