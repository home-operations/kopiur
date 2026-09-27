import { describe, expect, it, vi } from "vitest";

import type { GateDescriptor, GateSeverityView } from "../api/types";
import { gateScopeKinds, gateSeverityLamp, sortGates } from "./gates";

describe("gateSeverityLamp", () => {
  it("renders error as the failed lamp and warning as the degraded lamp, each with a word", () => {
    expect(gateSeverityLamp("error")).toMatchObject({ key: "failed", word: "Error" });
    expect(gateSeverityLamp("warning")).toMatchObject({ key: "degraded", word: "Warning" });
    expect(gateSeverityLamp("error").icon).not.toBe(gateSeverityLamp("warning").icon);
  });

  it("renders a severity this bundle has never seen as unknown, carrying the raw word", () => {
    // `GateSeverityView` has NO fallback variant (ui-model doc), so the switch
    // is exhaustive at compile time and the default arm must still render at
    // run time when a newer server adds a level.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    expect(gateSeverityLamp("critical" as GateSeverityView)).toMatchObject({
      key: "unknown",
      word: "critical",
    });
    expect(warn).toHaveBeenCalledOnce();
    warn.mockRestore();
  });
});

describe("gateScopeKinds", () => {
  it("splits the server's comma-joined kinds into one label strip per kind", () => {
    expect(gateScopeKinds("Snapshot,Restore")).toEqual(["Snapshot", "Restore"]);
    expect(gateScopeKinds("Repository,ClusterRepository")).toEqual([
      "Repository",
      "ClusterRepository",
    ]);
    expect(gateScopeKinds("SnapshotSchedule")).toEqual(["SnapshotSchedule"]);
    expect(gateScopeKinds(" Snapshot , Restore ,")).toEqual(["Snapshot", "Restore"]);
    expect(gateScopeKinds("")).toEqual([]);
  });
});

describe("sortGates", () => {
  const gate = (condition: string, severity: GateSeverityView, reason = "R"): GateDescriptor => ({
    scope: "Snapshot",
    condition,
    blockedStatus: "False",
    reason,
    severity,
  });

  it("puts errors before warnings, then orders by condition and reason, without mutating", () => {
    const input = [
      gate("RepositoryWritable", "warning"),
      gate("CredentialsAvailable", "error", "MissingServiceAccount"),
      gate("CredentialsAvailable", "error", "MissingCredentials"),
      gate("DeletionHeld", "error"),
    ];
    const snapshot = [...input];
    const sorted = sortGates(input);
    expect(sorted.map((g) => `${g.condition}/${g.reason}`)).toEqual([
      "CredentialsAvailable/MissingCredentials",
      "CredentialsAvailable/MissingServiceAccount",
      "DeletionHeld/R",
      "RepositoryWritable/R",
    ]);
    expect(input).toEqual(snapshot);
  });

  it("keeps an unrecognised severity after the known ones rather than losing it", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const sorted = sortGates([
      gate("Zed", "critical" as GateSeverityView),
      gate("Alpha", "warning"),
      gate("Beta", "error"),
    ]);
    expect(sorted.map((g) => g.condition)).toEqual(["Beta", "Alpha", "Zed"]);
    warn.mockRestore();
  });
});
