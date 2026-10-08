import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { renderWithRouter } from "../test-utils";
import { FlowLanes, type Lane } from "./FlowLanes";

const schedule = {
  namespace: "kopiur-dev",
  name: "app-data-frequent",
  policy: "app-data",
  cron: "H * * * *",
  suspended: false,
  consecutiveFailures: 0,
};

const lanes: Lane[] = [
  {
    title: "Fired by",
    label: "Fired by",
    items: [{ card: { kind: "snapshotSchedule", row: schedule } }],
    empty: "Nothing fires it.",
  },
  {
    title: "Written by",
    label: "Policies writing here",
    items: [{ ref: { kind: "snapshotPolicy", name: "app-data", namespace: "kopiur-dev" } }],
    empty: "No policy writes here.",
  },
  { title: "This repository", label: "This repository", items: [], empty: "—" },
  { title: "Copies to", label: "Copies to", items: [], empty: "Nothing copies this repository." },
];

describe("FlowLanes", () => {
  it("lays the relationships out left to right, each lane a named region", async () => {
    renderWithRouter(<FlowLanes label="Relationships" lanes={lanes} />);
    const flow = await screen.findByRole("region", { name: "Relationships" });
    const named = within(flow)
      .getAllByRole("region")
      .map((r) => r.getAttribute("aria-label"));
    expect(named).toEqual(["Fired by", "Policies writing here", "This repository", "Copies to"]);
    expect(
      within(screen.getByRole("region", { name: "Fired by" })).getByRole("article"),
    ).toHaveAttribute("data-kind", "snapshot-schedule");
    expect(
      within(screen.getByRole("region", { name: "Policies writing here" }))
        .getByRole("link", {
          name: /app-data/,
        })
        .getAttribute("href"),
    ).toMatch(/inspect=snapshot-policy%2Fkopiur-dev%2Fapp-data$/);
  });

  it("sizes its columns to the lanes it has, the object's own lane widest", async () => {
    const three: Lane[] = [nthLane(0), { ...nthLane(2), variant: "stats" }, nthLane(3)];
    renderWithRouter(<FlowLanes label="Relationships" lanes={three} />);
    const flow = await screen.findByRole("region", { name: "Relationships" });
    expect(flow.style.getPropertyValue("--lane-cols")).toBe(
      "minmax(0, 1fr) auto minmax(0, 1.2fr) auto minmax(0, 1fr)",
    );
  });

  it("says why a lane is empty, and keeps its arrows out of the accessibility tree", async () => {
    const { container } = renderWithRouter(<FlowLanes label="Relationships" lanes={lanes} />);
    expect(await screen.findByText("Nothing copies this repository.")).toBeInTheDocument();
    const arrows = container.querySelectorAll(".flow-lanes__arrow");
    expect(arrows).toHaveLength(3);
    arrows.forEach((a) => {
      expect(a).toHaveAttribute("aria-hidden", "true");
    });
  });
});

function nthLane(i: number): Lane {
  const lane = lanes[i];
  if (lane === undefined) throw new Error(`no lane ${String(i)}`);
  return lane;
}
