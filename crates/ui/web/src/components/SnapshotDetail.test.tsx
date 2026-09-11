import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { SnapshotDetail as SnapshotDetailData, SnapshotRow } from "../api/types";
import { renderWithRouter } from "../test-utils";
import { SnapshotDetail } from "./SnapshotDetail";

const NOW = new Date("2026-09-09T02:00:00Z");

const row: SnapshotRow = {
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
};

function detail(over: Partial<SnapshotDetailData> = {}): SnapshotDetailData {
  return {
    row,
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
    retentionPreview: null,
    failure: null,
    logTail: [],
    conditions: [],
    gates: [],
    browsable: true,
    browseBlocker: null,
    ...over,
  };
}

describe("SnapshotDetail", () => {
  it("leads with a verdict naming the phase and the policy", async () => {
    renderWithRouter(<SnapshotDetail detail={detail()} now={NOW} />);
    const verdict = await screen.findByRole("status", { name: "Snapshot verdict" });
    expect(verdict).toHaveTextContent("Succeeded");
    expect(verdict).toHaveTextContent("nightly");
  });

  it("renders new bytes as not reported, never as a blank or a zero", async () => {
    renderWithRouter(<SnapshotDetail detail={detail()} now={NOW} />);
    const run = await screen.findByRole("region", { name: "This run" });
    expect(run).toHaveTextContent("New bytes");
    expect(run).toHaveTextContent("not reported");
    expect(run).not.toHaveTextContent("New bytes0 B");
  });

  it("never renders an absent deletion policy as a default", async () => {
    renderWithRouter(
      <SnapshotDetail detail={detail({ row: { ...row, deletionPolicy: null } })} now={NOW} />,
    );
    const storage = await screen.findByRole("region", { name: "In the repository" });
    expect(storage).toHaveTextContent("not set (the operator decides)");
    expect(storage).not.toHaveTextContent("Deletion policyDelete");
  });

  it("says why browsing is unavailable in the blocker's own words", async () => {
    renderWithRouter(
      <SnapshotDetail
        now={NOW}
        detail={detail({
          browsable: false,
          browseBlocker: "This backup failed, so it wrote no snapshot to browse.",
        })}
      />,
    );
    expect(await screen.findByText(/wrote no snapshot to browse/)).toBeInTheDocument();
  });

  it("shows the failure with the operator's retry classification", async () => {
    renderWithRouter(
      <SnapshotDetail
        now={NOW}
        detail={detail({
          row: { ...row, phase: "failed" },
          failure: {
            kopiaErrorClass: "RepositoryUnreachable",
            message: "dial tcp: i/o timeout",
            exitCode: 1,
            retryRecommended: true,
            op: "snapshot",
          },
        })}
      />,
    );
    const finding = await screen.findByRole("article", { name: "RepositoryUnreachable" });
    expect(finding).toHaveTextContent("dial tcp: i/o timeout");
    expect(finding).toHaveTextContent(/worth retrying/);
    expect(finding).toHaveTextContent("exit 1");
  });

  it("shows the redacted log tail the server sent, verbatim", async () => {
    renderWithRouter(
      <SnapshotDetail now={NOW} detail={detail({ logTail: ["uploading /data", "done"] })} />,
    );
    expect(await screen.findByLabelText("Last lines of the mover log")).toHaveTextContent(
      "uploading /data",
    );
  });

  it("says the operator wrote no conditions rather than implying health", async () => {
    renderWithRouter(<SnapshotDetail detail={detail()} now={NOW} />);
    expect(
      await screen.findByText(/has not been reconciled — not that it is healthy/),
    ).toBeInTheDocument();
  });

  it("renders the retention section the route hands it, and nothing of its own", async () => {
    renderWithRouter(
      <SnapshotDetail detail={detail()} now={NOW} retention={<p>the plan goes here</p>} />,
    );
    const section = await screen.findByRole("region", { name: "Retention" });
    expect(section).toHaveTextContent("the plan goes here");
  });

  it("renders with no query client at all, because the route owns every hook", async () => {
    // The whole point of taking actions and retention as props: this file is
    // presentation, and a test of it needs no fetch mock.
    renderWithRouter(<SnapshotDetail detail={detail()} now={NOW} />);
    expect(await screen.findByRole("status", { name: "Snapshot verdict" })).toBeInTheDocument();
  });
});
