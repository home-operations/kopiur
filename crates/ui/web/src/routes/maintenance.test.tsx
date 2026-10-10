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
  unletteredLamps,
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
    expect(first.querySelector('a.ref[data-kind="repository"]')).toHaveTextContent("nas");
    expect(first).toHaveTextContent("media/nas-maintenance");
    const second = await region("Maintenance kopiur-system/shared-maintenance");
    expect(
      second.querySelector('a.ref[data-kind="cluster-repository"]')?.getAttribute("href"),
    ).toMatch(/inspect=cluster-repository%2Fshared$/);
  });

  it("opens a Maintenance object in the resource drawer from its title", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    await userEvent.click(await screen.findByRole("link", { name: "media/nas-maintenance" }));
    const dialog = await screen.findByRole("dialog", { name: /nas-maintenance/ });
    expect(dialog).toHaveAttribute("data-kind", "maintenance");
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

  it("lamps a track that has never run, and the count of failures behind it", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    const first = await region("Maintenance media/nas-maintenance");

    // "never run" keeps its own wording and gains the icon.
    const never = within(first).getByText("never run").closest(".health");
    expect(never).toHaveAttribute("data-health", "failed");
    expect(never?.querySelector("svg")).not.toBeNull();

    // The failure count is the one value that could not speak for itself: a
    // red "2" beside a plain "0" differed by hue and nothing else, and with
    // the icon aria-hidden a screen reader heard only the digit. Here the
    // lamp's word IS spoken, because the operator did publish the failures.
    const failures = within(first).getByText("2").closest(".health");
    expect(failures).toHaveAttribute("data-health", "failed");
    expect(failures?.querySelector("svg")).not.toBeNull();
    expect(failures).toHaveTextContent("2 (Failed)");

    // A measured zero stays quiet: an amber board hides the row that matters.
    expect(within(first).getByText("0").closest(".health")).toBeNull();
  });

  it("leaves no health colour on this screen carried by hue alone", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    await region("Maintenance media/nas-maintenance");
    expect(unletteredLamps(document.body)).toEqual([]);
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

  it("asks in a popover anchored to its button, one at a time, and Escape hands focus back", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows), [RUN]: jsonResponse(receipt) });
    mountApp("/maintenance");
    const user = userEvent.setup();
    const name = "Run maintenance for Repository/media/nas";
    const trigger = await screen.findByRole("button", { name });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    const popover = screen.getByRole("dialog", { name });
    expect(popover).toHaveClass("popover__panel");
    expect(popover.closest(".popover")).toContainElement(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(within(popover).getByLabelText("Mode")).toHaveFocus();

    // Opening the other card's run closes this one.
    const other = screen.getByRole("button", {
      name: /Run maintenance for ClusterRepository/,
    });
    await waitFor(() => {
      expect(other).not.toHaveAttribute("aria-disabled");
    });
    await user.click(other);
    expect(screen.queryByRole("dialog", { name })).toBeNull();
    expect(screen.getAllByRole("dialog")).toHaveLength(1);

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(other).toHaveFocus();
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

describe("Maintenance — kind identity", () => {
  it("marks each Maintenance with its kind and names its repository as a reference", async () => {
    mockApi({ "/api/v1/maintenance": jsonResponse(rows) });
    mountApp("/maintenance");
    const first = await region("Maintenance media/nas-maintenance");
    expect(first).toHaveAttribute("data-kind", "maintenance");
    expect(first).toHaveClass("has-stripe");
    expect(first.querySelector(".kind-chip svg")).not.toBeNull();
    expect(first.querySelector('a.ref[data-kind="repository"]')?.getAttribute("href")).toMatch(
      /inspect=repository%2Fmedia%2Fnas$/,
    );
  });
});
