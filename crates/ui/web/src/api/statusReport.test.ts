import { describe, expect, it } from "vitest";

import { narrowStatusReport } from "./statusReport";

/** A report exactly as `kopiur_ops::status::StatusReport` serializes today. */
const full = {
  repositories: [
    {
      kind: "Repository",
      name: "nas",
      namespace: "media",
      phase: "Ready",
      backend: "Filesystem",
      mode: "ReadWrite",
      suspended: false,
      maintenance: "configured",
    },
    {
      kind: "ClusterRepository",
      name: "offsite",
      phase: "Failed",
      backend: "S3",
      mode: "ReadOnly",
      suspended: true,
      maintenance: "none",
      problem: "password Secret offsite-pw not found",
    },
  ],
  policies: [
    { name: "nightly", namespace: "media", repository: "Repository/nas", suspended: false },
  ],
  schedules: [
    {
      name: "nightly",
      namespace: "media",
      policy: "nightly",
      cron: "0 2 * * *",
      suspended: false,
      consecutiveFailures: 3,
    },
  ],
  snapshotReplications: [],
  inFlight: { snapshots: 2, restores: 1 },
  stalled: [
    { kind: "Snapshot", object: "media/nightly-1", message: "MoverPermitted=False" },
    { kind: "Restore", object: "media/r1", message: "credentials missing" },
  ],
};

describe("narrowStatusReport", () => {
  it("reads every section of a well-formed report", () => {
    const report = narrowStatusReport(full);
    expect(report.repositories).toHaveLength(2);
    expect(report.repositories[0]).toEqual({
      kind: "Repository",
      name: "nas",
      namespace: "media",
      phase: "Ready",
      suspended: false,
      problem: null,
    });
    expect(report.repositories[1]?.namespace).toBeNull();
    expect(report.repositories[1]?.problem).toBe("password Secret offsite-pw not found");
    expect(report.policies).toBe(1);
    expect(report.schedules).toBe(1);
    expect(report.snapshotReplications).toBe(0);
    expect(report.inFlight).toEqual({ snapshots: 2, restores: 1 });
    expect(report.stalled).toEqual([
      { kind: "Snapshot", object: "media/nightly-1", message: "MoverPermitted=False" },
      { kind: "Restore", object: "media/r1", message: "credentials missing" },
    ]);
    expect(report.complete).toBe(true);
  });

  it("treats every field as possibly absent and says the report was incomplete", () => {
    // Addenda item 15: the report is `unknown` by design; a newer or older
    // server may omit or rename any of it. Nothing here may throw.
    for (const value of [null, undefined, 42, "text", [], {}]) {
      const report = narrowStatusReport(value);
      expect(report.repositories).toEqual([]);
      expect(report.stalled).toEqual([]);
      expect(report.inFlight).toEqual({ snapshots: null, restores: null });
      expect(report.policies).toBeNull();
      expect(report.complete).toBe(false);
    }
  });

  it("keeps the rows it can read and drops the ones it cannot, without inventing values", () => {
    const report = narrowStatusReport({
      repositories: [
        { kind: "Repository", name: "ok", phase: "Ready", suspended: false },
        { name: "no-kind" },
        "not-an-object",
        { kind: "Repository", name: "no-phase", suspended: "yes" },
      ],
      inFlight: { snapshots: "3", restores: 1 },
      stalled: [
        { kind: "Snapshot", object: "a/b" },
        { kind: "Snapshot", object: "c/d", message: "m" },
      ],
    });
    expect(report.repositories.map((r) => r.name)).toEqual(["ok", "no-phase"]);
    expect(report.repositories[1]).toEqual({
      kind: "Repository",
      name: "no-phase",
      namespace: null,
      phase: null,
      suspended: null,
      problem: null,
    });
    // A count that is not a number is unknown, never coerced to 3 or 0.
    expect(report.inFlight).toEqual({ snapshots: null, restores: 1 });
    expect(report.stalled).toEqual([
      { kind: "Snapshot", object: "a/b", message: "" },
      { kind: "Snapshot", object: "c/d", message: "m" },
    ]);
    expect(report.complete).toBe(false);
  });
});
