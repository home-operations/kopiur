import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, PolicyDetail, SnapshotNowBody, SuspendBody } from "../../../api/types";
import { snapshotPhaseLamp } from "../../snapshot";
import {
  bodyRows,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  meWith,
  mockApi,
  mountApp,
  nth,
  problemResponse,
  sentBody,
  unletteredLamps,
} from "../../../test-utils";

const PATH = "/api/v1/policies/media/nightly";

const detail: PolicyDetail = {
  row: {
    namespace: "media",
    name: "nightly",
    repositories: ["Repository/media/nas", "ClusterRepository/shared"],
    multiRepo: true,
    suspended: false,
    lastSuccessfulSnapshot: "2026-09-09T02:04:00Z",
    lastVerified: "2026-09-07T04:00:00Z",
    activeSnapshotCount: 42,
  },
  identity: "kopiur@media:/data",
  sources: ["/data", "/config"],
  retention: { keepDaily: 7, keepMonthly: 6, keepLatest: null },
  verification: [
    { repository: "Repository/media/nas", lastVerified: "2026-09-07T04:00:00Z" },
    { repository: "ClusterRepository/shared", lastVerified: null },
  ],
  schedules: [
    {
      namespace: "media",
      name: "nightly-cron",
      policy: "nightly",
      policySelector: null,
      cron: "H 2 * * *",
      timezone: "Europe/Berlin",
      suspended: false,
      lastFire: "2026-09-09T02:17:00Z",
      nextFire: "2026-09-10T02:17:00Z",
      lastSnapshot: "nightly-20260909",
      consecutiveFailures: 0,
    },
  ],
  recentSnapshots: [
    {
      namespace: "media",
      name: "nightly-20260909",
      phase: "succeeded",
      origin: null,
      policy: "nightly",
      repository: "Repository/media/nas",
      kopiaSnapshotId: "k1",
      identity: "kopiur@media:/data",
      startTime: "2026-09-09T02:04:00Z",
      endTime: "2026-09-09T02:09:00Z",
      sizeBytes: 1024,
      bytesNew: null,
      filesTotal: 12,
      filesFailed: 0,
      pinned: false,
      deletionPolicy: "Delete",
      copiedFrom: null,
    },
  ],
  gates: [],
  conditions: [
    {
      type: "Ready",
      status: "True",
      reason: "Reconciled",
      message: "The policy is resolved.",
      lastTransitionTime: "2026-09-09T02:00:00Z",
    },
  ],
};

const OPEN = "/doctor?inspect=snapshot-policy/media/nightly";

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

describe("Policy drawer", () => {
  it("leads with the verdict and keeps the recipe a tab away", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    expect(await screen.findByRole("status", { name: "Policy verdict" })).toHaveTextContent(
      "fans out into 2 repositories",
    );
    const panel = await drawer();
    expect(
      within(panel)
        .getAllByRole("tab")
        .map((t) => t.textContent),
    ).toEqual(["Backs up", "Retention", "Verification", "Snapshots 1", "Conditions"]);
    const tab = within(panel).getByRole("tabpanel");
    expect(within(tab).getByRole("region", { name: "What it backs up" })).toBeInTheDocument();
    expect(within(tab).getByRole("region", { name: "Where it writes" })).toBeInTheDocument();
  });

  it("shows the resolved identity and the sources the last run used", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const region = within(await openTab("Backs up")).getByRole("region", {
      name: "What it backs up",
    });
    expect(region).toHaveTextContent("kopiur@media:/data");
    expect(region).toHaveTextContent("/config");
  });

  it("lists the GFS rules the policy sets and no rule it leaves unset", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const rules = within(await openTab("Retention")).getByRole("list", { name: "Retention rules" });
    expect(rules).toHaveTextContent("keepDaily");
    expect(rules).toHaveTextContent("keepMonthly");
    // `keepLatest: null` is unset, not zero, and must not appear at all.
    expect(rules).not.toHaveTextContent("keepLatest");
  });

  it("says no GFS retention is configured rather than that snapshots will be pruned", async () => {
    mockApi({ [PATH]: jsonResponse({ ...detail, retention: null }) });
    mountApp(OPEN);
    const region = within(await openTab("Retention")).getByRole("region", { name: "Retention" });
    expect(region).toHaveTextContent("No GFS retention is configured");
    expect(region).not.toHaveTextContent("will be pruned");
  });

  it("names a repository that has never been verified", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const region = within(await openTab("Verification")).getByRole("region", {
      name: "Verification",
    });
    expect(within(region).getByText("never verified")).toBeInTheDocument();
  });

  it("chains the schedules that fire it to it, with each one's own pill", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    const fired = within(chain).getByRole("listitem", { name: "Fired by" });
    const ref = within(fired).getByRole("link", { name: /nightly-cron/ });
    expect(ref).toHaveAttribute("data-kind", "snapshot-schedule");
    expect(ref.querySelector(".health")).not.toBeNull();
  });

  it("lists recent runs, lamping the phase exactly as the snapshots ledger does", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const recent = within(await openTab("Snapshots")).getByRole("table", {
      name: "Recent snapshots",
    });
    const row = nth(bodyRows(recent), 0);
    expect(row).toHaveTextContent("nightly-20260909");
    const lamp = row.querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "healthy");
    expect(lamp).toHaveTextContent(snapshotPhaseLamp("succeeded").word);
    expect(lamp?.querySelector("svg")).not.toBeNull();
  });

  it("lamps a failed run rather than tinting the word", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        recentSnapshots: [{ ...nth(detail.recentSnapshots, 0), phase: "failed" }],
      }),
    });
    mountApp(OPEN);
    const recent = within(await openTab("Snapshots")).getByRole("table", {
      name: "Recent snapshots",
    });
    const lamp = nth(bodyRows(recent), 0).querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "failed");
    expect(lamp).toHaveTextContent("Failed");
    expect(lamp?.querySelector("svg")).not.toBeNull();
  });

  it("leaves no health colour carried by hue alone", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        lastVerified: null,
        recentSnapshots: [{ ...nth(detail.recentSnapshots, 0), phase: "failed" }],
      }),
    });
    mountApp(OPEN);
    await openTab("Snapshots");
    expect(unletteredLamps(document.body)).toEqual([]);
  });

  it("renders a phase this bundle has never seen as the operator wrote it", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        recentSnapshots: [
          { ...nth(detail.recentSnapshots, 0), phase: { unknown: { raw: "Quiescing" } } },
        ],
      }),
    });
    mountApp(OPEN);
    expect(
      within(await openTab("Snapshots")).getByRole("table", { name: "Recent snapshots" }),
    ).toHaveTextContent("Quiescing");
  });

  it("opens each recent run in the drawer, striped as a snapshot", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const row = nth(
      bodyRows(within(await openTab("Snapshots")).getByRole("table", { name: "Recent snapshots" })),
      0,
    );
    expect(row).toHaveAttribute("data-kind", "snapshot");
    expect(row.querySelector("td.has-stripe .kind-chip svg")).not.toBeNull();
    expect(
      within(row).getByRole("link", { name: "nightly-20260909" }).getAttribute("href"),
    ).toMatch(/inspect=snapshot%2Fmedia%2Fnightly-20260909$/);
  });

  it("takes a snapshot under this policy, with pin answered explicitly", async () => {
    mockApi({
      [PATH]: jsonResponse(detail),
      "/api/v1/actions/snapshot-now": jsonResponse({
        kind: "Snapshot",
        created: [{ namespace: "media", name: "nightly-now" }],
        requestedAt: null,
        note: null,
      } satisfies ActionReceipt),
    });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    const trigger = await within(bar).findByRole("button", { name: "Snapshot now" });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.click(within(bar).getByRole("radio", { name: /Prune it under/ }));
    await user.click(within(bar).getByRole("button", { name: "Take the snapshot" }));
    const expected: SnapshotNowBody = {
      namespace: "media",
      policy: "nightly",
      tags: [],
      pin: false,
    };
    expect(sentBody("/api/v1/actions/snapshot-now")).toEqual(expected);
  });

  it("suspends the policy with an explicit value in its own namespace", async () => {
    mockApi({
      [PATH]: jsonResponse(detail),
      "/api/v1/actions/suspend": jsonResponse({
        kind: "SnapshotPolicy",
        created: [],
        requestedAt: null,
        note: "SnapshotPolicy/nightly in namespace media is now suspended",
      } satisfies ActionReceipt),
    });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    const trigger = await within(bar).findByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.click(within(bar).getByRole("button", { name: "Suspend nightly" }));
    const expected: SuspendBody = {
      kind: "policy",
      name: "nightly",
      namespace: "media",
      suspend: true,
    };
    expect(sentBody("/api/v1/actions/suspend")).toEqual(expected);
    expect(await within(bar).findByText(/is now suspended/)).toBeInTheDocument();
  });

  it("asks only one question at a time", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const user = userEvent.setup();
    const bar = await foot();
    const snapshot = await within(bar).findByRole("button", { name: "Snapshot now" });
    await waitFor(() => {
      expect(snapshot).not.toHaveAttribute("aria-disabled");
    });
    await user.click(snapshot);
    expect(within(bar).getByRole("group", { name: "Snapshot now" })).toBeInTheDocument();
    await user.click(within(bar).getByRole("button", { name: "Suspend" }));
    expect(within(bar).queryByRole("group", { name: "Snapshot now" })).toBeNull();
    expect(within(bar).getByRole("group", { name: "Suspend" })).toBeInTheDocument();
  });

  it("keeps both controls visible and explained when the user may do neither", async () => {
    mockApi({ [PATH]: jsonResponse(detail), "/api/v1/me": meWith({}) });
    mountApp(OPEN);
    const bar = await foot();
    const snapshot = await within(bar).findByRole("button", { name: "Snapshot now" });
    await waitFor(() => {
      expect(snapshot).toHaveAttribute("aria-disabled", "true");
    });
    expect(within(bar).getByRole("button", { name: "Suspend" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    expect(snapshot).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Snapshot now is not permitted"),
    );
  });

  it("puts the gates that hold the policy at the top, with the operator's own text", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        gates: [
          {
            condition: "Ready",
            reason: "RepositoryNotReady",
            severity: "error",
            message: "Repository/media/nas is not ready.",
          },
        ],
      }),
    });
    mountApp(OPEN);
    const region = await screen.findByRole("region", { name: "Gates holding this policy" });
    expect(region).toHaveTextContent("RepositoryNotReady");
    expect(region).toHaveTextContent("Repository/media/nas is not ready.");
    // The gate's lamp is the verdict's too.
    const verdict = screen.getByRole("status", { name: "Policy verdict" });
    expect(verdict.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "failed");
  });

  it("renders the not-permitted state for a 403, with no retry", async () => {
    mockApi({ [PATH]: problemResponse(forbiddenProblem("The policy was refused.", PATH)) });
    mountApp(OPEN);
    const panel = await drawer();
    expect(await within(panel).findByRole("alert")).toHaveTextContent("The policy was refused");
    expect(within(panel).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("says a policy the server no longer has may have been deleted", async () => {
    mockApi({
      [PATH]: problemResponse({
        type: "urn:kopiur:problem:not-found",
        title: "Not Found",
        status: 404,
        detail: "There is no SnapshotPolicy called nightly in namespace media.",
        what: "There is no SnapshotPolicy called nightly in namespace media.",
        why: "It was deleted or renamed.",
        fix: "reload the policies list to see what the cluster holds now",
        instance: PATH,
        kubeReason: "NotFound",
      }),
    });
    mountApp(OPEN);
    expect(
      await within(await drawer()).findByText(/No SnapshotPolicy named nightly in media/),
    ).toBeInTheDocument();
  });

  it("shows a skeleton while the policy loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp(OPEN);
    expect(await within(await drawer()).findByRole("status", { busy: true })).toBeInTheDocument();
  });
});

describe("Policy drawer — head", () => {
  it("lamps the verdict even when the policy is active", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const verdict = await screen.findByRole("status", { name: "Policy verdict" });
    const lamp = verdict.querySelector(".verdict__lamp");
    expect(lamp).toHaveAttribute("data-health", "healthy");
    expect(lamp?.querySelector("svg")).not.toBeNull();
  });

  it("names every repository it writes into, a fan-out included", async () => {
    mockApi({ [PATH]: jsonResponse(detail), "/api/v1/repositories": jsonResponse([]) });
    mountApp(OPEN);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    const writes = within(chain).getByRole("listitem", { name: "Writes into" });
    expect(within(writes).getByRole("link", { name: /nas/ }).getAttribute("href")).toMatch(
      /inspect=repository%2Fmedia%2Fnas$/,
    );
    expect(
      within(writes)
        .getByRole("link", { name: /shared/ })
        .getAttribute("href"),
    ).toMatch(/inspect=cluster-repository%2Fshared$/);
    expect(chain).toHaveTextContent("this policy");
  });

  it("keeps a repository key it cannot read as its text, never claiming there is none", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        row: { ...detail.row, repositories: ["Vault/media/nas"] },
      }),
    });
    mountApp(OPEN);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    const writes = within(chain).getByRole("listitem", { name: "Writes into" });
    expect(writes).toHaveTextContent("Vault/media/nas");
  });

  it("shows the four facts, loud when it has never been verified", async () => {
    mockApi({
      [PATH]: jsonResponse({ ...detail, row: { ...detail.row, lastVerified: null } }),
    });
    mountApp(OPEN);
    await screen.findByRole("status", { name: "Policy verdict" });
    const facts = (await drawer()).querySelector<HTMLElement>("dl.stats");
    expect(facts?.querySelectorAll(".stats__item")).toHaveLength(4);
    expect(facts).toHaveTextContent("Live snapshots");
    expect(facts).toHaveTextContent("42");
    expect(facts).toHaveTextContent("never verified");
    expect(facts).toHaveTextContent("Schedules");
  });
});
