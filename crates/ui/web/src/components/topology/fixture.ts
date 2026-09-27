/**
 * The fixture topology every topology test renders — test data, imported by
 * tests only (nothing under `src/routes` or the components imports it, so it
 * never ships).
 *
 * It is the brief's cluster: two namespaced repositories (`mirror` seeded
 * from `nas`, and parked on a phase the server could not read, so `unknown`),
 * one `ClusterRepository` admitting two listed namespaces, one
 * `SnapshotReplication` (healthy), one `RepositoryReplication` to a bare S3
 * backend (failed), a policy writing into two repositories, and a policy
 * whose only repository does not exist — the dangling reference the server
 * mints as a `missing` ghost at `failed`.
 *
 * Ids and labels are spelled exactly as `crates/ui/src/api/graph.rs` builds
 * them (its own test fixture is the source of every string here), so the
 * assertions are about the same node and edge sets the server's tests pin.
 */

import type { GraphEdge, GraphNode, RepositoryGraph } from "../../api/types";

export const GENERATED_AT = "2026-09-08T12:00:00Z";

const node = (over: Partial<GraphNode> & Pick<GraphNode, "id" | "kind" | "name">): GraphNode => ({
  label: over.name,
  health: "healthy",
  missing: false,
  allowsAllNamespaces: false,
  gates: [],
  ...over,
});

const edge = (
  over: Partial<GraphEdge> & Pick<GraphEdge, "id" | "from" | "to" | "kind">,
): GraphEdge => ({
  health: "healthy",
  ...over,
});

export const NAS = node({
  id: "Repository/media/nas",
  kind: "repository",
  name: "nas",
  namespace: "media",
  label: "media/nas",
  backendKind: "filesystem",
});

/** Seeded from `nas`; `status.phase: Weird`, which the server reports as `unknown`. */
export const MIRROR = node({
  id: "Repository/media/mirror",
  kind: "repository",
  name: "mirror",
  namespace: "media",
  label: "media/mirror",
  health: "unknown",
  backendKind: "s3",
});

export const SHARED = node({
  id: "ClusterRepository/shared",
  kind: "clusterRepository",
  name: "shared",
  label: "shared",
  backendKind: "s3",
  gates: [
    {
      condition: "Ready",
      reason: "DeletionProtectionEngaged",
      severity: "warning",
      message: "12 Snapshots are queued for deletion; acknowledge to release",
    },
  ],
});

export const NIGHTLY = node({
  id: "Policy/media/nightly",
  kind: "policy",
  name: "nightly",
  namespace: "media",
  label: "media/nightly",
});

export const ORPHANED = node({
  id: "Policy/media/orphaned",
  kind: "policy",
  name: "orphaned",
  namespace: "media",
  label: "media/orphaned",
});

/** The dangling reference: `orphaned` names a repository that does not exist. */
export const GONE = node({
  id: "Repository/media/gone",
  kind: "repository",
  name: "gone",
  namespace: "media",
  label: "gone (missing)",
  health: "failed",
  missing: true,
});

/** The bare backend a `RepositoryReplication` writes to; the server never observes its health. */
export const BLOBSYNC_BACKEND = node({
  id: "Backend/media/blobsync",
  kind: "backend",
  name: "blobsync",
  namespace: "media",
  label: "s3 dr-bucket/nas/",
  health: "unknown",
  backendKind: "s3",
});

export const NS_PROD = node({ id: "Namespace/prod", kind: "namespace", name: "prod" });
export const NS_STAGING = node({ id: "Namespace/staging", kind: "namespace", name: "staging" });

export const FIXTURE_NODES: readonly GraphNode[] = [
  BLOBSYNC_BACKEND,
  SHARED,
  NS_PROD,
  NS_STAGING,
  NIGHTLY,
  ORPHANED,
  GONE,
  MIRROR,
  NAS,
];

export const FIXTURE_EDGES: readonly GraphEdge[] = [
  edge({
    id: "AllowedNamespace/ClusterRepository/shared/Namespace/prod",
    from: SHARED.id,
    to: NS_PROD.id,
    kind: "allowedNamespace",
  }),
  edge({
    id: "AllowedNamespace/ClusterRepository/shared/Namespace/staging",
    from: SHARED.id,
    to: NS_STAGING.id,
    kind: "allowedNamespace",
  }),
  edge({
    id: "PolicyMembership/Policy/media/nightly/Repository/media/nas",
    from: NIGHTLY.id,
    to: NAS.id,
    kind: "policyMembership",
  }),
  edge({
    id: "PolicyMembership/Policy/media/nightly/ClusterRepository/shared",
    from: NIGHTLY.id,
    to: SHARED.id,
    kind: "policyMembership",
  }),
  // An edge into a ghost is failed however healthy its source is.
  edge({
    id: "PolicyMembership/Policy/media/orphaned/Repository/media/gone",
    from: ORPHANED.id,
    to: GONE.id,
    kind: "policyMembership",
    health: "failed",
  }),
  edge({
    id: "Seed/Repository/media/nas/Repository/media/mirror",
    from: NAS.id,
    to: MIRROR.id,
    kind: "seed",
  }),
  edge({
    id: "SnapshotReplication/media/offsite",
    from: NAS.id,
    to: SHARED.id,
    kind: "snapshotReplication",
    label: "0 3 * * *",
  }),
  edge({
    id: "RepositoryReplication/media/blobsync",
    from: NAS.id,
    to: BLOBSYNC_BACKEND.id,
    kind: "repositoryReplication",
    label: "0 5 * * *",
    health: "failed",
  }),
];

export const FIXTURE_GRAPH: RepositoryGraph = {
  nodes: [...FIXTURE_NODES],
  edges: [...FIXTURE_EDGES],
  generatedAt: GENERATED_AT,
};

/** The node ids, sorted, as the server's own test pins them. */
export const FIXTURE_NODE_IDS: readonly string[] = [
  "Backend/media/blobsync",
  "ClusterRepository/shared",
  "Namespace/prod",
  "Namespace/staging",
  "Policy/media/nightly",
  "Policy/media/orphaned",
  "Repository/media/gone",
  "Repository/media/mirror",
  "Repository/media/nas",
];

/** The edge ids, sorted. */
export const FIXTURE_EDGE_IDS: readonly string[] = [
  "AllowedNamespace/ClusterRepository/shared/Namespace/prod",
  "AllowedNamespace/ClusterRepository/shared/Namespace/staging",
  "PolicyMembership/Policy/media/nightly/ClusterRepository/shared",
  "PolicyMembership/Policy/media/nightly/Repository/media/nas",
  "PolicyMembership/Policy/media/orphaned/Repository/media/gone",
  "RepositoryReplication/media/blobsync",
  "Seed/Repository/media/nas/Repository/media/mirror",
  "SnapshotReplication/media/offsite",
];
