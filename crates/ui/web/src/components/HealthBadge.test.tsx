import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { Health } from "../api/types";
import { HealthBadge } from "./HealthBadge";
import { healthLamp } from "./health";

const EVERY_HEALTH: readonly Health[] = [
  "healthy",
  "degraded",
  "failed",
  "suspended",
  "pending",
  "unknown",
];

describe("HealthBadge", () => {
  it.each(EVERY_HEALTH)("pairs the %s colour with an icon and a word", (health) => {
    const { container } = render(<HealthBadge health={health} />);
    const lamp = container.querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", health);
    expect(lamp?.querySelector("svg")).not.toBeNull();
    expect(lamp).toHaveTextContent(healthLamp(health).word);
  });

  it("uses a distinct icon for every lamp, so two lamps never differ by colour alone", () => {
    const icons = new Set(EVERY_HEALTH.map((health) => healthLamp(health).icon));
    expect(icons.size).toBe(EVERY_HEALTH.length);
  });

  it("renders a health this bundle has never seen as unknown, carrying the raw word", () => {
    // `Health` has no `{ unknown: { raw } }` object; a newer server may send a
    // plain string. It must not fall through to the healthy lamp.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { container } = render(<HealthBadge health={"archived" as Health} />);
    const lamp = container.querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "unknown");
    expect(lamp).toHaveTextContent("archived");
    expect(warn).toHaveBeenCalledOnce();
    warn.mockRestore();
  });

  it("keeps the icon and the spoken word when the visible word is replaced by a count", () => {
    // A bare "3" must still announce "Failed": with the icon aria-hidden,
    // colour would otherwise be the only carrier for a screen reader.
    const { container } = render(<HealthBadge health="failed" label="3" />);
    const lamp = container.querySelector(".health");
    expect(lamp).toHaveAttribute("data-health", "failed");
    expect(lamp?.querySelector("svg")).not.toBeNull();
    expect(lamp).toHaveTextContent("3 (Failed)");
    const hidden = lamp?.querySelector(".visually-hidden");
    expect(hidden).toHaveTextContent("Failed");
    // The visible span carries only the custom label.
    expect(lamp?.querySelector("span:not(.visually-hidden)")).toHaveTextContent(/^3$/);
  });

  it("adds no hidden word when the visible word is the lamp's own", () => {
    const { container } = render(<HealthBadge health="healthy" />);
    expect(container.querySelector(".visually-hidden")).toBeNull();
    expect(container.querySelector(".health")).toHaveTextContent(/^Healthy$/);
  });
});
