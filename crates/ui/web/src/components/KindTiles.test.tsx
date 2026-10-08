import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { OverviewView } from "../api/types";
import { renderWithRouter } from "../test-utils";
import { KindTiles } from "./KindTiles";

const overview: OverviewView = {
  snapshotWindowHours: 24,
  generatedAt: "2026-10-08T12:00:00Z",
  kinds: [
    {
      kind: "repository",
      total: 2,
      byHealth: [
        { health: "failed", count: 1 },
        { health: "healthy", count: 1 },
      ],
    },
    { kind: "clusterRepository", total: 1, byHealth: [{ health: "healthy", count: 1 }] },
    { kind: "maintenance", total: 1, byHealth: [{ health: "healthy", count: 1 }] },
    { kind: "snapshotPolicy", total: 2, byHealth: [{ health: "healthy", count: 2 }] },
    { kind: "snapshotSchedule", total: 0, byHealth: [] },
    {
      kind: "snapshot",
      total: 4,
      byHealth: [
        { health: "failed", count: 1 },
        { health: "unknown", count: 1 },
        { health: "healthy", count: 2 },
      ],
    },
    { kind: "restore", total: 0, byHealth: [] },
    { kind: "repositoryReplication", total: 1, byHealth: [{ health: "pending", count: 1 }] },
    { kind: "snapshotReplication", total: 1, byHealth: [{ health: "healthy", count: 1 }] },
  ],
};

describe("KindTiles", () => {
  it("shows seven tiles, merging the two repository kinds and the two replication kinds", async () => {
    renderWithRouter(<KindTiles overview={overview} namespace={undefined} />);
    const list = await screen.findByRole("list", { name: "Fleet by kind" });
    const tiles = within(list).getAllByRole("link");
    expect(tiles.map((t) => t.querySelector(".kind-tile__label")?.textContent)).toEqual([
      "Repositories",
      "Policies",
      "Schedules",
      "Snapshots · 24h",
      "Restores",
      "Replications",
      "Maintenance",
    ]);
    expect(within(list).getByRole("link", { name: /Repositories/ })).toHaveTextContent("3");
    expect(within(list).getByRole("link", { name: /Replications/ })).toHaveTextContent("2");
  });

  it("says each tile's breakdown in words, marks a failing tile, and counts an unknown as unknown", async () => {
    renderWithRouter(<KindTiles overview={overview} namespace={undefined} />);
    const snapshots = await screen.findByRole("link", { name: /Snapshots/ });
    expect(snapshots).toHaveAttribute("data-failing", "true");
    expect(snapshots).toHaveTextContent("1 failed");
    expect(snapshots).toHaveTextContent("1 unknown");
    expect(snapshots).toHaveTextContent("2 ok");
    expect(snapshots.querySelector(".status-bar")).toHaveAttribute("aria-hidden", "true");
    const policies = screen.getByRole("link", { name: /Policies/ });
    expect(policies).not.toHaveAttribute("data-failing");
  });

  it("links each tile to its list, the failing repositories tile to the failing filter, keeping the scope", async () => {
    renderWithRouter(<KindTiles overview={overview} namespace="media" />);
    expect(await screen.findByRole("link", { name: /Repositories/ })).toHaveAttribute(
      "href",
      "/repositories?health=failed&namespace=media",
    );
    expect(screen.getByRole("link", { name: /Schedules/ })).toHaveAttribute(
      "href",
      "/schedules?namespace=media",
    );
    expect(screen.getByRole("link", { name: /Schedules/ })).toHaveTextContent("none in scope");
  });
});
