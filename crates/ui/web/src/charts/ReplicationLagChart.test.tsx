import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { ReplicationRow } from "../components/replication";
import { bodyRows, nth } from "../test-utils";
import { ReplicationLagChart } from "./ReplicationLagChart";

const NOW = new Date("2026-09-10T12:00:00Z");

function row(over: Partial<ReplicationRow> = {}): ReplicationRow {
  return {
    id: "replication/media/nas-to-offsite",
    kind: "RepositoryReplication",
    kindToken: "replication",
    namespace: "media",
    name: "nas-to-offsite",
    source: "media/nas",
    destination: "b2",
    destinationIsRepository: false,
    cron: "0 4 * * *",
    suspended: false,
    phase: "succeeded",
    lastReplicated: "2026-09-10T04:00:00Z",
    ...over,
  };
}

describe("ReplicationLagChart", () => {
  it("says nothing is plottable rather than drawing an empty frame", () => {
    render(<ReplicationLagChart rows={[]} now={NOW} />);
    expect(screen.getByText(/no replication has recorded a copy/i)).toBeInTheDocument();
    expect(screen.queryByRole("figure")).not.toBeInTheDocument();
  });

  it("carries every bar as a row of a real table — the number, not just the picture", () => {
    render(
      <ReplicationLagChart
        rows={[
          row(),
          row({ id: "b", name: "archive-to-offsite", lastReplicated: "2026-09-03T04:00:00Z" }),
        ]}
        now={NOW}
      />,
    );
    const table = screen.getByRole("table", { name: /replication lag/i });
    const rows = bodyRows(table);
    expect(rows).toHaveLength(2);
    // Stalest first, and the age is spelled out in the cell.
    expect(nth(rows, 0)).toHaveTextContent("media/archive-to-offsite");
    expect(nth(rows, 0)).toHaveTextContent("7d 8h");
    expect(nth(rows, 1)).toHaveTextContent("8h");
  });

  it("keeps the plot out of the accessibility tree, since the table is the spoken copy", () => {
    const { container } = render(<ReplicationLagChart rows={[row()]} now={NOW} />);
    expect(container.querySelector("svg")).toHaveAttribute("aria-hidden", "true");
  });

  it("names a copy that has never run instead of drawing it as the longest bar", () => {
    render(<ReplicationLagChart rows={[row({ lastReplicated: null })]} now={NOW} />);
    const finding = screen.getByRole("article", { name: /never been copied/i });
    expect(finding).toHaveTextContent("media/nas-to-offsite");
    expect(screen.queryByRole("figure")).not.toBeInTheDocument();
  });

  it("marks a suspended copy in the table, so a stale bar is not read as a fault", () => {
    render(<ReplicationLagChart rows={[row({ suspended: true })]} now={NOW} />);
    const table = screen.getByRole("table", { name: /replication lag/i });
    expect(within(table).getByText(/suspended/i)).toBeInTheDocument();
  });

  it("says why there is no overdue marker rather than leaving the reader to wonder", () => {
    render(<ReplicationLagChart rows={[row()]} now={NOW} />);
    expect(screen.getByText(/no controller writes a next-run time/i)).toBeInTheDocument();
  });

  it("counts rows whose instant it could not read", () => {
    render(
      <ReplicationLagChart
        rows={[row(), row({ id: "b", lastReplicated: "nonsense" })]}
        now={NOW}
      />,
    );
    expect(
      screen.getByText(/1 replication recorded a time this build could not read/i),
    ).toBeInTheDocument();
  });
});
