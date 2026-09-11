import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type {
  Me,
  RetentionCandidate,
  RetentionPlan,
  SnapshotDetail,
  SnapshotRow,
} from "../api/types";
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

const PATH = "/api/v1/snapshots/media/nightly-29";
const RETENTION = `${PATH}/retention`;

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

function detail(over: Partial<SnapshotDetail> = {}): SnapshotDetail {
  return {
    row: row(),
    stats: {
      sizeBytes: 987654321,
      bytesNew: null,
      filesNew: 40,
      filesModified: 5,
      filesUnchanged: 12000,
      filesFailed: null,
    },
    durationSeconds: 240,
    sources: ["/data"],
    lineage: { copiedFromRepository: null, sourceManifestId: null, copies: [] },
    retentionPreview: {
      kept: true,
      reasons: ["keepDaily slot 1"],
      computedAt: "2026-09-10T08:00:00Z",
    },
    failure: null,
    logTail: [],
    conditions: [],
    gates: [],
    browsable: true,
    browseBlocker: null,
    ...over,
  };
}

function candidate(over: Partial<RetentionCandidate> = {}): RetentionCandidate {
  return {
    namespace: "media",
    name: "nightly-29",
    endTime: "2026-09-09T01:04:00Z",
    kept: true,
    rules: ["keepDaily slot 1"],
    pinned: false,
    subject: false,
    ...over,
  };
}

function plan(over: Partial<RetentionPlan> = {}): RetentionPlan {
  return {
    buckets: [
      {
        key: "",
        candidates: [
          candidate({ subject: true }),
          candidate({ name: "nightly-01", kept: false, rules: [] }),
        ],
      },
    ],
    policy: { namespace: "media", name: "nightly" },
    computedAt: "2026-09-10T08:59:00Z",
    unbounded: false,
    ...over,
  };
}

/**
 * The JSON body of the snapshot-now request, read off the recorded `fetch`
 * init rather than off the mock's `Request` (whose body is a stream).
 */
function snapshotNowBody(): unknown {
  for (const [, init] of fetchMock.mock.calls) {
    const body = init?.body;
    if (typeof body === "string" && body.includes('"policy"')) {
      return JSON.parse(body);
    }
  }
  return undefined;
}

/** `/me` with one capability cleared, for the not-permitted paths. */
function meWithout(flag: keyof Me["can"]): Me {
  return { ...ME, can: { ...ME.can, [flag]: false } };
}

describe("Snapshot detail route", () => {
  it("actually renders the page — the escaped route name is not a layout child", async () => {
    // The trap this asserts: `snapshots.$namespace.$name.tsx` beside
    // `snapshots.tsx` makes the list a LAYOUT, and the detail renders into an
    // `<Outlet/>` the list does not have — a silently blank page that a green
    // component suite would never catch.
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp("/snapshots/media/nightly-29");
    expect(await screen.findByRole("status", { name: "Snapshot verdict" })).toHaveTextContent(
      "Succeeded",
    );
    // And it is the detail, not the list, that rendered.
    expect(screen.queryByRole("table", { name: "Snapshots" })).not.toBeInTheDocument();
    expect(calledPaths()).toContain(PATH);
  });

  it("reaches the file browser, and the real route tree resolves the link", async () => {
    // The only screen that links to `…/browse`. A component test proves the
    // href string; this one proves the app can actually go there — the two
    // screens were built in separate worktrees and never met until now.
    mockApi({
      [PATH]: jsonResponse(detail()),
      // No session yet — the browse page's ordinary first state.
      [`${PATH}/session`]: problemResponse({
        type: "urn:kopiur:problem:session-required",
        title: "Session required",
        status: 404,
        detail: "No browse session is running for media/nightly-29.",
        what: "No browse session is running for media/nightly-29.",
        why: "Listing a snapshot needs a mover pod holding the repository open.",
        fix: "start a browse session",
        instance: `${PATH}/session`,
        kubeReason: null,
      }),
    });
    const { router } = mountApp("/snapshots/media/nightly-29?namespace=media");
    const link = await screen.findByRole("link", { name: /browse the files/i });
    expect(link).toHaveAttribute("href", "/snapshots/media/nightly-29/browse?namespace=media");
    await userEvent.click(link);
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/snapshots/media/nightly-29/browse");
    });
    // Not a blank `<Outlet/>`: the browse page's own content rendered, and the
    // scope survived the move.
    expect(await screen.findByText("No browse session is running")).toBeInTheDocument();
    expect(router.state.location.search).toEqual({ namespace: "media" });
  });

  it("shows the cheap verdict without fetching the plan", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp("/snapshots/media/nightly-29");
    await screen.findByRole("status", { name: "Snapshot verdict" });
    expect(screen.getByText(/Today's retention keeps this snapshot/)).toHaveTextContent(
      "keepDaily slot 1",
    );
    // A fan-out policy's plan is every child of the policy; the page does not
    // pay for it on every open.
    expect(calledPaths()).not.toContain(RETENTION);
  });

  it("fetches and renders the bucketed plan when the URL asks for it", async () => {
    mockApi({ [PATH]: jsonResponse(detail()), [RETENTION]: jsonResponse(plan()) });
    mountApp("/snapshots/media/nightly-29?retention=open");
    const table = await screen.findByRole("table", { name: /Candidates in/ });
    const rows = bodyRows(table);
    expect(rows).toHaveLength(2);
    expect(nth(rows, 0)).toHaveAttribute("data-subject", "true");
    expect(within(nth(rows, 1)).getByText("Pruned")).toBeInTheDocument();
    expect(calledPaths()).toContain(RETENTION);
  });

  it("puts the plan behind a link, so an expanded view is shareable", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp("/snapshots/media/nightly-29");
    expect(
      await screen.findByRole("link", { name: /Show the full retention plan/ }),
    ).toHaveAttribute("href", "/snapshots/media/nightly-29?retention=open");
  });

  it("renders an unbounded plan as no retention configured, never as a prune", async () => {
    mockApi({
      [PATH]: jsonResponse(detail({ retentionPreview: null })),
      [RETENTION]: jsonResponse(
        plan({
          unbounded: true,
          buckets: [{ key: "", candidates: [candidate({ kept: true, rules: [], subject: true })] }],
        }),
      ),
    });
    mountApp("/snapshots/media/nightly-29?retention=open");
    expect(await screen.findByText(/No GFS retention is configured/)).toBeInTheDocument();
    const rows = bodyRows(screen.getByRole("table", { name: /Candidates in/ }));
    expect(nth(rows, 0)).not.toHaveTextContent(/prun/i);
  });

  it("renders the plan's 422 explanations as findings, not as a broken page", async () => {
    mockApi({
      [PATH]: jsonResponse(detail({ retentionPreview: null, row: row({ policy: null }) })),
      [RETENTION]: problemResponse({
        type: "urn:kopiur:problem:no-retention-policy",
        title: "Unprocessable Entity",
        status: 422,
        detail: "not governed",
        what: "Snapshot media/nightly-29 is not governed by a SnapshotPolicy.",
        why: "GFS retention is configured on a SnapshotPolicy, and this snapshot names none.",
        fix: "look at the repository's catalog settings instead",
        instance: RETENTION,
      }),
    });
    mountApp("/snapshots/media/nightly-29?retention=open");
    // The server's own sentence, not a paraphrase.
    expect(
      await screen.findByText("Snapshot media/nightly-29 is not governed by a SnapshotPolicy."),
    ).toBeInTheDocument();
    // The remedy appears both on the cheap no-verdict finding and on the 422.
    expect(screen.getAllByText(/look at the repository's catalog settings/).length).toBeGreaterThan(
      0,
    );
    expect(screen.queryByText(/Could not load the retention plan/)).not.toBeInTheDocument();
  });

  it("says why there is no verdict, without claiming it could not be worked out", async () => {
    mockApi({
      [PATH]: jsonResponse(
        detail({ retentionPreview: null, row: row({ phase: "failed", policy: null }) }),
      ),
    });
    mountApp("/snapshots/media/nightly-29");
    expect(await screen.findByText(/because no SnapshotPolicy governs it/)).toBeInTheDocument();
  });

  it("names the deletion policy in the delete confirmation and words it as a request", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp("/snapshots/media/nightly-29");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^Delete/ }));
    const confirm = screen.getByRole("group", { name: "Delete" });
    expect(confirm).toHaveTextContent("Delete");
    expect(confirm).toHaveTextContent(/deleted from the repository/);
    expect(confirm).toHaveTextContent(/cannot be restored from afterwards/);
    // 202: requested, not performed.
    expect(confirm).toHaveTextContent(/requested/);
    expect(screen.getByRole("button", { name: "Request deletion" })).toBeInTheDocument();
  });

  it("refuses to guess the consequence when the CR sets no deletion policy", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ row: row({ deletionPolicy: null }) })) });
    mountApp("/snapshots/media/nightly-29");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^Delete/ }));
    const confirm = screen.getByRole("group", { name: "Delete" });
    expect(confirm).toHaveTextContent("not set (the operator decides)");
    expect(confirm).toHaveTextContent(/cannot tell you which will apply/);
    expect(confirm.querySelector('[data-consequence="unknown"]')).not.toBeNull();
  });

  it("renders the delete receipt's note, where a breaker hold explains itself", async () => {
    // The GET and the DELETE share a pathname, so `mockApi` (which keys on the
    // pathname alone) cannot tell them apart — the handler switches on method.
    fetchMock.resetMocks();
    fetchMock.mockResponse((request) => {
      const url = new URL(request.url, "http://localhost");
      if (url.pathname === "/api/v1/me") {
        return Promise.resolve(jsonResponse(ME));
      }
      if (url.pathname === PATH && request.method === "DELETE") {
        return Promise.resolve(
          jsonResponse(
            {
              kind: "deleteSnapshot",
              created: [],
              requestedAt: "2026-09-10T09:00:00Z",
              note: "Held by the repository's mass-deletion breaker: 14 external deletes exceed the threshold of 10.",
            },
            202,
          ),
        );
      }
      return Promise.resolve(jsonResponse(detail()));
    });
    mountApp("/snapshots/media/nightly-29");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^Delete/ }));
    await user.click(screen.getByRole("button", { name: "Request deletion" }));
    await waitFor(() => {
      expect(screen.getByText(/mass-deletion breaker/)).toBeInTheDocument();
    });
    // "requested", never "deleted".
    expect(screen.getByText(/Delete requested/)).toBeInTheDocument();
  });

  it("asks for the pin explicitly and says that it is permanent", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp("/snapshots/media/nightly-29");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /Snapshot now/ }));
    const pin = screen.getByRole("checkbox", { name: /Pin the new snapshot/ });
    // Required on the wire and never defaulted on: it starts cleared.
    expect(pin).not.toBeChecked();
    expect(screen.getByRole("group", { name: "Snapshot now" })).toHaveTextContent(
      /permanent and exempts the snapshot from GFS pruning entirely/,
    );
  });

  it("sends the pin the reader actually chose, with the policy's namespace", async () => {
    mockApi({
      [PATH]: jsonResponse(detail()),
      "/api/v1/actions/snapshot-now": jsonResponse(
        { kind: "snapshotNow", created: [], requestedAt: null, note: null },
        201,
      ),
    });
    mountApp("/snapshots/media/nightly-29");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /Snapshot now/ }));
    await user.click(screen.getByRole("checkbox", { name: /Pin the new snapshot/ }));
    await user.click(screen.getByRole("button", { name: "Take a pinned snapshot" }));
    await waitFor(() => {
      expect(snapshotNowBody()).not.toBeUndefined();
    });
    // `pin` is required on the wire and its consequence is permanent, so the
    // value sent has to be the one the reader actually set.
    expect(snapshotNowBody()).toEqual({
      namespace: "media",
      policy: "nightly",
      tags: [],
      pin: true,
    });
  });

  it("keeps a forbidden action visible, disabled and explained", async () => {
    mockApi({
      [PATH]: jsonResponse(detail()),
      "/api/v1/me": jsonResponse(meWithout("deleteSnapshots")),
    });
    mountApp("/snapshots/media/nightly-29");
    const del = await screen.findByRole("button", { name: /^Delete/ });
    expect(del).toHaveAttribute("aria-disabled", "true");
    // `/me` answers asynchronously; until it does the reason is "not loaded
    // yet", which is also honest — but the RBAC verdict is what must land.
    await waitFor(() => {
      expect(del).toHaveAttribute("data-reason", expect.stringContaining("is not permitted"));
    });
    expect(del.getAttribute("data-reason")).toContain("Delete snapshots");
  });

  it("refuses snapshot-now for a snapshot that names no policy, and says why", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ row: row({ policy: null }) })) });
    mountApp("/snapshots/media/nightly-29");
    const button = await screen.findByRole("button", { name: /Snapshot now/ });
    expect(button).toHaveAttribute("aria-disabled", "true");
    expect(button).toHaveAttribute(
      "data-reason",
      expect.stringContaining("names no SnapshotPolicy"),
    );
  });

  it("renders a 404 as the problem it is, with a way back to the list", async () => {
    mockApi({
      [PATH]: problemResponse({
        type: "urn:kopiur:problem:not-found",
        title: "Not Found",
        status: 404,
        detail: "gone",
        what: "There is no Snapshot called nightly-29 in namespace media.",
        why: "It was pruned by retention, deleted, or never existed.",
        fix: "reload the snapshots list to see what the cluster holds now",
        instance: PATH,
      }),
    });
    mountApp("/snapshots/media/nightly-29");
    expect(await screen.findByText(/There is no Snapshot called nightly-29/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /All snapshots/ })).toHaveAttribute(
      "href",
      "/snapshots",
    );
  });

  it("renders a 403 as not permitted rather than as a missing snapshot", async () => {
    mockApi({
      [PATH]: problemResponse(forbiddenProblem("read this snapshot", PATH)),
    });
    mountApp("/snapshots/media/nightly-29");
    expect(
      await screen.findByText(/Not permitted to view the snapshot nightly-29/),
    ).toBeInTheDocument();
  });

  it("announces the skeleton while the snapshot is in flight", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/snapshots/media/nightly-29");
    expect(await screen.findByText("Loading the snapshot nightly-29…")).toBeInTheDocument();
  });

  it("renders a phase this build does not know without crashing the route", async () => {
    mockApi({
      [PATH]: jsonResponse(detail({ row: row({ phase: { unknown: { raw: "Weird" } } }) })),
    });
    mountApp("/snapshots/media/nightly-29");
    const verdict = await screen.findByRole("status", { name: "Snapshot verdict" });
    expect(verdict).toHaveTextContent("Weird");
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
  });
});
