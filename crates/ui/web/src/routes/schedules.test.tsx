import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, ScheduleRow, SuspendBody } from "../api/types";
import {
  bodyRows,
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  meWith,
  mockApi,
  mountApp,
  nth,
  problemResponse,
  sentBody,
} from "../test-utils";

const SUSPEND = "/api/v1/actions/suspend";

const rows: ScheduleRow[] = [
  {
    namespace: "media",
    name: "nightly-cron",
    policy: "nightly",
    policySelector: null,
    cron: "H 2 * * *",
    timezone: "Europe/Berlin",
    suspended: false,
    lastFire: "2026-09-09T02:17:00Z",
    nextFire: "2026-09-10T02:17:00Z",
    lastSnapshot: "nightly-20260909",
    consecutiveFailures: 0,
  },
  {
    namespace: "prod",
    name: "gold-cron",
    policy: null,
    policySelector: "tier=gold",
    cron: "0 3 * * *",
    timezone: null,
    suspended: false,
    lastFire: null,
    nextFire: null,
    lastSnapshot: null,
    consecutiveFailures: 3,
  },
];

const receipt: ActionReceipt = {
  kind: "SnapshotSchedule",
  created: [],
  requestedAt: null,
  note: "SnapshotSchedule/nightly-cron in namespace media is now suspended",
};

function table() {
  return screen.findByRole("table", { name: "Schedules" });
}

describe("Schedules", () => {
  it("shows the cron as written, jitter token and timezone included", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse(rows) });
    mountApp("/schedules");
    const body = bodyRows(await table());
    expect(nth(body, 0)).toHaveTextContent("H 2 * * * (Europe/Berlin)");
    expect(nth(body, 1)).toHaveTextContent("0 3 * * *");
  });

  it("links a named policy and marks a selector as one", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse(rows) });
    mountApp("/schedules");
    const body = bodyRows(await table());
    expect(within(nth(body, 0)).getByRole("link", { name: "nightly" })).toHaveAttribute(
      "href",
      "/policies/media/nightly",
    );
    expect(nth(body, 1)).toHaveTextContent("tier=gold");
    expect(nth(body, 1)).toHaveTextContent("by selector");
  });

  it("says a schedule has never fired, and reports a failure run as a count", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse(rows) });
    mountApp("/schedules");
    const body = bodyRows(await table());
    expect(within(nth(body, 1)).getByText("never fired")).toBeInTheDocument();
    // A count, not a lamp: the operator published the number, not a verdict.
    expect(within(nth(body, 1)).getByText("3 failed runs")).toBeInTheDocument();
    expect(nth(body, 1).querySelector(".health")).toBeNull();
  });

  it("lights the suspended lamp only for a suspended schedule", async () => {
    mockApi({
      "/api/v1/schedules": jsonResponse([{ ...nth(rows, 0), suspended: true }]),
    });
    mountApp("/schedules");
    const body = bodyRows(await table());
    expect(nth(body, 0).querySelector(".health")).toHaveAttribute("data-health", "suspended");
  });

  it("suspends the row it was pressed on, with the schedule kind token", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse(rows), [SUSPEND]: jsonResponse(receipt) });
    mountApp("/schedules");
    const user = userEvent.setup();
    const body = bodyRows(await table());
    const trigger = within(nth(body, 1)).getByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.click(await screen.findByRole("button", { name: "Suspend gold-cron" }));
    const expected: SuspendBody = {
      kind: "schedule",
      name: "gold-cron",
      namespace: "prod",
      suspend: true,
    };
    expect(sentBody(SUSPEND)).toEqual(expected);
  });

  it("judges each row's control in the row's own namespace, not the page's scope", async () => {
    mockApi({
      "/api/v1/schedules": jsonResponse(rows),
      "/api/v1/me": (url) =>
        url.searchParams.get("namespace") === "media"
          ? meWith({ patchSchedules: true })
          : meWith({}),
    });
    mountApp("/schedules");
    const body = bodyRows(await table());
    const allowed = within(nth(body, 0)).getByRole("button", { name: "Suspend" });
    const refused = within(nth(body, 1)).getByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(refused).toHaveAttribute("aria-disabled", "true");
    });
    expect(allowed).not.toHaveAttribute("aria-disabled");
    expect(refused).toHaveAttribute("data-reason", expect.stringContaining("in namespace prod"));
    expect(calledPaths()).toContain("/api/v1/me?namespace=media");
    expect(calledPaths()).toContain("/api/v1/me?namespace=prod");
  });

  it("keeps one confirmation open at a time across the whole ledger", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse(rows), [SUSPEND]: jsonResponse(receipt) });
    mountApp("/schedules");
    const user = userEvent.setup();
    const body = bodyRows(await table());
    const first = within(nth(body, 0)).getByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(first).not.toHaveAttribute("aria-disabled");
    });
    await user.click(first);
    expect(await screen.findByRole("button", { name: "Suspend nightly-cron" })).toBeInTheDocument();
    await user.click(within(nth(body, 1)).getByRole("button", { name: "Suspend" }));
    expect(screen.queryByRole("button", { name: "Suspend nightly-cron" })).toBeNull();
    expect(screen.getByRole("button", { name: "Suspend gold-cron" })).toBeInTheDocument();
  });

  it("names the field a schedule really suspends at", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse(rows) });
    mountApp("/schedules");
    // The page says it in prose, and the confirmation says it again for the
    // row being changed — spec.schedule.suspend, not spec.suspend.
    expect(await screen.findByText(/spec\.schedule\.suspend/)).toBeInTheDocument();
  });

  it("teaches what a schedule is when there are none", async () => {
    mockApi({ "/api/v1/schedules": jsonResponse([]) });
    mountApp("/schedules?namespace=prod");
    const region = await screen.findByRole("region", { name: "Schedules" });
    const title = await within(region).findByText("No schedules in prod");
    expect(title.closest('[role="status"]')).toHaveTextContent("fires a SnapshotPolicy on a cron");
  });

  it("renders the not-permitted state for a 403 and offers no retry", async () => {
    mockApi({
      "/api/v1/schedules": problemResponse(
        forbiddenProblem("Schedules were refused.", "/api/v1/schedules"),
      ),
    });
    mountApp("/schedules");
    const region = await screen.findByRole("region", { name: "Schedules" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("Schedules were refused");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/schedules": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/schedules");
    const region = await screen.findByRole("region", { name: "Schedules" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while the schedules load", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/schedules");
    const region = await screen.findByRole("region", { name: "Schedules" });
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });
});
