import { describe, expect, it } from "vitest";

import type { AdmittedNamespacesView } from "../api/types";
import { admitsText } from "./admits";

describe("admitsText", () => {
  it("names every admission the spec can express", () => {
    expect(admitsText("all")).toBe("admits all namespaces");
    expect(admitsText("none")).toBe("admits no namespaces");
    expect(admitsText({ listed: { count: 1 } })).toBe("admits 1 namespace");
    expect(admitsText({ listed: { count: 3 } })).toBe("admits 3 namespaces");
    expect(admitsText({ selector: { selector: "backup=yes" } })).toBe(
      "admits namespaces matching backup=yes",
    );
  });

  it("never prints the controller's -1 sentinel, whatever a newer server sends", () => {
    const fromTheFuture = "everywhere" as unknown as AdmittedNamespacesView;
    expect(admitsText(fromTheFuture)).not.toContain("-1");
    expect(admitsText(fromTheFuture)).toContain("everywhere");
  });
});
