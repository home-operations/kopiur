import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";

import {
  calledPaths,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../../test-utils";
import { cssRules, readStyles } from "../../testing/css";

afterEach(() => {
  document.body.innerHTML = "";
});

const POLICY = {
  namespace: "kopiur-dev",
  name: "app-data",
  repositories: ["Repository/kopiur-dev/dev-repo"],
  multiRepo: false,
  suspended: false,
};

function snapshots(url: URL): Response {
  const q = url.searchParams.get("q") ?? "";
  const items =
    q.length > 0 ? [{ namespace: "kopiur-dev", name: "app-data-manual", pinned: true }] : [];
  return jsonResponse({ items, total: items.length, offset: 0, limit: 10 });
}

describe("ObjectSearch", () => {
  it("finds objects of every kind by name, each opening in the resource drawer", async () => {
    mockApi({
      "/api/v1/policies": jsonResponse([POLICY]),
      "/api/v1/snapshots": snapshots,
      "/api/v1/repositories": jsonResponse([]),
      "/api/v1/schedules": jsonResponse([]),
      "/api/v1/restores": jsonResponse([]),
      "/api/v1/maintenance": jsonResponse([]),
      "/api/v1/replications": jsonResponse({ repository: [], snapshot: [] }),
    });
    mountApp("/doctor");
    const user = userEvent.setup();
    await user.type(await screen.findByRole("searchbox", { name: "Find an object" }), "app");
    const results = await screen.findByRole("list", { name: "Search results" });
    expect(
      (await within(results).findByRole("link", { name: /app-data-manual/ })).getAttribute("href"),
    ).toMatch(/inspect=snapshot%2Fkopiur-dev%2Fapp-data-manual$/);
    expect(
      within(results)
        .getByRole("link", { name: /SnapshotPolicy.*app-data$/ })
        .getAttribute("href"),
    ).toMatch(/inspect=snapshot-policy%2Fkopiur-dev%2Fapp-data$/);
  });

  it("gets out of the way once a result is opened", async () => {
    mockApi({
      "/api/v1/policies": jsonResponse([POLICY]),
      "/api/v1/snapshots": snapshots,
      "/api/v1/repositories": jsonResponse([]),
      "/api/v1/schedules": jsonResponse([]),
      "/api/v1/restores": jsonResponse([]),
      "/api/v1/maintenance": jsonResponse([]),
      "/api/v1/replications": jsonResponse({ repository: [], snapshot: [] }),
    });
    mountApp("/doctor");
    const user = userEvent.setup();
    await user.type(await screen.findByRole("searchbox", { name: "Find an object" }), "app");
    const results = await screen.findByRole("list", { name: "Search results" });
    await user.click(within(results).getByRole("link", { name: /SnapshotPolicy.*app-data$/ }));
    expect(await screen.findByRole("dialog", { name: /app-data/ })).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Search results" })).toBeNull();
  });

  it("does not search on a single character", async () => {
    mockApi({ "/api/v1/snapshots": snapshots });
    mountApp("/doctor");
    const user = userEvent.setup();
    await user.type(await screen.findByRole("searchbox", { name: "Find an object" }), "a");
    expect(calledPaths().some((p) => p.includes("/api/v1/snapshots?") && p.includes("q="))).toBe(
      false,
    );
    expect(screen.queryByRole("list", { name: "Search results" })).toBeNull();
  });

  const quietLists = {
    "/api/v1/policies": jsonResponse([]),
    "/api/v1/repositories": jsonResponse([]),
    "/api/v1/schedules": jsonResponse([]),
    "/api/v1/restores": jsonResponse([]),
    "/api/v1/maintenance": jsonResponse([]),
    "/api/v1/replications": jsonResponse({ repository: [], snapshot: [] }),
  };

  it("says which reads could not be searched instead of claiming nothing matched", async () => {
    mockApi({
      ...quietLists,
      "/api/v1/snapshots": problemResponse(
        forbiddenProblem("Listing snapshots was refused.", "/api/v1/snapshots"),
      ),
    });
    mountApp("/doctor");
    const user = userEvent.setup();
    await user.type(await screen.findByRole("searchbox", { name: "Find an object" }), "zz");
    const results = await screen.findByRole("list", { name: "Search results" });
    expect(
      await within(results).findByText(/snapshots could not be searched/i),
    ).toBeInTheDocument();
    expect(results).not.toHaveTextContent("No object named like");
  });

  it("says it is still searching rather than that nothing matched", async () => {
    mockApi({
      ...quietLists,
      "/api/v1/snapshots": () => new Promise<Response>(() => undefined) as unknown as Response,
    });
    mountApp("/doctor");
    const user = userEvent.setup();
    await user.type(await screen.findByRole("searchbox", { name: "Find an object" }), "zz");
    const results = await screen.findByRole("list", { name: "Search results" });
    expect(within(results).getByText(/Searching/)).toBeInTheDocument();
    expect(results).not.toHaveTextContent("No object named like");
  });
});

describe("ObjectSearch — focus", () => {
  it("marks a focused search field with its border, not the tab-focus ring", () => {
    const rules = cssRules(readStyles());
    const input = rules.find((r) => r.selector.includes(".search__field input:focus-visible"));
    expect(input?.body).toMatch(/box-shadow:\s*none/);
    const field = rules.find((r) => r.selector.includes(".search__field:focus-within"));
    expect(field?.body).toMatch(/border-color:\s*var\(--accent\)/);
    expect(input?.selector).toContain(".ns-switcher__filter input:focus-visible");
  });
});
