import { Database, HardDrive, ListTree, TerminalSquare } from "lucide-react";

import type { RestoreDetail } from "../../../api/types";
import { EMPTY_CELL, relativeTime } from "../../../util/format";
import { Facts } from "../../Facts";
import { restoreProgress } from "../../restore";
import { DrawerSection } from "../DrawerSection";

/** What the restore reads: the source as it resolved, once, at creation. */
export function SourceTab({ detail, now }: { detail: RestoreDetail; now: Date }) {
  const { row, source } = detail;
  return (
    <DrawerSection title="What it reads" icon={Database}>
      <Facts
        label="Source"
        facts={[
          { term: "Source kind", value: <Mono value={row.sourceKind} /> },
          {
            term: "Resolution",
            value:
              source?.resolution === "NoSnapshot" ? (
                "NoSnapshot — no snapshot matched, so onMissingSnapshot left an empty volume. This is not a failure."
              ) : (
                <Mono value={source?.resolution} />
              ),
          },
          {
            term: "Snapshot",
            value:
              source?.snapshot !== null && source?.snapshot !== undefined ? (
                <span className="mono">
                  {source.snapshot.namespace}/{source.snapshot.name}
                </span>
              ) : (
                "not resolved yet"
              ),
          },
          { term: "Kopia manifest", value: <Mono value={row.kopiaSnapshotId} /> },
          { term: "Kopia identity", value: <Mono value={source?.identity} /> },
          { term: "Repository", value: <Mono value={row.repository} /> },
          {
            term: "Pinned at",
            value: <Instant at={source?.pinnedAt} now={now} absent="not pinned yet" />,
          },
        ]}
      />
      <p className="page__section-note">
        Resolved once at creation; later edits to <span className="mono">spec.source</span> do not
        change it.
      </p>
    </DrawerSection>
  );
}

/** Where it writes: the target claim, how far it got, and one row per claim of a fan-out. */
export function TargetTab({ detail, now }: { detail: RestoreDetail; now: Date }) {
  const { row, target } = detail;
  return (
    <>
      <DrawerSection title="Where it writes" icon={HardDrive}>
        <Facts
          label="Target"
          facts={[
            { term: "Target kind", value: <span className="mono">{row.targetKind}</span> },
            { term: "Claim", value: <Mono value={target?.pvc} /> },
            ...(target?.pvcPrime !== null && target?.pvcPrime !== undefined
              ? [{ term: "Populator prime claim", value: <Mono value={target.pvcPrime} /> }]
              : []),
            { term: "Restored so far", value: restoreProgress(row) },
            {
              term: "Started",
              value: <Instant at={row.startTime} now={now} absent="not started" />,
            },
            {
              term: "Finished",
              value: <Instant at={row.endTime} now={now} absent="still running" />,
            },
          ]}
        />
        <p className="page__section-note">
          No percentage: the operator reports no total. A restore writes only into its own
          namespace, <span className="mono">{row.namespace}</span>.
        </p>
      </DrawerSection>
      {row.claims.length > 0 ? (
        <DrawerSection title="Claims" icon={ListTree}>
          <div className="ledger-scroll">
            <table className="ledger" aria-label="Claims">
              <thead>
                <tr>
                  <th scope="col">Claim</th>
                  <th scope="col">Phase</th>
                  <th scope="col">Detail</th>
                </tr>
              </thead>
              <tbody>
                {row.claims.map((claim) => (
                  <tr key={claim.pvc}>
                    <td className="mono">{claim.pvc}</td>
                    <td className="mono">{claim.phase}</td>
                    <td>{claim.message ?? EMPTY_CELL}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <p className="page__section-note">
            One row per target claim. A claim not yet processed shows as{" "}
            <span className="mono">Pending</span>.
          </p>
        </DrawerSection>
      ) : null}
    </>
  );
}

export function LogTab({ detail }: { detail: RestoreDetail }) {
  return (
    <DrawerSection title="Log tail" icon={TerminalSquare}>
      {detail.logTail.length === 0 ? (
        <p className="page__section-note">
          The mover Job wrote no log lines. If it failed before starting, see the failure above.
        </p>
      ) : (
        <>
          <pre className="log-tail" aria-label="Log tail">
            {detail.logTail.join("\n")}
          </pre>
          <p className="page__section-note">
            The last lines kopia wrote, with URLs and tokens redacted at the server.
          </p>
        </>
      )}
    </DrawerSection>
  );
}

/** An identifier, or the ledger's empty cell when the server carries none. */
function Mono({ value }: { value: string | null | undefined }) {
  if (value === null || value === undefined || value.length === 0) {
    return <>{EMPTY_CELL}</>;
  }
  return <span className="mono">{value}</span>;
}

/** An instant with a direction, or the words for its absence. */
function Instant({
  at,
  now,
  absent,
}: {
  at: string | null | undefined;
  now: Date;
  absent: string;
}) {
  if (at === null || at === undefined || at.length === 0) {
    return <span className="restore-table__absent">{absent}</span>;
  }
  return (
    <time dateTime={at} title={at}>
      {relativeTime(at, now)}
    </time>
  );
}
