import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, Problem, SuspendBody } from "../../api/types";
import {
  jsonResponse,
  meWith,
  mockApi,
  problemResponse,
  renderWithClient,
  sentBody,
} from "../../test-utils";
import { SuspendToggle } from "./SuspendToggle";

const SUSPEND = "/api/v1/actions/suspend";

const receipt: ActionReceipt = {
  kind: "SnapshotPolicy",
  created: [],
  requestedAt: null,
  note: "SnapshotPolicy/nightly in namespace media was already suspended; nothing was changed",
};

/** Open the confirmation and press its primary button. */
async function confirm(trigger: string, primary: string) {
  const user = userEvent.setup();
  const open = await screen.findByRole("button", { name: trigger });
  await waitFor(() => {
    expect(open).not.toHaveAttribute("aria-disabled");
  });
  await user.click(open);
  await user.click(await screen.findByRole("button", { name: primary }));
}

describe("SuspendToggle", () => {
  it("sends the kind token, the name and an explicit suspend value", async () => {
    mockApi({ [SUSPEND]: jsonResponse(receipt) });
    renderWithClient(
      <SuspendToggle
        kind="policy"
        name="nightly"
        namespace="media"
        suspended={false}
        consequence="no schedule will fire it"
      />,
    );
    await confirm("Suspend", "Suspend nightly");
    // Pinned against the generated request type: a renamed field fails here
    // before it can reach a cluster.
    const expected: SuspendBody = {
      kind: "policy",
      name: "nightly",
      namespace: "media",
      suspend: true,
    };
    expect(sentBody(SUSPEND)).toEqual(expected);
  });

  it("asks for false when resuming — never a toggle of what the page last read", async () => {
    mockApi({ [SUSPEND]: jsonResponse(receipt) });
    renderWithClient(
      <SuspendToggle
        kind="schedule"
        name="nightly-cron"
        namespace="media"
        suspended
        consequence="it will not fire"
      />,
    );
    await confirm("Resume", "Resume nightly-cron");
    const expected: SuspendBody = {
      kind: "schedule",
      name: "nightly-cron",
      namespace: "media",
      suspend: false,
    };
    expect(sentBody(SUSPEND)).toEqual(expected);
  });

  it("quotes the path the patch really writes, which a schedule nests", async () => {
    mockApi({ [SUSPEND]: jsonResponse(receipt) });
    renderWithClient(
      <SuspendToggle
        kind="schedule"
        name="nightly-cron"
        namespace="media"
        suspended={false}
        consequence="it will not fire"
      />,
    );
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Suspend" }));
    const panel = await screen.findByRole("group", { name: "Suspend" });
    expect(panel).toHaveTextContent("spec.schedule.suspend");
    expect(panel).not.toHaveTextContent("spec.suspend to");
  });

  it("sends no namespace for a cluster-scoped kind, and reviews it cluster-wide", async () => {
    const asked: string[] = [];
    mockApi({
      [SUSPEND]: jsonResponse(receipt),
      "/api/v1/me": (url) => {
        asked.push(url.search);
        return meWith({ patchClusterRepositories: true });
      },
    });
    renderWithClient(
      <SuspendToggle
        kind="cluster-repository"
        name="shared"
        namespace="media"
        suspended={false}
        consequence="nothing will be written to it"
      />,
    );
    await confirm("Suspend", "Suspend shared");
    const expected: SuspendBody = { kind: "cluster-repository", name: "shared", suspend: true };
    expect(sentBody(SUSPEND)).toEqual(expected);
    // The page's scope is deliberately dropped: a namespaced review of
    // patchClusterRepositories would report a grant that cannot authorize it.
    expect(asked).toContain("");
    expect(asked).not.toContain("?namespace=media");
  });

  it("stays visible, disabled and explained when the user lacks the verb", async () => {
    mockApi({ "/api/v1/me": meWith({ patchPolicies: false }) });
    renderWithClient(
      <SuspendToggle
        kind="policy"
        name="nightly"
        namespace="media"
        suspended={false}
        consequence="no schedule will fire it"
      />,
    );
    const trigger = await screen.findByRole("button", { name: "Suspend" });
    await waitFor(() => {
      expect(trigger).toHaveAttribute("aria-disabled", "true");
    });
    expect(trigger).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Suspend / resume policies is not permitted"),
    );
    expect(trigger).toHaveAttribute("data-reason", expect.stringContaining("in namespace media"));
  });

  it("renders the problem's what, why and fix when the flip is refused", async () => {
    const refused: Problem = {
      type: "urn:kopiur:problem:forbidden",
      title: "Forbidden",
      status: 403,
      detail: "The apiserver refused the patch.",
      what: "The apiserver refused the patch.",
      why: "Your identity may not patch snapshotpolicies in media.",
      fix: "ask a cluster admin to bind kopiur-ui-editor to your user or group",
      instance: SUSPEND,
      kubeReason: "Forbidden",
    };
    mockApi({ [SUSPEND]: problemResponse(refused) });
    renderWithClient(
      <SuspendToggle
        kind="policy"
        name="nightly"
        namespace="media"
        suspended={false}
        consequence="no schedule will fire it"
      />,
    );
    await confirm("Suspend", "Suspend nightly");
    const banner = await screen.findByRole("alert");
    expect(banner).toHaveTextContent("The apiserver refused the patch.");
    expect(banner).toHaveTextContent("may not patch snapshotpolicies");
    expect(banner).toHaveTextContent("bind kopiur-ui-editor");
  });

  it("renders the receipt's note, where an idempotent no-op explains itself", async () => {
    mockApi({ [SUSPEND]: jsonResponse(receipt) });
    renderWithClient(
      <SuspendToggle
        kind="policy"
        name="nightly"
        namespace="media"
        suspended={false}
        consequence="no schedule will fire it"
      />,
    );
    await confirm("Suspend", "Suspend nightly");
    const answer = await screen.findByRole("status");
    expect(answer).toHaveTextContent("was already suspended; nothing was changed");
  });
});
