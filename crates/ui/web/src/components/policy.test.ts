import { describe, expect, it } from "vitest";

import type { PolicyRow } from "../api/types";
import { policyVerdict, repositoryName, retentionRules, snapshotCount } from "./policy";

function row(overrides: Partial<PolicyRow> = {}): PolicyRow {
  return {
    namespace: "media",
    name: "nightly",
    repositories: ["Repository/media/nas"],
    multiRepo: false,
    suspended: false,
    lastSuccessfulSnapshot: "2026-09-09T02:04:00Z",
    lastVerified: null,
    activeSnapshotCount: 42,
    ...overrides,
  };
}

describe("repositoryName", () => {
  it("takes the bare name out of either repo_key shape", () => {
    expect(repositoryName("Repository/media/nas")).toBe("nas");
    expect(repositoryName("ClusterRepository/shared")).toBe("shared");
  });

  it("hands back a key it cannot split rather than an empty string", () => {
    expect(repositoryName("nas")).toBe("nas");
  });
});

describe("retentionRules", () => {
  it("returns the slots the policy sets, in GFS order", () => {
    const rules = retentionRules({ keepDaily: 7, keepMonthly: 6, keepLatest: 3 });
    expect(rules.map((rule) => rule.field)).toEqual(["keepLatest", "keepDaily", "keepMonthly"]);
    expect(rules.map((rule) => rule.count)).toEqual([3, 7, 6]);
  });

  it("keeps an explicit zero, which is not the same as an unset rule", () => {
    // `keepHourly: 0` says "keep no hourly slots"; an absent keepHourly says
    // the policy never mentioned hourlies. Dropping the zero would erase a
    // rule the operator wrote.
    const rules = retentionRules({ keepHourly: 0 });
    expect(rules).toHaveLength(1);
    expect(rules[0]?.count).toBe(0);
  });

  it("is empty for a policy with no retention at all", () => {
    expect(retentionRules(null)).toEqual([]);
    expect(retentionRules(undefined)).toEqual([]);
    expect(retentionRules({})).toEqual([]);
  });
});

describe("policyVerdict", () => {
  it("leads with suspension, because nothing else about the recipe is happening", () => {
    const verdict = policyVerdict(row({ suspended: true }));
    expect(verdict.suspended).toBe(true);
    expect(verdict.text).toContain("no schedule will fire it");
  });

  it("says a policy has never succeeded rather than leaving it unsaid", () => {
    expect(policyVerdict(row({ lastSuccessfulSnapshot: null })).text).toContain(
      "never recorded a successful snapshot",
    );
  });

  it("counts a fan-out and names a single target", () => {
    expect(policyVerdict(row()).text).toContain("writes into Repository/media/nas");
    expect(
      policyVerdict(row({ repositories: ["Repository/media/nas", "ClusterRepository/shared"] }))
        .text,
    ).toContain("fans out into 2 repositories");
  });

  it("says a policy with no repository has nowhere to write", () => {
    expect(policyVerdict(row({ repositories: [] })).text).toContain("nowhere to write");
  });
});

describe("snapshotCount", () => {
  it("tells a measured zero apart from an absent count", () => {
    expect(snapshotCount(0)).toBe("0");
    expect(snapshotCount(null)).toBe("-");
    expect(snapshotCount(undefined)).toBe("-");
  });
});
