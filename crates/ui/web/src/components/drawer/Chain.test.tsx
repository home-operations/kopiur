import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { nth, renderWithRouter } from "../../test-utils";
import { healthLamp } from "../health";
import { Chain } from "./Chain";

describe("Chain", () => {
  it("names what feeds this resource, then this one, then what it feeds, in order", async () => {
    renderWithRouter(
      <Chain
        kind="snapshotPolicy"
        namespace="media"
        before={[
          {
            label: "Fired by",
            items: [
              { ref: { kind: "snapshotSchedule", namespace: "media", name: "nightly-cron" } },
            ],
          },
        ]}
        after={[
          {
            label: "Writes into",
            items: [
              {
                ref: { kind: "repository", namespace: "media", name: "nas" },
                health: healthLamp("failed"),
              },
              { text: "Vault/elsewhere" },
            ],
          },
        ]}
      />,
    );
    const chain = await screen.findByRole("list", { name: "Where this sits" });
    const steps = within(chain).getAllByRole("listitem", { name: /./ });
    expect(steps.map((s) => s.getAttribute("aria-label"))).toEqual([
      "Fired by",
      "This policy",
      "Writes into",
    ]);
    expect(within(nth(steps, 0)).getByRole("link", { name: /nightly-cron/ })).toBeInTheDocument();
    expect(within(nth(steps, 1)).getByText("this policy")).toBeInTheDocument();
    const into = nth(steps, 2);
    expect(within(into).getByRole("link", { name: /nas/ })).toBeInTheDocument();
    expect(into.querySelector(".health[data-health='failed']")).not.toBeNull();
    // A key it cannot read stays the server's own text, never a guessed link.
    expect(within(into).getByText("Vault/elsewhere")).toBeInTheDocument();
    expect(chain.querySelectorAll(".chain__arrow[aria-hidden='true']")).toHaveLength(2);
  });

  it("leaves out a step with nothing in it", async () => {
    renderWithRouter(
      <Chain kind="snapshot" before={[{ label: "Fired by", items: [] }]} after={[]} />,
    );
    const chain = await screen.findByRole("list", { name: "Where this sits" });
    expect(within(chain).queryByText("Fired by")).toBeNull();
    expect(chain.querySelectorAll(".chain__arrow")).toHaveLength(0);
  });
});
