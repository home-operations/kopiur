import { describe, expect, it } from "vitest";

import type { NodeBox } from "./elk";
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

/** A plate well away from every route below, so it obstructs nothing by accident. */
const FAR: NodeBox = { x: 4000, y: 4000, width: 232, height: 64 };

describe("edgeLabelPoint", () => {
  it("is the midpoint of the longest segment, not of the whole run", () => {
    // Two short ends and one long middle: the long one carries the label.
    expect(
      edgeLabelPoint(
        [
          { x: 0, y: 10 },
          { x: 20, y: 10 },
          { x: 20, y: 400 },
          { x: 40, y: 400 },
        ],
        [FAR],
      ),
    ).toEqual({ x: 20, y: 205 });
  });

  it("is the midpoint of a straight line with no bends", () => {
    expect(
      edgeLabelPoint(
        [
          { x: 0, y: 0 },
          { x: 400, y: 40 },
        ],
        [FAR],
      ),
    ).toEqual({ x: 200, y: 20 });
  });

  it("moves off a segment that runs behind a plate onto one that is clear", () => {
    const plate: NodeBox = { x: 100, y: 0, width: 232, height: 64 };
    // The long first segment passes straight through the plate; the shorter
    // second one is clear of it.
    const at = edgeLabelPoint(
      [
        { x: 0, y: 32 },
        { x: 600, y: 32 },
        { x: 600, y: 600 },
      ],
      [plate],
    );
    expect(at).toEqual({ x: 600, y: 316 });
  });

  it("draws nothing rather than a label the reader cannot trust", () => {
    const plate: NodeBox = { x: 0, y: 0, width: 232, height: 64 };
    expect(
      edgeLabelPoint(
        [
          { x: 0, y: 32 },
          { x: 200, y: 32 },
        ],
        [plate],
      ),
    ).toBeNull();
  });

  it("has nowhere to sit when the edge has fewer than two points", () => {
    expect(edgeLabelPoint([], [FAR])).toBeNull();
    expect(edgeLabelPoint([{ x: 1, y: 2 }], [FAR])).toBeNull();
  });
});
