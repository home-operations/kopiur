import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { RepositorySummary } from "../api/types";
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

const nas: RepositorySummary = {
  kind: "Repository",
  kindPath: "repository",
  name: "nas",
  namespace: "media",
  phase: "ready",
  health: "healthy",
  backend: "S3",
  mode: "ReadWrite",
  serverBacked: false,
  suspended: false,
  snapshotCount: 412,
  totalSizeBytes: 987654321,
  indexBlobCount: 17,
  lastObservedAt: null,
  serverEndpoint: null,
  admits: null,
};

const cold: RepositorySummary = {
  ...nas,
  name: "cold",
  phase: "failed",
  health: "failed",
  snapshotCount: null,
  totalSizeBytes: null,
  indexBlobCount: null,
};

const shared: RepositorySummary = {
  ...nas,
  kind: "ClusterRepository",
  kindPath: "cluster-repository",
  name: "shared",
  namespace: null,
  health: "degraded",
  phase: "degraded",
  admits: "all",
};

const fleet = [nas, cold, shared];

function list() {
  return screen.findByRole("table", { name: "Repositories" });
}

describe("Repositories list", () => {
  it("lists every repository of both kinds, scoped by the shell's namespace", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories?namespace=media");
    const rows = bodyRows(await list());
    expect(rows).toHaveLength(3);
    expect(nth(rows, 0)).toHaveTextContent("nas");
    expect(calledPaths()).toContain("/api/v1/repositories?namespace=media");
  });

  it("opens each row in the resource drawer, keeping the list's filter", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories?health=failed");
    expect((await screen.findByRole("link", { name: "cold" })).getAttribute("href")).toMatch(
      /health=failed.*inspect=/,
    );
  });

  it("filters on the ?health= key the overview's health strip links with", async () => {
    // This is a live contract: StatusCards links `/repositories?health=<lamp>`.
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories?health=failed");
    const rows = bodyRows(await list());
    expect(rows).toHaveLength(1);
    expect(nth(rows, 0)).toHaveTextContent("cold");
  });

  it("marks the filtered lamp, and clicking it again clears the filter", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories?health=failed");
    await list();
    const strip = screen.getByRole("navigation", { name: "Repositories by health" });
    const failed = within(strip).getByRole("link", {
      name: "1 failed repository, shown — select again to show all",
    });
    expect(failed).toHaveAttribute("aria-current", "true");
    expect(failed).toHaveAttribute("href", "/repositories");
    // The counts stay the whole fleet's, and the other lamps switch the filter.
    expect(within(strip).getByRole("link", { name: "1 healthy repository" })).toHaveAttribute(
      "href",
      "/repositories?health=healthy",
    );
    // No separate sentence or button to clear it: the lamp is the control.
    expect(screen.queryByText(/Showing the failed/)).toBeNull();
    expect(screen.queryByRole("link", { name: /Show all/ })).toBeNull();

    await userEvent.setup().click(failed);
    expect(bodyRows(await list())).toHaveLength(3);
    expect(
      within(screen.getByRole("navigation", { name: "Repositories by health" })).getByRole("link", {
        name: "1 failed repository",
      }),
    ).not.toHaveAttribute("aria-current");
  });

  it("keeps a ?health= value it does not know, says so, and filters nothing out", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories?health=archived");
    const rows = bodyRows(await list());
    expect(rows).toHaveLength(3);
    expect(screen.getByText(/not one of the six/)).toHaveTextContent("archived");
  });

  it("says the lamp is empty rather than showing a blank table", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse([nas]) });
    mountApp("/repositories?health=failed");
    const region = await screen.findByRole("region", { name: "Repositories" });
    expect(await within(region).findByText(/No failed repositories/)).toBeInTheDocument();
    // The lit lamp above clears it; the empty state carries no button of its own.
    expect(within(region).queryByRole("link")).toBeNull();
  });

  it("teaches what a repository is when the scope holds none", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse([]) });
    mountApp("/repositories?namespace=prod");
    const region = await screen.findByRole("region", { name: "Repositories" });
    const title = await within(region).findByText("No repositories in prod");
    expect(title.closest('[role="status"]')).toHaveTextContent("where backups are stored");
  });

  it("renders the not-permitted state for a 403 and offers no retry", async () => {
    mockApi({
      "/api/v1/repositories": problemResponse(
        forbiddenProblem("Repositories were refused.", "/api/v1/repositories"),
      ),
    });
    mountApp("/repositories");
    const region = await screen.findByRole("region", { name: "Repositories" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("Repositories were refused");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/repositories": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/repositories");
    const region = await screen.findByRole("region", { name: "Repositories" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while the fleet loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/repositories");
    const region = await screen.findByRole("region", { name: "Repositories" });
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });
});

describe("Repositories — whole-row link", () => {
  it("makes every row open its repository in the drawer, through the name", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories");
    const rows = bodyRows(await list());
    expect(nth(rows, 0).querySelector("a.row-link")?.getAttribute("href")).toMatch(
      /inspect=repository%2Fmedia%2Fnas$/,
    );
    expect(nth(rows, 2).querySelector("a.row-link")?.getAttribute("href")).toMatch(
      /inspect=cluster-repository%2Fshared$/,
    );
  });
});
