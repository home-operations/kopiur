import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { RestoreDetail } from "../api/types";
import {
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../test-utils";

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

describe("Restore detail", () => {
  it("renders the page rather than an empty outlet — the escaped route file works", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/restores/media/recover-db");
    expect(await screen.findByRole("status", { name: "Restore verdict" })).toHaveTextContent(
      "Restoring",
    );
    expect(screen.getByRole("region", { name: "What it reads" })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Where it writes" })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Conditions" })).toBeInTheDocument();
  });

  it("shows what the source was pinned to, not what the spec asks for now", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/restores/media/recover-db");
    const region = await screen.findByRole("region", { name: "What it reads" });
    expect(region).toHaveTextContent("media/nightly-1");
    expect(region).toHaveTextContent("k123");
    expect(region).toHaveTextContent("kopiur@media:/data");
    expect(region).toHaveTextContent("never re-resolves");
  });

  it("explains that NoSnapshot is a successful restore of nothing", async () => {
    mockApi({
      [PATH]: jsonResponse({
        ...detail,
        source: { ...detail.source, resolution: "NoSnapshot", snapshot: null },
      }),
    });
    mountApp("/restores/media/recover-db");
    const region = await screen.findByRole("region", { name: "What it reads" });
    expect(region).toHaveTextContent("not a failure");
  });

  it("reports bytes and files and never a percentage", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/restores/media/recover-db");
    const region = await screen.findByRole("region", { name: "Where it writes" });
    expect(region).toHaveTextContent("4.0 KiB");
    expect(region).toHaveTextContent("12 files");
    expect(region).not.toHaveTextContent("%");
  });

  it("lists one row per target claim, with the operator's own message", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/restores/media/recover-db");
    const claims = await screen.findByRole("table", { name: "Claims" });
    expect(claims).toHaveTextContent("config");
    expect(claims).toHaveTextContent("the claim is not bound");
  });

  it("puts the failure first, with the operator's class, message and retry advice", async () => {
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
    mountApp("/restores/media/recover-db");
    const region = await screen.findByRole("region", { name: "Why it failed" });
    expect(region).toHaveTextContent("RepositoryUnreachable");
    expect(region).toHaveTextContent("connection refused");
    expect(region).toHaveTextContent("The restore step");
    expect(region).toHaveTextContent("retry likely to succeed");
    expect(region).toHaveTextContent("mover exit code 1");
  });

  it("renders the redacted log tail the mover wrote", async () => {
    mockApi({ [PATH]: jsonResponse(detail) });
    mountApp("/restores/media/recover-db");
    const region = await screen.findByRole("region", { name: "Log tail" });
    expect(region).toHaveTextContent("wrote 12 files");
    expect(region).toHaveTextContent("redacted at the server");
  });

  it("says the mover wrote no lines rather than showing an empty block", async () => {
    mockApi({ [PATH]: jsonResponse({ ...detail, logTail: [] }) });
    mountApp("/restores/media/recover-db");
    const region = await screen.findByRole("region", { name: "Log tail" });
    expect(within(region).getByText(/wrote no log lines/)).toBeInTheDocument();
  });

  it("renders the not-permitted state for a 403, with a way back to the list", async () => {
    mockApi({ [PATH]: problemResponse(forbiddenProblem("The restore was refused.", PATH)) });
    mountApp("/restores/media/recover-db");
    expect(await screen.findByRole("alert")).toHaveTextContent("The restore was refused");
    expect(screen.getByRole("link", { name: "All restores" })).toHaveAttribute("href", "/restores");
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the server's 404 rather than a blank page", async () => {
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
    mountApp("/restores/media/recover-db");
    expect(await screen.findByRole("alert")).toHaveTextContent("no Restore called recover-db");
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while the restore loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/restores/media/recover-db");
    expect(await screen.findByRole("status", { busy: true })).toBeInTheDocument();
  });
});
