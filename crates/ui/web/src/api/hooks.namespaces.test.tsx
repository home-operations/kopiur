import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import {
  forbiddenProblem,
  jsonResponse,
  mockApi,
  problemResponse,
  renderWithClient,
} from "../test-utils";
import { useNamespaces } from "./hooks";

function Probe() {
  const result = useNamespaces();
  if (result.isPending) return <p>loading</p>;
  if (result.isError) return <p>refused</p>;
  return <p>ns:{result.data.map((n) => `${n.name}=${String(n.objects)}`).join(",")}</p>;
}

describe("useNamespaces", () => {
  it("reads the namespaces the caller may see, with their object counts", async () => {
    mockApi({
      "/api/v1/namespaces": jsonResponse([
        { name: "billing", objects: 3 },
        { name: "media", objects: 2 },
      ]),
    });
    renderWithClient(<Probe />);
    expect(await screen.findByText("ns:billing=3,media=2")).toBeInTheDocument();
  });

  it("surfaces a refusal as an error rather than an empty list", async () => {
    mockApi({
      "/api/v1/namespaces": problemResponse(
        forbiddenProblem("Namespaces were refused.", "/api/v1/namespaces"),
      ),
    });
    renderWithClient(<Probe />);
    expect(await screen.findByText("refused")).toBeInTheDocument();
  });
});
