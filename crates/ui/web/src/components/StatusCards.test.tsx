import { screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { Health, RepositorySummary } from "../api/types";
import { renderWithRouter as mount } from "../test-utils";
import { StatusCards } from "./StatusCards";
import { HEALTH_ORDER, countByHealth } from "./health";

const repo = (name: string, health: Health): RepositorySummary => ({
  kind: "Repository",
  kindPath: "repository",
  name,
  namespace: "media",
  health,
  mode: "ReadWrite",
  serverBacked: false,
  suspended: false,
});

const fleet = [
  repo("nas", "healthy"),
  repo("offsite", "healthy"),
  repo("cold", "failed"),
  repo("lab", "suspended"),
];

describe("countByHealth", () => {
  it("counts every lamp, with zero for the ones nothing is in", () => {
    expect(countByHealth(fleet)).toEqual({
      failed: 1,
      degraded: 0,
      pending: 0,
      unknown: 0,
      suspended: 1,
      healthy: 2,
    });
  });

  it("files a health this bundle has never seen under unknown, never under healthy", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const counts = countByHealth([repo("x", "archived" as Health)]);
    expect(counts.unknown).toBe(1);
    expect(counts.healthy).toBe(0);
    warn.mockRestore();
  });

  it("orders the strip worst first so the eye lands on what is lit", () => {
    expect(HEALTH_ORDER).toEqual([
      "failed",
      "degraded",
      "pending",
      "unknown",
      "suspended",
      "healthy",
    ]);
  });
});

describe("StatusCards", () => {
  it("renders one lamp link per health with its count, carrying the namespace", async () => {
    mount(<StatusCards repositories={fleet} namespace="media" />);
    const nav = await screen.findByRole("navigation", { name: "Repositories by health" });
    const links = within(nav).getAllByRole("link");
    expect(links).toHaveLength(HEALTH_ORDER.length);
    const failed = within(nav).getByRole("link", { name: "1 failed repository" });
    expect(failed).toHaveAttribute("href", "/repositories?namespace=media&health=failed");
    expect(failed.querySelector(".health")).toHaveAttribute("data-health", "failed");
    expect(failed.querySelector("svg")).not.toBeNull();
    expect(within(nav).getByRole("link", { name: "2 healthy repositories" })).toHaveAttribute(
      "href",
      "/repositories?namespace=media&health=healthy",
    );
    // A zero is still a fact — the lamp stays, dimmed, so the vocabulary is stable.
    const degraded = within(nav).getByRole("link", { name: "0 degraded repositories" });
    expect(degraded.closest("li")).toHaveAttribute("data-empty", "true");
    expect(failed.closest("li")).not.toHaveAttribute("data-empty");
  });

  it("omits the namespace from the links when the scope is cluster-wide", async () => {
    mount(<StatusCards repositories={fleet} namespace={undefined} />);
    const nav = await screen.findByRole("navigation", { name: "Repositories by health" });
    expect(within(nav).getByRole("link", { name: "1 failed repository" })).toHaveAttribute(
      "href",
      "/repositories?health=failed",
    );
  });
});
