import { describe, expect, it, vi } from "vitest";

import type { NodeKind } from "../../api/types";
import { nodeMeaning, nodeSection } from "./drawer";

/** Every kind the generated union has today; a new one fails to compile here too. */
const KINDS: readonly NodeKind[] = [
  "repository",
  "clusterRepository",
  "backend",
  "policy",
  "namespace",
  "namespaceSelector",
];

describe("nodeSection", () => {
  it("sends both repository scopes to the repositories section and a policy to policies", () => {
    expect(nodeSection("repository")).toEqual({ to: "/repositories", section: "repositories" });
    expect(nodeSection("clusterRepository")).toEqual({
      to: "/repositories",
      section: "repositories",
    });
    expect(nodeSection("policy")).toEqual({ to: "/policies", section: "policies" });
  });

  it("offers no link for the three nodes that are not objects in the cluster", () => {
    expect(nodeSection("backend")).toBeNull();
    expect(nodeSection("namespace")).toBeNull();
    expect(nodeSection("namespaceSelector")).toBeNull();
  });

  it("never builds a URL segment out of the display kind", () => {
    // addenda item 16: the segment is the server's `kindPath`, which the graph
    // does not carry — so no link here goes deeper than the section.
    for (const kind of KINDS) {
      const section = nodeSection(kind);
      if (section === null) {
        continue;
      }
      expect(section.to).not.toContain("cluster-repository");
      expect(section.to.split("/")).toHaveLength(2);
    }
  });

  it("answers null for a kind a newer server invented, rather than throwing", () => {
    const future = "sidecarRepository" as NodeKind;
    expect(nodeSection(future)).toBeNull();
  });
});

describe("nodeMeaning", () => {
  it("says what every kind is, in words an operator who has not read the CRDs can use", () => {
    for (const kind of KINDS) {
      expect(nodeMeaning(kind).length).toBeGreaterThan(20);
    }
    expect(nodeMeaning("backend")).toContain("not an object in the cluster");
    expect(nodeMeaning("namespaceSelector")).toContain("not an object in the cluster");
  });

  it("says a kind it does not recognise is unrecognised, never describing it as something else", () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    expect(nodeMeaning("sidecarRepository" as NodeKind)).toContain("does not recognise");
  });
});
