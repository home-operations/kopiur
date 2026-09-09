import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { DoctorCheckView } from "../api/types";
import { bodyRows, nth } from "../test-utils";
import { DoctorChecks } from "./DoctorChecks";

const checks: DoctorCheckView[] = [
  { check: "crds-installed", scope: "installation", title: "CRDs installed", outcome: "Pass" },
  {
    check: "credentials-present",
    scope: "mixed",
    title: "credential secrets present",
    outcome: "Warn",
    what: "cannot list secrets (RBAC); grant `list` on `secrets` or run with a more privileged kubeconfig to enable this check",
  },
  {
    check: "no-stuck-work",
    scope: "namespace",
    title: "no blocked or stuck work",
    outcome: "Fail",
    what: "Snapshot media/nightly-1 is parked on MoverPermitted=False",
    why: "namespace media has not opted in to privileged movers",
    fix: "annotate namespace media with kopiur.home-operations.com/allow-privileged-mover=true",
  },
  {
    check: "webhook-running",
    scope: "installation",
    title: "webhook running",
    outcome: "Warn",
    what: "skipped if not installed",
  },
  {
    check: "quantum-parity",
    // A scope this bundle has never heard of: `DoctorScopeView` is a closed
    // enum with no fallback variant, so a newer server is the only way this
    // arrives — and it must render as the unread word, never as a guess.
    scope: "galactic" as DoctorCheckView["scope"],
    title: "quantum parity",
    outcome: "Skipped",
  },
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

  it("says what the server says each check read, including the two that are both", () => {
    // The scope is `DoctorCheckView.scope`, not a table kept here. The client
    // used to own that table and it was already wrong: `list_repos` lists
    // `Repository` inside the namespace and `ClusterRepository` cluster-wide,
    // so the repository checks were never purely namespace-scoped and
    // "scoped to media" over them was an overstatement.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    render(<DoctorChecks checks={checks} namespace="media" />);
    const rows = bodyRows(screen.getByRole("table"));
    // Namespace-scoped: narrowed, and only narrowed.
    expect(nth(rows, 2)).toHaveTextContent("scoped to media");
    expect(nth(rows, 2)).not.toHaveTextContent(/cluster-scoped/i);
    // Mixed: narrowed for namespaced objects, and not for cluster-scoped ones.
    expect(nth(rows, 1)).toHaveTextContent("scoped to media");
    expect(nth(rows, 1)).toHaveTextContent(/cluster-scoped/i);
    // Installation-wide: says so, and that the namespace does not apply.
    expect(nth(rows, 0)).toHaveTextContent(/installation-wide/i);
    expect(nth(rows, 3)).toHaveTextContent(/installation-wide/i);
    // A scope this bundle cannot read renders the word, never a guess.
    expect(nth(rows, 4)).toHaveTextContent(/scope not known/i);
    expect(nth(rows, 4)).toHaveTextContent("galactic");
    warn.mockRestore();
  });

  it("does not claim a namespace narrowed anything when the run had none", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    render(<DoctorChecks checks={checks} namespace={undefined} />);
    const rows = bodyRows(screen.getByRole("table"));
    expect(nth(rows, 2)).toHaveTextContent(/every namespace/i);
    expect(nth(rows, 1)).toHaveTextContent(/every namespace/i);
    expect(nth(rows, 1)).toHaveTextContent(/cluster-scoped/i);
    expect(nth(rows, 0)).toHaveTextContent(/installation-wide/i);
    expect(nth(rows, 0)).not.toHaveTextContent(/does not apply/i);
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
