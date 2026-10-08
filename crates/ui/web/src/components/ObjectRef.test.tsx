import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { readStyles, cssRules } from "../testing/css";
import { renderWithRouter } from "../test-utils";
import { healthLamp } from "./health";
import { ObjectRef } from "./ObjectRef";

describe("ObjectRef", () => {
  it("is a link to the object when it has a detail route, carrying its kind", async () => {
    renderWithRouter(
      <ObjectRef
        kind="repository"
        name="dev-repo"
        namespace="kopiur-dev"
        to="/repositories/repository/dev-repo?namespace=kopiur-dev"
      />,
    );
    const link = await screen.findByRole("link", { name: /dev-repo/ });
    expect(link).toHaveAttribute("href", "/repositories/repository/dev-repo?namespace=kopiur-dev");
    expect(link).toHaveAttribute("data-kind", "repository");
    expect(link).toHaveTextContent("Repository");
  });

  it("is plain text, not a link, for a kind with no detail route", async () => {
    renderWithRouter(<ObjectRef kind="snapshotSchedule" name="nightly-cron" namespace="media" />);
    await screen.findByText("nightly-cron");
    expect(screen.queryByRole("link")).toBeNull();
  });

  it("names the namespace only when it differs from the surrounding one", async () => {
    const { unmount } = renderWithRouter(
      <ObjectRef
        kind="snapshotPolicy"
        name="app-data"
        namespace="kopiur-dev"
        contextNamespace="kopiur-dev"
      />,
    );
    await screen.findByText("app-data");
    expect(screen.queryByText("kopiur-dev/")).toBeNull();
    unmount();
    renderWithRouter(
      <ObjectRef
        kind="snapshotPolicy"
        name="app-data"
        namespace="kopiur-dev"
        contextNamespace="media"
      />,
    );
    expect(await screen.findByText("kopiur-dev/")).toBeInTheDocument();
  });

  it("carries the target's own status pill when it is known, and none when it is not", async () => {
    const { container, unmount } = renderWithRouter(
      <ObjectRef kind="repository" name="nas" health={healthLamp("failed")} />,
    );
    await screen.findByText("nas");
    const pill = container.querySelector(".health[data-health='failed']");
    expect(pill?.querySelector("svg")).not.toBeNull();
    unmount();
    const bare = renderWithRouter(<ObjectRef kind="repository" name="nas" />);
    await screen.findByText("nas");
    expect(bare.container.querySelector(".health")).toBeNull();
  });

  it("wraps a long name inside itself rather than widening the page", () => {
    const rule = cssRules(readStyles()).find((r) => r.selector === ".ref__name");
    expect(rule?.body).toMatch(/overflow-wrap:\s*anywhere/);
  });
});
