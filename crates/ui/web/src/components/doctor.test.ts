import { describe, expect, it, vi } from "vitest";

import type { DoctorCheckView } from "../api/types";
import {
  DOCTOR_DEFAULTS,
  doctorCheckScope,
  doctorOutcomeLamp,
  isRbacDegraded,
  summarizeDoctor,
} from "./doctor";

const check = (over: Partial<DoctorCheckView>): DoctorCheckView => ({
  check: "crds-installed",
  title: "CRDs installed",
  outcome: "Pass",
  // `installation` because that is what the CRD check actually reads; the
  // server now states each check's scope rather than leaving the client to
  // guess it. Spread last so a case can override it.
  scope: "installation",
  ...over,
});

describe("doctorOutcomeLamp", () => {
  it("maps the three outcomes onto three distinct lamps, each with a word", () => {
    expect(doctorOutcomeLamp("Pass")).toMatchObject({ key: "healthy", word: "Pass" });
    expect(doctorOutcomeLamp("Warn")).toMatchObject({ key: "degraded", word: "Warn" });
    expect(doctorOutcomeLamp("Fail")).toMatchObject({ key: "failed", word: "Fail" });
    const icons = new Set(["Pass", "Warn", "Fail"].map((o) => doctorOutcomeLamp(o).icon));
    expect(icons.size).toBe(3);
  });

  it("renders an outcome this bundle has never seen as unknown, never as a pass", () => {
    // `outcome` is a String on the wire (ui-model doc: "stringly-typed on
    // purpose"), so a default arm must not assume OK.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    expect(doctorOutcomeLamp("Skipped")).toMatchObject({ key: "unknown", word: "Skipped" });
    expect(doctorOutcomeLamp("")).toMatchObject({ key: "unknown" });
    expect(doctorOutcomeLamp("pass")).toMatchObject({ key: "unknown", word: "pass" });
    warn.mockRestore();
  });
});

describe("doctorCheckScope", () => {
  it("knows which checks a namespace narrows and which stay installation-wide", () => {
    // The split is `crates/ui/src/api/doctor.rs::doctor_ctx`'s, not a guess.
    for (const id of [
      "crds-installed",
      "controller-running",
      "webhook-running",
      "webhook-admits",
    ]) {
      expect(doctorCheckScope(id), id).toBe("installation");
    }
    for (const id of [
      "repositories-ready",
      "credentials-present",
      "snapshot-replications",
      "no-stuck-work",
      "recent-failures",
      "recent-warnings",
    ]) {
      expect(doctorCheckScope(id), id).toBe("namespace");
    }
  });

  it("does not guess the scope of a check it has never heard of", () => {
    expect(doctorCheckScope("quantum-parity")).toBe("unknown");
  });
});

describe("isRbacDegraded", () => {
  it("recognises a warning that doctor degraded for a missing grant", () => {
    expect(
      isRbacDegraded(
        check({
          outcome: "Warn",
          what: "cannot list secrets (RBAC); grant `list` on `secrets` or run with a more privileged kubeconfig to enable this check",
        }),
      ),
    ).toBe(true);
    expect(
      isRbacDegraded(
        check({
          outcome: "Warn",
          what: "cannot dry-run create snapshotpolicies (RBAC); grant `create` (dryRun) to enable this check",
        }),
      ),
    ).toBe(true);
  });

  it("leaves an ordinary warning and every failure alone", () => {
    expect(isRbacDegraded(check({ outcome: "Warn", what: "skipped if not installed" }))).toBe(
      false,
    );
    expect(isRbacDegraded(check({ outcome: "Fail", what: "cannot list secrets (RBAC)" }))).toBe(
      false,
    );
    expect(isRbacDegraded(check({ outcome: "Pass" }))).toBe(false);
  });
});

describe("summarizeDoctor", () => {
  it("counts by outcome from the checks, not from the exit code", () => {
    const summary = summarizeDoctor([
      check({ outcome: "Pass" }),
      check({ outcome: "Warn", what: "x" }),
      check({ outcome: "Fail", what: "a", why: "b", fix: "c" }),
      check({ outcome: "Fail", what: "d", why: "e", fix: "f" }),
      check({ outcome: "Weird" }),
    ]);
    expect(summary).toEqual({ pass: 1, warn: 1, rbac: 0, fail: 2, other: 1, total: 5 });
  });

  it("counts a check the viewer may not run as a permission gap, never as a cluster warning", () => {
    // A read-only viewer on a healthy cluster: every check that could run
    // passed, and the two that warned did so only because the console asked
    // as them and was refused. Counting those as `warn` tells the operator
    // their cluster is degraded when what is actually short is their RBAC.
    const summary = summarizeDoctor([
      check({ check: "crds-installed", outcome: "Pass" }),
      check({ check: "controller-running", outcome: "Pass" }),
      check({ check: "repositories-ready", outcome: "Pass" }),
      check({ check: "no-stuck-work", outcome: "Pass" }),
      check({
        check: "webhook-admits",
        outcome: "Warn",
        what: "cannot dry-run create snapshotpolicies (RBAC); grant `create` (dryRun) on `snapshotpolicies` to enable this check",
      }),
      check({
        check: "credentials-present",
        outcome: "Warn",
        what: "cannot list secrets (RBAC); grant `list` on `secrets` or run with a more privileged kubeconfig to enable this check",
      }),
    ]);
    expect(summary).toEqual({ pass: 4, warn: 0, rbac: 2, fail: 0, other: 0, total: 6 });
  });

  it("still counts a warning about the cluster as a warning", () => {
    const summary = summarizeDoctor([
      check({
        check: "recent-warnings",
        outcome: "Warn",
        what: "3 Warning events in the last hour",
      }),
      check({
        check: "credentials-present",
        outcome: "Warn",
        what: "cannot list secrets (RBAC); grant `list` on `secrets`",
      }),
    ]);
    expect(summary).toMatchObject({ warn: 1, rbac: 1, total: 2 });
  });
});

describe("DOCTOR_DEFAULTS", () => {
  it("matches the server's defaults so an empty control means what the server does", () => {
    // `crates/ui/src/api/doctor.rs`: DEFAULT_STUCK_THRESHOLD 1h, DEFAULT_FAILURE_LOOKBACK 24h.
    expect(DOCTOR_DEFAULTS).toEqual({ stuckThreshold: "1h", failureLookback: "24h" });
  });
});
