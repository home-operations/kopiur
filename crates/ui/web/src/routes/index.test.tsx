import { screen, waitFor, within } from "@testing-library/react";
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
    {
      check: "crds-installed",
      scope: "installation",
      title: "CRDs installed",
      outcome: "Pass",
      objects: [],
    },
    {
      check: "no-stuck-work",
      scope: "namespace",
      title: "no blocked or stuck work",
      outcome: "Fail",
      what: "Snapshot media/nightly-1 is parked on MoverPermitted=False",
      why: "namespace media has not opted in to privileged movers",
      fix: "run the command in the condition message above",
      objects: [
        {
          kind: "snapshot",
          namespace: "media",
          name: "nightly-1",
          failing: true,
          message:
            "blocked on MoverPermitted=False: namespace media has not opted in. Fix: annotate namespace media with kopiur.home-operations.com/allow-privileged-mover=true",
          fix: "run the command in the condition message above",
        },
      ],
    },
    {
      check: "recent-warnings",
      objects: [],
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
    {
      check: "crds-installed",
      scope: "installation",
      title: "CRDs installed",
      outcome: "Pass",
      objects: [],
    },
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
    "/api/v1/snapshots": jsonResponse({ items: [], total: 0, offset: 0, limit: 25 }),
    "/api/v1/restores": jsonResponse([]),
    "/api/v1/replications": jsonResponse({ repository: [], snapshot: [] }),
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
      "Needs attention: 1 repository failed, 1 maintenance failed, 1 snapshot failed, 1 object stalled, 1 doctor check failing, 1 warning.",
    );
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "failed");
    expect(verdict.querySelector("svg")).not.toBeNull();

    // The fleet by kind lives in the sidebar now: no section of its own here.
    expect(screen.queryByRole("region", { name: "Fleet by kind" })).toBeNull();
    const nav = screen.getByRole("navigation", { name: "Primary" });
    expect(
      await within(nav).findByRole("link", { name: "Snapshots", description: /1 failed/ }),
    ).toBeInTheDocument();
    expect(within(nav).getByRole("link", { name: "Repositories" })).toHaveAttribute(
      "href",
      "/repositories?health=failed&namespace=media",
    );

    // Below the verdict, two cards side by side with a handle between them:
    // what needs you first, then what has run.
    const split = screen.getByRole("group", { name: "Needs attention and recent activity" });
    const [first, second] = within(split).getAllByRole("region");
    expect(first).toHaveAccessibleName("Needs attention");
    expect(second).toHaveAccessibleName("Recent activity");
    expect(
      within(split).getByRole("separator", { name: "Resize the two columns" }),
    ).toBeInTheDocument();

    // What needs you: one row per object, worst first, whichever read
    // noticed it — the blocked snapshot is stalled AND named by the doctor,
    // and is shown once, with the doctor's account and the operator's fix.
    const attention = screen.getByRole("region", { name: "Needs attention" });
    const list = await within(attention).findByRole("list", { name: "Objects needing attention" });
    const rows = within(list).getAllByRole("listitem");
    expect(rows.map((r) => r.getAttribute("data-kind"))).toEqual([
      "snapshot",
      "repository",
      "maintenance",
    ]);
    const blocked = nth(rows, 0);
    expect(within(blocked).getByRole("link", { name: "nightly-1" })).toHaveAttribute(
      "href",
      expect.stringMatching(/inspect=snapshot%2Fmedia%2Fnightly-1$/),
    );
    // The whole row is the link: every row's name is the one stretched over it.
    for (const row of rows) {
      expect(row.querySelectorAll("a.row-link")).toHaveLength(1);
    }
    expect(blocked.querySelector(".health")).toHaveTextContent("Stuck");
    expect(blocked.querySelector(".attention-row__what")).toHaveTextContent(
      "blocked on MoverPermitted=False",
    );
    expect(blocked.querySelector(".finding__fix")).toHaveTextContent("annotate namespace media");
    expect(within(attention).getAllByText("nightly-1")).toHaveLength(1);
    expect(nth(rows, 1).querySelector(".health")).toHaveTextContent("Failed");
    expect(nth(rows, 2)).toHaveTextContent("2 quick maintenance runs in a row failed.");

    // No second and third style beside it: no stalled table, no check cards.
    expect(within(attention).queryByRole("table")).toBeNull();
    expect(within(attention).queryByRole("article")).toBeNull();
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

  it("lists every run, newest first, beside what needs you — and names a read it could not make", async () => {
    mockApi({
      ...calm({
        "/api/v1/snapshots": jsonResponse({
          items: [
            {
              namespace: "media",
              name: "nightly-2",
              phase: "succeeded",
              policy: "nightly",
              repository: "Repository/media/nas",
              startTime: "2026-09-08T11:00:00Z",
              endTime: "2026-09-08T11:00:20Z",
              pinned: false,
            },
          ],
          total: 1,
          offset: 0,
          limit: 25,
        }),
        "/api/v1/maintenance": jsonResponse([failingMaintenance]),
        "/api/v1/restores": problemResponse(
          forbiddenProblem("Listing restores was refused.", "/api/v1/restores"),
        ),
      }),
      "/api/v1/status": jsonResponse(status),
      "/api/v1/repositories": jsonResponse(repositories),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/?namespace=media");
    const recent = await screen.findByRole("region", { name: "Recent activity" });
    const runs = await within(recent).findByRole("list", { name: "Recent runs" });
    const rows = within(runs).getAllByRole("listitem");
    expect(rows.map((r) => r.getAttribute("data-kind"))).toEqual([
      "snapshot",
      "maintenance",
      "maintenance",
    ]);
    expect(within(nth(rows, 0)).getByRole("link", { name: "nightly-2" })).toHaveAttribute(
      "href",
      expect.stringMatching(/inspect=snapshot%2Fmedia%2Fnightly-2/),
    );
    expect(nth(rows, 0)).toHaveTextContent("nightly → nas");
    expect(nth(rows, 1)).toHaveTextContent("Quick maintenance of cold");
    expect(nth(rows, 1).querySelector(".health")).toHaveTextContent("Failed");
    expect(recent.querySelector('[data-state="not-permitted"]')).not.toBeNull();
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

  it("is not healthy beside a failed snapshot tile, and does not say nothing needs you", async () => {
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
      "/api/v1/repositories": jsonResponse([repo("nas", "healthy"), repo("offsite", "healthy")]),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    const verdict = await screen.findByRole("status", { name: "Vault verdict" });
    await waitFor(() => {
      expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "failed");
    });
    expect(verdict).toHaveTextContent("1 snapshot failed");
    expect(screen.queryByText("Nothing needs you")).toBeNull();
  });

  it("never says nothing needs you when the policies read was refused, and says it did not load", async () => {
    mockApi({
      ...calm({
        "/api/v1/overview": jsonResponse(emptyOverview),
        "/api/v1/policies": problemResponse(
          forbiddenProblem("Listing policies was refused.", "/api/v1/policies"),
        ),
      }),
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
    const attention = await screen.findByRole("region", { name: "Needs attention" });
    expect(await within(attention).findByText(/Listing policies was refused/)).toBeInTheDocument();
    const verdict = screen.getByRole("status", { name: "Vault verdict" });
    expect(verdict.querySelector(".verdict__lamp")).not.toHaveAttribute("data-health", "healthy");
    expect(verdict).toHaveTextContent("the policies");
    expect(within(attention).queryByText("Nothing needs you")).toBeNull();
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
    const nav = screen.getByRole("navigation", { name: "Primary" });
    expect(
      await within(nav).findByRole("link", { name: "Repositories", description: "none in scope" }),
    ).toBeInTheDocument();
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
    // Said where it matters — under what needs you — and the sidebar draws no
    // counts it could not read.
    const attention = screen.getByRole("region", { name: "Needs attention" });
    expect(attention.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(attention).queryByRole("button", { name: "Retry" })).toBeNull();
    const nav = screen.getByRole("navigation", { name: "Primary" });
    expect(nav.querySelector(".nav-item__count")).toBeNull();
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
      ...calm({ "/api/v1/overview": jsonResponse(emptyOverview) }),
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
            objects: [],
            scope: "installation",
            title: "CRDs installed",
            outcome: "Pass",
          },
          {
            check: "repositories-ready",
            objects: [],
            scope: "mixed",
            title: "repositories ready",
            outcome: "Pass",
          },
          {
            check: "webhook-admits",
            objects: [],
            scope: "installation",
            title: "webhook admits",
            outcome: "Warn",
            what: "cannot dry-run create snapshotpolicies (RBAC); grant `create` (dryRun) to enable this check",
          },
          {
            check: "credentials-present",
            objects: [],
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
    // The sidebar's counts do not depend on the report.
    const nav = screen.getByRole("navigation", { name: "Primary" });
    expect(
      await within(nav).findByRole("link", { name: "Snapshots", description: /1 failed/ }),
    ).toBeInTheDocument();
  });
});
