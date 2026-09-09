import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider, createMemoryHistory, createRouter } from "@tanstack/react-router";
import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import createFetchMock from "vitest-fetch-mock";

import { problemBanner } from "../api/problem";
import { createQueryClient } from "../api/queryClient";
import type { Me } from "../api/types";
import { routeTree } from "../routeTree.gen";
import { setThemePreference } from "../util/theme";
import { CAPABILITY_KEYS, capabilityReason } from "./capabilities";
import { NAV_ITEMS, sectionFor } from "./nav";

const fetchMock = createFetchMock(vi);
fetchMock.enableMocks();

const alice: Me = {
  user: "alice",
  groups: ["platform", "oncall"],
  email: "alice@example.test",
  source: "trustedHeaders",
  namespace: null,
  can: {
    createSnapshots: true,
    deleteSnapshots: false,
    createRestores: true,
    patchPolicies: true,
    patchSchedules: true,
    patchRepositories: false,
    patchClusterRepositories: false,
    patchMaintenances: true,
    patchRepositoryReplications: false,
    patchSnapshotReplications: false,
    createSessionJobs: true,
    deleteSessionJobs: true,
    execSessions: false,
  },
};

function mountAt(path: string) {
  const router = createRouter({
    routeTree,
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  const client = createQueryClient();
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

beforeEach(() => {
  fetchMock.resetMocks();
  problemBanner.dismiss();
  setThemePreference("system");
});

afterEach(() => {
  setThemePreference("system");
});

describe("AppShell", () => {
  it("lists the ten sections, marks the current one, and scopes the header to the namespace", async () => {
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    mountAt("/snapshots?namespace=prod");

    const nav = await screen.findByRole("navigation", { name: "Primary" });
    const links = within(nav).getAllByRole("link");
    expect(links.map((link) => link.textContent)).toEqual(NAV_ITEMS.map((item) => item.label));
    expect(links).toHaveLength(10);
    expect(within(nav).getByRole("link", { name: "Snapshots" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    expect(within(nav).getByRole("link", { name: "Overview" })).not.toHaveAttribute("aria-current");
    // The scope travels with every link so a namespaced view stays namespaced.
    expect(within(nav).getByRole("link", { name: "Policies" })).toHaveAttribute(
      "href",
      "/policies?namespace=prod",
    );

    const heading = screen.getByRole("heading", { level: 1 });
    expect(heading).toHaveTextContent("Snapshots");
    expect(heading).toHaveTextContent("prod");
  });

  it("shows the signed-in identity and re-asks /me for the current namespace", async () => {
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    mountAt("/policies?namespace=prod");

    const summary = await screen.findByText("alice", { selector: ".identity__user" });
    expect(summary.closest(".identity")).toHaveTextContent("via proxy headers");
    const called = fetchMock.mock.calls.map(([input]) => {
      if (typeof input === "string") {
        return input;
      }
      return input instanceof URL ? input.href : input.url;
    });
    expect(called).toContain("/api/v1/me?namespace=prod");
  });

  it("lists every capability with a check or a cross and the word", async () => {
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    // A placeholder section: the overview at "/" and the topology both have
    // reads of their own now, and these tests are about the shell, not the
    // route inside it.
    mountAt("/maintenance");
    await screen.findByText("alice", { selector: ".identity__user" });
    const list = screen.getByRole("list", { name: "Capabilities" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(CAPABILITY_KEYS.length);
    expect(within(list).getByText("Delete snapshots").closest("li")).toHaveAttribute(
      "data-allowed",
      "false",
    );
    expect(within(list).getByText("Delete snapshots").closest("li")).toHaveTextContent(
      "not permitted",
    );
    expect(within(list).getByText("Snapshot now").closest("li")).toHaveAttribute(
      "data-allowed",
      "true",
    );
    expect(list.closest(".identity")).toHaveTextContent("7 of 13 actions permitted cluster-wide");
  });

  it("reports a failed /me to the global banner and says the identity is unavailable", async () => {
    fetchMock.mockResponse("<html>gateway</html>", {
      status: 502,
      headers: { "content-type": "text/html" },
    });
    // A placeholder section: the overview at "/" and the topology both have
    // reads of their own now, and these tests are about the shell, not the
    // route inside it.
    mountAt("/maintenance");
    await screen.findByText("identity unavailable");
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Identity (/api/v1/me)");
    expect(alert).toHaveTextContent("GET /api/v1/me answered 502");
    expect(alert).toHaveTextContent(/Fix/);
  });

  it("lets the user override the OS theme and go back to following it", async () => {
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    // A placeholder section: the overview at "/" and the topology both have
    // reads of their own now, and these tests are about the shell, not the
    // route inside it.
    mountAt("/maintenance");
    const group = await screen.findByRole("group", { name: "Theme" });
    expect(within(group).getByRole("button", { name: "System" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(document.documentElement.dataset.theme).toBeUndefined();

    await userEvent.click(within(group).getByRole("button", { name: "Dark" }));
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(within(group).getByRole("button", { name: "Dark" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(window.localStorage.getItem("kopiur-ui.theme")).toBe("dark");

    await userEvent.click(within(group).getByRole("button", { name: "System" }));
    expect(document.documentElement.dataset.theme).toBeUndefined();
    expect(window.localStorage.getItem("kopiur-ui.theme")).toBeNull();
  });

  it("puts a skip link first, and activating it moves focus to the content", async () => {
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    mountAt("/snapshots?namespace=prod");
    await screen.findByRole("navigation", { name: "Primary" });

    // Eleven controls sit before the content; the first Tab must land here.
    await userEvent.tab();
    const skip = screen.getByRole("link", { name: "Skip to content" });
    expect(skip).toHaveFocus();
    expect(skip).toHaveAttribute("href", "#main");

    await userEvent.keyboard("{Enter}");
    const main = screen.getByRole("main");
    expect(main).toHaveAttribute("id", "main");
    expect(main).toHaveFocus();
  });

  it("names every theme button independently of its visible label", async () => {
    // Below 560px the label span is display:none and the icon is aria-hidden,
    // so the name must come from aria-label. jsdom ignores media queries, so
    // this asserts the attribute the name is computed from, not the query.
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    mountAt("/");
    const group = await screen.findByRole("group", { name: "Theme" });
    for (const label of ["System", "Light", "Dark"]) {
      const button = within(group).getByRole("button", { name: label });
      expect(button).toHaveAttribute("aria-label", label);
      // Strip the visible label the way the media query would, and the name
      // must survive.
      for (const span of button.querySelectorAll("span")) {
        span.remove();
      }
      expect(button).toHaveAccessibleName(label);
    }
  });

  it("keeps the namespace when the wordmark is used to go home", async () => {
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    mountAt("/snapshots?namespace=prod");
    await screen.findByRole("navigation", { name: "Primary" });
    const brand = screen.getByRole("link", { name: /Kopiur/ });
    expect(brand).toHaveAttribute("href", "/?namespace=prod");
  });

  it("renders the not-found state for an address no route serves", async () => {
    fetchMock.mockResponse(JSON.stringify(alice), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    mountAt("/nowhere/at/all");
    await act(async () => {
      await Promise.resolve();
    });
    expect(await screen.findByText("No such page")).toBeInTheDocument();
  });
});

describe("sectionFor", () => {
  it("maps a deep link to its section and an unknown path to Overview", () => {
    expect(sectionFor("/snapshots/prod/nightly-1").label).toBe("Snapshots");
    expect(sectionFor("/repositories/cluster-repository/nas").label).toBe("Repositories");
    expect(sectionFor("/").label).toBe("Overview");
    expect(sectionFor("/nowhere").label).toBe("Overview");
    // An off-rail page has its own title without becoming an eleventh section.
    expect(sectionFor("/gates").label).toBe("Gates");
    expect(NAV_ITEMS.map((item) => item.to)).not.toContain("/gates");
  });
});

describe("capabilityReason", () => {
  it("is undefined when allowed, and names the action, the user and the scope when not", () => {
    expect(capabilityReason(alice, "createSnapshots", "prod")).toBeUndefined();
    expect(capabilityReason(alice, "deleteSnapshots", "prod")).toContain(
      "Delete snapshots is not permitted for alice in namespace prod",
    );
    expect(capabilityReason(alice, "deleteSnapshots", undefined)).toContain("cluster-wide");
    expect(capabilityReason(alice, "deleteSnapshots", "prod")).toContain("kopiur-ui-editor");
  });

  it("requires every grant in a list — starting a browse session takes two", () => {
    expect(capabilityReason(alice, ["createSessionJobs", "execSessions"], "prod")).toContain(
      "Read through browse sessions is not permitted",
    );
    expect(
      capabilityReason(
        { ...alice, can: { ...alice.can, execSessions: true } },
        ["createSessionJobs", "execSessions"],
        "prod",
      ),
    ).toBeUndefined();
  });

  it("explains that permissions are still loading rather than pretending they are denied", () => {
    expect(capabilityReason(undefined, "createSnapshots", "prod")).toMatch(/not loaded/);
  });
});
