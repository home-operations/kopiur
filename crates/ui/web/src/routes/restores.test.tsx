import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, RestoreBody, RestoreRow } from "../api/types";
import {
  bodyRows,
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

const RESTORE = "/api/v1/actions/restore";

const rows: RestoreRow[] = [
  {
    namespace: "media",
    name: "recover-db",
    phase: "restoring",
    sourceKind: "SnapshotRef",
    targetKind: "PvcRef",
    repository: "Repository/media/nas",
    kopiaSnapshotId: "k123",
    startTime: "2026-09-10T12:00:00Z",
    endTime: null,
    bytesRestored: 4096,
    filesRestored: 12,
    claims: [{ pvc: "data", phase: "Restoring", message: null }],
  },
  {
    namespace: "media",
    name: "failed-one",
    phase: "failed",
    sourceKind: "FromPolicy",
    targetKind: "Pvc",
    repository: null,
    kopiaSnapshotId: null,
    startTime: null,
    endTime: null,
    bytesRestored: null,
    filesRestored: null,
    claims: [],
  },
];

function table() {
  return screen.findByRole("table", { name: "Restores" });
}

describe("Restores", () => {
  it("lists each restore with its route and the counters it actually publishes", async () => {
    mockApi({ "/api/v1/restores": jsonResponse(rows) });
    mountApp("/restores?namespace=media");
    const body = bodyRows(await table());
    expect(body).toHaveLength(2);
    expect(nth(body, 0)).toHaveTextContent("SnapshotRef");
    expect(nth(body, 0)).toHaveTextContent("PvcRef");
    expect(nth(body, 0)).toHaveTextContent("4.0 KiB");
    expect(nth(body, 0)).toHaveTextContent("12 files");
    // No percentage anywhere: there is no total on the wire to divide by.
    expect(nth(body, 0)).not.toHaveTextContent("%");
  });

  it("never reads a failed restore as healthy, and never a missing phase as pending", async () => {
    mockApi({
      "/api/v1/restores": jsonResponse([...rows, { ...nth(rows, 0), name: "unseen", phase: null }]),
    });
    mountApp("/restores?namespace=media");
    const body = bodyRows(await table());
    expect(nth(body, 0).querySelector(".health")).toHaveAttribute("data-health", "pending");
    expect(nth(body, 1).querySelector(".health")).toHaveAttribute("data-health", "failed");
    const unseen = nth(body, 2);
    expect(unseen.querySelector(".health")).toHaveAttribute("data-health", "unknown");
    expect(unseen).toHaveTextContent("Not reconciled");
  });

  it("renders a phase this bundle has never seen as the operator's own word", async () => {
    mockApi({
      "/api/v1/restores": jsonResponse([
        { ...nth(rows, 0), phase: { unknown: { raw: "Quiescing" } } },
      ]),
    });
    mountApp("/restores?namespace=media");
    const body = bodyRows(await table());
    expect(nth(body, 0)).toHaveTextContent("Quiescing");
    expect(nth(body, 0).querySelector(".health")).toHaveAttribute("data-health", "unknown");
  });

  it("links a row into its own detail route", async () => {
    mockApi({ "/api/v1/restores": jsonResponse(rows) });
    mountApp("/restores?namespace=media");
    const body = bodyRows(await table());
    expect(within(nth(body, 0)).getByRole("link", { name: "recover-db" })).toHaveAttribute(
      "href",
      "/restores/media/recover-db",
    );
  });

  it("creates a restore from the page's namespace, with overwrite said explicitly", async () => {
    mockApi({
      "/api/v1/restores": jsonResponse(rows),
      [RESTORE]: jsonResponse({
        kind: "Restore",
        created: [{ namespace: "media", name: "recover-db" }],
        requestedAt: null,
        note: null,
      } satisfies ActionReceipt),
    });
    mountApp("/restores?namespace=media");
    const user = userEvent.setup();
    const trigger = await screen.findByRole("button", { name: "Restore" });
    await waitFor(() => {
      expect(trigger).not.toHaveAttribute("aria-disabled");
    });
    await user.click(trigger);
    await user.type(screen.getByLabelText("Snapshot name"), "nightly-1");
    await user.type(screen.getByLabelText("Claim name"), "data");
    await user.click(screen.getByRole("radio", { name: /Overwrite them/ }));
    await user.click(screen.getByRole("button", { name: "Create the restore" }));
    const expected: RestoreBody = {
      namespace: "media",
      source: { snapshotRef: { name: "nightly-1" } },
      target: { pvcRef: { name: "data" } },
      overwrite: true,
    };
    expect(sentBody(RESTORE)).toEqual(expected);
  });

  it("offers no create control cluster-wide, and says which scope to pick", async () => {
    mockApi({ "/api/v1/restores": jsonResponse(rows) });
    mountApp("/restores");
    const region = await screen.findByRole("region", { name: "Actions" });
    expect(region).toHaveTextContent("Scope this page to a namespace");
    expect(within(region).queryByRole("button", { name: "Restore" })).toBeNull();
  });

  it("keeps the create control visible and explained without createRestores", async () => {
    mockApi({ "/api/v1/restores": jsonResponse(rows), "/api/v1/me": meWith({}) });
    mountApp("/restores?namespace=media");
    const trigger = await screen.findByRole("button", { name: "Restore" });
    await waitFor(() => {
      expect(trigger).toHaveAttribute("aria-disabled", "true");
    });
    expect(trigger).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Restore is not permitted"),
    );
  });

  it("teaches what a restore is when there are none", async () => {
    mockApi({ "/api/v1/restores": jsonResponse([]) });
    mountApp("/restores?namespace=prod");
    const region = await screen.findByRole("region", { name: "Restores" });
    const title = await within(region).findByText("No restores in prod");
    expect(title.closest('[role="status"]')).toHaveTextContent("how a backup comes back");
  });

  it("renders the not-permitted state for a 403 and offers no retry", async () => {
    mockApi({
      "/api/v1/restores": problemResponse(
        forbiddenProblem("Restores were refused.", "/api/v1/restores"),
      ),
    });
    mountApp("/restores");
    const region = await screen.findByRole("region", { name: "Restores" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("Restores were refused");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/restores": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/restores");
    const region = await screen.findByRole("region", { name: "Restores" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while the restores load", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/restores");
    const region = await screen.findByRole("region", { name: "Restores" });
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });
});
