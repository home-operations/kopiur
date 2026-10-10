import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRouter,
} from "@tanstack/react-router";
import { describe, expect, it, vi } from "vitest";

import { renderWithRouter } from "../test-utils";
import { InspectLink } from "./InspectLink";
import { useInspect } from "./inspect";

function Harness() {
  const { stack, close } = useInspect();
  return (
    <div>
      <InspectLink target={{ kind: "snapshotPolicy", namespace: "a", name: "app-data" }}>
        app-data
      </InspectLink>
      <InspectLink target={{ kind: "repository", namespace: "a", name: "nas" }}>nas</InspectLink>
      <InspectLink target={{ kind: "clusterRepository", name: "shared" }}>shared</InspectLink>
      <output aria-label="open">
        {stack.length === 0 ? "none" : stack.map((t) => `${t.kind}:${t.name}`).join(" > ")}
      </output>
      <button type="button" onClick={close}>
        close
      </button>
    </div>
  );
}

describe("InspectLink and useInspect", () => {
  it("opens beside the scope and every other parameter", async () => {
    renderWithRouter(<Harness />, "/policies?namespace=a&offset=20");
    const link = await screen.findByRole("link", { name: "app-data" });
    expect(link.getAttribute("href")).toMatch(/namespace=a/);
    expect(link.getAttribute("href")).toMatch(/offset=20/);
    expect(link.getAttribute("href")).toMatch(/inspect=snapshot-policy%2Fa%2Fapp-data/);
    await userEvent.click(link);
    expect(await screen.findByText("snapshotPolicy:app-data")).toBeInTheDocument();
  });

  it("opens another resource on top, and closing backs up one drawer at a time", async () => {
    const { router } = renderHarness("/policies?namespace=a");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("link", { name: "app-data" }));
    await screen.findByText("snapshotPolicy:app-data");
    expect(screen.getByRole("link", { name: "nas" }).getAttribute("href")).toMatch(
      /inspect=snapshot-policy%2Fa%2Fapp-data%2Crepository%2Fa%2Fnas/,
    );
    await user.click(screen.getByRole("link", { name: "nas" }));
    await screen.findByText("snapshotPolicy:app-data > repository:nas");
    await user.click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("snapshotPolicy:app-data");
    await user.click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("none");
    await waitFor(() => {
      expect(router.state.location.search).toEqual({ namespace: "a" });
    });
    // Each close stepped back over the entry its opening pushed.
    expect(router.history.canGoBack()).toBe(false);
  });

  it("Back closes the top drawer, like clicking out of it", async () => {
    const { router } = renderHarness("/policies?namespace=a");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("link", { name: "app-data" }));
    await user.click(await screen.findByRole("link", { name: "nas" }));
    await screen.findByText("snapshotPolicy:app-data > repository:nas");
    router.history.back();
    expect(await screen.findByText("snapshotPolicy:app-data")).toBeInTheDocument();
  });

  it("backs down to a resource already open rather than opening it twice", async () => {
    renderHarness("/policies?inspect=snapshot-policy%2Fa%2Fapp-data%2Crepository%2Fa%2Fnas");
    const user = userEvent.setup();
    await screen.findByText("snapshotPolicy:app-data > repository:nas");
    const link = screen.getByRole("link", { name: "app-data" });
    expect(link.getAttribute("href")).toMatch(/inspect=snapshot-policy%2Fa%2Fapp-data$/);
    await user.click(link);
    expect(await screen.findByText("snapshotPolicy:app-data")).toBeInTheDocument();
  });

  it("backs down to an earlier drawer through history, so Back never reopens a closed one", async () => {
    const { router } = renderHarness("/policies?namespace=a");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("link", { name: "app-data" }));
    await user.click(await screen.findByRole("link", { name: "nas" }));
    await user.click(await screen.findByRole("link", { name: "shared" }));
    await screen.findByText("snapshotPolicy:app-data > repository:nas > clusterRepository:shared");
    await user.click(screen.getByRole("link", { name: "app-data" }));
    await screen.findByText("snapshotPolicy:app-data");
    // Back closes the one drawer left, rather than reopening nas.
    router.history.back();
    expect(await screen.findByText("none")).toBeInTheDocument();
    expect(router.state.location.search).toEqual({ namespace: "a" });
    expect(router.history.canGoBack()).toBe(false);
  });

  it("closing the drawer backed down to leaves no closed drawer behind it", async () => {
    const { router } = renderHarness("/policies?namespace=a");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("link", { name: "app-data" }));
    await user.click(await screen.findByRole("link", { name: "nas" }));
    await user.click(await screen.findByRole("link", { name: "shared" }));
    await user.click(await screen.findByRole("link", { name: "app-data" }));
    await screen.findByText("snapshotPolicy:app-data");
    await user.click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("none");
    expect(router.history.canGoBack()).toBe(false);
  });

  it("replaces in place when the drawers it backs past were not opened here", async () => {
    // A deep link opened app-data and nas together: there is no entry for
    // app-data alone to step back to.
    const { router } = renderHarness(
      "/policies?inspect=snapshot-policy%2Fa%2Fapp-data%2Crepository%2Fa%2Fnas",
    );
    const user = userEvent.setup();
    await screen.findByText("snapshotPolicy:app-data > repository:nas");
    await user.click(screen.getByRole("link", { name: "shared" }));
    await screen.findByText("snapshotPolicy:app-data > repository:nas > clusterRepository:shared");
    await user.click(screen.getByRole("link", { name: "app-data" }));
    expect(await screen.findByText("snapshotPolicy:app-data")).toBeInTheDocument();
    expect(router.state.location.search).toEqual({ inspect: "snapshot-policy/a/app-data" });
  });

  it("closes the top of a deep-linked stack in place, then the next", async () => {
    const { router } = renderHarness(
      "/policies?inspect=snapshot-policy%2Fa%2Fapp-data%2Crepository%2Fa%2Fnas",
    );
    const user = userEvent.setup();
    await screen.findByText("snapshotPolicy:app-data > repository:nas");
    await user.click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("snapshotPolicy:app-data");
    expect(router.state.location.search).toEqual({ inspect: "snapshot-policy/a/app-data" });
    await user.click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("none");
  });

  it("closes a deep link in place, without leaving the page", async () => {
    const { router } = renderHarness("/policies?inspect=snapshot-policy%2Fa%2Fapp-data");
    const user = userEvent.setup();
    await screen.findByText("snapshotPolicy:app-data");
    await user.click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("none");
    expect(router.state.location.pathname).toBe("/policies");
  });
});

describe("opening and closing the drawer leaves the page where it was", () => {
  // The router scrolls the window to the top on a navigation unless told not
  // to, and opening or closing the drawer is a navigation.
  const toTop = (spy: { mock: { calls: unknown[][] } }) =>
    spy.mock.calls.filter(
      ([arg]) => typeof arg === "object" && (arg as { top?: number }).top === 0,
    );

  it("does not scroll to the top when a drawer opens, or when another opens on top", async () => {
    const scroll = vi.spyOn(window, "scrollTo").mockImplementation(() => undefined);
    renderHarness("/policies?namespace=a");
    const user = userEvent.setup();
    const link = await screen.findByRole("link", { name: "app-data" });
    // Landing on a page starts it at the top; only what follows counts.
    scroll.mockClear();
    await user.click(link);
    await screen.findByText("snapshotPolicy:app-data");
    await user.click(screen.getByRole("link", { name: "nas" }));
    await screen.findByText("snapshotPolicy:app-data > repository:nas");
    expect(toTop(scroll)).toEqual([]);
    scroll.mockRestore();
  });

  it("does not scroll to the top when a deep-linked drawer closes in place", async () => {
    const scroll = vi.spyOn(window, "scrollTo").mockImplementation(() => undefined);
    renderHarness("/policies?inspect=snapshot-policy%2Fa%2Fapp-data");
    await screen.findByText("snapshotPolicy:app-data");
    scroll.mockClear();
    await userEvent.setup().click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("none");
    expect(toTop(scroll)).toEqual([]);
    scroll.mockRestore();
  });
});

function renderHarness(path: string) {
  const router = createRouter({
    routeTree: createRootRoute({ component: Harness }),
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  render(<RouterProvider router={router} />);
  return { router };
}
