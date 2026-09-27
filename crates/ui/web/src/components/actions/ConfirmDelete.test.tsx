import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, Problem } from "../../api/types";
import {
  calledPaths,
  fetchMock,
  jsonResponse,
  meWith,
  mockApi,
  problemResponse,
  renderWithClient,
} from "../../test-utils";
import { ConfirmDelete } from "./ConfirmDelete";

const SNAPSHOT = "/api/v1/snapshots/media/nightly-1";

const held: ActionReceipt = {
  kind: "Snapshot",
  created: [],
  requestedAt: null,
  note: "deletion is held by the repository's mass-deletion breaker; a cluster admin can release it with the allow-mass-deletion annotation",
};

async function open() {
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name: "Delete" });
  await waitFor(() => {
    expect(trigger).not.toHaveAttribute("aria-disabled");
  });
  await user.click(trigger);
  return user;
}

describe("ConfirmDelete", () => {
  it("deletes the named Snapshot and nothing else", async () => {
    mockApi({ [SNAPSHOT]: jsonResponse(held, 202) });
    renderWithClient(<ConfirmDelete namespace="media" name="nightly-1" deletionPolicy="Delete" />);
    const user = await open();
    await user.click(screen.getByRole("button", { name: "Request the deletion" }));
    await screen.findByRole("status");
    const call = fetchMock.mock.calls.find(([input]) => input === SNAPSHOT);
    expect(call, `sent ${calledPaths().join(", ")}`).toBeDefined();
    expect(call?.[1]?.method).toBe("DELETE");
  });

  it("says the kopia snapshot is destroyed when the policy says Delete", async () => {
    mockApi({ [SNAPSHOT]: jsonResponse(held, 202) });
    renderWithClient(<ConfirmDelete namespace="media" name="nightly-1" deletionPolicy="Delete" />);
    await open();
    const panel = screen.getByRole("group", { name: "Delete" });
    expect(panel).toHaveTextContent("deletionPolicy: Delete");
    expect(panel).toHaveTextContent("kopia snapshot delete");
    expect(panel.querySelector('[data-destructive="true"]')).not.toBeNull();
  });

  it("does not invent a policy for a snapshot that sets none", async () => {
    mockApi({ [SNAPSHOT]: jsonResponse(held, 202) });
    renderWithClient(<ConfirmDelete namespace="media" name="nightly-1" />);
    await open();
    const panel = screen.getByRole("group", { name: "Delete" });
    expect(panel).toHaveTextContent("deletionPolicy: not set");
    expect(panel).toHaveTextContent("the operator decides at deletion time");
    expect(panel).not.toHaveTextContent("kopia snapshot delete");
    expect(panel.querySelector('[data-destructive="true"]')).toBeNull();
  });

  it("says a pin does not protect a snapshot from a deletion you asked for", async () => {
    mockApi({ [SNAPSHOT]: jsonResponse(held, 202) });
    renderWithClient(
      <ConfirmDelete namespace="media" name="nightly-1" deletionPolicy="Delete" pinned />,
    );
    await open();
    expect(screen.getByRole("group", { name: "Delete" })).toHaveTextContent(
      "does not protect it from a deletion you ask for here",
    );
  });

  it("words the request as accepted, and warns that a silent receipt proves nothing", async () => {
    mockApi({ [SNAPSHOT]: jsonResponse(held, 202) });
    renderWithClient(<ConfirmDelete namespace="media" name="nightly-1" deletionPolicy="Retain" />);
    const user = await open();
    expect(screen.getByRole("group", { name: "Delete" })).toHaveTextContent(
      "that is not a promise there is none",
    );
    await user.click(screen.getByRole("button", { name: "Request the deletion" }));
    const answer = await screen.findByRole("status");
    expect(answer).toHaveTextContent("Delete requested");
    expect(answer).toHaveTextContent("mass-deletion breaker");
  });

  it("stays visible, disabled and explained without deleteSnapshots", async () => {
    mockApi({ "/api/v1/me": meWith({ deleteSnapshots: false }) });
    renderWithClient(<ConfirmDelete namespace="media" name="nightly-1" deletionPolicy="Delete" />);
    const trigger = await screen.findByRole("button", { name: "Delete" });
    await waitFor(() => {
      expect(trigger).toHaveAttribute("aria-disabled", "true");
    });
    expect(trigger).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Delete snapshots is not permitted"),
    );
    expect(trigger).toHaveAttribute("data-reason", expect.stringContaining("in namespace media"));
  });

  it("renders the problem's what, why and fix when the deletion is refused", async () => {
    const refused: Problem = {
      type: "urn:kopiur:problem:forbidden",
      title: "Forbidden",
      status: 403,
      detail: "The apiserver refused the delete.",
      what: "The apiserver refused the delete.",
      why: "Your identity may not delete snapshots in media.",
      fix: "ask a cluster admin to bind kopiur-ui-editor to your user or group",
      instance: SNAPSHOT,
      kubeReason: "Forbidden",
    };
    mockApi({ [SNAPSHOT]: problemResponse(refused) });
    renderWithClient(<ConfirmDelete namespace="media" name="nightly-1" deletionPolicy="Delete" />);
    const user = await open();
    await user.click(screen.getByRole("button", { name: "Request the deletion" }));
    const banner = await screen.findByRole("alert");
    expect(banner).toHaveTextContent("refused the delete");
    expect(banner).toHaveTextContent("bind kopiur-ui-editor");
  });
});
