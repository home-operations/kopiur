import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { problemBanner, problemForHttpStatus } from "../api/problem";
import type { Problem } from "../api/types";
import { GlobalProblemBanner, ProblemBanner } from "./ProblemBanner";

const forbidden: Problem = {
  type: "urn:kopiur:problem:forbidden",
  title: "Forbidden",
  status: 403,
  detail: "Suspending policy nightly was refused.",
  what: "Suspending policy nightly was refused.",
  why: "The apiserver refused the request for the identity kopiur-ui impersonated.",
  fix: "ask a cluster admin to bind kopiur-ui-user, kopiur-ui-editor or kopiur-ui-viewer to your user or group",
  instance: "/api/v1/actions/suspend",
  kubeReason: "Forbidden",
};

describe("ProblemBanner", () => {
  it("renders what, why and fix, with fix labelled and set apart", () => {
    render(<ProblemBanner problem={forbidden} source="Suspend policy nightly" />);
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent(forbidden.what);
    expect(alert).toHaveTextContent(forbidden.why);
    expect(alert).toHaveTextContent(forbidden.fix);
    expect(alert).toHaveTextContent("Suspend policy nightly");
    const fix = screen.getByText(forbidden.fix).closest(".problem__fix");
    expect(fix).not.toBeNull();
    expect(fix).toHaveTextContent(/^Fix/);
  });

  it("shows the problem kind, the kube reason and the instance as metadata", () => {
    render(<ProblemBanner problem={forbidden} />);
    const meta = screen.getByRole("alert").querySelector(".problem__meta");
    expect(meta).toHaveTextContent("403 forbidden · Forbidden · /api/v1/actions/suspend");
  });

  it("frames a 403 as not-permitted rather than as a failure", () => {
    const { container } = render(<ProblemBanner problem={forbidden} />);
    expect(container.querySelector(".problem--forbidden")).not.toBeNull();
  });

  it("renders a synthetic problem's title when its type is not a kopiur URN", () => {
    const gateway = problemForHttpStatus({
      status: 502,
      statusText: "Bad Gateway",
      method: "GET",
      path: "/api/v1/status",
    });
    render(<ProblemBanner problem={gateway} />);
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent("GET /api/v1/status answered 502");
    expect(alert.querySelector(".problem__meta")).toHaveTextContent("502 Bad Gateway");
    expect(alert.textContent).not.toContain("[object Object]");
  });

  it("offers dismiss only when a handler is given", async () => {
    const onDismiss = vi.fn();
    const { rerender } = render(<ProblemBanner problem={forbidden} onDismiss={onDismiss} />);
    await userEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(onDismiss).toHaveBeenCalledOnce();
    rerender(<ProblemBanner problem={forbidden} />);
    expect(screen.queryByRole("button", { name: "Dismiss" })).toBeNull();
  });
});

describe("GlobalProblemBanner", () => {
  beforeEach(() => {
    problemBanner.dismiss();
  });

  it("renders nothing until a problem is reported, then the report, until dismissed", async () => {
    render(<GlobalProblemBanner />);
    expect(screen.queryByRole("alert")).toBeNull();

    act(() => {
      problemBanner.report(forbidden, { source: "Suspend policy nightly" });
    });
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent(forbidden.what);
    expect(alert).toHaveTextContent("Suspend policy nightly");

    await userEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(problemBanner.get()).toBeNull();
  });
});
