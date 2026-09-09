import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { problemBanner } from "../api/problem";
import type { DoctorReportView } from "../api/types";
import {
  bodyRows,
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  nth,
  problemResponse,
} from "../test-utils";

const NOW = "2026-09-08T12:00:00Z";

const report: DoctorReportView = {
  ranAt: "2026-09-08T11:59:30Z",
  exitCode: 1,
  checks: [
    { check: "crds-installed", scope: "installation", title: "CRDs installed", outcome: "Pass" },
    {
      check: "controller-running",
      scope: "installation",
      title: "controller running",
      outcome: "Pass",
    },
    {
      check: "credentials-present",
      scope: "mixed",
      title: "credential secrets present",
      outcome: "Warn",
      what: "cannot list secrets (RBAC); grant `list` on `secrets` or run with a more privileged kubeconfig to enable this check",
    },
    {
      check: "no-stuck-work",
      scope: "namespace",
      title: "no blocked or stuck work",
      outcome: "Fail",
      what: "Snapshot media/nightly-1 is parked on MoverPermitted=False",
      why: "namespace media has not opted in to privileged movers",
      fix: "annotate namespace media with kopiur.home-operations.com/allow-privileged-mover=true",
    },
  ],
};

beforeEach(() => {
  problemBanner.dismiss();
  vi.useFakeTimers({ shouldAdvanceTime: true, now: new Date(NOW) });
});

afterEach(() => {
  vi.useRealTimers();
});

describe("Doctor", () => {
  it("renders one row per check, a summary counted from the rows, and passes the windows through in seconds", async () => {
    mockApi({ "/api/v1/doctor": jsonResponse(report) });
    mountApp("/doctor?namespace=media&stuckThreshold=2h&failureLookback=24h");

    const table = await screen.findByRole("table", { name: "Doctor checks" });
    expect(bodyRows(table)).toHaveLength(4);
    const summary = screen.getByRole("heading", { level: 2, name: /4 checks/ });
    // The one warning here is doctor refusing to guess past the viewer's own
    // RBAC, so it is counted as a permission gap and not as a cluster warning.
    expect(summary).toHaveTextContent("4 checks: 2 pass, 0 warn, 1 fail, 1 not permitted");
    expect(summary.querySelector(".verdict__lamp")).toHaveAttribute("data-health", "failed");
    expect(screen.getByText(/ran 30s ago/)).toBeInTheDocument();

    // The controls reflect the URL, which is the state.
    expect(screen.getByRole("textbox", { name: "Namespace" })).toHaveValue("media");
    expect(screen.getByRole("textbox", { name: "Stuck threshold" })).toHaveValue("2h");
    expect(screen.getByRole("textbox", { name: "Failure lookback" })).toHaveValue("24h");

    // A namespaced run says which checks the namespace moved.
    const rows = bodyRows(table);
    expect(nth(rows, 0)).toHaveTextContent(/installation-wide/);
    expect(nth(rows, 2)).toHaveTextContent("scoped to media");
    expect(nth(rows, 2)).toHaveAttribute("data-degraded", "rbac");
    expect(nth(rows, 3).querySelector(".finding__fix")).toHaveTextContent(
      "annotate namespace media",
    );

    // This screen is the full report, so it names no subset: `checks` absent
    // runs all ten, and an absent row here would read as a check that passed.
    expect(calledPaths()).toContain(
      "/api/v1/doctor?namespace=media&stuckThreshold=7200&failureLookback=86400",
    );
    expect(calledPaths().filter((p) => p.includes("checks="))).toHaveLength(0);
    expect(screen.getByRole("link", { name: /gate registry/i })).toHaveAttribute(
      "href",
      "/gates?namespace=media",
    );
  });

  it("re-runs with new windows from the controls and refuses a duration the server would refuse", async () => {
    mockApi({ "/api/v1/doctor": jsonResponse(report) });
    mountApp("/doctor");
    await screen.findByRole("table", { name: "Doctor checks" });
    expect(calledPaths()).toContain("/api/v1/doctor");

    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    const stuck = screen.getByRole("textbox", { name: "Stuck threshold" });
    await user.clear(stuck);
    await user.type(stuck, "1d");
    await user.click(screen.getByRole("button", { name: "Run doctor" }));
    expect(stuck).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByText(/like 90s, 30m or 1h/)).toBeInTheDocument();
    expect(calledPaths().filter((p) => p.includes("stuckThreshold"))).toHaveLength(0);

    await user.clear(stuck);
    await user.type(stuck, "30m");
    await user.type(screen.getByRole("textbox", { name: "Namespace" }), "prod");
    await user.click(screen.getByRole("button", { name: "Run doctor" }));
    expect(await screen.findByRole("textbox", { name: "Stuck threshold" })).not.toHaveAttribute(
      "aria-invalid",
    );
    await vi.waitFor(() => {
      expect(calledPaths()).toContain("/api/v1/doctor?namespace=prod&stuckThreshold=1800");
    });
  });

  it("renders the not-permitted state for a 403", async () => {
    mockApi({
      "/api/v1/doctor": problemResponse(forbiddenProblem("Doctor was refused.", "/api/v1/doctor")),
    });
    mountApp("/doctor");
    const region = await screen.findByRole("region", { name: "Doctor checks" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("kopiur-ui-viewer");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/doctor": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/doctor");
    const region = await screen.findByRole("region", { name: "Doctor checks" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("renders the empty state when no check ran", async () => {
    mockApi({ "/api/v1/doctor": jsonResponse({ ranAt: NOW, exitCode: 0, checks: [] }) });
    mountApp("/doctor");
    const region = await screen.findByRole("region", { name: "Doctor checks" });
    const title = await within(region).findByText("No checks ran");
    expect(title.closest('[role="status"]')).toHaveTextContent("not that all passed");
  });

  it("shows a skeleton while the report is running", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/doctor");
    const region = await screen.findByRole("region", { name: "Doctor checks" });
    expect(within(region).getByRole("status", { busy: true })).toHaveTextContent(
      "Loading the doctor report",
    );
  });
});
