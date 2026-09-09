import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { problemBanner } from "../api/problem";
import type { ActionReceipt, Capabilities, ReplicationsView } from "../api/types";
import {
  ME,
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

const view: ReplicationsView = {
  repository: [
    {
      namespace: "media",
      name: "blobsync",
      source: "Repository/media/nas",
      destinationBackend: "S3",
      cron: "0 5 * * *",
      suspended: false,
      phase: "succeeded",
      lastReplicated: "2026-09-08T05:12:00Z",
      nextScheduledAt: null,
      lastReplicatedBytes: null,
      lastReplicatedBlobs: null,
    },
  ],
  snapshot: [
    {
      namespace: "prod",
      name: "offsite",
      source: "Repository/prod/db",
      destination: "ClusterRepository/shared",
      cron: "0 3 * * *",
      suspended: false,
      phase: "failed",
      lastReplicated: null,
      identitiesSelected: 3,
      snapshotsCopied: 7,
      alreadyPresent: 120,
      failed: 1,
      pruned: 2,
    },
  ],
};

const receipt: ActionReceipt = {
  kind: "RepositoryReplication",
  created: [],
  requestedAt: "2026-09-08T06:00:00Z",
  note: null,
};

function meWith(allowed: Partial<Capabilities>) {
  return jsonResponse({
    ...ME,
    can: Object.fromEntries(
      Object.keys(ME.can).map((key) => [key, allowed[key as keyof Capabilities] ?? false]),
    ),
  });
}

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

function table() {
  return screen.findByRole("table", { name: "Replications" });
}

beforeEach(() => {
  problemBanner.dismiss();
});

describe("Replications", () => {
  it("puts both kinds in one ledger with their route and schedule", async () => {
    mockApi({ "/api/v1/replications": jsonResponse(view) });
    mountApp("/replications");
    const rows = bodyRows(await table());
    expect(rows).toHaveLength(2);
    expect(nth(rows, 0)).toHaveTextContent("RepositoryReplication");
    expect(nth(rows, 0)).toHaveTextContent("blobsync");
    expect(nth(rows, 0)).toHaveTextContent("S3");
    expect(nth(rows, 0)).toHaveTextContent("0 5 * * *");
    expect(nth(rows, 1)).toHaveTextContent("SnapshotReplication");
    expect(nth(rows, 1)).toHaveTextContent("ClusterRepository/shared");
  });

  it("shows the lag, and says 'never' for a copy that has not once succeeded", async () => {
    mockApi({ "/api/v1/replications": jsonResponse(view) });
    mountApp("/replications");
    const rows = bodyRows(await table());
    expect(nth(rows, 0)).toHaveTextContent("ago");
    expect(within(nth(rows, 1)).getByText("never")).toBeInTheDocument();
  });

  it("never reads a failed replication as healthy", async () => {
    mockApi({ "/api/v1/replications": jsonResponse(view) });
    mountApp("/replications");
    const rows = bodyRows(await table());
    expect(nth(rows, 0).querySelector(".health")).toHaveAttribute("data-health", "healthy");
    expect(nth(rows, 1).querySelector(".health")).toHaveAttribute("data-health", "failed");
  });

  it("renders the next run as 'not reported' — nothing writes it, on either kind", async () => {
    mockApi({ "/api/v1/replications": jsonResponse(view) });
    mountApp("/replications");
    const rows = bodyRows(await table());
    expect(within(nth(rows, 0)).getAllByText("not reported").length).toBeGreaterThan(0);
    expect(within(nth(rows, 1)).getByText("not reported")).toBeInTheDocument();
    // The blob sync has a second one: both of its last-run counters are
    // never-written too, so it has no last-run summary to give either.
    expect(within(nth(rows, 0)).getAllByText("not reported")).toHaveLength(2);
    // The snapshot copy's counters ARE written, so it reports them.
    expect(nth(rows, 1)).toHaveTextContent("7 copied");
  });

  it("runs a replication with the kind the row came from, in the row's own namespace", async () => {
    mockApi({
      "/api/v1/replications": jsonResponse(view),
      "/api/v1/actions/replication-run": jsonResponse(receipt),
    });
    mountApp("/replications");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Run blobsync now" }));
    expect(sentBody("/api/v1/actions/replication-run")).toEqual({
      namespace: "media",
      name: "blobsync",
      kind: "replication",
    });
  });

  it("judges each row's run against the row's own namespace, not the page's scope", async () => {
    // The two rows live in different namespaces; a viewer with a RoleBinding
    // in only one of them must still see that one enabled (addenda item 17).
    mockApi({
      "/api/v1/replications": jsonResponse(view),
      "/api/v1/me": (url) =>
        url.searchParams.get("namespace") === "media"
          ? meWith({ patchRepositoryReplications: true })
          : meWith({}),
    });
    mountApp("/replications");
    await table();
    expect(await screen.findByRole("button", { name: "Run blobsync now" })).not.toHaveAttribute(
      "aria-disabled",
    );
    const refused = screen.getByRole("button", { name: "Run offsite now" });
    expect(refused).toHaveAttribute("aria-disabled", "true");
    expect(refused).toHaveAttribute("data-reason", expect.stringContaining("in namespace prod"));
    expect(calledPaths()).toContain("/api/v1/me?namespace=media");
    expect(calledPaths()).toContain("/api/v1/me?namespace=prod");
  });

  it("refuses to ask for a run on a suspended replication, and says so", async () => {
    mockApi({
      "/api/v1/replications": jsonResponse({
        repository: [{ ...nthRepository(), suspended: true }],
        snapshot: [],
      }),
    });
    mountApp("/replications");
    const run = await screen.findByRole("button", { name: "Run blobsync now" });
    expect(run).toHaveAttribute("aria-disabled", "true");
    expect(run).toHaveAttribute("data-reason", expect.stringContaining("is suspended"));
  });

  it("teaches what a replication is when there are none", async () => {
    mockApi({ "/api/v1/replications": jsonResponse({ repository: [], snapshot: [] }) });
    mountApp("/replications?namespace=prod");
    const region = await screen.findByRole("region", { name: "Replications" });
    const title = await within(region).findByText("No replications in prod");
    expect(title.closest('[role="status"]')).toHaveTextContent("two places the data lives");
  });

  it("renders the not-permitted state for a 403 and offers no retry", async () => {
    mockApi({
      "/api/v1/replications": problemResponse(
        forbiddenProblem("Replications were refused.", "/api/v1/replications"),
      ),
    });
    mountApp("/replications");
    const region = await screen.findByRole("region", { name: "Replications" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("Replications were refused");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/replications": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/replications");
    const region = await screen.findByRole("region", { name: "Replications" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while the replications load", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/replications");
    const region = await screen.findByRole("region", { name: "Replications" });
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });
});

function nthRepository() {
  return nth(view.repository, 0);
}
