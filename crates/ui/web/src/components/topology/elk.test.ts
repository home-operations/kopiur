import { describe, expect, it } from "vitest";

import { BLOBSYNC_BACKEND, FIXTURE_GRAPH, NAS, NIGHTLY, SHARED } from "./fixture";
import { NODE_HEIGHT, NODE_WIDTH, fromElkLayout, toElkGraph } from "./elk";
import { topologyModel } from "./model";

describe("toElkGraph", () => {
  const graph = toElkGraph(topologyModel(FIXTURE_GRAPH));

  it("hands every node to the engine at one fixed plate size, and every edge by id", () => {
    expect(graph.children?.map((c) => c.id).sort()).toEqual(
      FIXTURE_GRAPH.nodes.map((n) => n.id).sort(),
    );
    for (const child of graph.children ?? []) {
      expect(child.width).toBe(NODE_WIDTH);
      expect(child.height).toBe(NODE_HEIGHT);
    }
    expect(graph.edges?.map((e) => e.id).sort()).toEqual(
      FIXTURE_GRAPH.edges.map((e) => e.id).sort(),
    );
    const repl = graph.edges?.find((e) => e.id === "SnapshotReplication/media/offsite");
    expect(repl?.sources).toEqual([NAS.id]);
    expect(repl?.targets).toEqual([SHARED.id]);
  });

  it("lays out left to right: policies pinned to the first column, bare backends to the last", () => {
    expect(graph.layoutOptions?.["elk.algorithm"]).toBe("layered");
    expect(graph.layoutOptions?.["elk.direction"]).toBe("RIGHT");
    const policy = graph.children?.find((c) => c.id === NIGHTLY.id);
    const backend = graph.children?.find((c) => c.id === BLOBSYNC_BACKEND.id);
    const repo = graph.children?.find((c) => c.id === NAS.id);
    expect(policy?.layoutOptions?.["elk.layered.layering.layerConstraint"]).toBe("FIRST");
    expect(backend?.layoutOptions?.["elk.layered.layering.layerConstraint"]).toBe("LAST");
    expect(repo?.layoutOptions?.["elk.layered.layering.layerConstraint"]).toBeUndefined();
  });
});

describe("fromElkLayout", () => {
  it("reads node positions and edge polylines (start, bends, end) back out", () => {
    const laid = fromElkLayout({
      id: "root",
      width: 640,
      height: 200,
      children: [
        { id: "a", x: 10, y: 20, width: NODE_WIDTH, height: NODE_HEIGHT },
        { id: "b", x: 400, y: 20, width: NODE_WIDTH, height: NODE_HEIGHT },
      ],
      edges: [
        {
          id: "a-b",
          sources: ["a"],
          targets: ["b"],
          sections: [
            {
              id: "s0",
              startPoint: { x: 242, y: 52 },
              bendPoints: [{ x: 320, y: 52 }],
              endPoint: { x: 400, y: 52 },
            },
          ],
        },
      ],
    });
    expect(laid.width).toBe(640);
    expect(laid.height).toBe(200);
    expect(laid.nodes.get("a")).toEqual({ x: 10, y: 20, width: NODE_WIDTH, height: NODE_HEIGHT });
    expect(laid.edges.get("a-b")).toEqual({
      points: [
        { x: 242, y: 52 },
        { x: 320, y: 52 },
        { x: 400, y: 52 },
      ],
    });
  });

  it("refuses a layout that lost a node or returned no size, rather than drawing a blank", () => {
    expect(() =>
      fromElkLayout({
        id: "root",
        children: [{ id: "a", width: NODE_WIDTH, height: NODE_HEIGHT }],
      }),
    ).toThrow(/root/);
    expect(() =>
      fromElkLayout({
        id: "root",
        width: 10,
        height: 10,
        children: [{ id: "a", x: 0, width: NODE_WIDTH, height: NODE_HEIGHT }],
      }),
    ).toThrow(/a/);
  });

  it("straightens an edge the engine routed without sections into a line between its ends", () => {
    const laid = fromElkLayout({
      id: "root",
      width: 640,
      height: 200,
      children: [
        { id: "a", x: 0, y: 0, width: NODE_WIDTH, height: NODE_HEIGHT },
        { id: "b", x: 400, y: 100, width: NODE_WIDTH, height: NODE_HEIGHT },
      ],
      edges: [{ id: "a-b", sources: ["a"], targets: ["b"] }],
    });
    expect(laid.edges.get("a-b")).toEqual({
      points: [
        { x: NODE_WIDTH, y: NODE_HEIGHT / 2 },
        { x: 400, y: 100 + NODE_HEIGHT / 2 },
      ],
    });
  });
});
