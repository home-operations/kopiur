import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { PolicyRow } from "../api/types";
import {
  bodyRows,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  nth,
  problemResponse,
  unletteredLamps,
} from "../test-utils";

const rows: PolicyRow[] = [
  {
    namespace: "media",
    name: "nightly",
    repositories: ["Repository/media/nas", "ClusterRepository/shared"],
    multiRepo: true,
    suspended: false,
    lastSuccessfulSnapshot: "2026-09-09T02:04:00Z",
    lastVerified: "2026-09-07T04:00:00Z",
    activeSnapshotCount: 42,
  },
  {
    namespace: "prod",
    name: "hourly",
    repositories: ["Repository/prod/db"],
    multiRepo: false,
    suspended: true,
    lastSuccessfulSnapshot: null,
    lastVerified: null,
    activeSnapshotCount: 0,
  },
];

function table() {
  return screen.findByRole("table", { name: "Policies" });
}

describe("Policies", () => {
  it("lists each recipe with what it writes into and what it last did", async () => {
    mockApi({ "/api/v1/policies": jsonResponse(rows) });
    mountApp("/policies");
    const body = bodyRows(await table());
    expect(body).toHaveLength(2);
    expect(nth(body, 0)).toHaveTextContent("nightly");
    // What it writes into: references to each repository's own page.
    expect(
      nth(body, 0).querySelector('a.ref[data-kind="repository"]')?.getAttribute("href"),
    ).toMatch(/inspect=repository%2Fmedia%2Fnas$/);
    expect(
      nth(body, 0).querySelector('a.ref[data-kind="cluster-repository"]')?.getAttribute("href"),
    ).toMatch(/inspect=cluster-repository%2Fshared$/);
    expect(nth(body, 0)).toHaveAttribute("data-kind", "snapshot-policy");
    expect(nth(body, 0)).toHaveTextContent("fans out");
    expect(nth(body, 0)).toHaveTextContent("42");
    expect(nth(body, 1)).toHaveTextContent("hourly");
  });

  it("opens a row in the resource drawer, carrying namespace and name", async () => {
    mockApi({ "/api/v1/policies": jsonResponse(rows) });
    mountApp("/policies");
    const body = bodyRows(await table());
    expect(
      within(nth(body, 0)).getByRole("link", { name: "nightly" }).getAttribute("href"),
    ).toMatch(/inspect=snapshot-policy%2Fmedia%2Fnightly$/);
    expect(within(nth(body, 1)).getByRole("link", { name: "hourly" }).getAttribute("href")).toMatch(
      /inspect=snapshot-policy%2Fprod%2Fhourly$/,
    );
  });

  it("says a recipe has never succeeded rather than leaving the cell blank", async () => {
    mockApi({ "/api/v1/policies": jsonResponse(rows) });
    mountApp("/policies");
    const body = bodyRows(await table());
    expect(within(nth(body, 1)).getByText("never succeeded")).toBeInTheDocument();
    expect(within(nth(body, 1)).getByText("never verified")).toBeInTheDocument();
    // A measured zero is a measurement, not an absence.
    expect(nth(body, 1)).toHaveTextContent("0");
  });

  it("lamps those absences, because they are the only alarm a policy row has", async () => {
    mockApi({ "/api/v1/policies": jsonResponse(rows) });
    mountApp("/policies");
    const body = bodyRows(await table());
    // There is no health column here — a policy publishes none — so the
    // failed ink on these two cells was the whole signal, and drawn as ink
    // alone it was a signal a colour-blind operator could not receive.
    for (const word of ["never succeeded", "never verified"]) {
      const lamp = within(nth(body, 1)).getByText(word).closest(".health");
      expect(lamp).toHaveAttribute("data-health", "failed");
      expect(lamp?.querySelector("svg")).not.toBeNull();
      // The wording is the fact, not a verdict: nothing announces "Failed"
      // about a policy, which is a health the operator never published.
      expect(lamp).not.toHaveTextContent("(Failed)");
    }
  });

  it("leaves no health colour on this screen carried by hue alone", async () => {
    mockApi({ "/api/v1/policies": jsonResponse(rows) });
    mountApp("/policies");
    await table();
    expect(unletteredLamps(document.body)).toEqual([]);
  });

  it("lights the suspended lamp for a suspended policy and no lamp otherwise", async () => {
    mockApi({ "/api/v1/policies": jsonResponse(rows) });
    mountApp("/policies");
    const body = bodyRows(await table());
    // A policy has no operator health, so the running one gets a word, not a
    // green lamp.
    expect(nth(body, 0).querySelector(".health")).toBeNull();
    expect(within(nth(body, 0)).getByText("Active")).toBeInTheDocument();
    expect(nth(body, 1).querySelector(".health")).toHaveAttribute("data-health", "suspended");
  });

  it("passes the namespace scope to the list endpoint", async () => {
    mockApi({ "/api/v1/policies": jsonResponse([]) });
    mountApp("/policies?namespace=media");
    await screen.findByRole("region", { name: "Policies" });
    expect(fetchMock.mock.calls.map(([input]) => input)).toContain(
      "/api/v1/policies?namespace=media",
    );
  });

  it("teaches what a policy is when there are none", async () => {
    mockApi({ "/api/v1/policies": jsonResponse([]) });
    mountApp("/policies?namespace=prod");
    const region = await screen.findByRole("region", { name: "Policies" });
    const title = await within(region).findByText("No policies in prod");
    expect(title.closest('[role="status"]')).toHaveTextContent("names the sources to back up");
  });

  it("renders the not-permitted state for a 403 and offers no retry", async () => {
    mockApi({
      "/api/v1/policies": problemResponse(
        forbiddenProblem("Policies were refused.", "/api/v1/policies"),
      ),
    });
    mountApp("/policies");
    const region = await screen.findByRole("region", { name: "Policies" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("Policies were refused");
    expect(region.querySelector('[data-state="not-permitted"]')).not.toBeNull();
    expect(within(region).queryByRole("button", { name: "Retry" })).toBeNull();
  });

  it("renders the error state with a retry for any other failure", async () => {
    mockApi({
      "/api/v1/policies": new Response("<html>gateway</html>", {
        status: 502,
        headers: { "content-type": "text/html" },
      }),
    });
    mountApp("/policies");
    const region = await screen.findByRole("region", { name: "Policies" });
    expect(await within(region).findByRole("alert")).toHaveTextContent("answered 502");
    expect(within(region).getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("shows a skeleton while the policies load", async () => {
    fetchMock.resetMocks();
    fetchMock.mockResponse(() => new Promise<Response>(() => undefined));
    mountApp("/policies");
    const region = await screen.findByRole("region", { name: "Policies" });
    expect(within(region).getByRole("status", { busy: true })).toBeInTheDocument();
  });
});

describe("Policies — whole-row link", () => {
  it("makes every row open its policy in the drawer, through the name", async () => {
    mockApi({ "/api/v1/policies": jsonResponse(rows) });
    const { router } = mountApp("/policies");
    const link = nth(bodyRows(await table()), 0).querySelector<HTMLElement>("a.row-link");
    expect(link?.getAttribute("href")).toMatch(/inspect=snapshot-policy%2Fmedia%2Fnightly$/);
    if (link === null) throw new Error("no row link");
    await userEvent.click(link);
    expect(await screen.findByRole("dialog", { name: /nightly/ })).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/policies");
  });
});
