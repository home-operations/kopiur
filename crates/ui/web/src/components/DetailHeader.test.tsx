import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { nth, renderWithRouter } from "../test-utils";
import { DetailHeader } from "./DetailHeader";
import { healthLamp } from "./health";

describe("DetailHeader", () => {
  it("leads with the trail, then the striped hero: kind, name, pill, verdict, actions, stats", async () => {
    const { container } = renderWithRouter(
      <DetailHeader
        kind="snapshot"
        name="app-data-manual"
        namespace="kopiur-dev"
        lamp={healthLamp("healthy")}
        verdictLabel="Snapshot verdict"
        verdict="Succeeded: a manual run under app-data."
        trail={[
          {
            kind: "repository",
            name: "dev-repo",
            namespace: "kopiur-dev",
            to: "/repositories/repository/dev-repo?namespace=kopiur-dev",
          },
          {
            kind: "snapshotPolicy",
            name: "app-data",
            namespace: "kopiur-dev",
            to: "/policies/kopiur-dev/app-data",
          },
        ]}
        actions={<button type="button">Snapshot now</button>}
        stats={[
          { label: "Size", value: "2.0 MiB" },
          { label: "Files", value: "10" },
          { label: "Took", value: "4s" },
          { label: "Started", value: "58m ago" },
        ]}
      />,
    );

    // The shell owns the page's one h1; the object's name is an h2.
    expect(container.querySelector("h1")).toBeNull();
    expect(
      await screen.findByRole("heading", { level: 2, name: /app-data-manual/ }),
    ).toBeInTheDocument();

    const trail = screen.getByRole("list", { name: "Where this sits" });
    const hops = within(trail).getAllByRole("listitem");
    expect(within(nth(hops, 0)).getByRole("link")).toHaveAttribute(
      "href",
      "/repositories/repository/dev-repo?namespace=kopiur-dev",
    );
    expect(hops.at(-1)).toHaveTextContent("this snapshot");

    const hero = container.querySelector(".detail-hero");
    expect(hero).toHaveAttribute("data-kind", "snapshot");
    expect(hero).toHaveClass("has-stripe");

    const verdict = screen.getByRole("status", { name: "Snapshot verdict" });
    expect(verdict.querySelector(".verdict__lamp svg")).not.toBeNull();
    expect(verdict).toHaveTextContent("Succeeded: a manual run under app-data.");

    expect(screen.getByRole("region", { name: "Actions" })).toContainElement(
      screen.getByRole("button", { name: "Snapshot now" }),
    );
    expect(container.querySelectorAll("dl.stats .stats__item")).toHaveLength(4);
  });

  it("omits the trail and the stats when the object has neither", async () => {
    renderWithRouter(
      <DetailHeader
        kind="restore"
        name="r1"
        namespace="media"
        lamp={healthLamp("pending")}
        verdictLabel="Restore verdict"
        verdict="Pending."
      />,
    );
    await screen.findByRole("status", { name: "Restore verdict" });
    expect(screen.queryByRole("list", { name: "Where this sits" })).toBeNull();
    expect(document.querySelector("dl.stats")).toBeNull();
  });
});
