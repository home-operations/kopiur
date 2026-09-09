import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { problemBanner } from "../api/problem";
import type { DoctorReportView, RepositorySummary, StatusOverview } from "../api/types";
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

describe("Overview", () => {
  it("answers whether the data is safe, then shows the lamps, the work and the fixes", async () => {
    mockApi({
      "/api/v1/status": jsonResponse(status),
      "/api/v1/repositories": jsonResponse(repositories),
      "/api/v1/doctor": jsonResponse(doctor),
    });
    mountApp("/?namespace=media");

    // The verdict: worst thing first, in one sentence, on a lettered lamp.
    const verdict = await screen.findByRole("heading", { level: 2, name: /Needs attention/ });
    expect(verdict).toHaveTextContent(
      "Needs attention: 1 repository failed, 1 object stalled, 1 doctor check failing, 1 warning.",
    );
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "failed");
    expect(verdict.querySelector("svg")).not.toBeNull();

    // The health strip, from /repositories, filtered links carrying the scope.
    const strip = screen.getByRole("navigation", { name: "Repositories by health" });
    expect(within(strip).getByRole("link", { name: "1 failed repository" })).toHaveAttribute(
      "href",
      "/repositories?namespace=media&health=failed",
    );
    expect(within(strip).getByRole("link", { name: "2 healthy repositories" })).toBeInTheDocument();

    // Work in flight, from the status report.
    const inFlight = screen.getByRole("region", { name: "Work in flight" });
    expect(within(inFlight).getByText("2")).toBeInTheDocument();
    expect(within(inFlight).getByRole("link", { name: /snapshots running/ })).toHaveAttribute(
      "href",
      "/snapshots?namespace=media",
    );
    expect(within(inFlight).getByRole("link", { name: /restores? running/ })).toHaveAttribute(
      "href",
      "/restores?namespace=media",
    );

    // Stalled objects as work rows.
    const stalled = screen.getByRole("table", { name: "Stalled objects" });
    const row = nth(bodyRows(stalled), 0);
    expect(within(row).getByText("Snapshot")).toHaveClass("label-strip__kind");
    expect(within(row).getByText("nightly-1")).toHaveClass("label-strip__name");
    expect(row.querySelector(".health")).toHaveAttribute("data-health", "failed");
    expect(row.querySelector(".health")).toHaveTextContent("Stalled");
    expect(row).toHaveTextContent("has not opted in");

    // The fixes: only the failing doctor checks, with their fix text.
    const fixes = screen.getByRole("region", { name: "What needs fixing" });
    const findings = within(fixes).getAllByRole("article");
    expect(findings).toHaveLength(1);
    expect(nth(findings, 0)).toHaveAccessibleName("no blocked or stuck work");
    expect(nth(findings, 0).querySelector(".finding__fix")).toHaveTextContent(
      "annotate namespace media",
    );
    expect(within(fixes).getByRole("link", { name: /full doctor report/ })).toHaveAttribute(
      "href",
      "/doctor?namespace=media",
    );

    // Every read carried the namespace, and /status took nothing else.
    const paths = calledPaths();
    expect(paths).toContain("/api/v1/status?namespace=media");
    expect(paths).toContain("/api/v1/repositories?namespace=media");

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
    // The three heaviest reads doctor makes are none of the overview's business.
    expect(askedFor).not.toContain("webhook-admits");
    expect(askedFor).not.toContain("credentials-present");
    expect(askedFor).not.toContain("recent-warnings");
  });

  it("reads as calm when everything is healthy", async () => {
    mockApi({
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
    const verdict = await screen.findByRole("heading", { level: 2, name: /All 2 repositories/ });
    expect(verdict).toHaveTextContent(
      "All 2 repositories healthy, nothing stalled, no failing checks.",
    );
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "healthy");
    const stalled = screen.getByRole("region", { name: "Stalled objects" });
    expect(within(stalled).getByRole("status")).toHaveTextContent("Nothing is stalled");
    const fixes = screen.getByRole("region", { name: "What needs fixing" });
    expect(within(fixes).getByRole("status")).toHaveTextContent("Nothing to fix");
    expect(calledPaths()).toContain("/api/v1/status");
  });

  it("renders the empty state when the scope has no repositories, and never calls that healthy", async () => {
    mockApi({
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
    const verdict = await screen.findByRole("heading", {
      level: 2,
      name: /No repositories in scope/,
    });
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
    const region = screen.getByRole("region", { name: "Repositories by health" });
    expect(within(region).getByRole("status")).toHaveTextContent("No repositories in empty");
    expect(within(region).getByRole("status")).toHaveTextContent(/Repository|ClusterRepository/);
  });

  it("renders the not-permitted state for a 403 and says the verdict cannot be given", async () => {
    mockApi({
      "/api/v1/status": problemResponse(
        forbiddenProblem("Listing repositories was refused.", "/api/v1/status"),
      ),
      "/api/v1/repositories": jsonResponse(repositories),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    const verdict = await screen.findByRole("heading", { level: 2, name: /Needs attention/ });
    // What did load still counts — a failed repository outranks a missing report.
    expect(verdict).toHaveTextContent("The status report did not load.");
    // Both halves of the pair come from the one report: the refusal is said
    // once, in their place, not twice side by side.
    const region = screen.getByRole("region", { name: "Status report" });
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).getByRole("alert")).toHaveTextContent("kopiur-ui-viewer");
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
    expect(screen.queryByRole("region", { name: "Stalled objects" })).toBeNull();
    expect(screen.getAllByRole("alert")).toHaveLength(1);
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/status": jsonResponse(status),
      "/api/v1/repositories": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    const region = await screen.findByRole("region", { name: "Repositories by health" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
    const verdict = screen.getByRole("heading", { level: 2, name: /Cannot tell|Needs attention/ });
    expect(verdict).toHaveTextContent("did not load");
  });

  it("shows skeletons, not spinners, while the answers are on their way", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/");
    const busy = await screen.findAllByRole("status", { busy: true });
    expect(busy.length).toBeGreaterThanOrEqual(3);
    expect(document.querySelector(".spinner")).toBeNull();
    expect(screen.getByRole("heading", { level: 2, name: /Checking/ })).toHaveTextContent(
      "Checking the vault",
    );
  });

  it("tells a read-only viewer their permissions blocked two checks, not that the cluster is degraded", async () => {
    // Every check that could run passed; the two that warned did so because
    // the console asked the cluster as this user and was refused. The old
    // sentence read "Mostly healthy: 2 doctor checks warning" on a degraded
    // lamp, and the overview shows no warning detail — so there was nowhere
    // on this screen to learn that both were about the viewer's own RBAC.
    mockApi({
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
    const verdict = await screen.findByRole("heading", {
      level: 2,
      name: /Cannot fully check/,
    });
    expect(verdict).toHaveTextContent(
      "Cannot fully check: 2 doctor checks could not run with your permissions.",
    );
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
    expect(verdict).not.toHaveTextContent(/warning/);
  });

  it("survives a report this bundle cannot read and says it was incomplete", async () => {
    mockApi({
      "/api/v1/status": jsonResponse({ now: NOW, report: "not-the-report" }),
      "/api/v1/repositories": jsonResponse(repositories),
      "/api/v1/doctor": jsonResponse(allGood),
    });
    mountApp("/");
    await screen.findByRole("heading", { level: 2, name: /Needs attention/ });
    expect(screen.getByText(/could not read part of the status report/)).toBeInTheDocument();
    // Snapshots, restores, policies, schedules: every count the report did
    // not carry is "unknown", never 0.
    const inFlight = screen.getByRole("region", { name: "Work in flight" });
    expect(within(inFlight).getAllByText("unknown")).toHaveLength(4);
    expect(within(inFlight).queryByText("0")).toBeNull();
  });
});
