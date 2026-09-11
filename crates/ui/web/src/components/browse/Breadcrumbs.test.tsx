import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { renderWithRouter } from "../../test-utils";
import { Breadcrumbs } from "./Breadcrumbs";

function mount(path: string, scope?: string) {
  renderWithRouter(
    <Breadcrumbs
      namespace="media"
      name="nightly-1"
      path={path}
      rootLabel="nightly-1"
      scope={scope}
    />,
  );
  return screen.findByRole("navigation", { name: "Path inside the snapshot" });
}

describe("Breadcrumbs", () => {
  it("names the snapshot at the root and every component below it", async () => {
    const trail = await mount("var/log");
    const items = within(trail).getAllByRole("listitem");
    expect(items.map((item) => item.textContent)).toEqual(["nightly-1", "var", "log"]);
  });

  it("links every ancestor to its own path and leaves the current directory as text", async () => {
    await mount("var/log");
    expect(screen.getByRole("link", { name: "nightly-1" })).toHaveAttribute(
      "href",
      "/snapshots/media/nightly-1/browse",
    );
    expect(screen.getByRole("link", { name: "var" })).toHaveAttribute(
      "href",
      "/snapshots/media/nightly-1/browse?path=var",
    );
    // A link to the page you are on is a control that does nothing.
    expect(screen.queryByRole("link", { name: "log" })).toBeNull();
    expect(screen.getByText("log")).toHaveAttribute("aria-current", "page");
  });

  it("carries the console's namespace scope through every crumb", async () => {
    await mount("var/log", "media");
    expect(screen.getByRole("link", { name: "nightly-1" })).toHaveAttribute(
      "href",
      "/snapshots/media/nightly-1/browse?namespace=media",
    );
    expect(screen.getByRole("link", { name: "var" })).toHaveAttribute(
      "href",
      "/snapshots/media/nightly-1/browse?namespace=media&path=var",
    );
  });

  it("drops ?offset= on the way up, because a page number means nothing in the parent", async () => {
    await mount("var/log");
    for (const link of screen.getAllByRole("link")) {
      expect(link.getAttribute("href")).not.toContain("offset");
    }
  });

  it("is one crumb at the snapshot root, with no link at all", async () => {
    const trail = await mount("");
    expect(within(trail).getAllByRole("listitem")).toHaveLength(1);
    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });
});
