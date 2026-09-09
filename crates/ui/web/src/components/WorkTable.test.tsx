import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { bodyRows, nth } from "../test-utils";
import { WorkTable, type WorkRow } from "./WorkTable";

const now = new Date("2026-09-08T12:00:00Z");

const rows: WorkRow[] = [
  {
    id: "snapshot/media/nightly-1",
    kind: "Snapshot",
    namespace: "media",
    name: "nightly-1",
    health: "failed",
    stateWord: "Stalled",
    detail: "MoverPermitted=False: namespace media has not opted in to privileged movers",
    at: "2026-09-08T11:30:00Z",
  },
  {
    id: "restore/media/r1",
    kind: "Restore",
    namespace: "media",
    name: "r1",
    health: "pending",
    detail: null,
    at: null,
  },
  {
    id: "clusterrepository/offsite",
    kind: "ClusterRepository",
    namespace: null,
    name: "offsite",
    health: "degraded",
    detail: "password Secret offsite-pw not found",
    nameLink: <a href="/repositories/cluster-repository/offsite">offsite</a>,
  },
];

describe("WorkTable", () => {
  it("renders one ledger row per object with kind, name, a lamp, the detail and the age", () => {
    render(<WorkTable caption="Stalled objects" rows={rows} now={now} />);
    const table = screen.getByRole("table", { name: "Stalled objects" });
    const trs = bodyRows(table);
    expect(trs).toHaveLength(3);

    const first = nth(trs, 0);
    expect(within(first).getByText("Snapshot")).toHaveClass("label-strip__kind");
    expect(within(first).getByText("nightly-1")).toHaveClass("label-strip__name");
    expect(first).toHaveTextContent("media");
    const lamp = first.querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "failed");
    expect(lamp).toHaveTextContent("Stalled");
    expect(lamp?.querySelector("svg")).not.toBeNull();
    expect(first).toHaveTextContent("MoverPermitted=False");
    expect(first).toHaveTextContent("30m");

    // An absent detail or time is the empty cell, never "" or "now".
    const second = nth(trs, 1);
    expect(within(second).getAllByText("-")).toHaveLength(2);
    expect(second.querySelector(".health")).toHaveTextContent("Pending");

    // A cluster-scoped object has no namespace to show, and a caller may
    // supply the name as a link.
    const third = nth(trs, 2);
    expect(within(third).getByRole("link", { name: "offsite" })).toHaveAttribute(
      "href",
      "/repositories/cluster-repository/offsite",
    );
  });

  it("drops the age column when no row has an instant to measure", () => {
    const undated = rows.map((row) => ({ ...row, at: null }));
    render(<WorkTable caption="Stalled objects" rows={undated} now={now} ageLabel="Since" />);
    const table = screen.getByRole("table", { name: "Stalled objects" });
    expect(within(table).queryByRole("columnheader", { name: "Since" })).toBeNull();
    expect(within(table).getAllByRole("columnheader")).toHaveLength(3);
    // The detail cell still shows the empty cell when it has nothing.
    expect(within(nth(bodyRows(table), 1)).getAllByText("-")).toHaveLength(1);
  });

  it("renders the caller's empty state when there are no rows", () => {
    render(
      <WorkTable
        caption="Stalled objects"
        rows={[]}
        now={now}
        empty={<p>Nothing is stalled.</p>}
      />,
    );
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.getByText("Nothing is stalled.")).toBeInTheDocument();
  });
});
