import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import { problemBanner } from "../api/problem";
import type { GateDescriptor } from "../api/types";
import {
  bodyRows,
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  nth,
  problemResponse,
} from "../test-utils";

const registry: GateDescriptor[] = [
  {
    scope: "Snapshot",
    condition: "RepositoryWritable",
    blockedStatus: "False",
    reason: "RepositoryReadOnly",
    severity: "warning",
  },
  {
    scope: "Snapshot,Restore",
    condition: "MoverPermitted",
    blockedStatus: "False",
    reason: "PrivilegedMoverNotPermitted",
    severity: "error",
  },
];

beforeEach(() => {
  problemBanner.dismiss();
});

describe("Gates", () => {
  it("renders the registry with errors first and explains what a gate is", async () => {
    mockApi({ "/api/v1/gates": jsonResponse(registry) });
    mountApp("/gates");
    const table = await screen.findByRole("table", { name: "Structural gates" });
    const rows = bodyRows(table);
    expect(rows).toHaveLength(2);
    expect(nth(rows, 0)).toHaveTextContent("MoverPermitted");
    expect(nth(rows, 0).querySelector(".health")).toHaveTextContent("Error");
    expect(nth(rows, 1).querySelector(".health")).toHaveTextContent("Warning");
    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("Gates");
    expect(screen.getByText(/never self-heals/)).toBeInTheDocument();
    expect(calledPaths()).toContain("/api/v1/gates");
  });

  it("renders the not-permitted state for a 403", async () => {
    // /gates is a static registry that cannot 403 today (addenda item 23);
    // the state is still wired so a future extractor on the route degrades
    // the same way every other read does.
    mockApi({
      "/api/v1/gates": problemResponse(forbiddenProblem("Gates were refused.", "/api/v1/gates")),
    });
    mountApp("/gates");
    const region = await screen.findByRole("region", { name: "Structural gates" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("kopiur-ui-viewer");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/gates": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/gates");
    const region = await screen.findByRole("region", { name: "Structural gates" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("renders the empty state when the registry is empty", async () => {
    mockApi({ "/api/v1/gates": jsonResponse([]) });
    mountApp("/gates");
    const region = await screen.findByRole("region", { name: "Structural gates" });
    const title = await within(region).findByText("No gates registered");
    expect(title.closest('[role="status"]')).toHaveTextContent("Every gate the operator can raise");
  });

  it("shows a skeleton while the registry loads", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/gates");
    const region = await screen.findByRole("region", { name: "Structural gates" });
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });
});
