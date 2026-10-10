import { act, fireEvent, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ActionReceipt, Problem } from "../../api/types";
import { renderWithRouter } from "../../test-utils";
import { Toaster } from "./Toaster";
import { EXIT_MS } from "./durations";
import { RECEIPT_MS, RECEIPT_WITH_NOTE_MS, toasts } from "./index";

function receipt(over: Partial<ActionReceipt> = {}): ActionReceipt {
  return { kind: "Maintenance", created: [], note: null, requestedAt: null, ...over };
}

const problem: Problem = {
  type: "urn:kopiur:problem:forbidden",
  title: "Forbidden",
  status: 403,
  detail: "You may not suspend this schedule.",
  what: "You may not suspend this schedule.",
  why: "Your role grants no patch on snapshotschedules.",
  fix: "ask for patch on snapshotschedules in media",
  instance: "/api/v1/actions/suspend",
  kubeReason: null,
};

async function mount() {
  renderWithRouter(<Toaster />);
  const region = await screen.findByRole("region", { name: "Notifications" });
  // The clock is faked only once the router has rendered.
  vi.useFakeTimers();
  return region;
}

function advance(ms: number) {
  act(() => {
    vi.advanceTimersByTime(ms);
  });
}

/** Let a toast that has run out finish leaving. */
function settle() {
  advance(EXIT_MS);
}

afterEach(() => {
  act(() => {
    toasts.clear();
  });
  vi.useRealTimers();
});

describe("Toaster", () => {
  it("says an accepted action was requested, and leaves on its own", async () => {
    const region = await mount();
    act(() => {
      toasts.push({ kind: "receipt", label: "Run maintenance on media/nas", receipt: receipt() });
    });
    const toast = within(region).getByRole("status");
    expect(toast).toHaveTextContent("Run maintenance on media/nas requested");
    expect(toast.querySelector(".toast__timer")).not.toBeNull();
    advance(RECEIPT_MS - 50);
    expect(within(region).queryByRole("status")).not.toBeNull();
    advance(1000);
    settle();
    expect(within(region).queryByRole("status")).toBeNull();
  });

  it("gives a receipt with a note longer, since there is more to read", async () => {
    const region = await mount();
    act(() => {
      toasts.push({
        kind: "receipt",
        label: "Delete snapshot media/x",
        receipt: receipt({ note: "held by the deletion breaker" }),
      });
    });
    advance(RECEIPT_MS + 500);
    expect(within(region).getByRole("status")).toHaveTextContent("held by the deletion breaker");
    advance(RECEIPT_WITH_NOTE_MS);
    settle();
    expect(within(region).queryByRole("status")).toBeNull();
  });

  it("stops the clock and the line while the pointer is on it", async () => {
    const region = await mount();
    act(() => {
      toasts.push({ kind: "receipt", label: "Suspend", receipt: receipt() });
    });
    const toast = within(region).getByRole("status");
    advance(RECEIPT_MS - 2000);
    fireEvent.pointerEnter(toast);
    expect(toast).toHaveAttribute("data-paused", "true");
    advance(RECEIPT_MS * 3);
    expect(within(region).getByRole("status")).toBe(toast);
    fireEvent.pointerLeave(toast);
    expect(toast).not.toHaveAttribute("data-paused");
    // Only the time that was left runs out, not a fresh full duration.
    advance(1900);
    expect(within(region).queryByRole("status")).not.toBeNull();
    advance(200);
    settle();
    expect(within(region).queryByRole("status")).toBeNull();
  });

  it("keeps its line where it was across hovers: the line's start is fixed at mount", async () => {
    const region = await mount();
    act(() => {
      toasts.push({ kind: "receipt", label: "Run", receipt: receipt() });
    });
    const toast = within(region).getByRole("status");
    const start = toast.style.getPropertyValue("--toast-elapsed");
    advance(3000);
    fireEvent.pointerEnter(toast);
    advance(1000);
    fireEvent.pointerLeave(toast);
    fireEvent.pointerEnter(toast);
    fireEvent.pointerLeave(toast);
    // A changed delay would move the running line by the time already spent.
    expect(toast.style.getPropertyValue("--toast-elapsed")).toBe(start);
  });

  it("stops the clock while focus is inside it", async () => {
    const region = await mount();
    act(() => {
      toasts.push({ kind: "receipt", label: "Resume", receipt: receipt() });
    });
    const toast = within(region).getByRole("status");
    act(() => {
      within(toast).getByRole("button", { name: "Dismiss notification" }).focus();
    });
    expect(toast).toHaveAttribute("data-paused", "true");
    advance(RECEIPT_MS * 2);
    expect(within(region).queryByRole("status")).not.toBeNull();
  });

  it("keeps a refusal until it is closed, with no timer line", async () => {
    const region = await mount();
    act(() => {
      toasts.push({ kind: "problem", label: "Suspend schedule media/nightly", problem });
    });
    const alert = within(region).getByRole("alert");
    expect(alert).toHaveTextContent("You may not suspend this schedule.");
    expect(alert).toHaveTextContent("ask for patch on snapshotschedules in media");
    expect(alert).toHaveTextContent("Suspend schedule media/nightly");
    expect(alert.querySelector(".toast__timer")).toBeNull();
    advance(60_000);
    expect(within(region).getByRole("alert")).toBe(alert);
    fireEvent.click(within(alert).getByRole("button", { name: "Dismiss notification" }));
    advance(500);
    expect(within(region).queryByRole("alert")).toBeNull();
  });

  it("links what an action created", async () => {
    const region = await mount();
    act(() => {
      toasts.push({
        kind: "receipt",
        label: "Take a snapshot of nightly",
        receipt: receipt({
          kind: "Snapshot",
          created: [{ namespace: "media", name: "nightly-31" }],
        }),
      });
    });
    expect(within(region).getByRole("link", { name: /nightly-31/ })).toBeInTheDocument();
  });

  it("stacks toasts newest last", async () => {
    const region = await mount();
    act(() => {
      toasts.push({ kind: "receipt", label: "First", receipt: receipt() });
      toasts.push({ kind: "receipt", label: "Second", receipt: receipt() });
    });
    expect(
      within(region)
        .getAllByRole("status")
        .map((t) => t.textContent),
    ).toEqual([expect.stringContaining("First"), expect.stringContaining("Second")]);
  });
});
