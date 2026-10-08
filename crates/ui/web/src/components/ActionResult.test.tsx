import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { ActionReceipt } from "../api/types";
import { forbiddenProblem, renderWithRouter } from "../test-utils";
import { ActionResult } from "./ActionResult";

const receipt: ActionReceipt = {
  kind: "Repository",
  created: [],
  requestedAt: "2026-09-08T06:00:00Z",
  note: "Repository/media/nas is now suspended",
};

describe("ActionResult", () => {
  it("renders the receipt's note, which is where a held or refused action explains itself", () => {
    render(<ActionResult label="Suspend" receipt={receipt} />);
    expect(screen.getByRole("status")).toHaveTextContent("is now suspended");
  });

  it("words an accepted action as requested, not as done", () => {
    render(<ActionResult label="Scan catalog" receipt={receipt} />);
    expect(screen.getByRole("status")).toHaveTextContent(/requested/i);
  });

  it("says which action was accepted even when the server sent no note", () => {
    render(<ActionResult label="Run maintenance" receipt={{ ...receipt, note: null }} />);
    expect(screen.getByRole("status")).toHaveTextContent("Run maintenance");
  });

  it("names each resource the action created", async () => {
    renderWithRouter(
      <ActionResult
        label="Snapshot now"
        receipt={{ ...receipt, created: [{ namespace: "media", name: "nas-1" }] }}
      />,
    );
    const status = await screen.findByRole("status");
    expect(status).toHaveTextContent("nas-1");
    expect(status).toHaveTextContent("media");
  });

  it("links each created object to its page, marked in its kind", async () => {
    renderWithRouter(
      <ActionResult
        label="Snapshot now"
        receipt={{ ...receipt, kind: "Snapshot", created: [{ namespace: "media", name: "nas-1" }] }}
      />,
    );
    const created = await screen.findByRole("list", { name: "Created" });
    const link = within(created).getByRole("link", { name: /nas-1/ });
    expect(link).toHaveAttribute("data-kind", "snapshot");
    expect(link).toHaveAttribute("href", "/snapshots/media/nas-1");
  });

  it("still names a created object of a kind this bundle does not know", async () => {
    renderWithRouter(
      <ActionResult
        label="Run"
        receipt={{ ...receipt, kind: "Gizmo", created: [{ namespace: "media", name: "g-1" }] }}
      />,
    );
    const created = await screen.findByRole("list", { name: "Created" });
    expect(created).toHaveTextContent("g-1");
    expect(within(created).queryByRole("link")).toBeNull();
  });

  it("renders a refusal as the problem's what, why and fix rather than the receipt", () => {
    render(
      <ActionResult
        label="Suspend"
        problem={forbiddenProblem("Suspending was refused.", "/api/v1/actions/suspend")}
      />,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("Suspending was refused.");
    expect(screen.getByRole("alert")).toHaveTextContent("kopiur-ui-viewer");
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("renders nothing at all before the action has run", () => {
    const { container } = render(<ActionResult label="Suspend" />);
    expect(container).toBeEmptyDOMElement();
  });
});
