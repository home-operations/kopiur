import type { ReactNode } from "react";

import type { RepositorySummary } from "../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../util/format";
import { ColumnLedger } from "./ColumnLedger";
import { HealthBadge } from "./HealthBadge";
import { accessLabel, repositoryPhaseLabel } from "./repository";
import { NotReported } from "./NotReported";
import { admitsText } from "./admits";
import { KindChip, KindName } from "./KindMark";
import { KIND_META } from "./kind";
import { InspectLink } from "./InspectLink";
import type { ColumnSpec } from "./tableColumns";

/**
 * The fleet as a ledger: one row per `Repository` and `ClusterRepository`,
 * whichever CRD it came from.
 *
 * Two kinds, one table, because "my repositories" is one question — the same
 * reason `GET /api/v1/repositories` returns one array. The kind survives as
 * the label strip's KIND, so the row still says which CRD to `kubectl edit`.
 *
 * Every link is built from `summary.kindPath` (addenda item 16). The display
 * kind and the URL segment differ in case and punctuation, and the field
 * exists precisely so this bundle holds no mapping table for them.
 *
 * Gates are deliberately absent: `RepositorySummary` carries none. A gated
 * repository shows Degraded or Failed here — the backend folds gates into the
 * health it ships — and the gate itself is named on the detail screen.
 */
/** The table's mixed rows, each as its own kind. */
function repoKind(repository: RepositorySummary): "repository" | "clusterRepository" {
  return repository.kind === "ClusterRepository" ? "clusterRepository" : "repository";
}

export interface RepositoryTableProps {
  repositories: readonly RepositorySummary[];
  /** The table's accessible name, when the caption should say the filter. */
  caption?: string | undefined;
}

type RepositoryColumn =
  | "repository"
  | "health"
  | "phase"
  | "access"
  | "backend"
  | "snapshots"
  | "size"
  | "indexBlobs"
  | "lastObserved";

const REPOSITORY_COLUMNS: readonly ColumnSpec<RepositoryColumn>[] = [
  { id: "repository", label: "Repository", width: 240, min: 240, locked: true, stripe: true },
  { id: "health", label: "Health", width: 130, min: 130 },
  { id: "phase", label: "Phase", width: 120, min: 80 },
  { id: "access", label: "Access", width: 180, min: 150 },
  { id: "backend", label: "Backend", width: 110, min: 90 },
  { id: "snapshots", label: "Snapshots", width: 110, min: 110, numeric: true },
  { id: "size", label: "Size", width: 100, min: 96, numeric: true },
  { id: "indexBlobs", label: "Index blobs", width: 120, min: 120, numeric: true },
  { id: "lastObserved", label: "Last observed", width: 150, min: 130 },
];

export function RepositoryTable({ repositories, caption = "Repositories" }: RepositoryTableProps) {
  return (
    <ColumnLedger
      id="repositories"
      label={caption}
      className="repo-table"
      columns={REPOSITORY_COLUMNS}
      rows={repositories}
      rowKey={(r) => `${r.kindPath}/${r.namespace ?? ""}/${r.name}`}
      rowProps={(r) => ({ "data-kind": KIND_META[repoKind(r)].slug })}
      cell={repositoryCell}
    />
  );
}

function repositoryCell(repository: RepositorySummary, id: RepositoryColumn): ReactNode {
  switch (id) {
    case "repository":
      return (
        <div className="table__object">
          <KindChip kind={repoKind(repository)} size="sm" />
          <div className="repo-table__object">
            <span className="label-strip">
              <KindName kind={repoKind(repository)} />
              <span className="label-strip__name">
                <InspectLink
                  className="row-link"
                  target={{
                    kind: repoKind(repository),
                    namespace: repository.namespace ?? undefined,
                    name: repository.name,
                  }}
                >
                  {repository.name}
                </InspectLink>
              </span>
            </span>
            {repository.namespace !== null &&
            repository.namespace !== undefined &&
            repository.namespace.length > 0 ? (
              <span className="repo-table__namespace mono">{repository.namespace}</span>
            ) : null}
            {repository.admits !== null && repository.admits !== undefined ? (
              <span className="repo-table__namespace">{admitsText(repository.admits)}</span>
            ) : null}
          </div>
        </div>
      );
    case "health":
      return <HealthBadge health={repository.health} />;
    case "phase":
      return repositoryPhaseLabel(repository.phase);
    case "access":
      return (
        <div className="repo-table__access">
          <span>{repository.mode}</span>
          <span className="repo-table__note">{accessLabel(repository)}</span>
          {repository.serverEndpoint !== null &&
          repository.serverEndpoint !== undefined &&
          repository.serverEndpoint.length > 0 ? (
            <span className="repo-table__note mono">{repository.serverEndpoint}</span>
          ) : null}
        </div>
      );
    case "backend":
      return repository.backend ?? EMPTY_CELL;
    case "snapshots":
      return count(repository.snapshotCount);
    case "size":
      return humanBytes(repository.totalSizeBytes);
    case "indexBlobs":
      return count(repository.indexBlobCount);
    case "lastObserved":
      // No controller writes this today (see `unwired.tsx`), so in practice
      // it is always the "not reported" cell. It is still read first: the day
      // a controller starts writing it the wiring ratchet fails on the stale
      // entry and this column shows the value with no change here.
      return <LastObserved at={repository.lastObservedAt} />;
    default:
      return id satisfies never;
  }
}

/** A count, where zero is a measurement and absent is not one. */
function count(value: number | null | undefined): string {
  return value === null || value === undefined ? EMPTY_CELL : String(value);
}

/**
 * When the storage statistics were last observed — shared by this table and
 * the detail screen so the two cannot word the same absence differently.
 */
export function LastObserved({ at }: { at?: string | null | undefined }) {
  if (at === null || at === undefined || at.length === 0) {
    return <NotReported field="repositoryLastObserved" />;
  }
  return <time dateTime={at}>{relativeTime(at)}</time>;
}
