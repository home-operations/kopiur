import { PlayCircle } from "lucide-react";

import { useReplicationRun } from "../api/hooks";
import { ActionButton } from "./ActionButton";
import { ActionResult } from "./ActionResult";
import { HealthBadge } from "./HealthBadge";
import {
  type ReplicationRow,
  lastRunSummary,
  replicationHealth,
  replicationLag,
  replicationPhaseLabel,
} from "./replication";
import { NotReported } from "./NotReported";
import { useCapabilityReason } from "./useCapabilityReason";

/**
 * Both replication kinds in one ledger, worst question first: how far behind
 * is each copy?
 *
 * Lag is the column this table exists for. A replication that has never
 * succeeded reads "never" rather than an empty cell, because that is the most
 * alarming fact the table can carry and a dash would bury it.
 *
 * "Next run" is honest about a gap in the operator rather than papering over
 * it. `RepositoryReplication.status.nextScheduledAt` is declared and written
 * by nothing (the wiring ratchet's never-written set), and
 * `SnapshotReplication` has no such field on the CRD at all — so both render
 * "not reported", each with its own reason. The cron expression beside it is
 * what actually says when the next run is due.
 */
export interface ReplicationTableProps {
  rows: readonly ReplicationRow[];
  /** The clock lag is measured against. */
  now?: Date | undefined;
}

export function ReplicationTable({ rows, now = new Date() }: ReplicationTableProps) {
  return (
    <div className="ledger-scroll">
      <table className="ledger replication-table" aria-label="Replications">
        <thead>
          <tr>
            <th scope="col">Replication</th>
            <th scope="col">State</th>
            <th scope="col">Copies</th>
            <th scope="col">Schedule</th>
            <th scope="col">Last replicated</th>
            <th scope="col">Next run</th>
            <th scope="col">Last run</th>
            <th scope="col">Run</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.id}>
              <td>
                <div className="replication-table__object">
                  <span className="label-strip">
                    <span className="label-strip__kind">{row.kind}</span>
                    <span className="label-strip__name">{row.name}</span>
                  </span>
                  <span className="replication-table__namespace mono">{row.namespace}</span>
                </div>
              </td>
              <td>
                <HealthBadge
                  health={replicationHealth(row.phase, row.suspended)}
                  label={row.suspended ? "Suspended" : replicationPhaseLabel(row.phase)}
                />
              </td>
              <td>
                <div className="replication-table__route">
                  <span className="mono">{row.source}</span>
                  <span className="replication-table__arrow" aria-hidden="true">
                    →
                  </span>
                  <span className="mono">
                    {row.destination}
                    <span className="visually-hidden"> (destination)</span>
                  </span>
                  <span className="replication-table__note">
                    {row.destinationIsRepository
                      ? "snapshots copied into another repository"
                      : "blobs synced to a bare backend"}
                  </span>
                </div>
              </td>
              <td className="mono">{row.cron}</td>
              <td>
                <Lag at={row.lastReplicated} now={now} />
              </td>
              <td>
                {row.destinationIsRepository ? (
                  <NotReported reason="SnapshotReplication publishes no next-run time on its status; the cron expression is what says when the next copy is due." />
                ) : (
                  <NotReported field="repositoryReplicationNextRun" />
                )}
              </td>
              <td>
                <LastRun row={row} />
              </td>
              <td>
                <RunAction row={row} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** How far behind this copy is; "never" is louder than a dash, and truer. */
function Lag({ at, now }: { at: string | null | undefined; now: Date }) {
  const text = replicationLag(at, now);
  if (at === null || at === undefined || at.length === 0) {
    return <span className="replication-table__never">{text}</span>;
  }
  return <time dateTime={at}>{text}</time>;
}

function LastRun({ row }: { row: ReplicationRow }) {
  const summary = lastRunSummary(row);
  if (summary === null) {
    // A blob sync's only two counters are both in the never-written set, so
    // there is nothing this cell could truthfully summarise.
    return <NotReported field="repositoryReplicationBytes" />;
  }
  return <span>{summary}</span>;
}

/**
 * Run this replication now.
 *
 * The capability is asked for the row's **own** namespace, not the page's
 * scope: on a cluster-wide list the rows span namespaces, and a single
 * cluster-scoped review would disable every button for exactly the user whose
 * RoleBinding grants the write (addenda item 17). One hook per row, so
 * TanStack collapses the repeats into one request per distinct namespace.
 */
function RunAction({ row }: { row: ReplicationRow }) {
  const run = useReplicationRun();
  const reason = useCapabilityReason(
    row.namespace,
    row.kindToken === "replication" ? "patchRepositoryReplications" : "patchSnapshotReplications",
  );
  const suspended = row.suspended
    ? `${row.name} is suspended; resume it before asking for a run.`
    : undefined;
  return (
    <div className="replication-table__run">
      <ActionButton
        disabledReason={suspended ?? (run.isPending ? "The request is in flight." : reason)}
        onClick={() => {
          run.mutate({ namespace: row.namespace, name: row.name, kind: row.kindToken });
        }}
        aria-label={`Run ${row.name} now`}
      >
        <PlayCircle size={14} strokeWidth={2} aria-hidden="true" />
        Run now
      </ActionButton>
      <ActionResult label={`Run ${row.name}`} receipt={run.data} problem={run.error?.problem} />
    </div>
  );
}
