import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";

import { calledPaths, jsonResponse, mockApi, mountApp } from "../../test-utils";

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
  it("finds objects of every kind by name and links the ones with a page", async () => {
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
    expect(await within(results).findByRole("link", { name: /app-data-manual/ })).toHaveAttribute(
      "href",
      "/snapshots/kopiur-dev/app-data-manual",
    );
    expect(
      within(results).getByRole("link", { name: /SnapshotPolicy.*app-data$/ }),
    ).toHaveAttribute("href", "/policies/kopiur-dev/app-data");
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
});
