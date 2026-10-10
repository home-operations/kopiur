import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { problemBanner } from "../../../api/problem";
import type { ActionReceipt, Capabilities, RepositoryDetail } from "../../../api/types";
import {
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  ME,
  mockApi,
  mountApp,
  notifications,
  problemResponse,
} from "../../../test-utils";

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
    admits: null,
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
  schedules: [],
  replicationsOut: [{ kind: "snapshotReplication", namespace: "media", name: "offsite" }],
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
    admits: "all",
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

beforeEach(() => {
  problemBanner.dismiss();
});

/** A page with nothing of its own to confuse with the drawer's. */
const NAS = "/doctor?inspect=repository/media/nas";
const SHARED = "/doctor?inspect=cluster-repository/shared";

async function drawer() {
  return screen.findByRole("dialog");
}

async function openTab(name: string) {
  const panel = await drawer();
  await userEvent.click(await within(panel).findByRole("tab", { name: new RegExp(`^${name}`) }));
  return within(panel).getByRole("tabpanel");
}

/** The drawer's action bar, once the detail has loaded. */
async function foot() {
  const panel = await drawer();
  return waitFor(() => {
    const f = panel.querySelector<HTMLElement>(".side-panel__foot");
    if (f === null) throw new Error("no action bar yet");
    return f;
  });
}

describe("Repository drawer", () => {
  it("reads with the kindPath segment and the repository's own namespace", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp(NAS);
    await screen.findByRole("status", { name: "Repository verdict" });
    expect(calledPaths()).toContain("/api/v1/repositories/repository/nas?namespace=media");
    await foot();
    // The capability review is namespace-scoped, so it must be asked for the
    // namespace the object lives in (addenda item 17).
    await waitFor(() => {
      expect(calledPaths()).toContain("/api/v1/me?namespace=media");
    });
  });

  it("answers 'is this repository healthy, and why not' at the top", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp(NAS);
    const verdict = await screen.findByRole("status", { name: "Repository verdict" });
    expect(verdict).toHaveTextContent("Degraded");
    expect(verdict).toHaveTextContent("repository server");
    const gates = screen.getByRole("region", { name: "Gates holding this repository" });
    expect(gates).toHaveTextContent("PrivilegedMoverNotPermitted");
    expect(gates).toHaveTextContent("has not opted in to privileged movers");
    const stats = (await drawer()).querySelector('dl[aria-label="Repository nas at a glance"]');
    expect(stats).toHaveTextContent("412");
  });

  it("opens on Storage and keeps the rest a tab away", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp(NAS);
    const panel = await drawer();
    const tabs = await within(panel).findByRole("tablist", { name: "About this repository" });
    expect(
      within(tabs)
        .getAllByRole("tab")
        .map((t) => t.textContent),
    ).toEqual(["Storage", "Catalog", "Maintenance", "Sessions 1", "Conditions"]);
    expect(within(panel).getByRole("tabpanel", { name: "Storage" })).toBeInTheDocument();
  });

  it("shows the catalog, probe, seed and conditions the server reported", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp(NAS);
    expect(
      within(await openTab("Storage")).getByRole("region", { name: "Seed" }),
    ).toHaveTextContent("ClusterRepository/shared");
    const catalog = await openTab("Catalog");
    expect(within(catalog).getByRole("region", { name: "Catalog" })).toHaveTextContent("12");
    expect(within(catalog).getByRole("region", { name: "Health probe" })).toHaveTextContent("4");
    const conditions = await openTab("Conditions");
    expect(within(conditions).getByRole("table", { name: "Conditions" })).toHaveTextContent(
      "ProbeFailing",
    );
  });

  it("renders the never-written fields as 'not reported' rather than blank or zero", async () => {
    mockApi({ "/api/v1/repositories/repository/nas": jsonResponse(detail) });
    mountApp(NAS);
    const storage = within(await openTab("Storage")).getByRole("region", { name: "Storage" });
    expect(within(storage).getByText("not reported")).toBeInTheDocument();
    // Both maintenance tracks' next run, likewise.
    const maintenance = within(await openTab("Maintenance")).getByRole("region", {
      name: "Maintenance",
    });
    expect(within(maintenance).getAllByText("not reported")).toHaveLength(2);
  });

  it("asks about a ClusterRepository with no namespace at all", async () => {
    mockApi({ "/api/v1/repositories/cluster-repository/shared": jsonResponse(clusterDetail) });
    mountApp(SHARED);
    await screen.findByRole("status", { name: "Repository verdict" });
    expect(calledPaths()).toContain("/api/v1/repositories/cluster-repository/shared");
    // `patchClusterRepositories` is reviewed cluster-scoped whatever namespace
    // is asked, so the review must carry none.
    await foot();
    await waitFor(() => {
      expect(calledPaths()).toContain("/api/v1/me");
    });
  });

  it("disables an action the caller may not perform and says why, rather than hiding it", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/me": meWith({}),
    });
    mountApp(NAS);
    const bar = await foot();
    const suspend = await within(bar).findByRole("button", { name: /Suspend/ });
    await waitFor(() => {
      expect(suspend).toHaveAttribute("aria-disabled", "true");
    });
    expect(suspend).toHaveAttribute(
      "data-reason",
      expect.stringContaining("is not permitted for alice in namespace media"),
    );
    expect(within(bar).getByRole("button", { name: /Scan catalog/ })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });

  it("suspends with the kindPath as the kind, and renders the receipt's note", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/actions/suspend": jsonResponse(receipt),
    });
    mountApp(NAS);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /Suspend/ }));
    // The confirmation says what suspending does before it is done.
    const confirm = within(bar).getByRole("dialog", { name: "Suspend" });
    expect(confirm).toHaveTextContent("No new backups run here");
    await user.click(within(confirm).getByRole("button", { name: "Suspend this repository" }));

    expect(await within(await notifications()).findByText(/is now suspended/)).toBeInTheDocument();
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
    mountApp(NAS);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /Scan catalog/ }));
    const confirm = within(bar).getByRole("dialog", { name: "Scan catalog" });
    await user.click(within(confirm).getByRole("button", { name: "Request a scan" }));
    expect(
      await within(await notifications()).findByText(/Scan the catalog of .*nas requested/),
    ).toBeInTheDocument();
    expect(sentBody("/api/v1/actions/scan-catalog")).toEqual({
      kind: "repository",
      name: "nas",
      namespace: "media",
    });
  });

  it("says why a maintenance run cannot be asked for when nothing governs the repository", async () => {
    mockApi({ "/api/v1/repositories/cluster-repository/shared": jsonResponse(clusterDetail) });
    mountApp(SHARED);
    const run = await within(await foot()).findByRole("button", { name: /Run maintenance/ });
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
    mountApp(NAS);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /Suspend/ }));
    await user.click(within(bar).getByRole("button", { name: "Suspend this repository" }));
    const alerts = await screen.findAllByRole("alert");
    expect(alerts.map((a) => a.textContent).join(" ")).toContain("Suspending was refused.");
  });

  it("stops a browse session, judged in the namespace the session Job runs in", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/repositories/repository/nas/session": new Response(null, { status: 204 }),
    });
    mountApp(NAS);
    const user = userEvent.setup();
    const sessions = await openTab("Sessions");
    await user.click(await within(sessions).findByRole("button", { name: /Stop session/ }));
    // 204 with no body; the client must not try to parse one (addenda item 18).
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
    mountApp(NAS);
    const panel = await drawer();
    expect(await within(panel).findByRole("alert")).toHaveTextContent(
      "The repository was refused.",
    );
    expect(panel.querySelector('[data-state="not-permitted"]')).not.toBeNull();
  });

  it("shows a skeleton while the repository loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp(NAS);
    const panel = await drawer();
    expect(await within(panel).findByRole("status", { busy: true })).toBeInTheDocument();
  });

  it("goes back to the first tab when it walks to another resource", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/repositories/cluster-repository/shared": jsonResponse(clusterDetail),
    });
    const { router } = mountApp(NAS);
    await openTab("Conditions");
    await router.navigate({ to: "/doctor", search: { inspect: "cluster-repository/shared" } });
    const panel = await drawer();
    await within(panel).findByRole("status", { name: "Repository verdict" });
    expect(within(panel).getByRole("tab", { name: "Storage" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
  });
});

describe("Repository drawer — nothing reported yet", () => {
  const bare: RepositoryDetail = {
    ...detail,
    summary: {
      ...detail.summary,
      name: "fresh",
      phase: null,
      health: "unknown",
      serverBacked: false,
      snapshotCount: null,
      totalSizeBytes: null,
      indexBlobCount: null,
      serverEndpoint: null,
    },
    identityCluster: null,
    catalog: null,
    health: null,
    server: null,
    seed: null,
    maintenance: null,
    gates: [],
    conditions: [],
    policies: [],
    replicationsOut: [],
    sessions: [],
  };
  const FRESH = "/doctor?inspect=repository/media/fresh";
  const mock = (d: RepositoryDetail = bare) => {
    mockApi({ "/api/v1/repositories/repository/fresh": jsonResponse(d) });
  };

  it("never lights the healthy lamp over a repository with no phase", async () => {
    mock();
    mountApp(FRESH);
    const verdict = await screen.findByRole("status", { name: "Repository verdict" });
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
    expect(verdict).toHaveTextContent("has written no phase");
  });

  it("says no catalog scan has run rather than showing zeroes, nor any probe", async () => {
    mock();
    mountApp(FRESH);
    const tab = await openTab("Catalog");
    const catalog = within(tab).getByRole("region", { name: "Catalog" });
    expect(catalog).toHaveTextContent("No catalog scan has been recorded");
    expect(catalog).not.toHaveTextContent("0");
    expect(within(tab).getByRole("region", { name: "Health probe" })).toHaveTextContent(
      "No probe result has been recorded",
    );
  });

  it("qualifies a partial catalog count so it is not read as the whole repository", async () => {
    mock({ ...bare, catalog: { discoveredBackupCount: 1000, coverage: "partial" } });
    mountApp(FRESH);
    expect(
      within(await openTab("Catalog")).getByRole("region", { name: "Catalog" }),
    ).toHaveTextContent("1000 (newest window only; deleted snapshots are not expired)");
  });

  it("explains what an ungoverned repository loses by having no maintenance", async () => {
    mock();
    mountApp(FRESH);
    const maintenance = within(await openTab("Maintenance")).getByRole("region", {
      name: "Maintenance",
    });
    expect(maintenance).toHaveTextContent("No Maintenance resource governs this repository");
    expect(maintenance).toHaveTextContent("grows without bound");
  });

  it("says every mover talks to the backend itself when no server fronts it", async () => {
    mock();
    mountApp(FRESH);
    expect(
      within(await openTab("Storage")).getByRole("region", { name: "Access" }),
    ).toHaveTextContent("No repository server is running");
  });

  it("says an unreconciled repository is unreconciled, not healthy", async () => {
    mock();
    mountApp(FRESH);
    expect(
      within(await openTab("Conditions")).getByRole("region", { name: "Conditions" }),
    ).toHaveTextContent("the operator has not reconciled this repository");
  });

  it("draws no gates finding when nothing is holding the repository back", async () => {
    mock();
    mountApp(FRESH);
    await screen.findByRole("status", { name: "Repository verdict" });
    expect(screen.queryByRole("region", { name: "Gates holding this repository" })).toBeNull();
  });
});

describe("Repository drawer — where it sits", () => {
  const copy = (namespace: string, phase: "succeeded" | "failed") => ({
    namespace,
    name: "offsite",
    source: `Repository/${namespace}/nas`,
    destination: "ClusterRepository/shared",
    cron: "0 3 * * *",
    suspended: false,
    phase,
    lastReplicated: null,
    identitiesSelected: 1,
    snapshotsCopied: 1,
    alreadyPresent: 0,
    failed: 0,
    pruned: 0,
  });

  it("chains what fires it and what writes it, to it, to what copies it", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse({
        ...detail,
        schedules: [
          {
            namespace: "media",
            name: "nightly-cron",
            policy: "nightly",
            cron: "0 2 * * *",
            suspended: false,
            consecutiveFailures: 0,
          },
        ],
      }),
      "/api/v1/policies": jsonResponse([]),
      "/api/v1/replications": jsonResponse({ repository: [], snapshot: [] }),
    });
    mountApp(NAS);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    const steps = within(chain).getAllByRole("listitem", { name: /./ });
    expect(steps.map((s) => s.getAttribute("aria-label"))).toEqual([
      "Fired by",
      "Written by",
      "This repository",
      "Copies to",
    ]);
    // No row for "nightly" came back: it is still named, as a reference.
    expect(
      within(chain)
        .getByRole("link", { name: /SnapshotPolicy.*nightly/ })
        .getAttribute("href"),
    ).toMatch(/inspect=snapshot-policy%2Fmedia%2Fnightly$/);
  });

  it("joins a replication on kind, namespace and name — never a same-named one elsewhere", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse(detail),
      "/api/v1/policies": jsonResponse([]),
      "/api/v1/replications": jsonResponse({
        repository: [],
        snapshot: [copy("prod", "succeeded"), copy("media", "failed")],
      }),
    });
    mountApp(NAS);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    const copies = within(chain).getByRole("listitem", { name: "Copies to" });
    await waitFor(() => {
      expect(copies.querySelector(".health")).toHaveAttribute("data-health", "failed");
    });
  });

  it("keeps a replication it cannot read as text, and the drawer standing", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse({
        ...detail,
        // An older server's bare name, and a kind this bundle does not know.
        replicationsOut: ["offsite", { kind: "bucketReplication", namespace: "media", name: "b" }],
      }),
    });
    mountApp(NAS);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    const copies = within(chain).getByRole("listitem", { name: "Copies to" });
    expect(copies).toHaveTextContent("offsite");
    expect(copies).toHaveTextContent("bucketReplication");
    expect(screen.getByRole("status", { name: "Repository verdict" })).toBeInTheDocument();
  });

  it("names an unloaded replication in the kind the server gave, not a guessed one", async () => {
    mockApi({
      "/api/v1/repositories/repository/nas": jsonResponse({
        ...detail,
        replicationsOut: [{ kind: "repositoryReplication", namespace: "media", name: "blobsync" }],
      }),
      "/api/v1/policies": jsonResponse([]),
      "/api/v1/replications": jsonResponse({ repository: [], snapshot: [] }),
    });
    mountApp(NAS);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    const copies = within(chain).getByRole("listitem", { name: "Copies to" });
    expect(copies.querySelector('.ref[data-kind="repository-replication"]')).toHaveTextContent(
      "blobsync",
    );
  });
});
