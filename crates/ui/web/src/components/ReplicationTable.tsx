import { PlayCircle } from "lucide-react";
import type { ReactNode } from "react";

import { useReplicationRun } from "../api/hooks";
import { lagSeries, lagShare } from "../charts/lag";
import { ActionButton } from "./ActionButton";
import { ColumnLedger } from "./ColumnLedger";
import { HealthBadge } from "./HealthBadge";
import {
  type ReplicationRow,
  lastRunSummary,
  replicationHealth,
  replicationLag,
  replicationPhaseLabel,
} from "./replication";
import { NotReported } from "./NotReported";
import { KindChip, KindName } from "./KindMark";
import { KIND_META } from "./kind";
import { useCapabilityReason } from "./useCapabilityReason";
import { InspectLink } from "./InspectLink";
import type { ColumnSpec } from "./tableColumns";

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

/** A merged row as its own replication kind. */
function replicationKind(row: ReplicationRow): "repositoryReplication" | "snapshotReplication" {
  return row.kind === "RepositoryReplication" ? "repositoryReplication" : "snapshotReplication";
}

type ReplicationColumn =
  | "replication"
  | "state"
  | "copies"
  | "schedule"
  | "lastReplicated"
  | "lastRun"
  | "run";

const REPLICATION_COLUMNS: readonly ColumnSpec<ReplicationColumn>[] = [
  { id: "replication", label: "Replication", width: 260, min: 240, locked: true, stripe: true },
  { id: "state", label: "State", width: 140, min: 130 },
  { id: "copies", label: "Copies", width: 260, min: 140 },
  { id: "schedule", label: "Schedule", width: 170, min: 110 },
  { id: "lastReplicated", label: "Last replicated", width: 160, min: 150 },
  { id: "lastRun", label: "Last run", width: 160, min: 100 },
  { id: "run", label: "Run", width: 150, min: 120, locked: true, resizable: false },
];

export function ReplicationTable({ rows, now = new Date() }: ReplicationTableProps) {
  // One axis for every row's lag bar, scaled to the stalest copy.
  const lag = lagSeries(rows, now);
  return (
    <ColumnLedger
      id="replications"
      label="Replications"
      className="replication-table"
      columns={REPLICATION_COLUMNS}
      rows={rows}
      rowKey={(row) => row.id}
      rowProps={(row) => ({ "data-kind": KIND_META[replicationKind(row)].slug })}
      cell={(row, id) => replicationCell(row, id, now, lag.max)}
    />
  );
}

function replicationCell(
  row: ReplicationRow,
  id: ReplicationColumn,
  now: Date,
  lagMax: number,
): ReactNode {
  switch (id) {
    case "replication":
      return (
        <div className="table__object">
          <KindChip kind={replicationKind(row)} size="sm" />
          <div className="replication-table__object">
            <span className="label-strip">
              <KindName kind={replicationKind(row)} />
              <span className="label-strip__name">
                <InspectLink
                  className="row-link"
                  target={{
                    kind: replicationKind(row),
                    namespace: row.namespace,
                    name: row.name,
                  }}
                >
                  {row.name}
                </InspectLink>
              </span>
            </span>
            <span className="replication-table__namespace mono">{row.namespace}</span>
          </div>
        </div>
      );
    case "state":
      return (
        <HealthBadge
          health={replicationHealth(row.phase, row.suspended)}
          label={row.suspended ? "Suspended" : replicationPhaseLabel(row.phase)}
        />
      );
    case "copies":
      return (
        <div className="replication-table__route">
          <span className="replication-table__hops">
            <span className="mono">{row.source}</span>
            <span className="replication-table__arrow" aria-hidden="true">
              →
            </span>
            <span className="mono">
              {row.destination}
              <span className="visually-hidden"> (destination)</span>
            </span>
          </span>
          <span className="replication-table__note">
            {row.destinationIsRepository
              ? "snapshots copied into another repository"
              : "blobs synced to a bare backend"}
          </span>
        </div>
      );
    case "schedule":
      // The next run rides here rather than in a column of its own: it is
      // "not reported" on every row of both kinds, so a column would spend a
      // full width saying nothing, and the cron beside it is what actually
      // answers the question.
      return (
        <div className="replication-table__schedule">
          <span className="mono">{row.cron}</span>
          <span className="replication-table__note">
            next run{" "}
            {row.destinationIsRepository ? (
              <NotReported reason="SnapshotReplication reports no next-run time; see the cron." />
            ) : (
              <NotReported field="repositoryReplicationNextRun" />
            )}
          </span>
        </div>
      );
    case "lastReplicated":
      return <Lag at={row.lastReplicated} now={now} max={lagMax} />;
    case "lastRun":
      return <LastRun row={row} />;
    case "run":
      return <RunAction row={row} />;
    default:
      return id satisfies never;
  }
}

/**
 * How far behind this copy is: the age in words, and under it a bar on the
 * same axis as every other row's (`charts/lag.ts`), so the stalest copy
 * stands out down the column. "Never" is louder than a dash, and truer, and
 * has no bar — there is no age to draw.
 */
function Lag({ at, now, max }: { at: string | null | undefined; now: Date; max: number }) {
  const text = replicationLag(at, now);
  if (at === null || at === undefined || at.length === 0) {
    return <span className="replication-table__never">{text}</span>;
  }
  const ms = new Date(at).getTime();
  return (
    <div className="lag">
      <time dateTime={at}>{text}</time>
      {Number.isNaN(ms) ? null : (
        <span className="lag-bar" aria-hidden="true">
          <span
            className="lag-bar__fill"
            style={{
              width: `${String(Math.round(lagShare(Math.max(0, now.getTime() - ms), max) * 1000) / 10)}%`,
            }}
          />
        </span>
      )}
    </div>
  );
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
  // Both halves of the refusal, kept apart on purpose: the short word goes in
  // the cell, the sentence goes to assistive technology and to the native
  // title. A ledger clips a floating tooltip, so the reason has to be legible
  // without one — and a word that is always there beats a word behind a hover
  // for someone scanning the list for what they may act on.
  const [word, sentence] = row.suspended
    ? ["suspended", `${row.name} is suspended; resume it before asking for a run.`]
    : run.isPending
      ? ["in flight", "The request is in flight."]
      : reason !== undefined
        ? ["not permitted", reason]
        : [undefined, undefined];
  return (
    <div className="replication-table__run">
      <ActionButton
        disabledReason={sentence}
        reasonShown={false}
        title={sentence}
        onClick={() => {
          run.mutate({ namespace: row.namespace, name: row.name, kind: row.kindToken });
        }}
        aria-label={`Run ${row.name} now`}
      >
        <PlayCircle size={14} strokeWidth={2} aria-hidden="true" />
        Run now
      </ActionButton>
      {word !== undefined ? <span className="replication-table__note">{word}</span> : null}
    </div>
  );
}
