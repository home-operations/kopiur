/**
 * The six kinds `POST /actions/suspend` accepts, and what each one costs.
 *
 * The table mirrors `crates/ui/src/actions/mod.rs::SUSPENDABLE_KINDS` (the
 * kebab-case token the handler parses) and `kopiur_ops::suspend::patch_for`
 * (the spec path the patch actually writes). Both halves are here for the
 * same reason the repository helpers keep `kindPath` in one place: the token
 * in the body, the `/me` flag the control is judged against, and the sentence
 * the confirmation shows must come from one row, or two of the three will
 * eventually disagree.
 *
 * The path is not a detail. A `SnapshotSchedule` suspends at
 * `spec.schedule.suspend` and every other kind at `spec.suspend`, so a
 * confirmation that named `spec.suspend` for a schedule would send the reader
 * to a field that does not exist on the object they are about to change.
 */

import type { Capabilities } from "../../api/types";

/** What one suspendable kind needs from the UI's side. */
export interface SuspendableMeta {
  /** The CRD kind, for the confirmation's prose. */
  crd: string;
  /** The `/me` flag that decides whether the flip is offered at all. */
  capability: keyof Capabilities;
  /** The spec path the merge patch writes — quoted in the confirmation. */
  path: string;
  /**
   * False for a cluster-scoped kind. Such a kind's body carries no namespace
   * (the handler drops one sent anyway) and its access review is
   * cluster-scoped whatever namespace is asked, so the control must be judged
   * with none (addenda item 17).
   */
  namespaced: boolean;
}

/**
 * Every kind the suspend endpoint accepts, keyed by the token it is sent as.
 *
 * `satisfies` rather than an annotation, so the keys stay literal and
 * `SuspendableKind` is a closed union a caller must name a member of.
 */
export const SUSPENDABLE = {
  policy: {
    crd: "SnapshotPolicy",
    capability: "patchPolicies",
    path: "spec.suspend",
    namespaced: true,
  },
  schedule: {
    crd: "SnapshotSchedule",
    capability: "patchSchedules",
    path: "spec.schedule.suspend",
    namespaced: true,
  },
  repository: {
    crd: "Repository",
    capability: "patchRepositories",
    path: "spec.suspend",
    namespaced: true,
  },
  "cluster-repository": {
    crd: "ClusterRepository",
    capability: "patchClusterRepositories",
    path: "spec.suspend",
    namespaced: false,
  },
  replication: {
    crd: "RepositoryReplication",
    capability: "patchRepositoryReplications",
    path: "spec.suspend",
    namespaced: true,
  },
  "snapshot-replication": {
    crd: "SnapshotReplication",
    capability: "patchSnapshotReplications",
    path: "spec.suspend",
    namespaced: true,
  },
} as const satisfies Record<string, SuspendableMeta>;

/** One of the six tokens `SuspendBody.kind` takes. */
export type SuspendableKind = keyof typeof SUSPENDABLE;

/** The row for one kind. */
export function suspendable(kind: SuspendableKind): SuspendableMeta {
  return SUSPENDABLE[kind];
}

/**
 * The namespace this kind's capability review must be asked about.
 *
 * A cluster-scoped kind is reviewed with none — asking about a namespace
 * would report a RoleBinding grant that cannot authorize the write.
 */
export function suspendReviewNamespace(
  kind: SuspendableKind,
  namespace: string | undefined,
): string | undefined {
  return SUSPENDABLE[kind].namespaced ? namespace : undefined;
}
