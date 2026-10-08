import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { cssRules, readStyles } from "../testing/css";
import { type TabSpec, Tabs } from "./Tabs";

const TABS: TabSpec[] = [
  { id: "storage", label: "Storage", render: () => <p>storage body</p> },
  { id: "catalog", label: "Catalog", count: 3, render: () => <p>catalog body</p> },
  { id: "conditions", label: "Conditions", render: () => <p>conditions body</p> },
];

function Harness({ initial = "storage" }: { initial?: string }) {
  const [selected, setSelected] = useState(initial);
  return (
    <Tabs label="About this repository" tabs={TABS} selected={selected} onSelect={setSelected} />
  );
}

describe("Tabs", () => {
  it("is a named tablist whose selected tab labels the one panel on screen", () => {
    render(<Harness />);
    const list = screen.getByRole("tablist", { name: "About this repository" });
    expect(list).toBeInTheDocument();
    const tab = screen.getByRole("tab", { name: "Storage" });
    expect(tab).toHaveAttribute("aria-selected", "true");
    const panel = screen.getByRole("tabpanel", { name: "Storage" });
    expect(tab).toHaveAttribute("aria-controls", panel.id);
    expect(panel).toHaveTextContent("storage body");
    // Only the selected panel is mounted.
    expect(screen.queryByText("catalog body")).toBeNull();
    expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
  });

  it("shows a count beside a tab's name, read with it", () => {
    render(<Harness />);
    expect(screen.getByRole("tab", { name: "Catalog 3" })).toBeInTheDocument();
  });

  it("is one tab stop: only the selected tab is in the tab order", () => {
    render(<Harness />);
    expect(screen.getByRole("tab", { name: "Storage" })).toHaveAttribute("tabindex", "0");
    expect(screen.getByRole("tab", { name: "Catalog 3" })).toHaveAttribute("tabindex", "-1");
  });

  it("selects with a click", async () => {
    render(<Harness />);
    await userEvent.click(screen.getByRole("tab", { name: "Conditions" }));
    expect(screen.getByRole("tabpanel")).toHaveTextContent("conditions body");
  });

  it("moves with the arrow keys, wrapping, and jumps with Home and End", async () => {
    render(<Harness />);
    const user = userEvent.setup();
    screen.getByRole("tab", { name: "Storage" }).focus();
    await user.keyboard("{ArrowLeft}");
    expect(screen.getByRole("tab", { name: "Conditions" })).toHaveFocus();
    expect(screen.getByRole("tabpanel")).toHaveTextContent("conditions body");
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("tab", { name: "Storage" })).toHaveFocus();
    await user.keyboard("{End}");
    expect(screen.getByRole("tab", { name: "Conditions" })).toHaveFocus();
    await user.keyboard("{Home}");
    expect(screen.getByRole("tab", { name: "Storage" })).toHaveAttribute("aria-selected", "true");
  });

  it("falls back to the first tab when told to show one it does not have", () => {
    render(<Harness initial="sessions" />);
    expect(screen.getByRole("tab", { name: "Storage" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("tabpanel")).toHaveTextContent("storage body");
  });

  it("scrolls sideways rather than wrapping when the tabs outgrow the panel", () => {
    const list = cssRules(readStyles()).find((r) => r.selector === ".tabs__list");
    expect(list?.body).toMatch(/overflow-x:\s*auto/);
  });
});
