import { describe, expect, it, vi } from "vitest";

import type { NodeKind } from "../../api/types";
import { nodeMeaning } from "./drawer";

/** Every kind the generated union has today; a new one fails to compile here too. */
const KINDS: readonly NodeKind[] = [
  "repository",
  "clusterRepository",
  "backend",
  "policy",
  "namespace",
  "namespaceSelector",
];

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
