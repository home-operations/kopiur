import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { problemBanner } from "../api/problem";
import type {
  DoctorReportView,
  MaintenanceRow,
  OverviewView,
  RepositorySummary,
  StatusOverview,
} from "../api/types";
import {
  bodyRows,
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  nth,
  problemResponse,
} from "../test-utils";

const NOW = "2026-09-08T12:00:00Z";

const repo = (
  name: string,
  health: RepositorySummary["health"],
  over: Partial<RepositorySummary> = {},
): RepositorySummary => ({
  kind: "Repository",
  kindPath: "repository",
  name,
  namespace: "media",
  health,
  mode: "ReadWrite",
  serverBacked: false,
  suspended: false,
  ...over,
});

const repositories: RepositorySummary[] = [
  repo("nas", "healthy"),
  repo("offsite", "healthy"),
  repo("cold", "failed"),
  repo("lab", "suspended", { suspended: true }),
];

const status: StatusOverview = {
  now: NOW,
  report: {
    repositories: [
      { kind: "Repository", name: "nas", namespace: "media", phase: "Ready", suspended: false },
      {
        kind: "Repository",
        name: "cold",
        namespace: "media",
        phase: "Failed",
        suspended: false,
        problem: "password Secret cold-pw not found",
      },
    ],
    policies: [{ name: "nightly", namespace: "media", repository: "Repository/nas" }],
    schedules: [],
    snapshotReplications: [],
    inFlight: { snapshots: 2, restores: 1 },
    stalled: [
      {
        kind: "Snapshot",
        object: "media/nightly-1",
        message: "MoverPermitted=False: namespace media has not opted in to privileged movers",
      },
    ],
  },
};

const doctor: DoctorReportView = {
  ranAt: NOW,
  exitCode: 1,
  checks: [
    { check: "crds-installed", scope: "installation", title: "CRDs installed", outcome: "Pass" },
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
      check: "recent-warnings",
      scope: "namespace",
      title: "recent warning events",
      outcome: "Warn",
      what: "3 Warning events in the last hour",
    },
  ],
};

const allGood: DoctorReportView = {
  ranAt: NOW,
  exitCode: 0,
  checks: [
    { check: "crds-installed", scope: "installation", title: "CRDs installed", outcome: "Pass" },
  ],
};

beforeEach(() => {
  problemBanner.dismiss();
  vi.useFakeTimers({ shouldAdvanceTime: true, now: new Date(NOW) });
});

afterEach(() => {
  vi.useRealTimers();
});

/** The fleet by kind for {@link repositories}: one failed repository, one failed snapshot. */
const overview: OverviewView = {
  snapshotWindowHours: 24,
  generatedAt: NOW,
  kinds: [
    {
      kind: "repository",
      total: 4,
      byHealth: [
        { health: "failed", count: 1 },
        { health: "suspended", count: 1 },
        { health: "healthy", count: 2 },
      ],
    },
    { kind: "clusterRepository", total: 0, byHealth: [] },
    { kind: "maintenance", total: 1, byHealth: [{ health: "failed", count: 1 }] },
    { kind: "snapshotPolicy", total: 1, byHealth: [{ health: "healthy", count: 1 }] },
    { kind: "snapshotSchedule", total: 0, byHealth: [] },
    {
      kind: "snapshot",
      total: 3,
      byHealth: [
        { health: "failed", count: 1 },
        { health: "healthy", count: 2 },
      ],
    },
    { kind: "restore", total: 0, byHealth: [] },
    { kind: "repositoryReplication", total: 0, byHealth: [] },
    { kind: "snapshotReplication", total: 0, byHealth: [] },
  ],
};

const emptyOverview: OverviewView = {
  ...overview,
  kinds: overview.kinds.map((k) => ({ ...k, total: 0, byHealth: [] })),
};

const failingMaintenance: MaintenanceRow = {
  namespace: "media",
  name: "cold",
  repository: "Repository/media/cold",
  managedByRepository: true,
  quick: { lastRunAt: "2026-09-08T06:00:00Z", consecutiveFailures: 2 },
  full: { lastRunAt: "2026-09-07T03:00:00Z", consecutiveFailures: 0 },
};

/** The reads the overview makes besides the three each test sets: quiet unless overridden. */
function calm(over: Record<string, Response | ((url: URL) => Response)> = {}) {
  return {
    "/api/v1/overview": jsonResponse(overview),
    "/api/v1/policies": jsonResponse([
      {
        namespace: "media",
        name: "nightly",
        repositories: ["Repository/media/nas"],
        multiRepo: false,
        suspended: false,
        lastSuccessfulSnapshot: NOW,
        lastVerified: NOW,
      },
    ]),
    "/api/v1/schedules": jsonResponse([]),
    "/api/v1/maintenance": jsonResponse([]),
    ...over,
  };
}

describe("Overview", () => {
  it("answers whether the data is safe, then shows the fleet by kind and what needs you", async () => {
    mockApi({
      ...calm({ "/api/v1/maintenance": jsonResponse([failingMaintenance]) }),
      "/api/v1/status": jsonResponse(status),
      "/api/v1/repositories": jsonResponse(repositories),
      "/api/v1/doctor": jsonResponse(doctor),
    });
    mountApp("/?namespace=media");

    // The verdict: worst thing first, in one sentence, on a lettered lamp.
    const verdict = await screen.findByRole("status", { name: "Vault verdict" });
    expect(verdict).toHaveTextContent(
      "Needs attention: 1 repository failed, 1 object stalled, 1 doctor check failing, 1 warning.",
    );
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "failed");
    expect(verdict.querySelector("svg")).not.toBeNull();

    // The fleet by kind, from /overview, each tile a link carrying the scope.
    const fleet = screen.getByRole("region", { name: "Fleet by kind" });
    const snapshots = await within(fleet).findByRole("link", { name: /Snapshots/ });
    expect(snapshots).toHaveTextContent("1 failed");
    expect(snapshots).toHaveAttribute("data-failing", "true");
    expect(within(fleet).getByRole("link", { name: /Repositories/ })).toHaveAttribute(
      "href",
      "/repositories?health=failed&namespace=media",
    );

    // What needs you: the failing objects as cards, worst first.
    const attention = screen.getByRole("region", { name: "Needs attention" });
    const cards = await within(attention).findAllByRole("article");
    const kinds = cards.map((c) => c.getAttribute("data-kind")).filter((k) => k !== null);
    expect(kinds).toContain("repository");
    expect(kinds).toContain("maintenance");
    expect(within(attention).getByRole("link", { name: "cold" })).toHaveAttribute(
      "href",
      "/repositories/repository/cold?namespace=media",
    );

    // Stalled objects as work rows.
    const stalled = within(attention).getByRole("table", { name: "Stalled objects" });
    const row = nth(bodyRows(stalled), 0);
    expect(within(row).getByText("Snapshot")).toHaveClass("label-strip__kind");
    expect(within(row).getByText("nightly-1")).toHaveClass("label-strip__name");
    expect(row.querySelector(".health")).toHaveAttribute("data-health", "failed");
    expect(row.querySelector(".health")).toHaveTextContent("Stalled");
    expect(row).toHaveTextContent("has not opted in");

    // The failing doctor checks, with their fix text, and the way to the full report.
    const fixes = within(attention).getByRole("list", { name: "Failing checks" });
    const findings = within(fixes).getAllByRole("article");
    expect(findings).toHaveLength(1);
    expect(nth(findings, 0)).toHaveAccessibleName("no blocked or stuck work");
    expect(nth(findings, 0).querySelector(".finding__fix")).toHaveTextContent(
      "annotate namespace media",
    );
    expect(within(attention).getByRole("link", { name: /full doctor report/ })).toHaveAttribute(
      "href",
      "/doctor?namespace=media",
    );

    // Every read carried the namespace.
    const paths = calledPaths();
    expect(paths).toContain("/api/v1/status?namespace=media");
    expect(paths).toContain("/api/v1/repositories?namespace=media");
    expect(paths).toContain("/api/v1/overview?namespace=media");

    // The landing page asks for a subset, not the whole suite: running it all
    // here meant a dryRun create through the admission chain, a Secret read
    // per credential reference across the fleet and a cluster-wide Events
    // list, on every visit and again whenever the tab regained focus.
    const doctorPath = nth(
      paths.filter((p) => p.startsWith("/api/v1/doctor")),
      0,
    );
    const asked = new URL(doctorPath, "http://localhost").searchParams.get("checks");
    expect(asked).not.toBeNull();
    const askedFor = (asked ?? "").split(",");
    expect(askedFor).toContain("no-stuck-work");
    expect(askedFor).toContain("recent-failures");
    expect(askedFor).toContain("repositories-ready");
    expect(askedFor).not.toContain("webhook-admits");
    expect(askedFor).not.toContain("credentials-present");
    expect(askedFor).not.toContain("recent-warnings");
  });

  it("reads as calm when everything is healthy, and says nothing needs you", async () => {
    mockApi({
      ...calm({ "/api/v1/overview": jsonResponse(emptyOverview) }),
      "/api/v1/status": jsonResponse({
        now: NOW,
        report: {
          ...(status.report as object),
          stalled: [],
          inFlight: { snapshots: 0, restores: 0 },
        },
      }),
      "/api/v1/repositories": jsonResponse([repo("nas", "healthy"), repo("offsite", "healthy")]),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    const verdict = await screen.findByRole("status", { name: "Vault verdict" });
    expect(verdict).toHaveTextContent(
      "All 2 repositories healthy, nothing stalled, no failing checks.",
    );
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "healthy");
    const attention = screen.getByRole("region", { name: "Needs attention" });
    expect(await within(attention).findByRole("status")).toHaveTextContent("Nothing needs you");
    expect(within(attention).queryByRole("article")).toBeNull();
  });

  it("never calls an empty scope healthy", async () => {
    mockApi({
      ...calm({
        "/api/v1/overview": jsonResponse(emptyOverview),
        "/api/v1/policies": jsonResponse([]),
      }),
      "/api/v1/status": jsonResponse({
        now: NOW,
        report: {
          repositories: [],
          policies: [],
          schedules: [],
          snapshotReplications: [],
          inFlight: { snapshots: 0, restores: 0 },
          stalled: [],
        },
      }),
      "/api/v1/repositories": jsonResponse([]),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/?namespace=empty");
    const verdict = await screen.findByRole("status", { name: "Vault verdict" });
    expect(verdict).toHaveTextContent("No repositories in scope");
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
    const fleet = screen.getByRole("region", { name: "Fleet by kind" });
    expect(await within(fleet).findByRole("link", { name: /Repositories/ })).toHaveTextContent(
      "none in scope",
    );
  });

  it("says once, in its place, that the fleet overview was refused — and the verdict cannot be green", async () => {
    mockApi({
      ...calm({
        "/api/v1/overview": problemResponse(
          forbiddenProblem("Listing kopiur objects was refused.", "/api/v1/overview"),
        ),
      }),
      "/api/v1/status": jsonResponse({
        ...status,
        report: { ...(status.report as object), stalled: [] },
      }),
      "/api/v1/repositories": jsonResponse([repo("nas", "healthy")]),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    const verdict = await screen.findByRole("status", { name: "Vault verdict" });
    expect(verdict).toHaveTextContent("did not load");
    expect(verdict.querySelector(".verdict__lamp")).not.toHaveAttribute("data-health", "healthy");
    const fleet = screen.getByRole("region", { name: "Fleet by kind" });
    expect(fleet.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(fleet).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the status report's refusal where the stalled objects would be", async () => {
    mockApi({
      ...calm(),
      "/api/v1/status": problemResponse(
        forbiddenProblem("Listing repositories was refused.", "/api/v1/status"),
      ),
      "/api/v1/repositories": jsonResponse(repositories),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    const verdict = await screen.findByRole("status", { name: "Vault verdict" });
    // What did load still counts — a failed repository outranks a missing report.
    expect(verdict).toHaveTextContent("Needs attention");
    expect(verdict).toHaveTextContent("The status report did not load.");
    const attention = screen.getByRole("region", { name: "Needs attention" });
    expect(attention.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(attention).getByRole("alert")).toHaveTextContent("kopiur-ui-viewer");
    expect(within(attention).queryByRole("button", { name: "Retry" })).toBeNull();
    expect(screen.getAllByRole("alert")).toHaveLength(1);
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      ...calm(),
      "/api/v1/status": jsonResponse(status),
      "/api/v1/repositories": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    const attention = await screen.findByRole("region", { name: "Needs attention" });
    expect(await within(attention).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(attention).getByRole("button", { name: "Retry" })).toBeInTheDocument();
    const verdict = screen.getByRole("status", { name: "Vault verdict" });
    expect(verdict).toHaveTextContent("did not load");
  });

  it("shows skeletons, not spinners, while the answers are on their way", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/");
    const busy = await screen.findAllByRole("status", { busy: true });
    expect(busy.length).toBeGreaterThanOrEqual(2);
    expect(document.querySelector(".spinner")).toBeNull();
    expect(screen.getByRole("status", { name: "Vault verdict" })).toHaveTextContent(
      "Checking the vault",
    );
  });

  it("tells a read-only viewer their permissions blocked two checks, not that the cluster is degraded", async () => {
    // Every check that could run passed; the two that warned did so because
    // the console asked the cluster as this user and was refused.
    mockApi({
      ...calm(),
      "/api/v1/status": jsonResponse({
        now: NOW,
        report: {
          ...(status.report as object),
          stalled: [],
          inFlight: { snapshots: 0, restores: 0 },
        },
      }),
      "/api/v1/repositories": jsonResponse([repo("nas", "healthy")]),
      "/api/v1/doctor": jsonResponse({
        ranAt: NOW,
        exitCode: 0,
        checks: [
          {
            check: "crds-installed",
            scope: "installation",
            title: "CRDs installed",
            outcome: "Pass",
          },
          {
            check: "repositories-ready",
            scope: "mixed",
            title: "repositories ready",
            outcome: "Pass",
          },
          {
            check: "webhook-admits",
            scope: "installation",
            title: "webhook admits",
            outcome: "Warn",
            what: "cannot dry-run create snapshotpolicies (RBAC); grant `create` (dryRun) to enable this check",
          },
          {
            check: "credentials-present",
            scope: "mixed",
            title: "credential secrets present",
            outcome: "Warn",
            what: "cannot list secrets (RBAC); grant `list` on `secrets` to enable this check",
          },
        ],
      } satisfies DoctorReportView),
    });
    mountApp("/");
    const verdict = await screen.findByRole("status", { name: "Vault verdict" });
    expect(verdict).toHaveTextContent(
      "Cannot fully check: 2 doctor checks could not run with your permissions.",
    );
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
    expect(verdict).not.toHaveTextContent(/warning/);
  });

  it("survives a report this bundle cannot read and says it was incomplete", async () => {
    mockApi({
      ...calm(),
      "/api/v1/status": jsonResponse({ now: NOW, report: "not-the-report" }),
      "/api/v1/repositories": jsonResponse(repositories),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    await screen.findByRole("status", { name: "Vault verdict" });
    expect(screen.getByText(/could not read part of the status report/)).toBeInTheDocument();
    // The fleet by kind does not depend on the report.
    const fleet = screen.getByRole("region", { name: "Fleet by kind" });
    expect(await within(fleet).findByRole("link", { name: /Snapshots/ })).toBeInTheDocument();
  });
});
