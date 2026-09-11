import { describe, expect, it, vi } from "vitest";

import type { SnapshotRow } from "../api/types";
import {
  ORIGIN_FILTERS,
  PHASE_FILTERS,
  deletionConsequence,
  deletionPolicyLabel,
  isOriginFilter,
  isPhaseFilter,
  originLabel,
  snapshotPhaseLamp,
  snapshotVerdict,
} from "./snapshot";

const row: SnapshotRow = {
  namespace: "media",
  name: "nightly-1",
  phase: "succeeded",
  origin: "scheduled",
  policy: "nightly",
  repository: "media/nas",
  kopiaSnapshotId: "k-1",
  identity: "kopiur@media:/data",
  startTime: "2026-09-09T01:00:00Z",
  endTime: "2026-09-09T01:04:00Z",
  sizeBytes: 1024,
  bytesNew: null,
  filesTotal: 12,
  filesFailed: null,
  pinned: false,
  deletionPolicy: "Delete",
  copiedFrom: null,
};

describe("snapshotPhaseLamp", () => {
  it("gives each phase a lamp and the phase's own word", () => {
    expect(snapshotPhaseLamp("succeeded")).toMatchObject({ key: "healthy", word: "Succeeded" });
    expect(snapshotPhaseLamp("failed")).toMatchObject({ key: "failed", word: "Failed" });
    expect(snapshotPhaseLamp("running")).toMatchObject({ key: "pending", word: "Running" });
    expect(snapshotPhaseLamp("pending")).toMatchObject({ key: "pending", word: "Pending" });
    expect(snapshotPhaseLamp("deleting")).toMatchObject({ key: "pending", word: "Deleting" });
    expect(snapshotPhaseLamp("discovered")).toMatchObject({ key: "healthy", word: "Discovered" });
    expect(snapshotPhaseLamp("unchanged")).toMatchObject({ key: "healthy", word: "Unchanged" });
  });

  it("renders a phase this build does not know as the operator's own word, never as healthy", () => {
    // The fixture the brief asks for: `Unknown("Weird")` must reach the screen
    // as the raw string, not as a crash and not as a green lamp.
    const lamp = snapshotPhaseLamp({ unknown: { raw: "Weird" } });
    expect(lamp.word).toBe("Weird");
    expect(lamp.key).toBe("unknown");
  });

  it("says the operator has written no phase rather than inventing one", () => {
    expect(snapshotPhaseLamp(null)).toMatchObject({ key: "unknown", word: "Unreconciled" });
    expect(snapshotPhaseLamp(undefined)).toMatchObject({ key: "unknown", word: "Unreconciled" });
  });
});

describe("originLabel", () => {
  it("names each origin", () => {
    expect(originLabel("scheduled")).toBe("Scheduled");
    expect(originLabel("manual")).toBe("Manual");
    expect(originLabel("discovered")).toBe("Discovered");
    expect(originLabel("adopted")).toBe("Adopted");
    expect(originLabel("replicated")).toBe("Replicated");
  });

  it("renders an origin with no fallback variant as the raw string", () => {
    // `OriginView` is unit-only with NO fallback (addenda item 11), so the
    // default arm has to render at run time rather than throw.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    expect(originLabel("teleported" as never)).toBe("teleported");
    warn.mockRestore();
  });

  it("is the empty cell when the operator recorded none", () => {
    expect(originLabel(null)).toBe("-");
  });
});

describe("the filter vocabularies", () => {
  it("spells the phases the server's parse_phase accepts, including unknown", () => {
    expect(PHASE_FILTERS.map((f) => f.value)).toEqual([
      "pending",
      "running",
      "succeeded",
      "failed",
      "deleting",
      "discovered",
      "unchanged",
      "unknown",
    ]);
    expect(isPhaseFilter("succeeded")).toBe(true);
    expect(isPhaseFilter("Succeeded")).toBe(false);
    expect(isPhaseFilter(3)).toBe(false);
  });

  it("spells the origins the server's Origin::parse accepts", () => {
    expect(ORIGIN_FILTERS.map((f) => f.value)).toEqual([
      "scheduled",
      "manual",
      "discovered",
      "adopted",
      "replicated",
    ]);
    expect(isOriginFilter("replicated")).toBe(true);
    expect(isOriginFilter("unknown")).toBe(false);
  });
});

describe("deletionPolicyLabel", () => {
  it("names the three the CRD has", () => {
    expect(deletionPolicyLabel("Delete")).toBe("Delete");
    expect(deletionPolicyLabel("Retain")).toBe("Retain");
    expect(deletionPolicyLabel("Orphan")).toBe("Orphan");
  });

  it("never renders an absent policy as a default (addenda item 19)", () => {
    expect(deletionPolicyLabel(null)).toBe("not set (the operator decides)");
    expect(deletionPolicyLabel(undefined)).toBe("not set (the operator decides)");
    expect(deletionPolicyLabel(null)).not.toContain("Delete");
    expect(deletionPolicyLabel(null)).not.toContain("Retain");
  });

  it("prints a policy this build does not know as the CR's own word", () => {
    expect(deletionPolicyLabel("Shred")).toBe("Shred");
  });
});

describe("deletionConsequence", () => {
  it("says the restore point is destroyed under Delete", () => {
    const consequence = deletionConsequence("Delete");
    expect(consequence.known).toBe(true);
    expect(consequence.destroys).toBe(true);
    expect(consequence.text).toMatch(/deleted from the repository/);
  });

  it("says the kopia snapshot survives under Retain and Orphan", () => {
    for (const policy of ["Retain", "Orphan"]) {
      const consequence = deletionConsequence(policy);
      expect(consequence.destroys).toBe(false);
      expect(consequence.text).toMatch(/stays in the repository/);
    }
  });

  it("refuses to guess when the CR sets none, and says which way it could go", () => {
    const consequence = deletionConsequence(null);
    expect(consequence.known).toBe(false);
    // Not a claim in either direction: it names both outcomes and says the
    // operator, not this console, decides which applies.
    expect(consequence.text).toMatch(/produced/);
    expect(consequence.text).toMatch(/discovered/);
    expect(consequence.text).toMatch(/cannot tell/);
  });

  it("refuses to guess at a policy it does not recognise", () => {
    const consequence = deletionConsequence("Shred");
    expect(consequence.known).toBe(false);
    expect(consequence.text).toContain("Shred");
  });
});

describe("snapshotVerdict", () => {
  it("leads with the phase's lamp and names what the snapshot is", () => {
    const verdict = snapshotVerdict(row);
    expect(verdict.lamp.key).toBe("healthy");
    expect(verdict.text).toContain("Succeeded");
    expect(verdict.text).toContain("nightly");
  });

  it("says a pinned snapshot is exempt from pruning", () => {
    expect(snapshotVerdict({ ...row, pinned: true }).text).toMatch(/pinned/i);
  });

  it("does not claim a policy when the snapshot names none", () => {
    const verdict = snapshotVerdict({ ...row, policy: null, origin: "discovered" });
    expect(verdict.text).toMatch(/no SnapshotPolicy/);
  });

  it("never goes green on a phase it cannot interpret", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const verdict = snapshotVerdict({ ...row, phase: { unknown: { raw: "Weird" } } });
    expect(verdict.lamp.key).toBe("unknown");
    expect(verdict.text).toContain("Weird");
    warn.mockRestore();
  });
});
