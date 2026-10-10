import { afterEach, describe, expect, it } from "vitest";

import { toasts } from "../components/toast/toasts";
import { ApiProblemError } from "./client";
import { problemBanner } from "./problem";
import { createQueryClient } from "./queryClient";
import type { ActionReceipt, Problem } from "./types";

const receipt: ActionReceipt = { kind: "Maintenance", created: [], note: null, requestedAt: null };

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

/** Run one mutation through the app client's cache, as `useMutation` would. */
async function run(
  outcome: "ok" | "refused",
  meta: { describe?: (variables: unknown) => string; announce?: "receipt" | "errors" },
) {
  const client = createQueryClient();
  const mutation = client.getMutationCache().build(client, {
    mutationFn: () =>
      outcome === "ok" ? Promise.resolve(receipt) : Promise.reject(new ApiProblemError(problem)),
    meta,
  });
  await mutation.execute({ name: "nas-maintenance" }).catch(() => undefined);
}

afterEach(() => {
  toasts.clear();
  problemBanner.dismiss();
});

describe("createQueryClient's mutation cache", () => {
  it("toasts an accepted action's receipt under the label it describes itself with", async () => {
    await run("ok", { describe: (v) => `Run maintenance on ${(v as { name: string }).name}` });
    expect(toasts.get()).toEqual([
      expect.objectContaining({
        kind: "receipt",
        label: "Run maintenance on nas-maintenance",
        receipt,
      }),
    ]);
  });

  it("toasts a refusal, and no longer raises the page banner for it", async () => {
    await run("refused", { describe: () => "Suspend schedule media/nightly" });
    expect(toasts.get()).toEqual([
      expect.objectContaining({
        kind: "problem",
        label: "Suspend schedule media/nightly",
        problem,
      }),
    ]);
    expect(problemBanner.get()).toBeNull();
  });

  it("answers only refusals for a mutation whose success shows elsewhere", async () => {
    await run("ok", { describe: () => "Start browse session", announce: "errors" });
    expect(toasts.get()).toEqual([]);
    await run("refused", { describe: () => "Start browse session", announce: "errors" });
    expect(toasts.get()).toHaveLength(1);
  });
});
