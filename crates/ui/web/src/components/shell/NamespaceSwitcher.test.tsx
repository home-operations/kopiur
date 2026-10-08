import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";

import { cssRules, readStyles } from "../../testing/css";

import {
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../../test-utils";

const NAMESPACES = jsonResponse([
  { name: "billing", objects: 3 },
  { name: "media", objects: 2 },
]);

afterEach(() => {
  document.body.innerHTML = "";
});

describe("NamespaceSwitcher", () => {
  it("scopes the current page to the namespace chosen", async () => {
    mockApi({ "/api/v1/namespaces": NAMESPACES });
    const { router } = mountApp("/snapshots");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Namespace: all namespaces" }));
    const panel = screen.getByRole("dialog", { name: "Choose a namespace" });
    await user.click(await within(panel).findByRole("button", { name: /^media/ }));
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/snapshots");
      expect(router.state.location.search).toMatchObject({ namespace: "media" });
    });
    expect(screen.queryByRole("dialog", { name: "Choose a namespace" })).toBeNull();
  });

  it("filters the list as you type and keeps 'all namespaces' first", async () => {
    mockApi({ "/api/v1/namespaces": NAMESPACES });
    mountApp("/");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Namespace: all namespaces" }));
    const panel = screen.getByRole("dialog", { name: "Choose a namespace" });
    await within(panel).findByRole("button", { name: /^billing/ });
    await user.type(within(panel).getByRole("searchbox", { name: "Filter namespaces" }), "bil");
    const names = within(panel)
      .getAllByRole("button")
      .map((b) => b.textContent);
    expect(names[0]).toMatch(/^all namespaces/);
    expect(names.join("|")).toContain("billing");
    expect(names.join("|")).not.toContain("media");
  });

  it("still offers 'all namespaces' when the list is refused", async () => {
    mockApi({
      "/api/v1/namespaces": problemResponse(
        forbiddenProblem("Namespaces were refused.", "/api/v1/namespaces"),
      ),
    });
    mountApp("/");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Namespace: all namespaces" }));
    const panel = screen.getByRole("dialog", { name: "Choose a namespace" });
    await waitFor(() => {
      expect(
        within(panel)
          .getAllByRole("button")
          .map((b) => b.textContent),
      ).toEqual([expect.stringMatching(/^all namespaces/)]);
    });
  });

  it("closes on Escape and hands focus back to its button", async () => {
    mockApi({ "/api/v1/namespaces": NAMESPACES });
    mountApp("/");
    const user = userEvent.setup();
    const button = await screen.findByRole("button", { name: "Namespace: all namespaces" });
    await user.click(button);
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Choose a namespace" })).toBeNull();
    expect(button).toHaveFocus();
  });
});

describe("NamespaceSwitcher — fit", () => {
  it("keeps the open panel inside the sidebar: its column may shrink below the filter's natural width", () => {
    const rule = cssRules(readStyles()).find((r) => r.selector === ".ns-switcher__panel");
    expect(rule?.body).toMatch(/grid-template-columns:\s*minmax\(0,\s*1fr\)/);
  });
});
