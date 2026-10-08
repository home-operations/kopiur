import type { GraphNode } from "../../api/types";
import type { InspectTarget } from "../inspect";
import {
  type TopologyEdge,
  type TopologyModel,
  type TopologyNode,
  nodeObjectKind,
  relationships,
} from "../topology/model";

/**
 * Where a resource sits on the topology graph — read by the resource drawer,
 * which is how the board's nodes are inspected.
 *
 * The graph names its nodes `Repository/<ns>/<name>`, `ClusterRepository/<name>`
 * and `Policy/<ns>/<name>`; only those three kinds are on it.
 */
export function graphId(target: InspectTarget): string | null {
  switch (target.kind) {
    case "repository":
      return target.namespace !== undefined
        ? `Repository/${target.namespace}/${target.name}`
        : null;
    case "clusterRepository":
      return `ClusterRepository/${target.name}`;
    case "snapshotPolicy":
      return target.namespace !== undefined ? `Policy/${target.namespace}/${target.name}` : null;
    case "snapshotSchedule":
    case "snapshot":
    case "restore":
    case "maintenance":
    case "repositoryReplication":
    case "snapshotReplication":
      return null;
  }
}

/**
 * A graph node id back into the resource it names — what an old `?node=`
 * link pointed at — or `null` for a backend, namespace or selector.
 */
export function targetFromGraphId(id: string): InspectTarget | null {
  const parts = id.split("/");
  const [kind, a, b] = parts;
  if (kind === "ClusterRepository" && parts.length === 2 && a) {
    return { kind: "clusterRepository", name: a };
  }
  if (parts.length === 3 && a && b) {
    if (kind === "Repository") return { kind: "repository", namespace: a, name: b };
    if (kind === "Policy") return { kind: "snapshotPolicy", namespace: a, name: b };
  }
  return null;
}

/** A graph node as the resource it stands for, or `null` for a backend, namespace or selector. */
export function nodeTarget(node: GraphNode): InspectTarget | null {
  const kind = nodeObjectKind(node.kind);
  if (kind === null) return null;
  return kind === "clusterRepository"
    ? { kind, name: node.name }
    : { kind, name: node.name, namespace: node.namespace ?? undefined };
}

/** What the graph says about one resource. */
export interface GraphContext {
  model: TopologyModel;
  node: TopologyNode;
  /** Edges ending here: what points at it. */
  inbound: TopologyEdge[];
  /** Edges starting here: what it points at. */
  outbound: TopologyEdge[];
  /** What it points at that does not exist — the ghosts it refers to. */
  missingRefs: TopologyNode[];
}

export function graphContext(
  model: TopologyModel | undefined,
  target: InspectTarget,
): GraphContext | null {
  const id = graphId(target);
  if (model === undefined || id === null) return null;
  const node = model.byId.get(id);
  if (node === undefined) return null;
  const { inbound, outbound } = relationships(model, id);
  const missingRefs = outbound
    .map((edge) => model.byId.get(edge.to))
    .filter((end): end is TopologyNode => end?.missing === true);
  return { model, node, inbound, outbound, missingRefs };
}
