import { describe, expect, it } from "vitest";

import type { RetentionCandidate, RetentionPlan } from "../api/types";
import {
  bucketLabel,
  candidateVerdict,
  planTotals,
  planVerdict,
  subjectBucket,
  subjectCandidate,
} from "./retention";

function candidate(over: Partial<RetentionCandidate> = {}): RetentionCandidate {
  return {
    namespace: "media",
    name: "nightly-9",
    endTime: "2026-09-09T01:04:00Z",
    kept: true,
    rules: ["keepDaily slot 1"],
    pinned: false,
    subject: false,
    ...over,
  };
}

function plan(over: Partial<RetentionPlan> = {}): RetentionPlan {
  return {
    buckets: [
      {
        key: "",
        candidates: [
          candidate({ name: "nightly-9", rules: ["keepDaily slot 1"], subject: true }),
          candidate({ name: "nightly-8", rules: ["keepDaily slot 2"] }),
          candidate({ name: "nightly-7", kept: false, rules: [] }),
        ],
      },
    ],
    policy: { namespace: "media", name: "nightly" },
    computedAt: "2026-09-10T09:00:00Z",
    unbounded: false,
    ...over,
  };
}

describe("candidateVerdict", () => {
  it("shows the holding rule as text, not as something to hover for", () => {
    const verdict = candidateVerdict(candidate({ rules: ["keepDaily slot 3"] }), false);
    expect(verdict.kept).toBe(true);
    expect(verdict.lamp.key).toBe("healthy");
    expect(verdict.rule).toBe("keepDaily slot 3");
  });

  it("lists every rule holding it, because a pin is not necessarily the only one", () => {
    // The wire doc is explicit: `["pinned", "keepDaily slot 1"]` is the useful
    // answer, because it says unpinning would not lose the snapshot.
    const verdict = candidateVerdict(
      candidate({ pinned: true, rules: ["pinned", "keepDaily slot 1"] }),
      false,
    );
    expect(verdict.rule).toBe("pinned, keepDaily slot 1");
  });

  it("says plainly that a pruned candidate is not held by any rule", () => {
    const verdict = candidateVerdict(candidate({ kept: false, rules: [] }), false);
    expect(verdict.kept).toBe(false);
    expect(verdict.lamp.word).toBe("Pruned");
    expect(verdict.rule).toMatch(/No rule keeps it/);
    // Never the failed lamp: a prune is the policy working, not a fault, and a
    // ledger of amber rows would drown the one row that matters.
    expect(verdict.lamp.key).not.toBe("failed");
  });

  it("says an unbounded plan keeps everything, and never that it will be pruned", () => {
    const verdict = candidateVerdict(candidate({ rules: [] }), true);
    expect(verdict.kept).toBe(true);
    expect(verdict.rule).toMatch(/no GFS retention is configured/i);
    expect(verdict.rule).not.toMatch(/prun/i);
  });

  it("does not invent a rule for a keep the server attributed to none", () => {
    const verdict = candidateVerdict(candidate({ kept: true, rules: [] }), false);
    expect(verdict.kept).toBe(true);
    expect(verdict.rule).toMatch(/no rule was attributed/i);
  });
});

describe("bucketLabel", () => {
  it("names the single bucket of an un-fanned policy rather than showing an empty key", () => {
    expect(bucketLabel("")).toMatch(/every snapshot of this policy/i);
  });

  it("prints a fan-out key verbatim, because the key is opaque", () => {
    expect(bucketLabel("pvc/media-data|repo/nas")).toBe("pvc/media-data|repo/nas");
  });
});

describe("planTotals", () => {
  it("counts kept and pruned across every bucket", () => {
    const totals = planTotals(plan());
    expect(totals).toEqual({ buckets: 1, candidates: 3, kept: 2, pruned: 1 });
  });

  it("counts an unbounded plan as all kept even though the rules are empty", () => {
    const totals = planTotals(
      plan({
        unbounded: true,
        buckets: [
          {
            key: "",
            candidates: [
              candidate({ name: "a", kept: true, rules: [], subject: true }),
              candidate({ name: "b", kept: true, rules: [] }),
            ],
          },
        ],
      }),
    );
    expect(totals).toEqual({ buckets: 1, candidates: 2, kept: 2, pruned: 0 });
  });
});

describe("subjectBucket and subjectCandidate", () => {
  it("finds the bucket and the row the request was about", () => {
    const p = plan({
      buckets: [
        { key: "pvc/a", candidates: [candidate({ name: "other" })] },
        { key: "pvc/b", candidates: [candidate({ name: "mine", subject: true })] },
      ],
    });
    expect(subjectBucket(p)?.key).toBe("pvc/b");
    expect(subjectCandidate(p)?.name).toBe("mine");
  });

  it("is undefined when no row is flagged, rather than guessing at the first", () => {
    const p = plan({ buckets: [{ key: "", candidates: [candidate({ subject: false })] }] });
    expect(subjectBucket(p)).toBeUndefined();
    expect(subjectCandidate(p)).toBeUndefined();
  });
});

describe("planVerdict", () => {
  it("says the snapshot is kept and by which rule", () => {
    const verdict = planVerdict(plan());
    expect(verdict.lamp.key).toBe("healthy");
    expect(verdict.text).toContain("keepDaily slot 1");
    expect(verdict.text).not.toMatch(/will be removed/);
  });

  it("is loud when the subject itself is the one about to go", () => {
    const verdict = planVerdict(
      plan({
        buckets: [
          {
            key: "",
            candidates: [
              candidate({ name: "nightly-9" }),
              candidate({ name: "nightly-1", kept: false, rules: [], subject: true }),
            ],
          },
        ],
      }),
    );
    expect(verdict.lamp.key).toBe("degraded");
    expect(verdict.text).toMatch(/next retention run/i);
    expect(verdict.text).toMatch(/no longer be restorable/i);
  });

  it("renders an unbounded plan as no retention configured, never as a prune", () => {
    const verdict = planVerdict(
      plan({
        unbounded: true,
        buckets: [{ key: "", candidates: [candidate({ kept: true, rules: [], subject: true })] }],
      }),
    );
    expect(verdict.text).toMatch(/no GFS retention is configured/i);
    expect(verdict.text).not.toMatch(/prun/i);
    expect(verdict.lamp.key).toBe("healthy");
  });

  it("says it cannot answer when the plan does not contain the subject at all", () => {
    const verdict = planVerdict(
      plan({ buckets: [{ key: "", candidates: [candidate({ subject: false })] }] }),
    );
    expect(verdict.lamp.key).toBe("unknown");
    expect(verdict.text).toMatch(/does not appear/i);
  });

  it("names how many other snapshots compete in the same bucket", () => {
    expect(planVerdict(plan()).text).toMatch(/2 others/);
  });
});
