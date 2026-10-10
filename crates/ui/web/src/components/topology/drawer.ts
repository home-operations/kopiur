/**
 * What a topology node stands for, in one clause — said under the verdict of
 * the resource drawer for the kinds on the board.
 *
 * Exhaustive over `NodeKind`, which has no fallback variant
 * (`crates/ui-model/src/lib.rs`): every literal is named so a new kind fails
 * to compile, and the `default` arm still answers at run time rather than
 * rendering a node the server sent as a blank.
 */

import type { NodeKind } from "../../api/types";

/**
 * One clause saying what the node is, for an operator who has not read the
 * CRD reference. Synthetic nodes get the most words: a backend and a
 * namespace selector are the two things on this board that are not objects,
 * and an operator hunting for them in `kubectl get` needs to know that.
 */
export function nodeMeaning(kind: NodeKind): string {
  switch (kind) {
    case "repository":
      return "a namespaced Repository — where snapshots in its namespace are written";
    case "clusterRepository":
      return "a cluster-scoped ClusterRepository, shared by the namespaces it admits";
    case "backend":
      return "a storage backend a repository replication writes to; it is not an object in the cluster";
    case "policy":
      return "a SnapshotPolicy — what to back up and which repositories to write it to";
    case "namespace":
      return "a namespace this cluster repository admits";
    case "namespaceSelector":
      return "a label selector standing for the namespaces it matches; it is not an object in the cluster";
    default:
      return "an element of the topology this console does not recognise";
  }
}
