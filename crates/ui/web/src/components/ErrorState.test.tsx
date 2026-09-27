import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { problemForHttpStatus } from "../api/problem";
import type { Problem } from "../api/types";
import { EmptyState } from "./EmptyState";
import { ErrorState } from "./ErrorState";
import { LoadingState } from "./LoadingState";

const forbidden: Problem = {
  type: "urn:kopiur:problem:forbidden",
  title: "Forbidden",
  status: 403,
  detail: "Listing snapshots in namespace prod was refused.",
  what: "Listing snapshots in namespace prod was refused.",
  why: "The apiserver refused the request for the identity kopiur-ui impersonated.",
  fix: "ask a cluster admin to bind kopiur-ui-user, kopiur-ui-editor or kopiur-ui-viewer to your user or group",
  kubeReason: "Forbidden",
};

describe("ErrorState", () => {
  it("derives the not-permitted state from a 403 problem and offers no retry", () => {
    const onRetry = vi.fn();
    const { container } = render(
      <ErrorState problem={forbidden} what="snapshots" onRetry={onRetry} />,
    );
    expect(container.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(screen.getByRole("heading", { level: 2 })).toHaveTextContent(
      "Not permitted to view snapshots",
    );
    expect(screen.getByRole("alert")).toHaveTextContent(forbidden.fix);
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders any other problem as what/why/fix with a retry", async () => {
    const onRetry = vi.fn();
    const gateway = problemForHttpStatus({
      status: 502,
      statusText: "Bad Gateway",
      method: "GET",
      path: "/api/v1/snapshots",
    });
    render(<ErrorState problem={gateway} what="snapshots" onRetry={onRetry} />);
    expect(screen.getByRole("heading", { level: 2 })).toHaveTextContent("Could not load snapshots");
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent(gateway.what);
    expect(alert).toHaveTextContent(gateway.why);
    expect(alert).toHaveTextContent(gateway.fix);
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });
});

describe("EmptyState", () => {
  it("says what would appear and how to create it", () => {
    render(
      <EmptyState title="No snapshots in prod" action={<button type="button">Snapshot now</button>}>
        Snapshots taken under a policy in prod are listed here. A schedule creates them on its cron;
        Snapshot now creates one immediately.
      </EmptyState>,
    );
    const status = screen.getByRole("status");
    expect(status).toHaveTextContent("No snapshots in prod");
    expect(status).toHaveTextContent("A schedule creates them");
    expect(screen.getByRole("button", { name: "Snapshot now" })).toBeInTheDocument();
  });
});

describe("LoadingState", () => {
  it("is a skeleton announced to assistive technology, not a spinner", () => {
    const { container } = render(<LoadingState what="snapshots" rows={3} />);
    const status = screen.getByRole("status");
    expect(status).toHaveAttribute("aria-busy", "true");
    expect(status).toHaveTextContent("Loading snapshots…");
    expect(container.querySelectorAll(".skeleton-ledger__row")).toHaveLength(3);
    expect(container.querySelector(".spinner")).toBeNull();
  });
});
