import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, Problem, SnapshotNowBody } from "../../api/types";
import {
  jsonResponse,
  meWith,
  mockApi,
  problemResponse,
  renderWithClient,
  sentBody,
} from "../../test-utils";
import { SnapshotNowDialog } from "./SnapshotNowDialog";

const SNAPSHOT_NOW = "/api/v1/actions/snapshot-now";

const receipt: ActionReceipt = {
  kind: "Snapshot",
  created: [{ namespace: "media", name: "nightly-20260910" }],
  requestedAt: null,
  note: null,
};

/** Open the dialog once `/me` has answered, so the trigger is not swallowed. */
async function open() {
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name: "Snapshot now" });
  await waitFor(() => {
    expect(trigger).not.toHaveAttribute("aria-disabled");
  });
  await user.click(trigger);
  return user;
}

describe("SnapshotNowDialog", () => {
  it("sends the policy, an empty tag list and pin: false when pruning is chosen", async () => {
    mockApi({ [SNAPSHOT_NOW]: jsonResponse(receipt) });
    renderWithClient(<SnapshotNowDialog namespace="media" policy="nightly" />);
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /Prune it under/ }));
    await user.click(screen.getByRole("button", { name: "Take the snapshot" }));
    const expected: SnapshotNowBody = {
      namespace: "media",
      policy: "nightly",
      tags: [],
      pin: false,
    };
    expect(sentBody(SNAPSHOT_NOW)).toEqual(expected);
  });

  it("sends pin: true only when the permanent option is actually chosen", async () => {
    mockApi({ [SNAPSHOT_NOW]: jsonResponse(receipt) });
    renderWithClient(<SnapshotNowDialog namespace="media" policy="nightly" />);
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /Pin it/ }));
    await user.click(screen.getByRole("button", { name: "Take the snapshot" }));
    const expected: SnapshotNowBody = {
      namespace: "media",
      policy: "nightly",
      tags: [],
      pin: true,
    };
    expect(sentBody(SNAPSHOT_NOW)).toEqual(expected);
  });

  it("refuses to send until the pin question is answered, and says why", async () => {
    mockApi({ [SNAPSHOT_NOW]: jsonResponse(receipt) });
    renderWithClient(<SnapshotNowDialog namespace="media" policy="nightly" />);
    const user = await open();
    const confirm = screen.getByRole("button", { name: "Take the snapshot" });
    expect(confirm).toHaveAttribute("aria-disabled", "true");
    expect(confirm).toHaveAttribute("data-reason", expect.stringContaining("Pinning is permanent"));
    await user.click(confirm);
    expect(() => sentBody(SNAPSHOT_NOW)).toThrow(/no request/);
  });

  it("carries an optional name and description trimmed, and omits them when blank", async () => {
    mockApi({ [SNAPSHOT_NOW]: jsonResponse(receipt) });
    renderWithClient(<SnapshotNowDialog namespace="media" policy="nightly" />);
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /Prune it under/ }));
    await user.type(screen.getByLabelText("Name (optional)"), "  before-upgrade  ");
    await user.click(screen.getByRole("button", { name: "Take the snapshot" }));
    const expected: SnapshotNowBody = {
      namespace: "media",
      policy: "nightly",
      tags: [],
      pin: false,
      name: "before-upgrade",
    };
    expect(sentBody(SNAPSHOT_NOW)).toEqual(expected);
  });

  it("offers the repository restriction only for a fan-out, and sends the bare name", async () => {
    mockApi({ [SNAPSHOT_NOW]: jsonResponse(receipt) });
    const { unmount } = renderWithClient(
      <SnapshotNowDialog
        namespace="media"
        policy="nightly"
        repositories={["Repository/media/nas"]}
      />,
    );
    await open();
    // One repository is not a choice: there is nothing to restrict to.
    expect(screen.queryByLabelText("Repository")).toBeNull();
    unmount();

    renderWithClient(
      <SnapshotNowDialog
        namespace="media"
        policy="nightly"
        repositories={["Repository/media/nas", "ClusterRepository/shared"]}
      />,
    );
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /Prune it under/ }));
    // The server matches the restriction by bare name, not by the display key
    // the row carries (`kopiur_ops::actions::snapshot::planned_cells`).
    await user.selectOptions(screen.getByLabelText("Repository"), "shared");
    await user.click(screen.getByRole("button", { name: "Take the snapshot" }));
    const expected: SnapshotNowBody = {
      namespace: "media",
      policy: "nightly",
      tags: [],
      pin: false,
      repository: "shared",
    };
    expect(sentBody(SNAPSHOT_NOW)).toEqual(expected);
  });

  it("stays visible, disabled and explained without createSnapshots", async () => {
    mockApi({ "/api/v1/me": meWith({ createSnapshots: false }) });
    renderWithClient(<SnapshotNowDialog namespace="media" policy="nightly" />);
    const trigger = await screen.findByRole("button", { name: "Snapshot now" });
    await waitFor(() => {
      expect(trigger).toHaveAttribute("aria-disabled", "true");
    });
    expect(trigger).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Snapshot now is not permitted"),
    );
  });

  it("renders the problem's what, why and fix when the policy is gone", async () => {
    const missing: Problem = {
      type: "urn:kopiur:problem:not-found",
      title: "Not Found",
      status: 404,
      detail: "There is no SnapshotPolicy called nightly in namespace media.",
      what: "There is no SnapshotPolicy called nightly in namespace media.",
      why: "It was deleted or renamed.",
      fix: "reload the policies list to see what the cluster holds now",
      instance: SNAPSHOT_NOW,
      kubeReason: null,
    };
    mockApi({ [SNAPSHOT_NOW]: problemResponse(missing) });
    renderWithClient(<SnapshotNowDialog namespace="media" policy="nightly" />);
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /Prune it under/ }));
    await user.click(screen.getByRole("button", { name: "Take the snapshot" }));
    const banner = await screen.findByRole("alert");
    expect(banner).toHaveTextContent("no SnapshotPolicy called nightly");
    expect(banner).toHaveTextContent("reload the policies list");
  });

  it("names every Snapshot the fan-out created", async () => {
    mockApi({ [SNAPSHOT_NOW]: jsonResponse(receipt) });
    renderWithClient(<SnapshotNowDialog namespace="media" policy="nightly" />);
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /Prune it under/ }));
    await user.click(screen.getByRole("button", { name: "Take the snapshot" }));
    const created = await screen.findByRole("list", { name: "Created" });
    expect(created).toHaveTextContent("nightly-20260910");
  });
});
