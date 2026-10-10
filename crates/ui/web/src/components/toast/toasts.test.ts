import { afterEach, describe, expect, it, vi } from "vitest";

import type { ActionReceipt, Problem } from "../../api/types";
import { MAX_TOASTS, toasts } from "./toasts";

const receipt: ActionReceipt = {
  kind: "SnapshotSchedule",
  created: [],
  note: null,
  requestedAt: null,
};

const problem: Problem = {
  type: "urn:kopiur:problem:forbidden",
  title: "Forbidden",
  status: 403,
  detail: "no",
  what: "You may not.",
  why: "RBAC.",
  fix: "ask an admin",
  instance: null,
  kubeReason: null,
};

afterEach(() => {
  toasts.clear();
});

describe("toasts", () => {
  it("pushes entries with unique ids and tells subscribers", () => {
    const heard = vi.fn();
    const stop = toasts.subscribe(heard);
    const a = toasts.push({ kind: "receipt", label: "Suspend", receipt });
    const b = toasts.push({ kind: "problem", label: "Suspend", problem });
    expect(a).not.toBe(b);
    expect(toasts.get().map((t) => t.id)).toEqual([a, b]);
    expect(heard).toHaveBeenCalledTimes(2);
    stop();
  });

  it("dismisses one, and keeps the list identity stable when nothing changed", () => {
    const a = toasts.push({ kind: "receipt", label: "Run", receipt });
    const before = toasts.get();
    toasts.dismiss("missing");
    expect(toasts.get()).toBe(before);
    toasts.dismiss(a);
    expect(toasts.get()).toEqual([]);
  });

  it("drops the oldest timed toast past the cap, never a problem", () => {
    const refused = toasts.push({ kind: "problem", label: "Delete", problem });
    const timed = Array.from({ length: MAX_TOASTS }, (_, i) =>
      toasts.push({ kind: "receipt", label: `Run ${String(i)}`, receipt }),
    );
    const ids = toasts.get().map((t) => t.id);
    expect(ids).toHaveLength(MAX_TOASTS);
    expect(ids).toContain(refused);
    expect(ids).not.toContain(timed[0]);
  });
});
