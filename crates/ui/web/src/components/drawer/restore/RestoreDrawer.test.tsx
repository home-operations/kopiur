import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { RestoreDetail } from "../../../api/types";
import {
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../../../test-utils";

const PATH = "/api/v1/restores/media/recover-db";

const detail: RestoreDetail = {
  row: {
    namespace: "media",
    name: "recover-db",
    phase: "restoring",
    sourceKind: "SnapshotRef",
    targetKind: "PvcRef",
    repository: "Repository/media/nas",
    kopiaSnapshotId: "k123",
    startTime: "2026-09-10T12:00:00Z",
    endTime: null,
    bytesRestored: 4096,
    filesRestored: 12,
    claims: [
      { pvc: "data", phase: "Restoring", message: null },
      { pvc: "config", phase: "Failed", message: "the claim is not bound" },
    ],
  },
  source: {
    resolution: "Snapshot",
    snapshot: { namespace: "media", name: "nightly-1" },
    pinnedAt: "2026-09-10T11:59:00Z",
    identity: "kopiur@media:/data",
  },
  target: { pvc: "data", pvcPrime: null },
  conditions: [
    {
      type: "Ready",
      status: "False",
      reason: "Restoring",
      message: "The mover Job is running.",
      lastTransitionTime: "2026-09-10T12:00:00Z",
    },
  ],
  failure: null,
  logTail: ["restoring /data", "wrote 12 files"],
};

const OPEN = "/doctor?inspect=restore/media/recover-db";

async function drawer() {
  return screen.findByRole("dialog");
}

async function openTab(name: string) {
  const panel = await drawer();
  await userEvent.click(await within(panel).findByRole("tab", { name: new RegExp(`^${name}`) }));
  return within(panel).getByRole("tabpanel");
}

describe("Restore drawer", () => {
  it("leads with the verdict, and keeps the source, target, log and conditions a tab away", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    expect(await screen.findByRole("status", { name: "Restore verdict" })).toHaveTextContent(
      "Restoring",
    );
    const panel = await drawer();
    expect(
      within(panel)
        .getAllByRole("tab")
        .map((t) => t.textContent),
    ).toEqual(["Source", "Target", "Log", "Conditions"]);
    // A restore has nothing to act on: no action bar at all.
    expect(panel.querySelector(".side-panel__foot")).toBeNull();
  });

  it("shows what the source was pinned to, not what the spec asks for now", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const region = within(await openTab("Source")).getByRole("region", { name: "What it reads" });
    expect(region).toHaveTextContent("media/nightly-1");
    expect(region).toHaveTextContent("k123");
    expect(region).toHaveTextContent("kopiur@media:/data");
    expect(region).toHaveTextContent("Resolved once at creation");
  });

  it("explains that NoSnapshot is a successful restore of nothing", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        source: { ...detail.source, resolution: "NoSnapshot", snapshot: null },
      }),
    });
    mountApp(OPEN);
    expect(
      within(await openTab("Source")).getByRole("region", { name: "What it reads" }),
    ).toHaveTextContent("not a failure");
  });

  it("reports bytes and files and never a percentage", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const region = within(await openTab("Target")).getByRole("region", { name: "Where it writes" });
    expect(region).toHaveTextContent("4.0 KiB");
    expect(region).toHaveTextContent("12 files");
    expect(region).not.toHaveTextContent("%");
  });

  it("lists one row per target claim, with the operator's own message", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const claims = within(await openTab("Target")).getByRole("table", { name: "Claims" });
    expect(claims).toHaveTextContent("config");
    expect(claims).toHaveTextContent("the claim is not bound");
  });

  it("puts the failure at the top, with the operator's class, message and retry advice", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        row: { ...detail.row, phase: "failed" },
        failure: {
          kopiaErrorClass: "RepositoryUnreachable",
          message: "dial tcp: connection refused",
          exitCode: 1,
          retryRecommended: true,
          op: "restore",
        },
      }),
    });
    mountApp(OPEN);
    const region = await within(await drawer()).findByRole("region", { name: "Why it failed" });
    expect(region).toHaveTextContent("RepositoryUnreachable");
    expect(region).toHaveTextContent("connection refused");
    expect(region).toHaveTextContent("The restore step");
    expect(region).toHaveTextContent("retry is likely to succeed");
    expect(region).toHaveTextContent("mover exit code 1");
  });

  it("renders the redacted log tail the mover wrote", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const region = within(await openTab("Log")).getByRole("region", { name: "Log tail" });
    expect(region).toHaveTextContent("wrote 12 files");
    expect(region).toHaveTextContent("redacted at the server");
  });

  it("says the mover wrote no lines rather than showing an empty block", async () => {
    mockApi({ [PATH]: jsonResponse({ ...detail, logTail: [] }) });
    mountApp(OPEN);
    const region = within(await openTab("Log")).getByRole("region", { name: "Log tail" });
    expect(within(region).getByText(/wrote no log lines/)).toBeInTheDocument();
  });

  it("says the operator wrote no conditions rather than implying health", async () => {
    mockApi({ [PATH]: jsonResponse({ ...detail, conditions: [] }) });
    mountApp(OPEN);
    expect(
      within(await openTab("Conditions")).getByText(/the operator has not reconciled this restore/),
    ).toBeInTheDocument();
  });

  it("renders the not-permitted state for a 403, with no retry", async () => {
    mockApi({ [PATH]: problemResponse(forbiddenProblem("The restore was refused.", PATH)) });
    mountApp(OPEN);
    const panel = await drawer();
    expect(await within(panel).findByRole("alert")).toHaveTextContent("The restore was refused");
    expect(within(panel).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("says a restore the server no longer has may have been deleted", async () => {
    mockApi({
      [PATH]: problemResponse({
        type: "urn:kopiur:problem:not-found",
        title: "Not Found",
        status: 404,
        detail: "There is no Restore called recover-db in namespace media.",
        what: "There is no Restore called recover-db in namespace media.",
        why: "It was deleted or renamed.",
        fix: "reload the restores list to see what the cluster holds now",
        instance: PATH,
        kubeReason: "NotFound",
      }),
    });
    mountApp(OPEN);
    expect(
      await within(await drawer()).findByText(/No Restore named recover-db in media/),
    ).toBeInTheDocument();
  });

  it("shows a skeleton while the restore loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp(OPEN);
    expect(await within(await drawer()).findByRole("status", { busy: true })).toBeInTheDocument();
  });
});

describe("Restore drawer — head", () => {
  it("chains the repository and the snapshot it reads, to it, to the claims it writes", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    const chain = await within(await drawer()).findByRole("list", { name: "Where this sits" });
    expect(
      within(chain)
        .getByRole("link", { name: /nightly-1/ })
        .getAttribute("href"),
    ).toMatch(/inspect=[^&]+%2Csnapshot%2Fmedia%2Fnightly-1$/);
    expect(within(chain).getByRole("link", { name: /nas/ }).getAttribute("href")).toMatch(
      /inspect=[^&]+%2Crepository%2Fmedia%2Fnas$/,
    );
    expect(chain).toHaveTextContent("this restore");
    expect(within(chain).getByRole("listitem", { name: "Writes into" })).toHaveTextContent("data");
  });

  it("shows what it restored as the four facts, never a percentage", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp(OPEN);
    await screen.findByRole("status", { name: "Restore verdict" });
    const facts = (await drawer()).querySelector<HTMLElement>("dl.stats");
    expect(facts?.querySelectorAll(".stats__item")).toHaveLength(4);
    expect(facts).toHaveTextContent("4.0 KiB");
    expect(facts).toHaveTextContent("12");
    expect(facts).toHaveTextContent("Started");
    expect(facts).not.toHaveTextContent("%");
  });
});
