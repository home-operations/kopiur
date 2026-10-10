import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type {
  Me,
  RetentionCandidate,
  RetentionPlan,
  SnapshotDetail,
  SnapshotRow,
} from "../../../api/types";
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
} from "../../../test-utils";

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

const OPEN = "/doctor?inspect=snapshot/media/nightly-29";

async function drawer() {
  return screen.findByRole("dialog");
}

async function openTab(name: string) {
  const panel = await drawer();
  await userEvent.click(await within(panel).findByRole("tab", { name: new RegExp(`^${name}`) }));
  return within(panel).getByRole("tabpanel");
}

async function foot() {
  const panel = await drawer();
  return waitFor(() => {
    const f = panel.querySelector<HTMLElement>(".side-panel__foot");
    if (f === null) throw new Error("no action bar yet");
    return f;
  });
}

describe("Snapshot drawer", () => {
  it("leads with a verdict naming the phase and the policy, and tabs the rest", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp(OPEN);
    const verdict = await screen.findByRole("status", { name: "Snapshot verdict" });
    expect(verdict).toHaveTextContent("Succeeded");
    expect(verdict).toHaveTextContent("nightly");
    expect(calledPaths()).toContain(PATH);
    expect(
      within(await drawer())
        .getAllByRole("tab")
        .map((t) => t.textContent),
    ).toEqual(["Run", "Storage", "Retention", "Lineage", "Conditions"]);
  });

  it("chains the repository and the policy it was made through", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ row: row({ repository: "Repository/media/nas" }) })) });
    mountApp(OPEN);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    expect(
      within(chain)
        .getByRole("link", { name: /SnapshotPolicy.*nightly/ })
        .getAttribute("href"),
    ).toMatch(/inspect=snapshot-policy%2Fmedia%2Fnightly$/);
    expect(within(chain).getByRole("link", { name: /Repository.*nas/ })).toBeInTheDocument();
    expect(chain).toHaveTextContent("this snapshot");
  });

  it("renders new bytes as not reported, never as a blank or a zero", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp(OPEN);
    const run = within(await openTab("Run")).getByRole("region", { name: "This run" });
    expect(run).toHaveTextContent("New bytes");
    expect(run).toHaveTextContent("not reported");
    expect(run).not.toHaveTextContent("New bytes0 B");
  });

  it("never renders an absent deletion policy as a default", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ row: row({ deletionPolicy: null }) })) });
    mountApp(OPEN);
    const storage = within(await openTab("Storage")).getByRole("region", {
      name: "In the repository",
    });
    expect(storage).toHaveTextContent("not set (the operator decides)");
    expect(storage).not.toHaveTextContent("Deletion policyDelete");
  });

  it("charts this snapshot's size among its policy's runs into the same repository, on the Storage tab only", async () => {
    mockApi({
      [PATH]: jsonResponse(detail({ row: row({ repository: "Repository/media/nas" }) })),
      "/api/v1/snapshots": jsonResponse({
        items: [
          row({ name: "nightly-30", endTime: "2026-09-10T01:04:00Z", sizeBytes: 1_100_000_000 }),
          row({ repository: "Repository/media/nas" }),
          row({ name: "nightly-28", endTime: "2026-09-08T01:04:00Z", sizeBytes: 900_000_000 }),
        ],
        total: 3,
        offset: 0,
        limit: 100,
      }),
    });
    mountApp(OPEN);
    await drawer();
    // Not read until the tab is opened: a policy's history is a list call.
    expect(calledPaths().filter((p) => p.startsWith("/api/v1/snapshots?"))).toHaveLength(0);
    const storage = await openTab("Storage");
    const section = within(storage).getByRole("region", { name: "Size over time" });
    const figure = await within(section).findByRole("figure");
    // Its own point is the one the readout shows, not the newest run's.
    expect(within(figure).getByRole("status")).toHaveTextContent("nightly-29");
    expect(within(figure).getByRole("status")).toHaveTextContent("(this snapshot)");
    const asked = calledPaths().find((p) => p.startsWith("/api/v1/snapshots?")) ?? "";
    expect(section).toHaveTextContent("The 3 runs of nightly");
    expect(asked).toContain("namespace=media");
    expect(asked).toContain("policy=nightly");
    expect(asked).toContain(`repository=${encodeURIComponent("Repository/media/nas")}`);
  });

  it("says there is no history to chart for a snapshot no policy governs", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ row: row({ policy: null }) })) });
    mountApp(OPEN);
    const storage = await openTab("Storage");
    expect(within(storage).getByRole("region", { name: "Size over time" })).toHaveTextContent(
      "no runs to trend against",
    );
    expect(calledPaths().filter((p) => p.startsWith("/api/v1/snapshots?"))).toHaveLength(0);
  });

  it("opens the file browser from the action bar, keeping the console's scope", async () => {
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
    const { router } = mountApp(`/doctor?namespace=media&inspect=snapshot/media/nightly-29`);
    const link = await within(await foot()).findByRole("link", { name: /Browse files/ });
    expect(link).toHaveAttribute("href", "/snapshots/media/nightly-29/browse?namespace=media");
    await userEvent.click(link);
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/snapshots/media/nightly-29/browse");
    });
    expect(await screen.findByText("No browse session is running")).toBeInTheDocument();
    expect(router.state.location.search).toEqual({ namespace: "media" });
  });

  it("offers no browse link when the operator says it cannot be browsed, and says why", async () => {
    mockApi({
      [PATH]: jsonResponse(
        detail({
          browsable: false,
          browseBlocker: "This backup failed, so it wrote no snapshot to browse.",
        }),
      ),
    });
    mountApp(OPEN);
    const storage = await openTab("Storage");
    expect(within(storage).getByText(/wrote no snapshot to browse/)).toBeInTheDocument();
    expect(within(await foot()).queryByRole("link", { name: /Browse files/ })).toBeNull();
  });

  it("puts the failure at the top, with the operator's retry classification", async () => {
    mockApi({
      [PATH]: jsonResponse(
        detail({
          row: row({ phase: "failed" }),
          failure: {
            kopiaErrorClass: "RepositoryUnreachable",
            message: "dial tcp: i/o timeout",
            exitCode: 1,
            retryRecommended: true,
            op: "snapshot",
          },
        }),
      ),
    });
    mountApp(OPEN);
    const finding = await within(await drawer()).findByRole("article", {
      name: "RepositoryUnreachable",
    });
    expect(finding).toHaveTextContent("dial tcp: i/o timeout");
    expect(finding).toHaveTextContent(/worth retrying/);
    expect(finding).toHaveTextContent("exit 1");
  });

  it("shows the redacted log tail the server sent, verbatim, in a tab of its own", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ logTail: ["uploading /data", "done"] })) });
    mountApp(OPEN);
    expect(
      within(await openTab("Log")).getByLabelText("Last lines of the mover log"),
    ).toHaveTextContent("uploading /data");
  });

  it("says the operator wrote no conditions rather than implying health", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp(OPEN);
    expect(
      within(await openTab("Conditions")).getByText(
        /No conditions yet: the operator has not reconciled/,
      ),
    ).toBeInTheDocument();
  });

  it("shows the cheap retention verdict without fetching the plan", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp(OPEN);
    await screen.findByRole("status", { name: "Snapshot verdict" });
    // Not until the Retention tab is opened.
    expect(calledPaths()).not.toContain(RETENTION);
    mockApi({ [PATH]: jsonResponse(detail()), [RETENTION]: jsonResponse(plan()) });
    const tab = await openTab("Retention");
    expect(within(tab).getByText(/Today's retention keeps this snapshot/)).toHaveTextContent(
      "keepDaily slot 1",
    );
  });

  it("reads the bucketed plan when its tab is opened, and only then", async () => {
    mockApi({ [PATH]: jsonResponse(detail()), [RETENTION]: jsonResponse(plan()) });
    mountApp(OPEN);
    await screen.findByRole("status", { name: "Snapshot verdict" });
    expect(calledPaths()).not.toContain(RETENTION);
    const tab = await openTab("Retention");
    const table = await within(tab).findByRole("table", { name: /Candidates in/ });
    const rows = bodyRows(table);
    expect(rows).toHaveLength(2);
    expect(nth(rows, 0)).toHaveAttribute("data-subject", "true");
    expect(within(nth(rows, 1)).getByText("Pruned")).toBeInTheDocument();
    expect(calledPaths()).toContain(RETENTION);
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
    mountApp(OPEN);
    const tab = await openTab("Retention");
    expect(await within(tab).findByText(/No GFS retention is configured/)).toBeInTheDocument();
    const rows = bodyRows(within(tab).getByRole("table", { name: /Candidates in/ }));
    expect(nth(rows, 0)).not.toHaveTextContent(/prun/i);
  });

  it("renders the plan's 422 explanations as findings, not as a broken panel", async () => {
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
    mountApp(OPEN);
    const tab = await openTab("Retention");
    // The server's own sentence, not a paraphrase.
    expect(
      await within(tab).findByText(
        "Snapshot media/nightly-29 is not governed by a SnapshotPolicy.",
      ),
    ).toBeInTheDocument();
    expect(
      within(tab).getAllByText(/look at the repository's catalog settings/).length,
    ).toBeGreaterThan(0);
    expect(within(tab).queryByText(/Could not load the retention plan/)).not.toBeInTheDocument();
  });

  it("says why there is no verdict, without claiming it could not be worked out", async () => {
    mockApi({
      [PATH]: jsonResponse(
        detail({ retentionPreview: null, row: row({ phase: "failed", policy: null }) }),
      ),
    });
    mountApp(OPEN);
    expect(
      within(await openTab("Retention")).getByText(/because no SnapshotPolicy governs it/),
    ).toBeInTheDocument();
  });

  it("names the deletion policy in the delete confirmation and words it as a request", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /^Delete/ }));
    const confirm = within(bar).getByRole("dialog", { name: "Delete" });
    expect(confirm).toHaveTextContent("Delete");
    expect(confirm).toHaveTextContent(/deleted from the repository/);
    expect(confirm).toHaveTextContent(/cannot be restored from afterwards/);
    // 202: requested, not performed.
    expect(confirm).toHaveTextContent(/requested/);
    expect(within(bar).getByRole("button", { name: "Request deletion" })).toBeInTheDocument();
  });

  it("manages focus on the delete confirmation, and Escape closes only it", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    const trigger = await within(bar).findByRole("button", { name: /^Delete/ });
    await user.click(trigger);
    // Focus moves into the question, which floats above the bar.
    expect(within(bar).getByRole("dialog", { name: "Delete" })).toContainElement(
      document.activeElement as HTMLElement,
    );

    await user.keyboard("{Escape}");
    expect(within(bar).queryByRole("dialog", { name: "Delete" })).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("gives focus back after the deletion is requested", async () => {
    // The GET and the DELETE share a pathname, so the handler switches on method.
    fetchMock.resetMocks();
    fetchMock.mockResponse((request) => {
      const url = new URL(request.url, "http://localhost");
      if (url.pathname === "/api/v1/me") {
        return Promise.resolve(jsonResponse(ME));
      }
      if (url.pathname === PATH && request.method === "DELETE") {
        return Promise.resolve(
          jsonResponse({ kind: "deleteSnapshot", created: [], requestedAt: null, note: null }, 202),
        );
      }
      if (url.pathname === PATH) return Promise.resolve(jsonResponse(detail()));
      return Promise.resolve(new Response(null, { status: 404 }));
    });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    const trigger = await within(bar).findByRole("button", { name: /^Delete/ });
    await user.click(trigger);
    await user.click(within(bar).getByRole("button", { name: "Request deletion" }));
    await waitFor(() => {
      expect(within(bar).queryByRole("dialog", { name: "Delete" })).not.toBeInTheDocument();
    });
    expect(trigger).toHaveFocus();
  });

  it("refuses to guess the consequence when the CR sets no deletion policy", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ row: row({ deletionPolicy: null }) })) });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /^Delete/ }));
    const confirm = within(bar).getByRole("dialog", { name: "Delete" });
    expect(confirm).toHaveTextContent("not set (the operator decides)");
    expect(confirm).toHaveTextContent(/cannot tell which applies/);
    expect(confirm.querySelector('[data-consequence="unknown"]')).not.toBeNull();
  });

  it("renders the delete receipt's note, where a breaker hold explains itself", async () => {
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
      if (url.pathname === PATH) return Promise.resolve(jsonResponse(detail()));
      return Promise.resolve(new Response(null, { status: 404 }));
    });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /^Delete/ }));
    await user.click(within(bar).getByRole("button", { name: "Request deletion" }));
    await waitFor(() => {
      expect(within(bar).getByText(/mass-deletion breaker/)).toBeInTheDocument();
    });
    // "requested", never "deleted".
    expect(within(bar).getByText(/Delete requested/)).toBeInTheDocument();
  });

  it("asks for the pin explicitly and says that it is permanent", async () => {
    mockApi({ [PATH]: jsonResponse(detail()) });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /Snapshot now/ }));
    const pin = within(bar).getByRole("radio", { name: /Pin it/ });
    const prune = within(bar).getByRole("radio", { name: /Prune it/ });
    expect(pin).not.toBeChecked();
    expect(prune).not.toBeChecked();
    const confirm = within(bar).getByRole("button", { name: "Take a snapshot" });
    expect(confirm).toHaveAttribute("aria-disabled", "true");
    expect(confirm).toHaveAccessibleDescription("Choose whether to pin it first.");
    expect(within(bar).getByRole("dialog", { name: "Snapshot now" })).toHaveTextContent(
      /permanent: retention never removes a pinned snapshot/,
    );
    await user.click(prune);
    expect(confirm).not.toHaveAttribute("aria-disabled");
  });

  it("sends the pin the reader actually chose, with the policy's namespace", async () => {
    mockApi({
      [PATH]: jsonResponse(detail()),
      "/api/v1/actions/snapshot-now": jsonResponse(
        { kind: "snapshotNow", created: [], requestedAt: null, note: null },
        201,
      ),
    });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    await user.click(await within(bar).findByRole("button", { name: /Snapshot now/ }));
    await user.click(within(bar).getByRole("radio", { name: /Pin it/ }));
    await user.click(within(bar).getByRole("button", { name: "Take a pinned snapshot" }));
    await waitFor(() => {
      expect(snapshotNowBody()).not.toBeUndefined();
    });
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
    mountApp(OPEN);
    const del = await within(await foot()).findByRole("button", { name: /^Delete/ });
    expect(del).toHaveAttribute("aria-disabled", "true");
    await waitFor(() => {
      expect(del).toHaveAttribute("data-reason", expect.stringContaining("is not permitted"));
    });
    expect(del.getAttribute("data-reason")).toContain("Delete snapshots");
  });

  it("refuses snapshot-now for a snapshot that names no policy, and says why", async () => {
    mockApi({ [PATH]: jsonResponse(detail({ row: row({ policy: null }) })) });
    mountApp(OPEN);
    const button = await within(await foot()).findByRole("button", { name: /Snapshot now/ });
    expect(button).toHaveAttribute("aria-disabled", "true");
    expect(button).toHaveAttribute(
      "data-reason",
      expect.stringContaining("names no SnapshotPolicy"),
    );
  });

  it("says a snapshot the server no longer has may have been deleted", async () => {
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
    mountApp(OPEN);
    expect(
      await within(await drawer()).findByText(/No Snapshot named nightly-29 in media/),
    ).toBeInTheDocument();
  });

  it("renders a 403 as not permitted rather than as a missing snapshot", async () => {
    mockApi({
      [PATH]: problemResponse(forbiddenProblem("read this snapshot", PATH)),
    });
    mountApp(OPEN);
    expect(
      await within(await drawer()).findByText(/Not permitted to view Snapshot nightly-29/),
    ).toBeInTheDocument();
  });

  it("announces the skeleton while the snapshot is in flight", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp(OPEN);
    expect(await within(await drawer()).findByRole("status", { busy: true })).toBeInTheDocument();
  });

  it("renders a phase this build does not know without crashing", async () => {
    mockApi({
      [PATH]: jsonResponse(detail({ row: row({ phase: { unknown: { raw: "Weird" } } }) })),
    });
    mountApp(OPEN);
    const verdict = await screen.findByRole("status", { name: "Snapshot verdict" });
    expect(verdict).toHaveTextContent("Weird");
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "unknown");
  });
});
