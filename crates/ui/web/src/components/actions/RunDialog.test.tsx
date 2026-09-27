import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type {
  ActionReceipt,
  MaintenanceRunBody,
  Problem,
  ReplicationRunBody,
} from "../../api/types";
import {
  jsonResponse,
  meWith,
  mockApi,
  problemResponse,
  renderWithClient,
  sentBody,
} from "../../test-utils";
import { RunDialog } from "./RunDialog";

const MAINTENANCE_RUN = "/api/v1/actions/maintenance-run";
const REPLICATION_RUN = "/api/v1/actions/replication-run";

const receipt: ActionReceipt = {
  kind: "Maintenance",
  created: [],
  requestedAt: "2026-09-10T06:00:00Z",
  note: null,
};

async function open(name: string) {
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name });
  await waitFor(() => {
    expect(trigger).not.toHaveAttribute("aria-disabled");
  });
  await user.click(trigger);
  return user;
}

describe("RunDialog", () => {
  it("asks for a quick maintenance run on the Maintenance resource, by name", async () => {
    mockApi({ [MAINTENANCE_RUN]: jsonResponse(receipt) });
    renderWithClient(
      <RunDialog
        target={{
          kind: "maintenance",
          namespace: "kopiur-system",
          name: "nas-maintenance",
          repository: "Repository/media/nas",
        }}
      />,
    );
    const user = await open("Run maintenance");
    await user.click(screen.getByRole("button", { name: "Request the run" }));
    const expected: MaintenanceRunBody = {
      namespace: "kopiur-system",
      name: "nas-maintenance",
      mode: "quick",
    };
    expect(sentBody(MAINTENANCE_RUN)).toEqual(expected);
  });

  it("sends the mode the operator chose, not the one it opened with", async () => {
    mockApi({ [MAINTENANCE_RUN]: jsonResponse(receipt) });
    renderWithClient(
      <RunDialog
        target={{
          kind: "maintenance",
          namespace: "media",
          name: "nas-maintenance",
          repository: "Repository/media/nas",
        }}
      />,
    );
    const user = await open("Run maintenance");
    await user.selectOptions(screen.getByLabelText("Mode"), "full");
    await user.click(screen.getByRole("button", { name: "Request the run" }));
    const expected: MaintenanceRunBody = {
      namespace: "media",
      name: "nas-maintenance",
      mode: "full",
    };
    expect(sentBody(MAINTENANCE_RUN)).toEqual(expected);
  });

  it("judges a maintenance run in the Maintenance's namespace, not the repository's", async () => {
    const asked: string[] = [];
    mockApi({
      [MAINTENANCE_RUN]: jsonResponse(receipt),
      "/api/v1/me": (url) => {
        asked.push(url.search);
        return meWith({ patchMaintenances: true });
      },
    });
    renderWithClient(
      <RunDialog
        target={{
          kind: "maintenance",
          // A ClusterRepository's Maintenance lives in the operator's namespace.
          namespace: "kopiur-system",
          name: "shared-maintenance",
          repository: "ClusterRepository/shared",
        }}
      />,
    );
    await open("Run maintenance");
    expect(asked).toContain("?namespace=kopiur-system");
  });

  it("names the replication kind explicitly, so the server never has to detect it", async () => {
    mockApi({ [REPLICATION_RUN]: jsonResponse({ ...receipt, kind: "SnapshotReplication" }) });
    renderWithClient(
      <RunDialog
        target={{
          kind: "replication",
          namespace: "prod",
          name: "offsite",
          replicationKind: "snapshot-replication",
        }}
        labelSuffix="offsite"
      />,
    );
    const user = await open("Run now for offsite");
    await user.click(screen.getByRole("button", { name: "Request the run" }));
    const expected: ReplicationRunBody = {
      namespace: "prod",
      name: "offsite",
      kind: "snapshot-replication",
    };
    expect(sentBody(REPLICATION_RUN)).toEqual(expected);
  });

  it("asks about the flag that matches the replication kind", async () => {
    mockApi({ "/api/v1/me": meWith({ patchRepositoryReplications: true }) });
    renderWithClient(
      <RunDialog
        target={{
          kind: "replication",
          namespace: "prod",
          name: "offsite",
          replicationKind: "snapshot-replication",
        }}
      />,
    );
    const trigger = await screen.findByRole("button", { name: "Run now" });
    await waitFor(() => {
      expect(trigger).toHaveAttribute("aria-disabled", "true");
    });
    // Holding the *repository* replication grant is not holding the snapshot
    // replication one, and the reason names the flag that is missing.
    expect(trigger).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Run, suspend snapshot replications is not permitted"),
    );
  });

  it("stays visible, disabled and explained without patchMaintenances", async () => {
    mockApi({ "/api/v1/me": meWith({ patchMaintenances: false }) });
    renderWithClient(
      <RunDialog
        target={{
          kind: "maintenance",
          namespace: "media",
          name: "nas-maintenance",
          repository: "Repository/media/nas",
        }}
      />,
    );
    const trigger = await screen.findByRole("button", { name: "Run maintenance" });
    await waitFor(() => {
      expect(trigger).toHaveAttribute("aria-disabled", "true");
    });
    expect(trigger).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Run maintenance is not permitted"),
    );
  });

  it("prefers the caller's own refusal over the RBAC one", async () => {
    mockApi({});
    renderWithClient(
      <RunDialog
        target={{
          kind: "replication",
          namespace: "prod",
          name: "offsite",
          replicationKind: "replication",
        }}
        unavailable="offsite is suspended, so a run would not start."
      />,
    );
    const trigger = await screen.findByRole("button", { name: "Run now" });
    expect(trigger).toHaveAttribute("aria-disabled", "true");
    expect(trigger).toHaveAttribute(
      "data-reason",
      "offsite is suspended, so a run would not start.",
    );
  });

  it("words the answer as requested, never as done", async () => {
    mockApi({ [MAINTENANCE_RUN]: jsonResponse(receipt) });
    renderWithClient(
      <RunDialog
        target={{
          kind: "maintenance",
          namespace: "media",
          name: "nas-maintenance",
          repository: "Repository/media/nas",
        }}
      />,
    );
    const user = await open("Run maintenance");
    expect(screen.getByRole("group", { name: "Run maintenance" })).toHaveTextContent(
      "never “finished”",
    );
    await user.click(screen.getByRole("button", { name: "Request the run" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Run maintenance requested");
  });

  it("renders the problem's what, why and fix when the run is refused", async () => {
    const refused: Problem = {
      type: "urn:kopiur:problem:not-found",
      title: "Not Found",
      status: 404,
      detail: "No Maintenance called nas-maintenance in media.",
      what: "No Maintenance called nas-maintenance in media.",
      why: "It was deleted, or the repository never projected one.",
      fix: "check the repository's spec.maintenance",
      instance: MAINTENANCE_RUN,
      kubeReason: "NotFound",
    };
    mockApi({ [MAINTENANCE_RUN]: problemResponse(refused) });
    renderWithClient(
      <RunDialog
        target={{
          kind: "maintenance",
          namespace: "media",
          name: "nas-maintenance",
          repository: "Repository/media/nas",
        }}
      />,
    );
    const user = await open("Run maintenance");
    await user.click(screen.getByRole("button", { name: "Request the run" }));
    const banner = await screen.findByRole("alert");
    expect(banner).toHaveTextContent("No Maintenance called nas-maintenance");
    expect(banner).toHaveTextContent("check the repository's spec.maintenance");
  });
});
