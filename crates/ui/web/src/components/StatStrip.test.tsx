import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { StatStrip } from "./StatStrip";

describe("StatStrip", () => {
  it("lays out named values as a labelled definition list", () => {
    render(
      <StatStrip
        label="This run"
        stats={[
          { label: "Size", value: "2.0 MiB" },
          { label: "Started", value: "58m ago", abs: "2026-10-07T15:32:38Z" },
        ]}
      />,
    );
    const list = screen.getByLabelText("This run");
    expect(list.tagName).toBe("DL");
    expect(within(list).getByText("Size")).toBeInTheDocument();
    const started = within(list).getByText("58m ago").closest("dd");
    expect(started?.querySelector("time")).toHaveAttribute("datetime", "2026-10-07T15:32:38Z");
  });

  it("says each kind of absence differently, and never as a blank", () => {
    render(
      <StatStrip
        label="Facts"
        stats={[
          { label: "Last verified", value: { absent: "loud", text: "never verified" } },
          { label: "New bytes", value: { absent: "unreported", field: "snapshotBytesNew" } },
          { label: "Files failed", value: { absent: "na" } },
        ]}
      />,
    );
    const loud = screen.getByText("never verified").closest(".absent--loud");
    expect(loud).not.toBeNull();
    expect(loud?.querySelector("svg")).not.toBeNull();
    expect(screen.getByText("not reported")).toHaveAttribute("title");
    expect(screen.getByText("—")).toHaveClass("absent");
  });

  it("comes in a page variant and a card variant", () => {
    const { container } = render(
      <StatStrip label="Card" variant="card" stats={[{ label: "A", value: "1" }]} />,
    );
    expect(container.querySelector("dl")).toHaveClass("stats", "stats--card");
  });
});
