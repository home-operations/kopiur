import type { ReactElement } from "react";
import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { DoctorCheckView } from "../api/types";
import { bodyRows, nth, renderWithRouter } from "../test-utils";
import { DoctorChecks } from "./DoctorChecks";

/** The table sorts through the address bar, so it renders inside a router. */
async function render(element: ReactElement) {
  renderWithRouter(element);
  await screen.findByRole("table");
}

const checks: DoctorCheckView[] = [
  {
    check: "crds-installed",
    scope: "installation",
    title: "CRDs installed",
    outcome: "Pass",
    objects: [],
  },
  {
    check: "credentials-present",
    objects: [],
    scope: "mixed",
    title: "credential secrets present",
    outcome: "Warn",
    what: "cannot list secrets (RBAC); grant `list` on `secrets` or run with a more privileged kubeconfig to enable this check",
  },
  {
    check: "no-stuck-work",
    objects: [],
    scope: "namespace",
    title: "no blocked or stuck work",
    outcome: "Fail",
    what: "Snapshot media/nightly-1 is parked on MoverPermitted=False",
    why: "namespace media has not opted in to privileged movers",
    fix: "annotate namespace media with kopiur.home-operations.com/allow-privileged-mover=true",
  },
  {
    check: "webhook-running",
    objects: [],
    scope: "installation",
    title: "webhook running",
    outcome: "Warn",
    what: "skipped if not installed",
  },
  {
    check: "quantum-parity",
    objects: [],
    // A scope this bundle has never heard of: `DoctorScopeView` is a closed
    // enum with no fallback variant, so a newer server is the only way this
    // arrives — and it must render as the unread word, never as a guess.
    scope: "galactic" as DoctorCheckView["scope"],
    title: "quantum parity",
    outcome: "Skipped",
  },
];

describe("DoctorChecks", () => {
  it("renders one row per check with its outcome lamp, and only a failure carries what/why/fix", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    await render(<DoctorChecks checks={checks} namespace={undefined} />);
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

  it("names each check as `id: title` on one line (#695)", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    await render(<DoctorChecks checks={checks} namespace={undefined} />);
    const rows = bodyRows(screen.getByRole("table"));
    const cell = nth(rows, 2).querySelector(".doctor-checks__check");
    expect(cell).not.toBeNull();
    // One element, read in one breath: the id, a separator, the title.
    expect(cell?.textContent).toBe("no-stuck-work: no blocked or stuck work");
    expect(cell?.firstElementChild).toHaveClass("doctor-checks__id");
    warn.mockRestore();
  });

  it("never renders a finding on a passing check, even if text rode along", async () => {
    // A pass has nothing to say. #695: the console printed a long sentence
    // under a check the CLI reported `ok`; whatever a server sends with a
    // Pass, the row must not read as a finding.
    const chatty: DoctorCheckView[] = [
      {
        check: "recent-failures",
        objects: [],
        scope: "namespace",
        title: "no recent failed snapshots/restores",
        outcome: "Pass",
        what: "131 failed object(s) retained as history",
      },
    ];
    await render(<DoctorChecks checks={chatty} namespace={undefined} />);
    const row = nth(bodyRows(screen.getByRole("table")), 0);
    expect(row).toHaveTextContent("recent-failures: no recent failed snapshots/restores");
    expect(row).not.toHaveTextContent("retained as history");
    expect(row.querySelector(".finding")).toBeNull();
  });

  it("says what the server says each check read, including the two that are both", async () => {
    // The scope is `DoctorCheckView.scope`, not a table kept here. The client
    // used to own that table and it was already wrong: `list_repos` lists
    // `Repository` inside the namespace and `ClusterRepository` cluster-wide,
    // so the repository checks were never purely namespace-scoped and
    // "scoped to media" over them was an overstatement.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    await render(<DoctorChecks checks={checks} namespace="media" />);
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

  it("does not claim a namespace narrowed anything when the run had none", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    await render(<DoctorChecks checks={checks} namespace={undefined} />);
    const rows = bodyRows(screen.getByRole("table"));
    expect(nth(rows, 2)).toHaveTextContent(/every namespace/i);
    expect(nth(rows, 1)).toHaveTextContent(/every namespace/i);
    expect(nth(rows, 1)).toHaveTextContent(/cluster-scoped/i);
    expect(nth(rows, 0)).toHaveTextContent(/installation-wide/i);
    expect(nth(rows, 0)).not.toHaveTextContent(/does not apply/i);
    warn.mockRestore();
  });

  it("explains an RBAC-degraded check as a missing grant for the signed-in user", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    await render(<DoctorChecks checks={checks} namespace={undefined} />);
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
