import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, MaintenanceRow, MaintenanceRunBody } from "../api/types";
import {
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  meWith,
  mockApi,
  mountApp,
  problemResponse,
  sentBody,
} from "../test-utils";

const RUN = "/api/v1/actions/maintenance-run";

const rows: MaintenanceRow[] = [
  {
    namespace: "media",
    name: "nas-maintenance",
    repository: "Repository/media/nas",
    owner: "Repository/nas",
    managedByRepository: true,
    quick: {
      lastRunAt: "2026-09-10T09:00:00Z",
      nextScheduledAt: null,
      consecutiveFailures: 0,
      lastContentReclaimedBytes: null,
    },
    full: {
      lastRunAt: null,
      nextScheduledAt: null,
      consecutiveFailures: 2,
      lastContentReclaimedBytes: 4096,
    },
    manualRun: null,
  },
  {
    // A ClusterRepository's Maintenance lives in the operator's namespace,
    // not the repository's — which is the namespace the grant is judged in.
    namespace: "kopiur-system",
    name: "shared-maintenance",
    repository: "ClusterRepository/shared",
    owner: null,
    managedByRepository: false,
    quick: {
      lastRunAt: null,
      nextScheduledAt: null,
      consecutiveFailures: 0,
      lastContentReclaimedBytes: null,
    },
    full: {
      lastRunAt: null,
      nextScheduledAt: null,
      consecutiveFailures: 0,
      lastContentReclaimedBytes: null,
    },
    manualRun: {
      requestedAt: "2026-09-10T06:00:00Z",
      mode: "full",
      phase: "Running",
      completedAt: null,
    },
  },
];

const receipt: ActionReceipt = {
  kind: "Maintenance",
  created: [],
  requestedAt: "2026-09-10T10:00:00Z",
  note: null,
};

function region(name: string) {
  return screen.findByRole("region", { name });
}

describe("Maintenance", () => {
  it("gives each Maintenance its own region, titled by the repository it governs", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    const first = await region("Maintenance media/nas-maintenance");
    expect(first).toHaveTextContent("Repository/media/nas");
    expect(first).toHaveTextContent("media/nas-maintenance");
    expect(await region("Maintenance kopiur-system/shared-maintenance")).toHaveTextContent(
      "ClusterRepository/shared",
    );
  });

  it("says whether editing this object will stick or be reconciled away", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    expect(await region("Maintenance media/nas-maintenance")).toHaveTextContent("reconciled away");
    expect(await region("Maintenance kopiur-system/shared-maintenance")).toHaveTextContent(
      "never rewrites it",
    );
  });

  it("renders both tracks, a track that has never run, and a failure count", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    const first = await region("Maintenance media/nas-maintenance");
    expect(first).toHaveTextContent("Quick last run");
    expect(first).toHaveTextContent("Full last run");
    expect(within(first).getByText("never run")).toBeInTheDocument();
    expect(first).toHaveTextContent("Full failures since success");
    expect(first).toHaveTextContent("4.0 KiB");
  });

  it("renders the next run as 'not reported' on both tracks — nothing writes it", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    const first = await region("Maintenance media/nas-maintenance");
    expect(within(first).getAllByText("not reported")).toHaveLength(2);
    expect(within(first).getAllByTitle(/No controller writes/)[0]).toBeDefined();
  });

  it("shows a pending manual run with the token the operator echoes back", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    const second = await region("Maintenance kopiur-system/shared-maintenance");
    expect(second).toHaveTextContent("Manual run");
    expect(second).toHaveTextContent("full");
    expect(second).toHaveTextContent("Running");
    expect(second).toHaveTextContent("asked for");
  });

  it("asks for a run on the Maintenance resource, with the mode chosen", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows), [RUN]: jsonResponse(receipt) });
    mountApp("/maintenance");
    const user = userEvent.setup();
    const trigger = await screen.findByRole("button", {
      name: "Run maintenance for Repository/media/nas",
    });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.selectOptions(screen.getByLabelText("Mode"), "full");
    await user.click(screen.getByRole("button", { name: "Request the run" }));
    const expected: MaintenanceRunBody = {
      namespace: "media",
      name: "nas-maintenance",
      mode: "full",
    };
    expect(sentBody(RUN)).toEqual(expected);
  });

  it("judges each run in the Maintenance's own namespace, not the repository's", async () => {
    mockApi({
      "/api/v1/maintenance": jsonResponse(rows),
      "/api/v1/me": (url) =>
        url.searchParams.get("namespace") === "media"
          ? meWith({ patchMaintenances: true })
          : meWith({}),
    });
    mountApp("/maintenance");
    const allowed = await screen.findByRole("button", {
      name: "Run maintenance for Repository/media/nas",
    });
    const refused = screen.getByRole("button", {
      name: "Run maintenance for ClusterRepository/shared",
    });
    await waitFor(() => {
      expect(refused).toHaveAttribute("aria-disabled", "true");
    });
    expect(allowed).not.toHaveAttribute("aria-disabled");
    expect(refused).toHaveAttribute(
      "data-reason",
      expect.stringContaining("in namespace kopiur-system"),
    );
    expect(calledPaths()).toContain("/api/v1/me?namespace=media");
    expect(calledPaths()).toContain("/api/v1/me?namespace=kopiur-system");
  });

  it("keeps one confirmation open at a time", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows), [RUN]: jsonResponse(receipt) });
    mountApp("/maintenance");
    const user = userEvent.setup();
    const first = await screen.findByRole("button", {
      name: "Run maintenance for Repository/media/nas",
    });
    await waitFor(() => {
      expect(first).not.toHaveAttribute("aria-disabled");
    });
    await user.click(first);
    expect(
      screen.getByRole("group", { name: "Run maintenance for Repository/media/nas" }),
    ).toBeInTheDocument();
    await user.click(
      screen.getByRole("button", { name: "Run maintenance for ClusterRepository/shared" }),
    );
    expect(
      screen.queryByRole("group", { name: "Run maintenance for Repository/media/nas" }),
    ).toBeNull();
  });

  it("says an empty page is a risk rather than a quiet state", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse([]) });
    mountApp("/maintenance?namespace=prod");
    const empty = await screen.findByText("No maintenance in prod");
    expect(empty.closest('[role="status"]')).toHaveTextContent("grows without bound");
  });

  it("renders the not-permitted state for a 403 and offers no retry", async () => {
    mockApi({
      "/api/v1/maintenance": problemResponse(
        forbiddenProblem("Maintenance was refused.", "/api/v1/maintenance"),
      ),
    });
    mountApp("/maintenance");
    const section = await region("Maintenance");
    expect(await within(section).findByRole("alert")).toHaveTextContent("Maintenance was refused");
    expect(section.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(section).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/maintenance": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/maintenance");
    const section = await region("Maintenance");
    expect(await within(section).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(section).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while maintenance loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/maintenance");
    const section = await region("Maintenance");
    expect(within(section).getByRole("status", { busy: true })).toBeInTheDocument();
  });
});
