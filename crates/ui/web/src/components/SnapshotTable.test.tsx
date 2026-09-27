import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { SnapshotRow } from "../api/types";
import { bodyRows, nth, renderWithRouter } from "../test-utils";
import { SnapshotTable } from "./SnapshotTable";

const nightly: SnapshotRow = {
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

function table() {
  return screen.findByRole("table", { name: "Snapshots" });
}

const NOW = new Date("2026-09-09T02:00:00Z");

describe("SnapshotTable", () => {
  it("shows the name, namespace, phase, origin, policy, repository and size of a row", async () => {
    renderWithRouter(<SnapshotTable rows={[nightly]} now={NOW} />);
    const row = nth(bodyRows(await table()), 0);
    expect(row).toHaveTextContent("nightly-29");
    expect(row).toHaveTextContent("media");
    expect(row.querySelector(".health")).toHaveTextContent("Succeeded");
    expect(row).toHaveTextContent("Scheduled");
    expect(row).toHaveTextContent("media/nas");
    expect(row).toHaveTextContent("941.9 MiB");
  });

  it("shows how long the run took, from the row's own two instants", async () => {
    renderWithRouter(<SnapshotTable rows={[nightly]} now={NOW} />);
    expect(nth(bodyRows(await table()), 0)).toHaveTextContent("4m");
  });

  it("leaves the duration empty for a run that has not finished, never a zero", async () => {
    renderWithRouter(
      <SnapshotTable rows={[{ ...nightly, phase: "running", endTime: null }]} now={NOW} />,
    );
    const row = nth(bodyRows(await table()), 0);
    expect(row.querySelector(".health")).toHaveTextContent("Running");
    expect(row).not.toHaveTextContent("0s");
  });

  it("marks a pinned snapshot in the object cell, because a pin outranks retention", async () => {
    renderWithRouter(<SnapshotTable rows={[{ ...nightly, pinned: true }]} now={NOW} />);
    expect(nth(bodyRows(await table()), 0)).toHaveTextContent("pinned");
  });

  it("names the files kopia could not read, and says nothing when there are none", async () => {
    renderWithRouter(
      <SnapshotTable
        rows={[
          { ...nightly, filesFailed: 3 },
          { ...nightly, name: "clean" },
        ]}
        now={NOW}
      />,
    );
    const rows = bodyRows(await table());
    expect(nth(rows, 0)).toHaveTextContent("3 failed");
    expect(nth(rows, 1)).not.toHaveTextContent("failed");
  });

  it("renders a phase this build does not know as the operator's own word", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    renderWithRouter(
      <SnapshotTable rows={[{ ...nightly, phase: { unknown: { raw: "Weird" } } }]} now={NOW} />,
    );
    const row = nth(bodyRows(await table()), 0);
    expect(row.querySelector(".health")).toHaveTextContent("Weird");
    expect(row.querySelector(".health")).toHaveAttribute("data-health", "unknown");
    warn.mockRestore();
  });

  it("keeps the server's order rather than sorting client-side", async () => {
    // The window the server paged is the order it sorted in; re-sorting here
    // would make page 2 repeat rows from page 1.
    renderWithRouter(
      <SnapshotTable
        rows={[
          { ...nightly, name: "b", startTime: "2026-09-01T00:00:00Z" },
          { ...nightly, name: "a", startTime: "2026-09-08T00:00:00Z" },
        ]}
        now={NOW}
      />,
    );
    const rows = bodyRows(await table());
    expect(nth(rows, 0)).toHaveTextContent("b");
    expect(nth(rows, 1)).toHaveTextContent("a");
  });

  it("does not carry a new-bytes column, which no controller would ever fill", async () => {
    renderWithRouter(<SnapshotTable rows={[nightly]} now={NOW} />);
    const headers = (await table()).querySelectorAll("th");
    expect([...headers].map((h) => h.textContent)).not.toContain("New bytes");
  });
});
