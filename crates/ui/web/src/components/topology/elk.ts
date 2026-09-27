/**
 * The conversion between the topology model and the ELK graph the layout
 * worker runs — pure in both directions, so the layout contract (which node
 * is pinned where, what comes back) is a unit test rather than something
 * only a browser can show.
 *
 * Only the types are imported from elkjs here; the engine itself is loaded
 * by `layout.worker.ts` and nowhere else, which is what keeps it out of the
 * entry chunk and off the main thread.
 */

import type { ElkExtendedEdge, ElkNode, ElkPoint } from "elkjs/lib/elk-api";

import type { TopologyModel } from "./model";

/** Every plate is one size: the layout is about relationships, not about who has the longest name. */
export const NODE_WIDTH = 232;
export const NODE_HEIGHT = 64;

/** Where a laid-out node sits, in canvas pixels. */
export interface NodeBox {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** The polyline an edge follows: start, every bend, end. */
export interface EdgePath {
  points: ElkPoint[];
}

export interface LaidOut {
  width: number;
  height: number;
  nodes: Map<string, NodeBox>;
  edges: Map<string, EdgePath>;
}

/**
 * The board reads left to right — sources, the repositories, their copies —
 * so policies are pinned to the first column and bare backends to the last;
 * everything else falls where its edges put it. Orthogonal routing draws the
 * lines as a chassis does, with bends, not curves. Model order is honoured so
 * the same graph lays out the same way on every visit.
 */
export function toElkGraph(model: TopologyModel): ElkNode {
  const children: ElkNode[] = model.nodes.map((node) => {
    const child: ElkNode = { id: node.id, width: NODE_WIDTH, height: NODE_HEIGHT };
    if (node.node.kind === "policy") {
      child.layoutOptions = { "elk.layered.layering.layerConstraint": "FIRST" };
    } else if (node.node.kind === "backend") {
      child.layoutOptions = { "elk.layered.layering.layerConstraint": "LAST" };
    }
    return child;
  });
  const edges: ElkExtendedEdge[] = model.edges
    .filter((edge) => !edge.dangling)
    .map((edge) => ({ id: edge.id, sources: [edge.from], targets: [edge.to] }));
  return {
    id: "topology",
    layoutOptions: {
      "elk.algorithm": "layered",
      "elk.direction": "RIGHT",
      "elk.edgeRouting": "ORTHOGONAL",
      "elk.layered.considerModelOrder.strategy": "NODES_AND_EDGES",
      "elk.layered.spacing.nodeNodeBetweenLayers": "96",
      "elk.layered.spacing.edgeNodeBetweenLayers": "32",
      "elk.layered.spacing.edgeEdgeBetweenLayers": "16",
      "elk.spacing.nodeNode": "32",
      "elk.spacing.edgeNode": "24",
      "elk.spacing.edgeEdge": "16",
      "elk.spacing.componentComponent": "48",
      "elk.padding": "[top=24,left=24,bottom=24,right=24]",
    },
    children,
    edges,
  };
}

/**
 * Read the engine's answer back. Refuses, by throwing, a result that lost a
 * node's position or the root's size: the board would rather show a failure
 * than a blank panel with plates stacked at the origin.
 */
export function fromElkLayout(root: ElkNode): LaidOut {
  if (root.width === undefined || root.height === undefined) {
    throw new Error("layout returned no size for root");
  }
  const nodes = new Map<string, NodeBox>();
  for (const child of root.children ?? []) {
    if (
      child.x === undefined ||
      child.y === undefined ||
      child.width === undefined ||
      child.height === undefined
    ) {
      throw new Error(`layout returned no position for ${child.id}`);
    }
    nodes.set(child.id, { x: child.x, y: child.y, width: child.width, height: child.height });
  }
  const edges = new Map<string, EdgePath>();
  for (const edge of root.edges ?? []) {
    const sections = edge.sections ?? [];
    if (sections.length > 0) {
      const points: ElkPoint[] = [];
      for (const section of sections) {
        points.push(section.startPoint, ...(section.bendPoints ?? []), section.endPoint);
      }
      edges.set(edge.id, { points });
      continue;
    }
    // No routing came back: a straight line between the ends, so the
    // relationship is still drawn.
    const from = nodes.get(edge.sources[0] ?? "");
    const to = nodes.get(edge.targets[0] ?? "");
    if (from !== undefined && to !== undefined) {
      edges.set(edge.id, {
        points: [
          { x: from.x + from.width, y: from.y + from.height / 2 },
          { x: to.x, y: to.y + to.height / 2 },
        ],
      });
    }
  }
  return { width: root.width, height: root.height, nodes, edges };
}
