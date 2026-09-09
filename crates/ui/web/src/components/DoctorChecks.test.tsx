import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { DoctorCheckView } from "../api/types";
import { bodyRows, nth } from "../test-utils";
import { DoctorChecks } from "./DoctorChecks";

const checks: DoctorCheckView[] = [
  { check: "crds-installed", title: "CRDs installed", outcome: "Pass" },
  {
    check: "credentials-present",
    title: "credential secrets present",
    outcome: "Warn",
    what: "cannot list secrets (RBAC); grant `list` on `secrets` or run with a more privileged kubeconfig to enable this check",
  },
  {
    check: "no-stuck-work",
    title: "no blocked or stuck work",
    outcome: "Fail",
    what: "Snapshot media/nightly-1 is parked on MoverPermitted=False",
    why: "namespace media has not opted in to privileged movers",
    fix: "annotate namespace media with kopiur.home-operations.com/allow-privileged-mover=true",
  },
  {
    check: "webhook-running",
    title: "webhook running",
    outcome: "Warn",
    what: "skipped if not installed",
  },
  { check: "quantum-parity", title: "quantum parity", outcome: "Skipped" },
];

describe("DoctorChecks", () => {
  it("renders one row per check with its outcome lamp, and only a failure carries what/why/fix", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    render(<DoctorChecks checks={checks} namespace={undefined} />);
    const table = screen.getByRole("table", { name: "Doctor checks" });
    const rows = bodyRows(table);
    expect(rows).toHaveLength(5);

    const pass = nth(rows, 0);
    expect(pass.querySelector(".health")).toHaveAttribute("data-health", "healthy");
    expect(pass.querySelector(".health")).toHaveTextContent("Pass");
    expect(pass).toHaveTextContent("CRDs installed");
    expect(pass).toHaveTextContent("crds-installed");

    const fail = nth(rows, 2);
    expect(fail.querySelector(".health")).toHaveAttribute("data-health", "failed");
    expect(fail).toHaveTextContent("Snapshot media/nightly-1 is parked");
    expect(fail).toHaveTextContent("has not opted in");
    const fix = fail.querySelector(".finding__fix");
    expect(fix).toHaveTextContent("Fix");
    expect(fix).toHaveTextContent("annotate namespace media");
    // A warning is one sentence and has no fix plate to invent.
    expect(nth(rows, 1).querySelector(".finding__fix")).toBeNull();

    const unread = nth(rows, 4);
    expect(unread.querySelector(".health")).toHaveAttribute("data-health", "unknown");
    expect(unread.querySelector(".health")).toHaveTextContent("Skipped");
    warn.mockRestore();
  });

  it("says which checks a namespace narrows and which stay installation-wide", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    render(<DoctorChecks checks={checks} namespace="media" />);
    const rows = bodyRows(screen.getByRole("table"));
    // Namespaced check: scoped to media.
    expect(rows[1]).toHaveTextContent("media");
    // Installation-wide check: says so, and that the namespace does not apply.
    expect(rows[0]).toHaveTextContent(/installation-wide/i);
    expect(rows[3]).toHaveTextContent(/installation-wide/i);
    // Unknown check: does not guess.
    expect(rows[4]).toHaveTextContent(/scope not known/i);
    warn.mockRestore();
  });

  it("explains an RBAC-degraded check as a missing grant for the signed-in user", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    render(<DoctorChecks checks={checks} namespace={undefined} />);
    const rows = bodyRows(screen.getByRole("table"));
    const degraded = nth(rows, 1);
    expect(degraded).toHaveAttribute("data-degraded", "rbac");
    expect(degraded).toHaveTextContent(/not permitted/i);
    expect(degraded).toHaveTextContent("grant `list` on `secrets`");
    // The other warning is not about RBAC and gets no such note.
    expect(rows[3]).not.toHaveAttribute("data-degraded");
    warn.mockRestore();
  });
});
