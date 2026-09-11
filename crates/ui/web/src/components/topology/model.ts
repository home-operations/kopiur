/**
 * The topology's view model: the server's `RepositoryGraph` with each node
 * and edge given its lamp, its line style and the facts the board draws.
 *
 * Everything here is pure and about the graph model, never about pixels:
 * layout is `elk.ts` (the conversion) and `layout.ts` (the worker). The
 * route tests therefore assert node and edge sets, the missing treatment and
 * the verdict on this module's output, with the layout mocked.
 *
 * Two of the three generated unions this module switches on have no
 * fallback variant at all (`NodeKind`, `EdgeKind`; `crates/ui-model/src/lib.rs`),
 * and the third — `Health` — falls back to the plain string `"unknown"`, not
 * an object. Every switch names every literal (the lint rule
 * `switch-exhaustiveness-check` refuses a missing one) and its `default`
 * arm renders the raw word through `unknownVariant`, so a newer server's
 * value is drawn, labelled, and never mistaken for a healthy one.
 */

import { CircleDashed } from "lucide-react";

import type { EdgeKind, GraphEdge, GraphNode, NodeKind, RepositoryGraph } from "../../api/types";
import { unknownVariant } from "../../util/assertNever";
import { type HealthKey, type Lamp, healthLamp } from "../health";

/** The `data-edge` keys the stylesheet draws; `unknown` is a kind this bundle has not seen. */
export type EdgeStyleKey = EdgeKind | "unknown";

/** How a line of one kind is drawn — the legend renders exactly these fields. */
export interface EdgeStyle {
  key: EdgeStyleKey;
  /** The relationship's name, as the CRD spells it. */
  word: string;
  /** One clause saying what the line means, for the legend and the drawer. */
  meaning: string;
  /** SVG `stroke-dasharray`, or a solid line. */
  dash: string | undefined;
  /** Stroke width in CSS pixels. */
  width: number;
  /** The end marker: an arrow for a copy or a write, a square for an admission. */
  marker: "arrow" | "square" | "none";
}

/** A node with its lamp and the facts the plate shows. */
export interface TopologyNode {
  id: string;
  node: GraphNode;
  lamp: Lamp;
  /** The kind, as a word: "Cluster repository", not `clusterRepository`. */
  kindWord: string;
  /** The server's dangling-reference flag, here so a renderer never reads `node.missing` and forgets the lamp. */
  missing: boolean;
  /** A repository (or cluster repository) that exists and that no replication copies anywhere. */
  replicatesNowhere: boolean;
}

/** An edge with its lamp and line style. */
export interface TopologyEdge {
  id: string;
  edge: GraphEdge;
  from: string;
  to: string;
  lamp: Lamp;
  style: EdgeStyle;
  /** True when an endpoint is not in the node set — the server mints ghosts, so this is a contract violation, drawn anyway. */
  dangling: boolean;
}

export interface TopologyModel {
  nodes: TopologyNode[];
  edges: TopologyEdge[];
  byId: Map<string, TopologyNode>;
  generatedAt: string;
}

/** The edge kinds, in the order the legend lists them: copies first, then what feeds and who may. */
export const EDGE_LEGEND: readonly EdgeKind[] = [
  "snapshotReplication",
  "repositoryReplication",
  "seed",
  "policyMembership",
  "allowedNamespace",
];

/** Whether a kind is a repository of either scope. Exhaustive. */
export function isRepositoryKind(kind: NodeKind): boolean {
  switch (kind) {
    case "repository":
    case "clusterRepository":
      return true;
    case "backend":
    case "policy":
    case "namespace":
    case "namespaceSelector":
      return false;
    default:
      return false;
  }
}

/** Whether an edge kind copies data out of a repository. Exhaustive. */
export function isReplicationKind(kind: EdgeKind): boolean {
  switch (kind) {
    case "snapshotReplication":
    case "repositoryReplication":
      return true;
    case "seed":
    case "policyMembership":
    case "allowedNamespace":
      return false;
    default:
      return false;
  }
}

/** The kind as a word. `NodeKind` has no fallback variant; an unseen one renders as itself. */
export function nodeKindWord(kind: NodeKind): string {
  switch (kind) {
    case "repository":
      return "Repository";
    case "clusterRepository":
      return "Cluster repository";
    case "backend":
      return "Backend";
    case "policy":
      return "Policy";
    case "namespace":
      return "Namespace";
    case "namespaceSelector":
      return "Namespace selector";
    default:
      return unknownVariant(kind, "NodeKind");
  }
}

/**
 * The lamp for a node. A missing node is the failed lamp wearing the words
 * that matter — "referenced but not found" — and a dashed icon, so a dangling
 * reference cannot be read as an ordinary failure, let alone as healthy. Any
 * other node lamps its server-reported health; a health this bundle has never
 * seen is the unknown lamp carrying the raw word (`healthLamp`).
 */
export function nodeLamp(node: GraphNode): Lamp {
  if (node.missing) {
    return { key: "failed", word: "referenced but not found", icon: CircleDashed };
  }
  return healthLamp(node.health);
}

/**
 * The line style per edge kind — five distinguishable strokes, so the legend
 * and the board agree on what a dash means. `EdgeKind` has no fallback
 * variant; an unseen kind draws a faint dotted line under its raw word.
 */
export function edgeStyle(kind: EdgeKind): EdgeStyle {
  switch (kind) {
    case "snapshotReplication":
      return {
        key: kind,
        word: "Snapshot replication",
        meaning: "copies selected snapshots into another repository",
        dash: undefined,
        width: 2,
        marker: "arrow",
      };
    case "repositoryReplication":
      return {
        key: kind,
        word: "Repository replication",
        meaning: "copies every blob to a bare backend",
        dash: "10 5",
        width: 2,
        marker: "arrow",
      };
    case "seed":
      return {
        key: kind,
        word: "Seed",
        meaning: "the destination was bootstrapped from the source",
        dash: "2 5",
        width: 1.5,
        marker: "arrow",
      };
    case "policyMembership":
      return {
        key: kind,
        word: "Policy membership",
        meaning: "the policy writes its snapshots here",
        dash: undefined,
        width: 1,
        marker: "arrow",
      };
    case "allowedNamespace":
      return {
        key: kind,
        word: "Allowed namespace",
        meaning: "the cluster repository admits this namespace",
        dash: "3 3",
        width: 1,
        marker: "square",
      };
    default:
      return {
        key: "unknown",
        word: unknownVariant(kind, "EdgeKind"),
        meaning: "a relationship this console does not know",
        dash: "1 3",
        width: 1,
        marker: "none",
      };
  }
}

/** Assemble the model. Nodes keep the server's order (its ids sort); edges likewise. */
export function topologyModel(graph: RepositoryGraph): TopologyModel {
  const byId = new Map<string, TopologyNode>();
  const replicating = new Set<string>();
  for (const edge of graph.edges) {
    if (isReplicationKind(edge.kind)) {
      replicating.add(edge.from);
    }
  }
  const nodes: TopologyNode[] = graph.nodes.map((node) => {
    const entry: TopologyNode = {
      id: node.id,
      node,
      lamp: nodeLamp(node),
      kindWord: nodeKindWord(node.kind),
      missing: node.missing,
      replicatesNowhere: !node.missing && isRepositoryKind(node.kind) && !replicating.has(node.id),
    };
    byId.set(node.id, entry);
    return entry;
  });
  const edges: TopologyEdge[] = graph.edges.map((edge) => ({
    id: edge.id,
    edge,
    from: edge.from,
    to: edge.to,
    lamp: healthLamp(edge.health),
    style: edgeStyle(edge.kind),
    dangling: !byId.has(edge.from) || !byId.has(edge.to),
  }));
  return { nodes, edges, byId, generatedAt: graph.generatedAt };
}

/** A node's edges, split by direction. */
export function relationships(
  model: TopologyModel,
  id: string,
): { inbound: TopologyEdge[]; outbound: TopologyEdge[] } {
  return {
    inbound: model.edges.filter((e) => e.to === id),
    outbound: model.edges.filter((e) => e.from === id),
  };
}

/** The counts the section head states beside the verdict. */
export interface TopologyFacts {
  repositories: number;
  policies: number;
  replications: number;
  replicateNowhere: number;
  missing: number;
}

export interface TopologyVerdict {
  health: HealthKey;
  /** One sentence: the worst thing first. */
  text: string;
  facts: TopologyFacts;
}

const plural = (n: number, one: string, many: string): string => `${n} ${n === 1 ? one : many}`;

/**
 * The one sentence over the board, worst first: a dangling reference, then a
 * failed repository, then a failing replication, then degraded, pending,
 * unread, suspended — and only then healthy. An empty graph is unknown, not
 * healthy: nothing was observed. A bare backend's health is never observed
 * by the server (it is always `unknown` by construction), so it does not
 * count towards "cannot read".
 */
export function topologyVerdict(model: TopologyModel): TopologyVerdict {
  const facts: TopologyFacts = {
    repositories: model.nodes.filter((n) => !n.missing && isRepositoryKind(n.node.kind)).length,
    policies: model.nodes.filter((n) => n.node.kind === "policy").length,
    replications: model.edges.filter((e) => isReplicationKind(e.edge.kind)).length,
    replicateNowhere: model.nodes.filter((n) => n.replicatesNowhere).length,
    missing: model.nodes.filter((n) => n.missing).length,
  };
  const present = model.nodes.filter((n) => !n.missing && n.node.kind !== "backend");
  const replications = model.edges.filter((e) => isReplicationKind(e.edge.kind));
  const nodesAt = (key: HealthKey) => present.filter((n) => n.lamp.key === key).length;
  const replicationsAt = (key: HealthKey) => replications.filter((e) => e.lamp.key === key).length;
  const otherEdgesAt = (key: HealthKey) =>
    model.edges.filter((e) => !isReplicationKind(e.edge.kind) && e.lamp.key === key).length;

  const verdict = (health: HealthKey, text: string): TopologyVerdict => ({ health, text, facts });

  if (model.nodes.length === 0) {
    return verdict("unknown", "Nothing to draw");
  }
  if (facts.missing > 0) {
    return verdict(
      "failed",
      `${plural(facts.missing, "reference points", "references point")} at a repository that does not exist`,
    );
  }
  const failedNodes = nodesAt("failed");
  if (failedNodes > 0) {
    return verdict("failed", `${plural(failedNodes, "object has", "objects have")} failed`);
  }
  const failedReplications = replicationsAt("failed");
  if (failedReplications > 0) {
    return verdict(
      "failed",
      `${plural(failedReplications, "replication is", "replications are")} failing`,
    );
  }
  const degraded = nodesAt("degraded");
  if (degraded > 0) {
    return verdict("degraded", `${plural(degraded, "object is", "objects are")} degraded`);
  }
  const pending = nodesAt("pending") + replicationsAt("pending") + otherEdgesAt("pending");
  if (pending > 0) {
    return verdict("pending", `${plural(pending, "relationship is", "relationships are")} pending`);
  }
  const unread = nodesAt("unknown") + replicationsAt("unknown") + otherEdgesAt("unknown");
  if (unread > 0) {
    return verdict(
      "unknown",
      `${plural(unread, "object reports", "objects report")} a state this console cannot read`,
    );
  }
  const suspended = nodesAt("suspended") + replicationsAt("suspended");
  if (suspended > 0) {
    return verdict("suspended", `${plural(suspended, "object is", "objects are")} suspended`);
  }
  return verdict("healthy", "Every repository, policy and replication reports healthy");
}
