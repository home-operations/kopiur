import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { calledPaths, jsonResponse, mockApi, renderWithClient } from "../test-utils";
import { useOverview } from "./hooks";

function Probe({ namespace }: { namespace?: string }) {
  const result = useOverview(namespace);
  if (!result.isSuccess) return <p>waiting</p>;
  return <p>{result.data.kinds.map((k) => `${k.kind}:${String(k.total)}`).join(",")}</p>;
}

describe("useOverview", () => {
  it("reads the per-kind tallies for the current scope", async () => {
    mockApi({
      "/api/v1/overview": jsonResponse({
        kinds: [
          { kind: "repository", total: 3, byHealth: [{ health: "failed", count: 1 }] },
          { kind: "snapshot", total: 6, byHealth: [] },
        ],
        snapshotWindowHours: 24,
        generatedAt: "2026-10-08T12:00:00Z",
      }),
    });
    renderWithClient(<Probe namespace="media" />);
    expect(await screen.findByText("repository:3,snapshot:6")).toBeInTheDocument();
    expect(calledPaths()).toContain("/api/v1/overview?namespace=media");
  });
});
