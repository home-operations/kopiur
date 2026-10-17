import {
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRouter,
} from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { PolicyDetail, SnapshotRow } from "../../../api/types";
import { bodyRows, nth } from "../../../test-utils";
import { SnapshotsTab } from "./PolicyTabs";

function snapshot(name: string): SnapshotRow {
  return {
    namespace: "media",
    name,
    phase: "succeeded",
    origin: null,
    policy: "nightly",
    repository: "Repository/media/nas",
    kopiaSnapshotId: name,
    identity: "kopiur@media:/data",
    startTime: "2026-09-09T02:04:00Z",
    endTime: "2026-09-09T02:09:00Z",
    sizeBytes: 1024,
    bytesNew: null,
    filesTotal: 12,
    filesFailed: 0,
    pinned: false,
    deletionPolicy: "Delete",
    copiedFrom: null,
  };
}

const detail = {
  row: { namespace: "media", name: "nightly" },
  recentSnapshots: [snapshot("nightly-0"), snapshot("nightly-1"), snapshot("nightly-2")],
} as unknown as PolicyDetail;

async function mountTab() {
  const router = createRouter({
    routeTree: createRootRoute({
      component: () => <SnapshotsTab detail={detail} now={new Date("2026-09-10T00:00:00Z")} />,
    }),
    history: createMemoryHistory({ initialEntries: ["/policies?namespace=media"] }),
  });
  render(<RouterProvider router={router} />);
  const table = await screen.findByRole("table", { name: "Recent snapshots" });
  const inspect = () => (router.state.location.search as Record<string, unknown>).inspect;
  return { rows: bodyRows(table), inspect };
}

/* #583: the policy drawer's recent-snapshots table is hand-drawn, not a
   ColumnLedger, so it opts in to the row click on its own. */
describe("Policy drawer — recent snapshots, whole-row link", () => {
  it("marks each row as a link target", async () => {
    const { rows } = await mountTab();
    expect(rows).toHaveLength(3);
    for (const row of rows) {
      expect(row).toHaveClass("row--link");
      expect(row).not.toHaveAttribute("tabindex");
    }
  });

  it("opens the clicked row's snapshot from a plain cell, for the first and last rows", async () => {
    const { rows, inspect } = await mountTab();
    await userEvent.click(within(nth(rows, 0)).getByText("1.0 KiB"));
    await waitFor(() => {
      expect(inspect()).toBe("snapshot/media/nightly-0");
    });
    await userEvent.click(within(nth(rows, 2)).getByText("1.0 KiB"));
    await waitFor(() => {
      expect(String(inspect())).toMatch(/snapshot\/media\/nightly-2$/);
    });
  });
});
