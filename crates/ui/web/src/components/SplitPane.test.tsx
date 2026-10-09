import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";

import { readStyles, stripComments } from "../testing/css";
import { SplitPane } from "./SplitPane";
import { SPLIT_GUTTER, SPLIT_MIN_PANE } from "./splitRatio";

const KEY = "kopiur-ui.test-split";

afterEach(() => {
  window.localStorage.clear();
});

function mount() {
  render(
    <SplitPane
      label="Two columns"
      storageKey={KEY}
      start={<section aria-label="Left">left</section>}
      end={<section aria-label="Right">right</section>}
    />,
  );
  return screen.getByRole("separator", { name: "Resize the two columns" });
}

describe("SplitPane", () => {
  it("puts the start side first, then the handle, then the end side", () => {
    mount();
    const left = screen.getByRole("region", { name: "Left" });
    const right = screen.getByRole("region", { name: "Right" });
    expect(left.compareDocumentPosition(right) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("is a focusable separator that moves with the arrow keys and remembers it", async () => {
    const handle = mount();
    expect(handle).toHaveAttribute("aria-orientation", "vertical");
    expect(handle).toHaveAttribute("aria-valuenow", "50");
    const user = userEvent.setup();
    handle.focus();
    expect(handle).toHaveFocus();
    await user.keyboard("{ArrowRight}");
    expect(handle).toHaveAttribute("aria-valuenow", "54");
    await user.keyboard("{ArrowLeft}{ArrowLeft}");
    expect(handle).toHaveAttribute("aria-valuenow", "46");
    expect(window.localStorage.getItem(KEY)).toBe("0.46");
  });

  it("goes to either end with Home and End, and back to half on a double click", async () => {
    const handle = mount();
    const user = userEvent.setup();
    handle.focus();
    await user.keyboard("{Home}");
    expect(handle).toHaveAttribute("aria-valuenow", "20");
    await user.keyboard("{End}");
    expect(handle).toHaveAttribute("aria-valuenow", "80");
    await user.dblClick(handle);
    expect(handle).toHaveAttribute("aria-valuenow", "50");
  });

  it("opens where it was left", () => {
    window.localStorage.setItem(KEY, "0.3");
    expect(mount()).toHaveAttribute("aria-valuenow", "30");
  });

  it("stacks the two sides, start first, when there is no room for both, and drops the handle", () => {
    const css = stripComments(readStyles());
    const query = /@container split \(max-width: (\d+)px\) \{([\s\S]*?)\n\}/.exec(css);
    expect(query).not.toBeNull();
    // Too narrow means less than both minimums and the gutter.
    expect(Number(query?.[1]) + 1).toBe(2 * SPLIT_MIN_PANE + SPLIT_GUTTER);
    const body = query?.[2] ?? "";
    expect(body).toMatch(/\.split__grid \{[^}]*grid-template-columns:\s*minmax\(0, 1fr\)/);
    expect(body).toMatch(/\.split__gutter \{[^}]*display:\s*none/);
  });
});
