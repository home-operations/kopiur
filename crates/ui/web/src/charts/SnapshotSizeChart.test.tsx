import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { SnapshotRow } from "../api/types";
import { bodyRows, nth } from "../test-utils";
import { SnapshotSizeChart } from "./SnapshotSizeChart";

function row(over: Partial<SnapshotRow> = {}): SnapshotRow {
  return {
    namespace: "media",
    name: "nightly-1",
    phase: "succeeded",
    origin: "scheduled",
    policy: "nightly",
    repository: "media/nas",
    kopiaSnapshotId: "k-1",
    identity: "kopiur@media:/data",
    startTime: "2026-09-07T01:00:00Z",
    endTime: "2026-09-07T01:04:00Z",
    sizeBytes: 1024,
    bytesNew: null,
    filesTotal: 5,
    filesFailed: null,
    pinned: false,
    deletionPolicy: "Delete",
    copiedFrom: null,
    ...over,
  };
}

const history = [
  row({ name: "n1", endTime: "2026-09-07T01:04:00Z", sizeBytes: 1024 }),
  row({ name: "n2", endTime: "2026-09-08T01:04:00Z", sizeBytes: 2048 }),
  row({ name: "n3", endTime: "2026-09-09T01:04:00Z", sizeBytes: 4096 }),
];

describe("SnapshotSizeChart", () => {
  it("draws one chart per policy rather than one chart with a line per policy", () => {
    render(
      <SnapshotSizeChart
        rows={[...history, row({ name: "w1", policy: "weekly", sizeBytes: 999 })]}
      />,
    );
    expect(screen.getAllByRole("figure")).toHaveLength(2);
    expect(screen.getByText("nightly")).toBeInTheDocument();
    expect(screen.getByText("weekly")).toBeInTheDocument();
  });

  it("reads out the newest measurement before anything is hovered", () => {
    render(<SnapshotSizeChart rows={history} />);
    const readout = screen.getByRole("status");
    expect(readout).toHaveTextContent("4.0 KiB");
    expect(readout).toHaveTextContent("n3");
    expect(readout).toHaveTextContent("(latest)");
  });

  it("reads out the point the pointer is on", () => {
    const { container } = render(<SnapshotSizeChart rows={history} />);
    const first = container.querySelector('[data-point="n1"]');
    if (first === null) {
      throw new Error("the oldest measurement should have a marker");
    }
    fireEvent.mouseEnter(first);
    const readout = screen.getByRole("status");
    expect(readout).toHaveTextContent("1.0 KiB");
    expect(readout).toHaveTextContent("n1");
    expect(readout).not.toHaveTextContent("(latest)");
  });

  it("gives every drawn point a spoken twin in a real table", () => {
    render(<SnapshotSizeChart rows={history} />);
    const table = screen.getByRole("table", { name: "Snapshot sizes for nightly" });
    const rows = bodyRows(table);
    expect(rows).toHaveLength(3);
    // Newest first in the table, oldest first on the axis — each in the order
    // its own medium reads in.
    expect(nth(rows, 0)).toHaveTextContent("n3");
  });

  it("hides the plot itself from assistive technology, since the table says it", () => {
    const { container } = render(<SnapshotSizeChart rows={history} />);
    expect(container.querySelector("svg")).toHaveAttribute("aria-hidden", "true");
  });

  it("counts the runs it could not plot instead of drawing them at zero", () => {
    render(
      <SnapshotSizeChart
        rows={[...history, row({ name: "failed", phase: "failed", sizeBytes: null })]}
      />,
    );
    const details = screen.getByRole("group");
    expect(within(details).getByText(/1 run recorded no size/)).toBeInTheDocument();
  });

  it("says nothing can be plotted rather than drawing an empty frame", () => {
    render(<SnapshotSizeChart rows={[row({ sizeBytes: null })]} />);
    expect(screen.getByText(/None of these snapshots recorded a size/)).toBeInTheDocument();
    expect(screen.queryByRole("figure")).not.toBeInTheDocument();
  });

  it("states a lone measurement as a number instead of drawing a chart of it", () => {
    // "Over time" needs a second point. One dot in an empty frame with an
    // invented axis top is a picture of nothing; the value is the whole story.
    const { container } = render(<SnapshotSizeChart rows={[row({ name: "only" })]} />);
    expect(container.querySelector(".chart__plot")).toBeNull();
    const tile = screen.getByRole("figure", { name: /Snapshot size for nightly/ });
    expect(tile).toHaveTextContent("1.0 KiB");
    expect(tile).toHaveTextContent("only");
    expect(tile).toHaveTextContent(/one run recorded a size/i);
  });

  it("draws the plot as soon as there are two measurements to join", () => {
    const { container } = render(
      <SnapshotSizeChart
        rows={[
          row({ name: "a" }),
          row({ name: "b", endTime: "2026-09-08T01:04:00Z", sizeBytes: 2048 }),
        ]}
      />,
    );
    const path = container.querySelector(".chart__line");
    expect(path?.getAttribute("d")).not.toMatch(/NaN/);
    expect(container.querySelectorAll(".chart__dot")).toHaveLength(2);
  });

  it("still gives a lone measurement its row in a table", () => {
    render(<SnapshotSizeChart rows={[row({ name: "only" })]} />);
    const table = screen.getByRole("table", { name: /Snapshot sizes for nightly/ });
    expect(bodyRows(table)).toHaveLength(1);
  });

  it("caps how many policies it draws and names the rest", () => {
    const many = ["a", "b", "c", "d", "e"].map((policy) =>
      row({ name: policy, policy, sizeBytes: 100 }),
    );
    render(<SnapshotSizeChart rows={many} maxPolicies={2} />);
    expect(screen.getAllByRole("figure")).toHaveLength(2);
    expect(screen.getByText(/3 more policies are in this list/)).toBeInTheDocument();
  });

  it("says in words why new bytes is not a second series", () => {
    render(<SnapshotSizeChart rows={history} />);
    expect(screen.getByText(/New bytes after deduplication is not charted/)).toBeInTheDocument();
    expect(screen.getByText("not reported")).toBeInTheDocument();
  });
});
