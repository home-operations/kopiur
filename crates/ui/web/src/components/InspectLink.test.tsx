import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRouter,
} from "@tanstack/react-router";
import { describe, expect, it } from "vitest";

import { renderWithRouter } from "../test-utils";
import { InspectLink } from "./InspectLink";
import { useInspect } from "./inspect";

function Harness() {
  const { target, close } = useInspect();
  return (
    <div>
      <InspectLink target={{ kind: "snapshotPolicy", namespace: "a", name: "app-data" }}>
        app-data
      </InspectLink>
      <InspectLink target={{ kind: "repository", namespace: "a", name: "nas" }}>nas</InspectLink>
      <output aria-label="open">
        {target === null ? "none" : `${target.kind}:${target.name}`}
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

  it("closing goes back to where it opened from, so Back does not reopen it", async () => {
    const { router } = renderHarness("/policies?namespace=a");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("link", { name: "app-data" }));
    await screen.findByText("snapshotPolicy:app-data");
    // Another resource from inside replaces, it does not stack.
    await user.click(screen.getByRole("link", { name: "nas" }));
    await screen.findByText("repository:nas");
    await user.click(screen.getByRole("button", { name: "close" }));
    await screen.findByText("none");
    await waitFor(() => {
      expect(router.state.location.search).toEqual({ namespace: "a" });
    });
    expect(router.history.canGoBack()).toBe(false);
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

function renderHarness(path: string) {
  const router = createRouter({
    routeTree: createRootRoute({ component: Harness }),
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  render(<RouterProvider router={router} />);
  return { router };
}
