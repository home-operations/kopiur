import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { EdgeLegend } from "./EdgeLegend";
import { EDGE_LEGEND, edgeStyle } from "./model";

describe("EdgeLegend", () => {
  it("lists every edge kind the board can draw, copies first", () => {
    render(<EdgeLegend />);
    const items = screen.getAllByRole("listitem");
    expect(items.map((item) => item.dataset.edge)).toEqual([...EDGE_LEGEND]);
    for (const kind of EDGE_LEGEND) {
      const style = edgeStyle(kind);
      expect(screen.getByText(style.word)).toBeInTheDocument();
      expect(screen.getByText(style.meaning)).toBeInTheDocument();
    }
  });

  it("draws each sample with the stroke the board uses, so the two cannot drift", () => {
    render(<EdgeLegend />);
    for (const item of screen.getAllByRole("listitem")) {
      const kind = EDGE_LEGEND.find((candidate) => candidate === item.dataset.edge);
      expect(kind).toBeDefined();
      if (kind === undefined) {
        continue;
      }
      const style = edgeStyle(kind);
      const line = item.querySelector("line");
      expect(line?.getAttribute("stroke-width")).toBe(String(style.width));
      expect(line?.getAttribute("stroke-dasharray")).toBe(style.dash ?? null);
      // The end marker is the kind's own: an arrow for a copy or a write, a
      // square for an admission.
      expect(item.querySelector("path") !== null).toBe(style.marker === "arrow");
      expect(item.querySelector("rect") !== null).toBe(style.marker === "square");
    }
  });
});
