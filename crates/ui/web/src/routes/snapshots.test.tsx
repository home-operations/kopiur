import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { Page, SnapshotRow } from "../api/types";
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

function row(over: Partial<SnapshotRow> = {}): SnapshotRow {
  return {
    namespace: "media",
    name: "nightly-29",
    phase: "succeeded",
    origin: "scheduled",
    policy: "nightly",
    repository: "media/nas",
    kopiaSnapshotId: "k9f2",
    identity: "kopiur@media:/data",
    startTime: "2026-09-09T01:00:00Z",
    endTime: "2026-09-09T01:04:00Z",
    sizeBytes: 987654321,
    bytesNew: null,
    filesTotal: 12045,
    filesFailed: null,
    pinned: false,
    deletionPolicy: "Delete",
    copiedFrom: null,
    ...over,
  };
}

function page(items: SnapshotRow[], over: Partial<Page<SnapshotRow>> = {}): Page<SnapshotRow> {
  return { items, total: items.length, offset: 0, limit: 50, ...over };
}

function list() {
  return screen.findByRole("table", { name: "Snapshots" });
}

/**
 * The MOST RECENT URL the list requested.
 *
 * The most recent, not the first: a filter round-trip re-requests, and reading
 * the initial unfiltered load would pass whatever the form did.
 */
function snapshotRequest(): string {
  const paths = calledPaths().filter((url) => url.startsWith("/api/v1/snapshots"));
  const path = paths.at(-1);
  if (path === undefined) {
    throw new Error(`no /snapshots request was made; got ${calledPaths().join(", ")}`);
  }
  return path;
}

describe("Snapshots list", () => {
  it("lists the rows the server returned, scoped by the shell's namespace", async () => {
    mockApi({ "/api/v1/snapshots": jsonResponse(page([row(), row({ name: "nightly-28" })])) });
    mountApp("/snapshots?namespace=media");
    expect(bodyRows(await list())).toHaveLength(2);
    expect(snapshotRequest()).toContain("namespace=media");
  });

  it("links each row to its detail route, which is NOT nested under the list", async () => {
    mockApi({ "/api/v1/snapshots": jsonResponse(page([row()])) });
    mountApp("/snapshots");
    expect(await screen.findByRole("link", { name: "nightly-29" })).toHaveAttribute(
      "href",
      "/snapshots/media/nightly-29",
    );
  });

  it("sends every filter the URL carries, with the server's own parameter names", async () => {
    mockApi({ "/api/v1/snapshots": jsonResponse(page([row()])) });
    mountApp(
      "/snapshots?namespace=media&repository=nas&repositoryKind=repository&repositoryNamespace=media&policy=nightly&origin=scheduled&phase=succeeded",
    );
    await list();
    const request = snapshotRequest();
    for (const expected of [
      "namespace=media",
      "repository=nas",
      "repositoryKind=repository",
      "repositoryNamespace=media",
      "policy=nightly",
      "origin=scheduled",
      "phase=succeeded",
    ]) {
      expect(request).toContain(expected);
    }
  });

  it("round-trips a filter through the address bar when the form is submitted", async () => {
    mockApi({ "/api/v1/snapshots": jsonResponse(page([row()])) });
    const { router } = mountApp("/snapshots");
    await list();
    const user = userEvent.setup();
    await user.selectOptions(screen.getByLabelText("Phase"), "failed");
    await user.type(screen.getByLabelText("Policy"), "nightly");
    await user.click(screen.getByRole("button", { name: /Apply filters/ }));
    // The URL is the state: the filter is in the address bar, so the view is a
    // link a colleague can open.
    expect(router.state.location.search).toMatchObject({ phase: "failed", policy: "nightly" });
    expect(snapshotRequest()).toContain("phase=failed");
  });

  it("keeps a filter value the server would refuse, says so, and does not send it", async () => {
    mockApi({ "/api/v1/snapshots": jsonResponse(page([row()])) });
    mountApp("/snapshots?phase=Succeeded");
    await list();
    // Not sent — the handler answers 400 for a value outside its vocabulary.
    expect(snapshotRequest()).not.toContain("phase=Succeeded");
    // But not silently dropped either: the page names the reader's own value.
    expect(screen.getByText(/which the operator would reject/)).toHaveTextContent("Succeeded");
  });

  it("renders an over-cap 422 as narrow-the-filter, with the filter bar still live", async () => {
    mockApi({
      "/api/v1/snapshots": problemResponse({
        type: "urn:kopiur:problem:list-too-large",
        title: "Unprocessable Entity",
        status: 422,
        detail: "9001 snapshots matched",
        what: "9001 snapshots matched, which is more than this kopiur-ui will assemble in one response (the cap is 5000).",
        why: "Rendering an unbounded list would hold the whole result set in memory.",
        fix: "filter by repository or policy",
        instance: "/api/v1/snapshots",
      }),
    });
    mountApp("/snapshots");
    expect(
      await screen.findByRole("heading", { name: /Too many snapshots to list/ }),
    ).toBeInTheDocument();
    expect(screen.getByText(/9001 snapshots matched/)).toBeInTheDocument();
    expect(screen.getByText(/filter by repository or policy/)).toBeInTheDocument();
    // The remedy is on the page, so the control that applies it must be too.
    expect(screen.getByRole("form", { name: "Snapshot filters" })).toBeInTheDocument();
    // It is not framed as a broken cluster.
    expect(screen.queryByRole("button", { name: "Retry" })).not.toBeInTheDocument();
  });

  it("renders a 403 as not permitted, with no retry to offer", async () => {
    mockApi({
      "/api/v1/snapshots": problemResponse(
        forbiddenProblem("list snapshots in media", "/api/v1/snapshots"),
      ),
    });
    mountApp("/snapshots?namespace=media");
    expect(await screen.findByText(/Not permitted to view snapshots/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry" })).not.toBeInTheDocument();
  });

  it("offers a retry on a gateway failure that carried no problem body", async () => {
    mockApi({
      "/api/v1/snapshots": new Response("<html>502</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/snapshots");
    expect(await screen.findByText(/Could not load snapshots/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("teaches what a snapshot is when there are none", async () => {
    mockApi({ "/api/v1/snapshots": jsonResponse(page([])) });
    mountApp("/snapshots?namespace=media");
    // Queried by its heading, not by `role=status`: the skeleton is a status
    // region too, and it is what is on screen first.
    const heading = await screen.findByRole("heading", { name: "No snapshots in media" });
    const empty = heading.closest(".state");
    expect(empty).not.toBeNull();
    // The state teaches how a row comes to exist, rather than saying "nothing here".
    expect(empty).toHaveTextContent(/SnapshotSchedule/);
    expect(empty).toHaveTextContent(/catalog scan/);
  });

  it("announces the skeleton while the list is in flight", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/snapshots");
    const region = await screen.findByRole("region", { name: "Snapshots" });
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });

  it("states the server's own window and pages forward from it", async () => {
    mockApi({
      "/api/v1/snapshots": jsonResponse(page([row()], { total: 137, offset: 20, limit: 20 })),
    });
    mountApp("/snapshots?offset=20&limit=20");
    await list();
    expect(screen.getByText("Showing 21–21 of 137")).toBeInTheDocument();
    const next = screen.getByRole("link", { name: "Next" });
    expect(next).toHaveAttribute("href", expect.stringContaining("offset=40"));
    // Back to the first page drops the key rather than spelling `offset=0`:
    // the default is the absence, and the two must not both exist as URLs.
    const previous = screen.getByRole("link", { name: "Previous" });
    expect(previous).toHaveAttribute("href", "/snapshots?limit=20");
  });

  it("degrades to an empty state when the body is not a page at all", async () => {
    // Not hypothetical: a proxy, a sign-in page or a version skew can put
    // something else on this path, and indexing into it took the whole route to
    // the error boundary — a blank screen where an empty state belongs. The
    // shell's own test caught it, which is the only reason it did not ship.
    mockApi({ "/api/v1/snapshots": jsonResponse({ user: "alice", groups: [] }) });
    mountApp("/snapshots?namespace=media");
    expect(
      await screen.findByRole("heading", { name: "No snapshots in media" }),
    ).toBeInTheDocument();
  });

  it("says a bookmarked window is past the end rather than showing an empty list", async () => {
    // What a link saved before retention pruned rows looks like.
    mockApi({ "/api/v1/snapshots": jsonResponse(page([], { total: 12, offset: 200, limit: 50 })) });
    mountApp("/snapshots?offset=200");
    expect(
      await screen.findByRole("heading", { name: "This page is past the end of the list" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Back to the first page" })).toHaveAttribute(
      "href",
      "/snapshots",
    );
  });

  it("renders a phase this build has never seen as the raw string rather than crashing", async () => {
    mockApi({
      "/api/v1/snapshots": jsonResponse(page([row({ phase: { unknown: { raw: "Weird" } } })])),
    });
    mountApp("/snapshots");
    const first = nth(bodyRows(await list()), 0);
    expect(first.querySelector(".health")).toHaveTextContent("Weird");
    expect(first.querySelector(".health")).toHaveAttribute("data-health", "unknown");
  });

  it("charts the rows on the page and says the chart follows the filter", async () => {
    mockApi({
      "/api/v1/snapshots": jsonResponse(
        page([row(), row({ name: "nightly-28", endTime: "2026-09-08T01:04:00Z" })]),
      ),
    });
    mountApp("/snapshots");
    await list();
    expect(screen.getByRole("figure")).toBeInTheDocument();
    expect(screen.getByText(/moves with the filter and the page window/)).toBeInTheDocument();
  });
});
