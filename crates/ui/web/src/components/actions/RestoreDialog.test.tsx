import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { ActionReceipt, Problem, RestoreBody } from "../../api/types";
import {
  jsonResponse,
  meWith,
  mockApi,
  problemResponse,
  renderWithClient,
  sentBody,
} from "../../test-utils";
import { RestoreDialog } from "./RestoreDialog";

const RESTORE = "/api/v1/actions/restore";

const receipt: ActionReceipt = {
  kind: "Restore",
  created: [{ namespace: "media", name: "recover-db" }],
  requestedAt: null,
  note: null,
};

async function open() {
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name: "Restore" });
  await waitFor(() => {
    expect(trigger).not.toHaveAttribute("aria-disabled");
  });
  await user.click(trigger);
  return user;
}

describe("RestoreDialog", () => {
  it("builds a snapshotRef source into an existing claim, with overwrite said explicitly", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(<RestoreDialog namespace="media" />);
    const user = await open();
    await user.type(screen.getByLabelText("Snapshot name"), "nightly-1");
    await user.type(screen.getByLabelText("Claim name"), "data");
    await user.click(screen.getByRole("radio", { name: /Leave existing files alone/ }));
    await user.click(screen.getByRole("button", { name: "Create the restore" }));
    const expected: RestoreBody = {
      namespace: "media",
      source: { snapshotRef: { name: "nightly-1" } },
      target: { pvcRef: { name: "data" } },
      overwrite: false,
    };
    expect(sentBody(RESTORE)).toEqual(expected);
  });

  it("builds a fromPolicy source into a claim it creates, with the step-back and the instant", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(<RestoreDialog namespace="media" />);
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /SnapshotPolicy produced/ }));
    await user.type(screen.getByLabelText("Policy name"), "nightly");
    await user.type(screen.getByLabelText("As of (optional)"), "2026-09-08T02:00:00Z");
    await user.type(screen.getByLabelText("Steps back (optional)"), "2");
    await user.click(screen.getByRole("radio", { name: /new claim the operator creates/ }));
    await user.type(screen.getByLabelText("New claim name"), "restore-target");
    await user.type(screen.getByLabelText("Size"), "10Gi");
    await user.click(screen.getByRole("radio", { name: /Overwrite them/ }));
    await user.click(screen.getByRole("button", { name: "Create the restore" }));
    const expected: RestoreBody = {
      namespace: "media",
      source: { fromPolicy: { name: "nightly", asOf: "2026-09-08T02:00:00Z", offset: 2 } },
      target: { pvc: { name: "restore-target", size: "10Gi" } },
      overwrite: true,
    };
    expect(sentBody(RESTORE)).toEqual(expected);
  });

  it("builds an identity source with an explicit repository, which inference cannot supply", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(<RestoreDialog namespace="media" />);
    const user = await open();
    await user.click(screen.getByRole("radio", { name: /kopia identity/ }));
    await user.type(screen.getByLabelText("Kopia username"), "kopiur");
    await user.type(screen.getByLabelText("Kopia hostname"), "media");
    await user.type(screen.getByLabelText("Kopia manifest ID (optional)"), "k123");
    await user.type(screen.getByLabelText("Claim name"), "data");
    await user.click(screen.getByRole("radio", { name: /Leave existing files alone/ }));
    await user.selectOptions(screen.getByLabelText("Repository (optional)"), "ClusterRepository");
    await user.type(screen.getByLabelText("Repository name"), "shared");
    await user.click(screen.getByRole("button", { name: "Create the restore" }));
    const expected: RestoreBody = {
      namespace: "media",
      source: { identity: { username: "kopiur", hostname: "media", snapshotId: "k123" } },
      target: { pvcRef: { name: "data" } },
      overwrite: false,
      // A ClusterRepository carries no namespace: the CRD forbids one.
      repository: { kind: "ClusterRepository", name: "shared" },
    };
    expect(sentBody(RESTORE)).toEqual(expected);
  });

  it("carries sourcePath for a policy source, and never offers it for a snapshotRef", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(<RestoreDialog namespace="media" />);
    const user = await open();
    // The handler refuses sourcePath with a snapshotRef outright, so the
    // input is not there to be filled in.
    expect(screen.queryByLabelText("Source path (optional)")).toBeNull();
    expect(screen.getByRole("group", { name: "Restore" })).toHaveTextContent(
      "A source path cannot be chosen for this source",
    );

    await user.click(screen.getByRole("radio", { name: /SnapshotPolicy produced/ }));
    await user.type(screen.getByLabelText("Policy name"), "nightly");
    await user.type(screen.getByLabelText("Source path (optional)"), "/pvc/data");
    await user.type(screen.getByLabelText("Claim name"), "data");
    await user.click(screen.getByRole("radio", { name: /Leave existing files alone/ }));
    await user.click(screen.getByRole("button", { name: "Create the restore" }));
    const expected: RestoreBody = {
      namespace: "media",
      source: { fromPolicy: { name: "nightly" } },
      target: { pvcRef: { name: "data" } },
      overwrite: false,
      sourcePath: "/pvc/data",
    };
    expect(sentBody(RESTORE)).toEqual(expected);
  });

  it("will not send until overwrite is answered, because unset means overwrite", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(<RestoreDialog namespace="media" />);
    const user = await open();
    await user.type(screen.getByLabelText("Snapshot name"), "nightly-1");
    await user.type(screen.getByLabelText("Claim name"), "data");
    const confirm = screen.getByRole("button", { name: "Create the restore" });
    expect(confirm).toHaveAttribute("aria-disabled", "true");
    expect(confirm).toHaveAttribute("data-reason", expect.stringContaining("no neutral answer"));
    await user.click(confirm);
    expect(() => sentBody(RESTORE)).toThrow(/no request/);
  });

  it("names the field that decides whether existing data survives", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(<RestoreDialog namespace="media" />);
    await open();
    const panel = screen.getByRole("group", { name: "Restore" });
    expect(panel).toHaveTextContent("spec.options.overwriteFiles");
    expect(panel).toHaveTextContent("--[no-]overwrite-files");
  });

  it("blocks on an incomplete source before it blames anything else", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(<RestoreDialog namespace="media" />);
    await open();
    expect(screen.getByRole("button", { name: "Create the restore" })).toHaveAttribute(
      "data-reason",
      expect.stringContaining("The source is not complete"),
    );
  });

  it("pre-fills the snapshot a caller opened it from", async () => {
    mockApi({ [RESTORE]: jsonResponse(receipt) });
    renderWithClient(
      <RestoreDialog namespace="media" snapshot={{ namespace: "media", name: "nightly-1" }} />,
    );
    await open();
    expect(screen.getByLabelText("Snapshot name")).toHaveValue("nightly-1");
  });

  it("stays visible, disabled and explained without createRestores", async () => {
    mockApi({ "/api/v1/me": meWith({ createRestores: false }) });
    renderWithClient(<RestoreDialog namespace="media" />);
    const trigger = await screen.findByRole("button", { name: "Restore" });
    await waitFor(() => {
      expect(trigger).toHaveAttribute("aria-disabled", "true");
    });
    expect(trigger).toHaveAttribute(
      "data-reason",
      expect.stringContaining("Restore is not permitted"),
    );
  });

  it("renders the problem's what, why and fix when the restore is refused", async () => {
    const refused: Problem = {
      type: "urn:kopiur:problem:invalid-body",
      title: "Bad Request",
      status: 400,
      detail: "kopiur-ui could not read the request.",
      what: "kopiur-ui could not read the request: unknown field `pvcRefs`.",
      why: "The request body did not match what this endpoint accepts, so nothing was created.",
      fix: "correct the request and try again",
      instance: RESTORE,
      kubeReason: null,
    };
    mockApi({ [RESTORE]: problemResponse(refused) });
    renderWithClient(<RestoreDialog namespace="media" />);
    const user = await open();
    await user.type(screen.getByLabelText("Snapshot name"), "nightly-1");
    await user.type(screen.getByLabelText("Claim name"), "data");
    await user.click(screen.getByRole("radio", { name: /Leave existing files alone/ }));
    await user.click(screen.getByRole("button", { name: "Create the restore" }));
    const banner = await screen.findByRole("alert");
    expect(banner).toHaveTextContent("could not read the request");
    expect(banner).toHaveTextContent("nothing was created");
    expect(banner).toHaveTextContent("correct the request");
  });
});
