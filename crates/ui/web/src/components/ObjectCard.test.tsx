import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { renderWithRouter } from "../test-utils";
import { ObjectCard } from "./ObjectCard";

const row = {
  namespace: "kopiur-dev",
  name: "app-data",
  repositories: ["Repository/kopiur-dev/dev-repo"],
  multiRepo: false,
  suspended: false,
};

describe("ObjectCard", () => {
  it("is a striped card with the kind, the name as its link, and the status pill", async () => {
    const { container } = renderWithRouter(
      <ObjectCard card={{ kind: "snapshotPolicy", row }} variant="stats" />,
    );
    const link = await screen.findByRole("link", { name: "app-data" });
    expect(link.getAttribute("href")).toMatch(/inspect=snapshot-policy%2Fkopiur-dev%2Fapp-data$/);
    const card = container.querySelector("article.object-card");
    expect(card).toHaveAttribute("data-kind", "snapshot-policy");
    expect(card).toHaveClass("has-stripe");
    expect(card?.querySelector(".kind-name")).toHaveTextContent("SnapshotPolicy");
    expect(card?.querySelector(".health[data-health] svg")).not.toBeNull();
    expect(card?.querySelectorAll("dl .stats__item")).toHaveLength(3);
  });

  it("drops the stat strip in the summary variant", async () => {
    const { container } = renderWithRouter(<ObjectCard card={{ kind: "snapshotPolicy", row }} />);
    await screen.findByRole("link", { name: "app-data" });
    expect(container.querySelector("dl")).toBeNull();
  });
});
