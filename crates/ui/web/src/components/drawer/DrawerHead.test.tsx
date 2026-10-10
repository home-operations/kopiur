import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { healthLamp } from "../health";
import { DrawerHead } from "./DrawerHead";

describe("DrawerHead", () => {
  it("leads with the verdict as a lettered lamp and a sentence, then the facts", () => {
    const { container } = render(
      <DrawerHead
        verdict={{
          label: "Snapshot verdict",
          lamp: healthLamp("failed"),
          text: "Failed: the mover exited 1.",
        }}
        stats={[
          { label: "Size", value: "2.0 MiB" },
          { label: "Files", value: "10" },
        ]}
        statsLabel="Snapshot nightly at a glance"
        findings={<p>a gate</p>}
      />,
    );
    const verdict = screen.getByRole("status", { name: "Snapshot verdict" });
    expect(verdict).toHaveTextContent("Failed: the mover exited 1.");
    const lamp = verdict.querySelector(".verdict__lamp[data-health='failed']");
    expect(lamp?.querySelector("svg")).not.toBeNull();
    expect(container.querySelector('dl[aria-label="Snapshot nightly at a glance"]')).not.toBeNull();
    expect(screen.getByText("a gate")).toBeInTheDocument();
    expect(container.querySelector("h1, h2")).toBeNull();
  });

  it("lays four headline facts in one row, three as a card's", () => {
    const four = [1, 2, 3, 4].map((n) => ({ label: `F${String(n)}`, value: String(n) }));
    const { container, rerender } = render(<DrawerHead stats={four} statsLabel="facts" />);
    expect(container.querySelector("dl.stats")).toHaveClass("stats--page");
    rerender(<DrawerHead stats={four.slice(0, 3)} statsLabel="facts" />);
    expect(container.querySelector("dl.stats")).toHaveClass("stats--card");
  });
});
