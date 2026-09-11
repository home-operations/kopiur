import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, PolicyDetail, SnapshotNowBody, SuspendBody } from "../api/types";
import { snapshotPhaseLamp } from "../components/snapshot";
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
} from "../test-utils";

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

describe("Policy detail", () => {
  it("renders the page rather than an empty outlet — the escaped route file works", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/policies/media/nightly");
    // If `policies.tsx` had become a layout, every one of these would be
    // absent and nothing would have errored.
    expect(await screen.findByRole("status", { name: "Policy verdict" })).toHaveTextContent(
      "fans out into 2 repositories",
    );
    expect(screen.getByRole("region", { name: "What it backs up" })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Where it writes" })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Conditions" })).toBeInTheDocument();
  });

  it("shows the resolved identity and the sources the last run used", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/policies/media/nightly");
    const region = await screen.findByRole("region", { name: "What it backs up" });
    expect(region).toHaveTextContent("kopiur@media:/data");
    expect(region).toHaveTextContent("/config");
  });

  it("lists the GFS rules the policy sets and no rule it leaves unset", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/policies/media/nightly");
    const rules = await screen.findByRole("list", { name: "Retention rules" });
    expect(rules).toHaveTextContent("keepDaily");
    expect(rules).toHaveTextContent("keepMonthly");
    // `keepLatest: null` is unset, not zero, and must not appear at all.
    expect(rules).not.toHaveTextContent("keepLatest");
  });

  it("says no GFS retention is configured rather than that snapshots will be pruned", async () => {
    mockApi({ [PATH]: jsonResponse({ ...detail, retention: null }) });
    mountApp("/policies/media/nightly");
    const region = await screen.findByRole("region", { name: "Retention" });
    expect(region).toHaveTextContent("No GFS retention is configured");
    expect(region).not.toHaveTextContent("will be pruned");
  });

  it("names a repository that has never been verified", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/policies/media/nightly");
    const region = await screen.findByRole("region", { name: "Verification" });
    expect(within(region).getByText("never verified")).toBeInTheDocument();
  });

  it("lists the schedules that fire it, with the cron exactly as written", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/policies/media/nightly");
    const schedules = await screen.findByRole("table", { name: "Schedules" });
    // The H token stays: the resolved slot is the operator's and rewriting it
    // would show a time the reader cannot find in their own manifest.
    expect(schedules).toHaveTextContent("H 2 * * * (Europe/Berlin)");
    expect(schedules).toHaveTextContent("nightly-cron");
  });

  it("lists recent runs, lamping the phase exactly as the snapshots ledger does", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/policies/media/nightly");
    const recent = await screen.findByRole("table", { name: "Recent snapshots" });
    const row = nth(bodyRows(recent), 0);
    expect(row).toHaveTextContent("nightly-20260909");
    // One fact, one rendering. This cell used to print the raw lowercase word
    // and tint only `failed`, so the same snapshot was a lamp on /snapshots
    // and red prose here — and here the failure was told by hue alone.
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
    mountApp("/policies/media/nightly");
    const recent = await screen.findByRole("table", { name: "Recent snapshots" });
    const lamp = nth(bodyRows(recent), 0).querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "failed");
    expect(lamp).toHaveTextContent("Failed");
    expect(lamp?.querySelector("svg")).not.toBeNull();
  });

  it("leaves no health colour on this screen carried by hue alone", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        lastVerified: null,
        recentSnapshots: [{ ...nth(detail.recentSnapshots, 0), phase: "failed" }],
      }),
    });
    mountApp("/policies/media/nightly");
    await screen.findByRole("table", { name: "Recent snapshots" });
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
    mountApp("/policies/media/nightly");
    const recent = await screen.findByRole("table", { name: "Recent snapshots" });
    expect(recent).toHaveTextContent("Quiescing");
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
    mountApp("/policies/media/nightly");
    const user = userEvent.setup();
    const trigger = await screen.findByRole("button", { name: "Snapshot now" });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.click(screen.getByRole("radio", { name: /Prune it under/ }));
    await user.click(screen.getByRole("button", { name: "Take the snapshot" }));
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
    mountApp("/policies/media/nightly");
    const user = userEvent.setup();
    const trigger = await screen.findByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.click(screen.getByRole("button", { name: "Suspend nightly" }));
    const expected: SuspendBody = {
      kind: "policy",
      name: "nightly",
      namespace: "media",
      suspend: true,
    };
    expect(sentBody("/api/v1/actions/suspend")).toEqual(expected);
    expect(await screen.findByRole("status", { name: "" })).toBeDefined();
  });

  it("asks only one question at a time", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/policies/media/nightly");
    const user = userEvent.setup();
    const snapshot = await screen.findByRole("button", { name: "Snapshot now" });
    await waitFor(() => {
      expect(snapshot).not.toHaveAttribute("aria-disabled");
    });
    await user.click(snapshot);
    expect(screen.getByRole("group", { name: "Snapshot now" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Suspend" }));
    expect(screen.queryByRole("group", { name: "Snapshot now" })).toBeNull();
    expect(screen.getByRole("group", { name: "Suspend" })).toBeInTheDocument();
  });

  it("keeps both controls visible and explained when the user may do neither", async () => {
    mockApi({ [PATH]: jsonResponse(detail), "/api/v1/me": meWith({}) });
    mountApp("/policies/media/nightly");
    const snapshot = await screen.findByRole("button", { name: "Snapshot now" });
    await waitFor(() => {
      expect(snapshot).toHaveAttribute("aria-disabled", "true");
    });
    expect(screen.getByRole("button", { name: "Suspend" })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    expect(snapshot).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Snapshot now is not permitted"),
    );
  });

  it("renders the gates that hold the policy, with the operator's own text", async () => {
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
    mountApp("/policies/media/nightly");
    const region = await screen.findByRole("region", { name: "Gates holding this policy" });
    expect(region).toHaveTextContent("RepositoryNotReady");
    expect(region).toHaveTextContent("Repository/media/nas is not ready.");
  });

  it("renders the not-permitted state for a 403, with a way back to the list", async () => {
    mockApi({ [PATH]: problemResponse(forbiddenProblem("The policy was refused.", PATH)) });
    mountApp("/policies/media/nightly");
    expect(await screen.findByRole("alert")).toHaveTextContent("The policy was refused");
    expect(screen.getByRole("link", { name: "All policies" })).toHaveAttribute("href", "/policies");
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the server's 404 rather than a blank page", async () => {
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
    mountApp("/policies/media/nightly");
    expect(await screen.findByRole("alert")).toHaveTextContent("no SnapshotPolicy called nightly");
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while the policy loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/policies/media/nightly");
    expect(await screen.findByRole("status", { busy: true })).toBeInTheDocument();
  });
});
