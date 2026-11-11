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
    const table = screen.getByRole("table", { name: /Logical snapshot sizes for nightly/ });
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
    const tile = screen.getByRole("figure", { name: /Logical snapshot size for nightly/ });
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

  it("fills the area under the line, drawn beneath it", () => {
    const { container } = render(
      <SnapshotSizeChart
        rows={[
          row({ name: "a" }),
          row({ name: "b", endTime: "2026-09-08T01:04:00Z", sizeBytes: 2048 }),
        ]}
      />,
    );
    const areas = container.querySelectorAll(".chart__area");
    expect(areas).toHaveLength(1);
    expect(areas[0]?.getAttribute("d")).toMatch(/Z$/);
    expect(areas[0]?.getAttribute("d")).not.toMatch(/NaN/);
    const line = container.querySelector(".chart__line");
    // Drawn first, so the line sits on top of its fill.
    expect(areas[0]?.compareDocumentPosition(line as Node)).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
  });

  it("still gives a lone measurement its row in a table", () => {
    render(<SnapshotSizeChart rows={[row({ name: "only" })]} />);
    const table = screen.getByRole("table", { name: /Logical snapshot sizes for nightly/ });
    expect(bodyRows(table)).toHaveLength(1);
  });

  it("caps how many series it draws and names the rest", () => {
    const many = ["a", "b", "c", "d", "e"].map((policy) =>
      row({ name: policy, policy, sizeBytes: 100 }),
    );
    render(<SnapshotSizeChart rows={many} maxSeries={2} />);
    expect(screen.getAllByRole("figure")).toHaveLength(2);
    expect(screen.getByText(/3 more series are in this list/)).toBeInTheDocument();
  });

  it("says in words why new bytes is not a second series", () => {
    render(<SnapshotSizeChart rows={history} />);
    expect(screen.getByText(/New bytes after deduplication is not charted/)).toBeInTheDocument();
    expect(screen.getByText("not reported")).toBeInTheDocument();
  });

  // #689 finding 3: a policy over 8 PVCs mints 8 Snapshots per run. Joined
  // into one line they drew a saw-tooth between unrelated volumes; each source
  // must be its own series, labelled with what it is.
  describe("a policy that backs up several sources", () => {
    const pvcs = ["mp3", "photos", "docs", "home", "media", "backup", "scratch", "music"];
    const runs = ["2026-09-07", "2026-09-08", "2026-09-09"];
    const multi = runs.flatMap((day, run) =>
      pvcs.map((pvc, i) =>
        row({
          name: `samba-${day}-pvc-${pvc}`,
          policy: "samba",
          identity: `samba@samba:/pvc/${pvc}`,
          endTime: `${day}T06:${String(10 + i).padStart(2, "0")}:00Z`,
          // Wildly different volume sizes, each growing a little per run.
          sizeBytes: (i + 1) ** 4 * 1_000_000 + run * 1000,
        }),
      ),
    );

    it("labels the quantity and the source, and says it is one of several", () => {
      render(<SnapshotSizeChart rows={multi} maxSeries={1} />);
      const figure = screen.getByRole("figure");
      expect(figure).toHaveTextContent("Logical size per snapshot");
      expect(figure).toHaveTextContent("samba@samba:/pvc/");
      expect(figure).toHaveTextContent("One of 8 sources this policy backs up");
      expect(screen.getByText(/7 more series are in this list/)).toBeInTheDocument();
    });

    it("draws the focused snapshot's own source, with only that source's runs", () => {
      const focus = {
        namespace: "media",
        name: "samba-2026-09-08-pvc-home",
        policy: "samba",
        source: "samba@samba:/pvc/home",
      };
      const { container } = render(<SnapshotSizeChart rows={multi} maxSeries={1} focus={focus} />);
      expect(screen.getByRole("figure")).toHaveTextContent("samba@samba:/pvc/home");
      // Three runs of ONE volume — not 24 points zig-zagging across eight.
      const points = [...container.querySelectorAll("[data-point]")].map((el) =>
        el.getAttribute("data-point"),
      );
      expect(points).toEqual([
        "samba-2026-09-07-pvc-home",
        "samba-2026-09-08-pvc-home",
        "samba-2026-09-09-pvc-home",
      ]);
      expect(screen.getByRole("status")).toHaveTextContent("(this snapshot)");
    });

    // PR #706 review (Greptile P2): the drawer caps at ONE chart, and the
    // focused snapshot may have no plotted point of its own — a pending or
    // failed run records no size, and an older run can sit outside the newest
    // page of history. Selecting by "the series that contains this point" then
    // fell back to the alphabetically first source: the drawer of a `home`
    // snapshot drew `backup`. The series is selected by the snapshot's own
    // (policy, source), never by its point.
    it("draws the focused snapshot's source even when that snapshot has no size", () => {
      const pending = row({
        name: "samba-2026-09-10-pvc-home",
        policy: "samba",
        identity: "samba@samba:/pvc/home",
        phase: "running",
        endTime: null,
        sizeBytes: null,
      });
      const focus = {
        namespace: "media",
        name: pending.name,
        policy: "samba",
        source: "samba@samba:/pvc/home",
      };
      render(<SnapshotSizeChart rows={[...multi, pending]} maxSeries={1} focus={focus} />);
      const figure = screen.getByRole("figure");
      expect(figure).toHaveTextContent("samba@samba:/pvc/home");
      expect(figure).not.toHaveTextContent("samba@samba:/pvc/backup");
      // No point of its own: the readout must not claim to be "this snapshot".
      expect(screen.getByRole("status")).toHaveTextContent("(latest)");
    });

    it("says the source is unavailable rather than drawing a sibling's", () => {
      const focus = {
        namespace: "media",
        name: "samba-2026-09-10-pvc-gone",
        policy: "samba",
        source: "samba@samba:/pvc/gone",
      };
      render(<SnapshotSizeChart rows={multi} maxSeries={1} focus={focus} />);
      expect(screen.queryByRole("figure")).toBeNull();
      expect(screen.getByText(/has no recorded size/)).toBeInTheDocument();
      expect(screen.queryByText(/samba@samba:\/pvc\/backup/)).toBeNull();
    });

    it("never says one-of-several for a single-source policy", () => {
      render(<SnapshotSizeChart rows={history} />);
      expect(screen.queryByText(/sources this policy backs up/)).toBeNull();
    });
  });
});
