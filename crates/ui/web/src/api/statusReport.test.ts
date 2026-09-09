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
  it("reads every section the overview renders", () => {
    const report = narrowStatusReport(full);
    expect(report.policies).toBe(1);
    expect(report.schedules).toBe(1);
    expect(report.inFlight).toEqual({ snapshots: 2, restores: 1 });
    expect(report.stalled).toEqual([
      { kind: "Snapshot", object: "media/nightly-1", message: "MoverPermitted=False" },
      { kind: "Restore", object: "media/r1", message: "credentials missing" },
    ]);
    expect(report.complete).toBe(true);
  });

  it("narrows nothing the overview does not render, so no unread row can flip complete", () => {
    // The module narrows only what a route reads. `repositories` and
    // `snapshotReplications` were narrowed and never rendered, so an
    // unreadable repository row raised "report incomplete" on a page that
    // shows no repository section — a warning about something invisible.
    const report = narrowStatusReport({
      ...full,
      repositories: ["not-an-object", { name: "no-kind" }],
      snapshotReplications: "not-a-list",
    });
    expect(report).not.toHaveProperty("repositories");
    expect(report).not.toHaveProperty("snapshotReplications");
    expect(report.complete).toBe(true);
  });

  it("treats every field as possibly absent and says the report was incomplete", () => {
    // Addenda item 15: the report is `unknown` by design; a newer or older
    // server may omit or rename any of it. Nothing here may throw.
    for (const value of [null, undefined, 42, "text", [], {}]) {
      const report = narrowStatusReport(value);
      expect(report.stalled).toEqual([]);
      expect(report.inFlight).toEqual({ snapshots: null, restores: null });
      expect(report.policies).toBeNull();
      expect(report.complete).toBe(false);
    }
  });

  it("keeps the rows it can read and drops the ones it cannot, without inventing values", () => {
    const report = narrowStatusReport({
      ...full,
      inFlight: { snapshots: "3", restores: 1 },
      stalled: [
        { kind: "Snapshot", object: "a/b", message: 7 },
        { kind: "Snapshot", object: "c/d", message: "m" },
        "not-an-object",
      ],
    });
    // A count that is not a number is unknown, never coerced to 3 or 0.
    expect(report.inFlight).toEqual({ snapshots: null, restores: 1 });
    // A stalled object is a real fact and is kept — but its unreadable
    // message is `null`, not `""`. An empty string reads as "the operator
    // said nothing", which is a claim this bundle is in no position to make.
    expect(report.stalled).toEqual([
      { kind: "Snapshot", object: "a/b", message: null },
      { kind: "Snapshot", object: "c/d", message: "m" },
    ]);
    expect(report.complete).toBe(false);
  });

  it("counts a stalled row whose message it could not read as incomplete", () => {
    // The module doc promises a row is never "patched with a default that
    // looks like a fact". `message: ""` was exactly that, and it left
    // `complete` true, so the page said nothing was missing.
    const report = narrowStatusReport({
      ...full,
      stalled: [{ kind: "Snapshot", object: "a/b" }],
    });
    expect(report.stalled).toEqual([{ kind: "Snapshot", object: "a/b", message: null }]);
    expect(report.complete).toBe(false);
  });
});
