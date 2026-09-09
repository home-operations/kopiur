import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { problemBanner } from "../api/problem";
import type { ActionReceipt, Capabilities, Problem, RepositoryDetail } from "../api/types";
import {
  ME,
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../test-utils";

const detail: RepositoryDetail = {
  summary: {
    kind: "Repository",
    kindPath: "repository",
    name: "nas",
    namespace: "media",
    phase: "degraded",
    health: "degraded",
    backend: "S3",
    mode: "ReadWrite",
    serverBacked: true,
    suspended: false,
    snapshotCount: 412,
    totalSizeBytes: 987654321,
    indexBlobCount: 17,
    lastObservedAt: null,
    serverEndpoint: "https://kopia.internal:51515",
    allowedNamespaceCount: null,
  },
  identityCluster: "east",
  catalog: {
    discoveredBackupCount: 12,
    foreignSnapshotCount: 3,
    lastRefreshAt: "2026-09-08T01:00:00Z",
  },
  health: {
    lastProbeAt: "2026-09-08T05:00:00Z",
    lastHealthyAt: "2026-09-07T05:00:00Z",
    consecutiveProbeFailures: 4,
  },
  server: { endpoint: "https://kopia.internal:51515", readOnly: false, authMode: "repository" },
  seed: {
    mode: "Clone",
    source: "ClusterRepository/shared",
    seededAt: "2026-08-01T00:00:00Z",
    snapshotsCopied: 90,
  },
  maintenance: {
    namespace: "media",
    name: "nas-maintenance",
    repository: "Repository/media/nas",
    owner: "Repository/media/nas",
    managedByRepository: true,
    quick: {
      lastRunAt: "2026-09-08T04:00:00Z",
      nextScheduledAt: null,
      consecutiveFailures: 0,
      lastContentReclaimedBytes: null,
    },
    full: {
      lastRunAt: "2026-09-01T04:00:00Z",
      nextScheduledAt: null,
      consecutiveFailures: 2,
      lastContentReclaimedBytes: 1048576,
    },
    manualRun: null,
  },
  gates: [
    {
      condition: "MoverPermitted",
      reason: "PrivilegedMoverNotPermitted",
      severity: "error",
      message: "namespace media has not opted in to privileged movers",
    },
  ],
  conditions: [
    {
      type: "Ready",
      status: "False",
      reason: "ProbeFailing",
      message: "the backend refused four consecutive probes",
      lastTransitionTime: "2026-09-08T05:00:00Z",
    },
  ],
  policies: [{ namespace: "media", name: "nightly" }],
  replicationsOut: ["offsite"],
  replicationsIn: [],
  sessions: [
    {
      namespace: "media",
      job: "kopiur-browse-nas-abc",
      pod: null,
      reused: true,
      expiresAt: "2026-09-08T07:00:00Z",
      downloadMaxBytes: 1000,
      manifestMaxBytes: 100,
    },
  ],
};

const clusterDetail: RepositoryDetail = {
  ...detail,
  summary: {
    ...detail.summary,
    kind: "ClusterRepository",
    kindPath: "cluster-repository",
    name: "shared",
    namespace: null,
    allowedNamespaceCount: 3,
  },
  maintenance: null,
  gates: [],
  sessions: [],
};

const receipt: ActionReceipt = {
  kind: "Repository",
  created: [],
  requestedAt: "2026-09-08T06:00:00Z",
  note: "Repository/media/nas in namespace media is now suspended",
};

/** `/me` answering with only these flags true. */
function meWith(allowed: Partial<Capabilities>) {
  return jsonResponse({
    ...ME,
    can: Object.fromEntries(
      Object.keys(ME.can).map((key) => [key, allowed[key as keyof Capabilities] ?? false]),
    ),
  });
}

/** The JSON body the SPA sent to `path`. */
function sentBody(path: string): unknown {
  const call = fetchMock.mock.calls.find(([input]) => input === path);
  if (call === undefined) {
    throw new Error(`no request to ${path}; sent ${calledPaths().join(", ")}`);
  }
  const body = call[1]?.body;
  if (typeof body !== "string") {
    throw new Error(`request to ${path} carried no JSON body`);
  }
  return JSON.parse(body) as unknown;
}

/** The 400 the handler answers when a namespaced repository is asked for without one. */
const namespaceRequired: Problem = {
  type: "urn:kopiur:problem:namespace-required",
  title: "Bad request",
  status: 400,
  detail: "A Repository lookup needs the namespace it lives in.",
  what: "A Repository lookup needs the namespace it lives in.",
  why: "`Repository` is namespaced, so its name alone does not identify one object — two namespaces may each hold a repository called the same thing.",
  fix: "add ?namespace=<namespace> to the request, or ask for a cluster-repository instead",
  instance: "/api/v1/repositories/repository/nas",
};

beforeEach(() => {
  problemBanner.dismiss();
});

describe("Repository detail", () => {
  it("reads with the kindPath segment and the repository's own namespace", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp("/repositories/repository/nas?namespace=media");
    await screen.findByRole("status", { name: "Repository verdict" });
    expect(calledPaths()).toContain("/api/v1/repositories/repository/nas?namespace=media");
    // The capability review is namespace-scoped, so it must be asked for the
    // namespace the object lives in (addenda item 17).
    expect(calledPaths()).toContain("/api/v1/me?namespace=media");
  });

  it("answers 'is this repository healthy, and why not' before anything else", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp("/repositories/repository/nas?namespace=media");
    const verdict = await screen.findByRole("status", { name: "Repository verdict" });
    expect(verdict).toHaveTextContent("Degraded");
    expect(verdict).toHaveTextContent("repository server");
    const gates = screen.getByRole("region", { name: "Gates holding this repository" });
    expect(gates).toHaveTextContent("PrivilegedMoverNotPermitted");
    expect(gates).toHaveTextContent("has not opted in to privileged movers");
  });

  it("shows the catalog, probe, seed, policies and conditions the server reported", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp("/repositories/repository/nas?namespace=media");
    await screen.findByRole("status", { name: "Repository verdict" });
    expect(screen.getByRole("region", { name: "Catalog" })).toHaveTextContent("12");
    expect(screen.getByRole("region", { name: "Health probe" })).toHaveTextContent("4");
    expect(screen.getByRole("region", { name: "Seed" })).toHaveTextContent(
      "ClusterRepository/shared",
    );
    expect(screen.getByRole("region", { name: "Policies writing here" })).toHaveTextContent(
      "nightly",
    );
    expect(screen.getByRole("table", { name: "Conditions" })).toHaveTextContent("ProbeFailing");
  });

  it("renders the never-written fields as 'not reported' rather than blank or zero", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp("/repositories/repository/nas?namespace=media");
    const storage = await screen.findByRole("region", { name: "Storage" });
    expect(within(storage).getByText("not reported")).toBeInTheDocument();
    // Both maintenance tracks' next run, likewise.
    const maintenance = screen.getByRole("region", { name: "Maintenance" });
    expect(within(maintenance).getAllByText("not reported")).toHaveLength(2);
  });

  it("surfaces the server's 400 when a namespaced repository is opened without one", async () => {
    // The route deliberately does not short-circuit: the handler's own
    // sentence names the field to add, which is better than anything the SPA
    // could invent, and a blank page would say nothing at all.
    mockApi({
      "/api/v1/repositories/repository/nas": problemResponse(namespaceRequired),
    });
    mountApp("/repositories/repository/nas");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("needs the namespace it lives in");
    expect(alert).toHaveTextContent("add ?namespace=");
    expect(screen.getByRole("link", { name: "All repositories" })).toHaveAttribute(
      "href",
      "/repositories",
    );
  });

  it("asks about a ClusterRepository with no namespace at all", async () => {
    mockApi({ "/api/v1/repositories/cluster-repository/shared": jsonResponse(clusterDetail) });
    mountApp("/repositories/cluster-repository/shared");
    await screen.findByRole("status", { name: "Repository verdict" });
    expect(calledPaths()).toContain("/api/v1/repositories/cluster-repository/shared");
    // `patchClusterRepositories` is reviewed cluster-scoped whatever namespace
    // is asked, so the review must carry none.
    expect(calledPaths()).toContain("/api/v1/me");
  });

  it("disables an action the caller may not perform and says why, rather than hiding it", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/me": meWith({}),
    });
    mountApp("/repositories/repository/nas?namespace=media");
    const suspend = await screen.findByRole("button", { name: /Suspend/ });
    expect(suspend).toHaveAttribute("aria-disabled", "true");
    expect(suspend).toHaveAttribute(
      "data-reason",
      expect.stringContaining("is not permitted for alice in namespace media"),
    );
    expect(screen.getByRole("button", { name: /Scan catalog/ })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  it("suspends with the kindPath as the kind, and renders the receipt's note", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/actions/suspend": jsonResponse(receipt),
    });
    mountApp("/repositories/repository/nas?namespace=media");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /Suspend/ }));
    // The confirmation says what suspending does before it is done.
    const confirm = screen.getByRole("group", { name: "Suspend" });
    expect(confirm).toHaveTextContent("No new backup will be written here");
    await user.click(within(confirm).getByRole("button", { name: "Suspend this repository" }));

    expect(await screen.findByText(/is now suspended/)).toBeInTheDocument();
    expect(sentBody("/api/v1/actions/suspend")).toEqual({
      kind: "repository",
      name: "nas",
      suspend: true,
      namespace: "media",
    });
  });

  it("sends the namespace a Repository catalog scan requires", async () => {
    // `ScanCatalogBody` needs `namespace` for a Repository — without it the
    // handler answers 400 (addenda item 23).
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/actions/scan-catalog": jsonResponse({ ...receipt, note: null }),
    });
    mountApp("/repositories/repository/nas?namespace=media");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /Scan catalog/ }));
    const confirm = screen.getByRole("group", { name: "Scan catalog" });
    await user.click(within(confirm).getByRole("button", { name: "Request a scan" }));
    expect(await screen.findByText(/Scan catalog requested/)).toBeInTheDocument();
    expect(sentBody("/api/v1/actions/scan-catalog")).toEqual({
      kind: "repository",
      name: "nas",
      namespace: "media",
    });
  });

  it("says why a maintenance run cannot be asked for when nothing governs the repository", async () => {
    mockApi({ "/api/v1/repositories/cluster-repository/shared": jsonResponse(clusterDetail) });
    mountApp("/repositories/cluster-repository/shared");
    const run = await screen.findByRole("button", { name: /Run maintenance/ });
    expect(run).toHaveAttribute("aria-disabled", "true");
    expect(run).toHaveAttribute(
      "data-reason",
      expect.stringContaining("No Maintenance resource governs shared"),
    );
  });

  it("renders a refused action as the problem's what, why and fix", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/actions/suspend": problemResponse(
        forbiddenProblem("Suspending was refused.", "/api/v1/actions/suspend"),
      ),
    });
    mountApp("/repositories/repository/nas?namespace=media");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /Suspend/ }));
    await user.click(screen.getByRole("button", { name: "Suspend this repository" }));
    const alerts = await screen.findAllByRole("alert");
    expect(alerts.map((a) => a.textContent).join(" ")).toContain("Suspending was refused.");
  });

  it("stops a browse session, judged in the namespace the session Job runs in", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/repositories/repository/nas/session": new Response(null, { status: 204 }),
    });
    mountApp("/repositories/repository/nas?namespace=media");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /Stop session/ }));
    // 204 with no body; the client must not try to parse one (addenda item 18).
    expect(
      calledPaths().some((p) => p.startsWith("/api/v1/repositories/repository/nas/session")),
    ).toBe(true);
    expect(calledPaths()).toContain(
      "/api/v1/repositories/repository/nas/session?namespace=media&sessionNamespace=media",
    );
  });

  it("renders the not-permitted state for a refused read", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": problemResponse(
        forbiddenProblem("The repository was refused.", "/api/v1/repositories/repository/nas"),
      ),
    });
    mountApp("/repositories/repository/nas?namespace=media");
    expect(await screen.findByRole("alert")).toHaveTextContent("The repository was refused.");
    expect(document.querySelector('[data-state="not-permitted"]')).not.toBeNull();
  });

  it("shows a skeleton while the repository loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/repositories/repository/nas?namespace=media");
    expect(await screen.findByRole("status", { busy: true })).toBeInTheDocument();
  });
});
