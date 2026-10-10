import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type {
  ActionReceipt,
  MaintenanceRow,
  MaintenanceRunBody,
  ReplicationsView,
  ScheduleRow,
  SuspendBody,
} from "../../api/types";
import { cssRules, readStyles } from "../../testing/css";
import { jsonResponse, mockApi, mountApp, sentBody } from "../../test-utils";

const schedule: ScheduleRow = {
  namespace: "media",
  name: "nightly-cron",
  policy: "nightly",
  cron: "H 2 * * *",
  suspended: false,
  consecutiveFailures: 0,
};

const maintenance: MaintenanceRow = {
  namespace: "media",
  name: "nas-maintenance",
  repository: "Repository/media/nas",
  managedByRepository: true,
  quick: { consecutiveFailures: 0 },
  full: { consecutiveFailures: 0 },
};

const replications: ReplicationsView = {
  repository: [],
  snapshot: [
    {
      namespace: "media",
      name: "offsite",
      source: "Repository/media/nas",
      destination: "ClusterRepository/shared",
      cron: "0 3 * * *",
      suspended: false,
    },
  ],
};

const receipt = (kind: string): ActionReceipt => ({
  kind,
  created: [],
  requestedAt: "2026-10-08T10:00:00Z",
  note: null,
});

// A page with no tables of its own, so every control found is the drawer's.
const PAGE = "/doctor";

async function drawer(name: RegExp) {
  return screen.findByRole("dialog", { name });
}

describe("the drawer for a kind with no page of its own", () => {
  it("leads with where it sits and its stats, and draws no tabs for a single panel", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse([schedule]) });
    mountApp(`${PAGE}?inspect=snapshot-schedule/media/nightly-cron`);
    const panel = await drawer(/nightly-cron/);
    const chain = await within(panel).findByRole("list", { name: "Where this sits" });
    expect(
      panel.querySelector('dl[aria-label="SnapshotSchedule nightly-cron at a glance"]'),
    ).not.toBeNull();
    expect(within(chain).getByRole("listitem", { name: "Fires" })).toBeInTheDocument();
    expect(within(panel).queryByRole("tablist")).toBeNull();
    expect(within(panel).getByText("Cron")).toBeInTheDocument();
  });

  it("suspends a schedule from the drawer's action bar", async () => {
    mockApi({
      "/api/v1/schedules": jsonResponse([schedule]),
      "/api/v1/actions/suspend": jsonResponse(receipt("SnapshotSchedule")),
    });
    mountApp(`${PAGE}?inspect=snapshot-schedule/media/nightly-cron`);
    const user = userEvent.setup();
    const panel = await drawer(/nightly-cron/);
    const foot = await waitFor(() => {
      const f = panel.querySelector<HTMLElement>(".side-panel__foot");
      if (f === null) throw new Error("no action bar yet");
      return f;
    });
    const trigger = within(foot).getByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    // Escape answers the open question, not the drawer.
    await user.keyboard("{Escape}");
    expect(within(foot).queryByRole("dialog", { name: "Suspend" })).toBeNull();
    expect(screen.getByRole("dialog", { name: /nightly-cron/ })).toBeInTheDocument();
    await user.click(trigger);
    await user.click(within(foot).getByRole("button", { name: "Suspend nightly-cron" }));
    const expected: SuspendBody = {
      kind: "schedule",
      name: "nightly-cron",
      namespace: "media",
      suspend: true,
    };
    expect(sentBody("/api/v1/actions/suspend")).toEqual(expected);
    expect(await within(foot).findByRole("status")).toHaveTextContent(/Requested/);
  });

  it("asks for a maintenance run from the drawer", async () => {
    mockApi({
      "/api/v1/maintenance": jsonResponse([maintenance]),
      "/api/v1/actions/maintenance-run": jsonResponse(receipt("Maintenance")),
    });
    mountApp(`${PAGE}?inspect=maintenance/media/nas-maintenance`);
    const user = userEvent.setup();
    const panel = await drawer(/nas-maintenance/);
    const trigger = await within(panel).findByRole("button", { name: /^Run maintenance/ });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.click(within(panel).getByRole("button", { name: "Request the run" }));
    const sent = sentBody("/api/v1/actions/maintenance-run") as MaintenanceRunBody;
    expect(sent).toMatchObject({ namespace: "media", name: "nas-maintenance" });
  });

  it("offers a replication both a run and a suspend, one question at a time", async () => {
    mockApi({ "/api/v1/replications": jsonResponse(replications) });
    mountApp(`${PAGE}?inspect=snapshot-replication/media/offsite`);
    const user = userEvent.setup();
    const panel = await drawer(/offsite/);
    const run = await within(panel).findByRole("button", { name: /^Run/ });
    const suspend = within(panel).getByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(run).not.toHaveAttribute("aria-disabled");
    });
    await user.click(run);
    expect(within(panel).getAllByRole("dialog")).toHaveLength(1);
    await user.click(suspend);
    expect(within(panel).getAllByRole("dialog")).toHaveLength(1);
    expect(within(panel).getByRole("dialog", { name: "Suspend" })).toBeInTheDocument();
  });

  it("never scrolls the action buttons out of view: only a tall question scrolls", () => {
    const rules = cssRules(readStyles());
    const foot = rules
      .filter((r) => r.selector === ".side-panel__foot")
      .map((r) => r.body)
      .join("\n");
    expect(foot).not.toMatch(/overflow(-y)?:\s*(auto|scroll)/);
    expect(foot).not.toMatch(/max-height/);
    const confirm = rules.find((r) => r.selector === ".side-panel__foot .action__confirm");
    expect(confirm?.body).toMatch(/max-height/);
    expect(confirm?.body).toMatch(/overflow-y:\s*auto/);
  });

  it("leaves no gap above the buttons for a receipt that has not arrived", () => {
    const rule = cssRules(readStyles()).find((r) => r.selector === ".drawer-actions > :empty");
    expect(rule?.body).toMatch(/display:\s*none/);
  });

  it("stacks an open confirmation and its receipt above the action buttons", () => {
    const rules = cssRules(readStyles());
    const confirm = rules.find(
      (r) => r.selector === ".side-panel__foot .action-bar > .action > .action__confirm",
    );
    expect(confirm?.body).toMatch(/order:\s*-/);
  });
});
