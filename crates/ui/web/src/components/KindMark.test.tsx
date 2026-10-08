import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { KindChip, KindName } from "./KindMark";

describe("KindChip", () => {
  it("is a decorative chip keyed by the kind's token slug", () => {
    const { container } = render(<KindChip kind="snapshotPolicy" />);
    const chip = container.querySelector(".kind-chip");
    expect(chip).toHaveAttribute("aria-hidden", "true");
    expect(chip).toHaveAttribute("data-kind", "snapshot-policy");
    expect(chip?.querySelector("svg")).not.toBeNull();
    expect(chip).toHaveClass("kind-chip--md");
  });

  it("comes in the three sizes the primitives use", () => {
    const { container } = render(<KindChip kind="snapshot" size="sm" />);
    expect(container.querySelector(".kind-chip")).toHaveClass("kind-chip--sm");
  });
});

describe("KindName", () => {
  it("says the CRD kind, and keeps the label-strip hook older screens' tests read", () => {
    const { getByText } = render(<KindName kind="snapshotPolicy" />);
    const name = getByText("SnapshotPolicy");
    expect(name).toHaveClass("kind-name", "label-strip__kind");
  });
});
