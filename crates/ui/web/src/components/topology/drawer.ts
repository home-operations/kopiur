/**
 * What the drawer says about one node: where its object is listed, and what
 * the node stands for.
 *
 * Both are exhaustive over `NodeKind`, which has no fallback variant
 * (`crates/ui-model/src/lib.rs`): every literal is named so a new kind fails
 * to compile, and the `default` arm still answers at run time rather than
 * rendering a node the server sent as a blank.
 *
 * There is deliberately no kind-to-URL-segment table here. A repository's
 * URL segment is the server's `kindPath` field on `RepositorySummary` /
 * `RepositoryDetail` (addenda item 16), and the graph does not carry it — so
 * a node links to the *section* that lists its object, never to a detail
 * path this bundle guessed from the display kind. When the detail routes land
 * (Task 5), this is the one place that changes, and the segment must come
 * from `kindPath`.
 */

import type { NodeKind } from "../../api/types";
import type { NavPath } from "../nav";

/** The route that lists a node's object, and the section's own name. */
export interface NodeSection {
  to: NavPath;
  section: string;
}

/** Where this node's object is listed, or `null` when it has no object. */
export function nodeSection(kind: NodeKind): NodeSection | null {
  switch (kind) {
    case "repository":
    case "clusterRepository":
      return { to: "/repositories", section: "repositories" };
    case "policy":
      return { to: "/policies", section: "policies" };
    case "backend":
    case "namespace":
    case "namespaceSelector":
      return null;
    default:
      return null;
  }
}

/**
 * One clause saying what the node is, for an operator who has not read the
 * CRD reference. Synthetic nodes get the most words: a backend and a
 * namespace selector are the two things on this board that are not objects,
 * and an operator hunting for them in `kubectl get` needs to know that.
 */
export function nodeMeaning(kind: NodeKind): string {
  switch (kind) {
    case "repository":
      return "a namespaced Repository — the kopia repository snapshots in its namespace are written to";
    case "clusterRepository":
      return "a cluster-scoped ClusterRepository, shared by the namespaces it admits";
    case "backend":
      return "a bare storage backend a repository replication writes blobs to; it is not an object in the cluster and has no health of its own";
    case "policy":
      return "a SnapshotPolicy — the recipe naming what to back up and which repositories to write it to";
    case "namespace":
      return "a namespace this cluster repository admits";
    case "namespaceSelector":
      return "a label selector standing for the namespaces it matches; it is not an object in the cluster";
    default:
      return "an element of the topology this console does not recognise";
  }
}
