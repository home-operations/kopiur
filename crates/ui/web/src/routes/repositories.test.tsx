import { screen, within } from "@testing-library/react";
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
  allowedNamespaceCount: null,
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
  allowedNamespaceCount: 3,
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

  it("links each row with the summary's kindPath, not a segment built from the kind", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories");
    expect(await screen.findByRole("link", { name: "nas" })).toHaveAttribute(
      "href",
      "/repositories/repository/nas?namespace=media",
    );
    expect(screen.getByRole("link", { name: "shared" })).toHaveAttribute(
      "href",
      "/repositories/cluster-repository/shared",
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

  it("marks the filtered lamp and offers a way back to the whole fleet", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse(fleet) });
    mountApp("/repositories?health=failed");
    await list();
    const strip = screen.getByRole("navigation", { name: "Repositories by health" });
    const failed = within(strip).getByRole("link", { name: "1 failed repository" });
    // The router marks it: the link's search is a subset of the URL's.
    expect(failed).toHaveAttribute("aria-current", "page");
    // The counts stay the whole fleet's, so the strip can be used to leave the filter.
    expect(within(strip).getByRole("link", { name: "1 healthy repository" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Show all 3" })).toHaveAttribute(
      "href",
      "/repositories",
    );
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
    expect(within(region).getByRole("link", { name: "Show all 1" })).toBeInTheDocument();
  });

  it("teaches what a repository is when the scope holds none", async () => {
    mockApi({ "/api/v1/repositories": jsonResponse([]) });
    mountApp("/repositories?namespace=prod");
    const region = await screen.findByRole("region", { name: "Repositories" });
    const title = await within(region).findByText("No repositories in prod");
    expect(title.closest('[role="status"]')).toHaveTextContent("kopia repository backups land in");
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
