import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { calledPaths, jsonResponse, mockApi, renderWithClient } from "../test-utils";
import { useSnapshotSearch } from "./hooks";

function Probe({ q }: { q: string }) {
  const result = useSnapshotSearch(q, "kopiur-dev");
  const names = result.data?.items.map((s) => s.name).join(",") ?? "";
  return <p>found:{names}</p>;
}

const page = (names: string[]) =>
  jsonResponse({
    items: names.map((name) => ({ namespace: "kopiur-dev", name, pinned: false })),
    total: names.length,
    offset: 0,
    limit: 10,
  });

describe("useSnapshotSearch", () => {
  it("asks the server for at most ten name matches in the current scope", async () => {
    mockApi({ "/api/v1/snapshots": () => page(["app-data-manual"]) });
    renderWithClient(<Probe q="app" />);
    expect(await screen.findByText("found:app-data-manual")).toBeInTheDocument();
    const call = calledPaths().find((p) => p.startsWith("/api/v1/snapshots"));
    const url = new URL(call ?? "", "http://localhost");
    expect(url.searchParams.get("q")).toBe("app");
    expect(url.searchParams.get("limit")).toBe("10");
    expect(url.searchParams.get("namespace")).toBe("kopiur-dev");
  });

  it("does not search on a single character or only whitespace", async () => {
    mockApi({ "/api/v1/snapshots": () => page(["x"]) });
    renderWithClient(<Probe q="a" />);
    await screen.findByText("found:");
    renderWithClient(<Probe q="   " />);
    expect(calledPaths().some((p) => p.startsWith("/api/v1/snapshots"))).toBe(false);
  });
});
