import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { PolicyDetail, ScheduleRow } from "../../api/types";
import {
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../../test-utils";

const schedule: ScheduleRow = {
  namespace: "kopiur-dev",
  name: "nightly",
  policy: "app-data",
  cron: "H 2 * * *",
  timezone: "Europe/Paris",
  suspended: false,
  lastFire: "2026-10-08T02:17:00Z",
  nextFire: "2026-10-09T02:17:00Z",
  consecutiveFailures: 0,
};

const policy: PolicyDetail = {
  row: {
    namespace: "kopiur-dev",
    name: "app-data",
    repositories: ["Repository/kopiur-dev/dev-repo"],
    multiRepo: false,
    suspended: false,
    lastSuccessfulSnapshot: "2026-10-08T02:20:00Z",
    lastVerified: null,
    activeSnapshotCount: 4,
  },
  sources: [],
  verification: [],
  schedules: [],
  recentSnapshots: [],
  gates: [],
  conditions: [],
};

describe("ResourceDrawer", () => {
  it("shows a kind with no page of its own, straight from the URL", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse([schedule]) });
    mountApp("/schedules?inspect=snapshot-schedule/kopiur-dev/nightly");
    const dialog = await screen.findByRole("dialog", { name: /nightly/ });
    expect(dialog).toHaveAttribute("data-kind", "snapshot-schedule");
    await within(dialog).findByText("H 2 * * * · Europe/Paris");
    expect(dialog.querySelector(".side-panel__status .health[data-health]")).not.toBeNull();
    const fires = within(dialog).getByRole("link", { name: /app-data/ });
    expect(fires.getAttribute("href")).toMatch(/inspect=snapshot-policy%2Fkopiur-dev%2Fapp-data/);
    expect(within(dialog).queryByRole("link", { name: "Open full page" })).toBeNull();
  });

  it("offers the full page for a kind that has one", async () => {
    mockApi({
      "/api/v1/policies": jsonResponse([policy.row]),
      "/api/v1/policies/kopiur-dev/app-data": jsonResponse(policy),
    });
    mountApp("/policies?inspect=snapshot-policy/kopiur-dev/app-data");
    const dialog = await screen.findByRole("dialog", { name: /app-data/ });
    const full = await within(dialog).findByRole("link", { name: "Open full page" });
    expect(full).toHaveAttribute("href", "/policies/kopiur-dev/app-data");
    expect(within(dialog).getByRole("link", { name: /dev-repo/ })).toBeInTheDocument();
  });

  it("says when the resource is not there, rather than showing an empty panel", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse([schedule]) });
    mountApp("/schedules?inspect=snapshot-schedule/kopiur-dev/gone");
    const dialog = await screen.findByRole("dialog", { name: /gone/ });
    expect(
      await within(dialog).findByText(/No SnapshotSchedule named gone in kopiur-dev/),
    ).toBeInTheDocument();
    expect(dialog.querySelector(".side-panel__status")).toBeNull();
  });

  it("treats a detail read's 404 as gone", async () => {
    mockApi({
      "/api/v1/policies": jsonResponse([]),
      "/api/v1/policies/kopiur-dev/app-data": problemResponse({
        type: "urn:kopiur:problem:not-found",
        title: "Not Found",
        status: 404,
        detail: "No SnapshotPolicy kopiur-dev/app-data.",
        what: "No SnapshotPolicy kopiur-dev/app-data.",
        why: "It does not exist.",
        fix: "Check the name.",
      }),
    });
    mountApp("/policies?inspect=snapshot-policy/kopiur-dev/app-data");
    const dialog = await screen.findByRole("dialog", { name: /app-data/ });
    expect(
      await within(dialog).findByText(/No SnapshotPolicy named app-data in kopiur-dev/),
    ).toBeInTheDocument();
  });

  it("says the cluster refused, never shows a healthy empty panel", async () => {
    mockApi({
      "/api/v1/schedules": problemResponse(
        forbiddenProblem("list snapshotschedules", "/api/v1/schedules"),
      ),
    });
    mountApp("/schedules?inspect=snapshot-schedule/kopiur-dev/nightly");
    const dialog = await screen.findByRole("dialog", { name: /nightly/ });
    await waitFor(() => {
      expect(dialog.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    });
    expect(dialog.querySelector(".health")).toBeNull();
  });

  it("walks to a related resource in place, and closes back to the page", async () => {
    mockApi({
      "/api/v1/schedules": jsonResponse([schedule]),
      "/api/v1/policies/kopiur-dev/app-data": jsonResponse(policy),
    });
    const { router } = mountApp("/schedules?inspect=snapshot-schedule/kopiur-dev/nightly");
    const user = userEvent.setup();
    const first = await screen.findByRole("dialog", { name: /nightly/ });
    await user.click(await within(first).findByRole("link", { name: /app-data/ }));
    expect(await screen.findByRole("dialog", { name: /app-data/ })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Close details" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(router.state.location.pathname).toBe("/schedules");
  });
});
