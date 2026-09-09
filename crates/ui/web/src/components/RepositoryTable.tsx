import { Link } from "@tanstack/react-router";

import type { RepositorySummary } from "../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../util/format";
import { HealthBadge } from "./HealthBadge";
import { accessLabel, detailSearch, repositoryPhaseLabel } from "./repository";
import { NotReported } from "./NotReported";

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
export interface RepositoryTableProps {
  repositories: readonly RepositorySummary[];
  /** The table's accessible name, when the caption should say the filter. */
  caption?: string | undefined;
}

export function RepositoryTable({ repositories, caption = "Repositories" }: RepositoryTableProps) {
  return (
    <div className="ledger-scroll">
      <table className="ledger repo-table" aria-label={caption}>
        <thead>
          <tr>
            <th scope="col">Repository</th>
            <th scope="col">Health</th>
            <th scope="col">Phase</th>
            <th scope="col">Access</th>
            <th scope="col">Backend</th>
            <th scope="col" className="num">
              Snapshots
            </th>
            <th scope="col" className="num">
              Size
            </th>
            <th scope="col" className="num">
              Index blobs
            </th>
            <th scope="col">Last observed</th>
          </tr>
        </thead>
        <tbody>
          {repositories.map((repository) => (
            <tr key={`${repository.kindPath}/${repository.namespace ?? ""}/${repository.name}`}>
              <td>
                <div className="repo-table__object">
                  <span className="label-strip">
                    <span className="label-strip__kind">{repository.kind}</span>
                    <span className="label-strip__name">
                      <Link
                        to="/repositories/$kind/$name"
                        params={{ kind: repository.kindPath, name: repository.name }}
                        search={detailSearch(repository)}
                      >
                        {repository.name}
                      </Link>
                    </span>
                  </span>
                  {repository.namespace !== null &&
                  repository.namespace !== undefined &&
                  repository.namespace.length > 0 ? (
                    <span className="repo-table__namespace mono">{repository.namespace}</span>
                  ) : null}
                  {repository.allowedNamespaceCount !== null &&
                  repository.allowedNamespaceCount !== undefined ? (
                    <span className="repo-table__namespace">
                      admits {repository.allowedNamespaceCount}{" "}
                      {repository.allowedNamespaceCount === 1 ? "namespace" : "namespaces"}
                    </span>
                  ) : null}
                </div>
              </td>
              <td>
                <HealthBadge health={repository.health} />
              </td>
              <td>{repositoryPhaseLabel(repository.phase)}</td>
              <td>
                <div className="repo-table__access">
                  <span>{repository.mode}</span>
                  <span className="repo-table__note">{accessLabel(repository)}</span>
                  {repository.serverEndpoint !== null &&
                  repository.serverEndpoint !== undefined &&
                  repository.serverEndpoint.length > 0 ? (
                    <span className="repo-table__note mono">{repository.serverEndpoint}</span>
                  ) : null}
                </div>
              </td>
              <td>{repository.backend ?? EMPTY_CELL}</td>
              <td className="num">{count(repository.snapshotCount)}</td>
              <td className="num">{humanBytes(repository.totalSizeBytes)}</td>
              <td className="num">{count(repository.indexBlobCount)}</td>
              <td>
                {/* No controller writes this today (see `unwired.tsx`), so in
                    practice it is always the "not reported" cell. It is still
                    read first: the day a controller starts writing it the
                    wiring ratchet fails on the stale entry and this column
                    shows the value with no change here. */}
                <LastObserved at={repository.lastObservedAt} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
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
