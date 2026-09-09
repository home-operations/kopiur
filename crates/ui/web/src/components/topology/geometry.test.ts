import { describe, expect, it } from "vitest";

import { edgeLabelPoint, polylinePath } from "./geometry";

describe("polylinePath", () => {
  it("moves to the first point and lines to every bend", () => {
    expect(
      polylinePath([
        { x: 0, y: 10 },
        { x: 50, y: 10 },
        { x: 50, y: 90 },
        { x: 120, y: 90 },
      ]),
    ).toBe("M 0 10 L 50 10 L 50 90 L 120 90");
  });

  it("is empty for no points, so a routed-nowhere edge draws nothing rather than a stray dot", () => {
    expect(polylinePath([])).toBe("");
  });
});

describe("edgeLabelPoint", () => {
  it("is the midpoint of the middle segment, not of the whole run", () => {
    // Three segments: the middle one runs from (50,10) to (50,90).
    expect(
      edgeLabelPoint([
        { x: 0, y: 10 },
        { x: 50, y: 10 },
        { x: 50, y: 90 },
        { x: 120, y: 90 },
      ]),
    ).toEqual({ x: 50, y: 50 });
  });

  it("is the midpoint of a straight line with no bends", () => {
    expect(
      edgeLabelPoint([
        { x: 0, y: 0 },
        { x: 100, y: 40 },
      ]),
    ).toEqual({ x: 50, y: 20 });
  });

  it("has nowhere to sit when the edge has fewer than two points", () => {
    expect(edgeLabelPoint([])).toBeNull();
    expect(edgeLabelPoint([{ x: 1, y: 2 }])).toBeNull();
  });
});
