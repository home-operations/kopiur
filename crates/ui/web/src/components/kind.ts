import {
  ArchiveRestore,
  ArrowLeftRight,
  CalendarClock,
  Camera,
  Copy,
  Database,
  ScrollText,
  Server,
  Wrench,
  type LucideIcon,
} from "lucide-react";

import type { ObjectKind } from "../api/types";

/**
 * One kind's identity: the token slug its colour is keyed on, the CRD kind as
 * written, the plural a navigation label uses, and its glyph.
 *
 * The `kopiur-ui-design` skill (`references/kinds.md`) is the source; a kind
 * the server adds to `ObjectKind` fails to compile here until it is given one.
 */
export interface KindMeta {
  kind: ObjectKind;
  /** `--kind-<slug>` and `[data-kind="<slug>"]` in styles.css. */
  slug: string;
  /** The CRD kind, exactly — the word an operator types into `kubectl get`. */
  label: string;
  plural: string;
  icon: LucideIcon;
}

export const KIND_META: Readonly<Record<ObjectKind, KindMeta>> = {
  repository: {
    kind: "repository",
    slug: "repository",
    label: "Repository",
    plural: "Repositories",
    icon: Database,
  },
  clusterRepository: {
    kind: "clusterRepository",
    slug: "cluster-repository",
    label: "ClusterRepository",
    plural: "ClusterRepositories",
    icon: Server,
  },
  maintenance: {
    kind: "maintenance",
    slug: "maintenance",
    label: "Maintenance",
    plural: "Maintenance",
    icon: Wrench,
  },
  snapshotPolicy: {
    kind: "snapshotPolicy",
    slug: "snapshot-policy",
    label: "SnapshotPolicy",
    plural: "Policies",
    icon: ScrollText,
  },
  snapshotSchedule: {
    kind: "snapshotSchedule",
    slug: "snapshot-schedule",
    label: "SnapshotSchedule",
    plural: "Schedules",
    icon: CalendarClock,
  },
  snapshot: {
    kind: "snapshot",
    slug: "snapshot",
    label: "Snapshot",
    plural: "Snapshots",
    icon: Camera,
  },
  restore: {
    kind: "restore",
    slug: "restore",
    label: "Restore",
    plural: "Restores",
    icon: ArchiveRestore,
  },
  repositoryReplication: {
    kind: "repositoryReplication",
    slug: "repository-replication",
    label: "RepositoryReplication",
    plural: "Replications",
    icon: Copy,
  },
  snapshotReplication: {
    kind: "snapshotReplication",
    slug: "snapshot-replication",
    label: "SnapshotReplication",
    plural: "Replications",
    icon: ArrowLeftRight,
  },
};

/** CRD kind → `ObjectKind`, for the wire's reference strings. */
const BY_LABEL: ReadonlyMap<string, ObjectKind> = new Map(
  Object.values(KIND_META).map((m) => [m.label, m.kind]),
);

export interface ParsedRef {
  kind: ObjectKind;
  namespace?: string;
  name: string;
}

/**
 * A wire reference string — `Repository/media/nas`, `ClusterRepository/shared`
 * — as a kind and its parts. Anything that is not a kopiur kind (a `Backend`,
 * a bare name) is `null`: it is shown as a plain identifier, never given an
 * identity it does not have.
 */
export function parseRef(ref: string): ParsedRef | null {
  const parts = ref.split("/");
  const kind = BY_LABEL.get(parts[0] ?? "");
  if (kind === undefined) return null;
  if (parts.length === 2 && parts[1]) return { kind, name: parts[1] };
  if (parts.length === 3 && parts[1] && parts[2]) {
    return { kind, namespace: parts[1], name: parts[2] };
  }
  return null;
}
