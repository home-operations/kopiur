import {
  Database,
  HardDrive,
  ListTree,
  OctagonX,
  ScrollText,
  TerminalSquare,
  type LucideIcon,
} from "lucide-react";
import type { ReactNode } from "react";

import type { ConditionView, RestoreDetail } from "../api/types";
import { EMPTY_CELL, humanBytes, humanDuration, relativeTime } from "../util/format";
import { DetailHeader, type TrailHop } from "./DetailHeader";
import { Facts } from "./Facts";
import { Finding } from "./Finding";
import { healthLamp } from "./health";
import { detailHref, parseRef } from "./kind";
import { restoreProgress, restoreVerdict } from "./restore";

/**
 * One restore, in the order someone recovering data needs it.
 *
 * The verdict first: what phase it is in and how far it got. Then the
 * failure, if there is one, because a restore that failed is the only thing
 * on the page worth reading. Then what the source actually *resolved to* —
 * which is not what the spec asks for, and is what the run will read — then
 * the claim being written, the per-claim rows of a fan-out, the conditions,
 * and the redacted log tail.
 *
 * Presentation only; the route owns every hook.
 */
export interface RestoreDetailViewProps {
  detail: RestoreDetail;
  now?: Date | undefined;
}

/** Seconds from start to end; `null` while it runs or when either is unwritten. */
function tookSeconds(start: string | null | undefined, end: string | null | undefined) {
  if (start === null || start === undefined || end === null || end === undefined) return null;
  const ms = Date.parse(end) - Date.parse(start);
  return Number.isNaN(ms) ? null : ms / 1000;
}

/**
 * Where a restore sits: the repository it reads, then the snapshot it was
 * pinned to. The wire carries no policy for that snapshot, so the trail stops
 * there rather than guessing one.
 */
function restoreTrail(detail: RestoreDetail): TrailHop[] {
  const hops: TrailHop[] = [];
  const repo = detail.row.repository ? parseRef(detail.row.repository) : null;
  if (repo !== null) {
    hops.push({
      kind: repo.kind,
      name: repo.name,
      namespace: repo.namespace,
      to: detailHref(repo.kind, repo.name, repo.namespace),
    });
  }
  const snapshot = detail.source?.snapshot;
  if (snapshot !== null && snapshot !== undefined) {
    hops.push({
      kind: "snapshot",
      name: snapshot.name,
      namespace: snapshot.namespace,
      to: detailHref("snapshot", snapshot.name, snapshot.namespace),
    });
  }
  return hops;
}

export function RestoreDetailView({ detail, now = new Date() }: RestoreDetailViewProps) {
  const { row } = detail;
  const verdict = restoreVerdict(row);
  const source = detail.source;
  const target = detail.target;

  return (
    <div className="page">
      <DetailHeader
        kind="restore"
        name={row.name}
        namespace={row.namespace}
        lamp={verdict.lamp}
        verdictLabel="Restore verdict"
        verdict={verdict.text}
        trail={restoreTrail(detail)}
        stats={[
          { label: "Restored", value: humanBytes(row.bytesRestored) },
          {
            label: "Files",
            value:
              row.filesRestored !== null && row.filesRestored !== undefined
                ? row.filesRestored.toLocaleString()
                : { absent: "na" },
          },
          { label: "Took", value: humanDuration(tookSeconds(row.startTime, row.endTime)) },
          {
            label: "Started",
            value:
              row.startTime !== null && row.startTime !== undefined
                ? relativeTime(row.startTime, now)
                : { absent: "na" },
            abs: row.startTime ?? undefined,
          },
        ]}
      />

      {detail.failure !== null && detail.failure !== undefined ? (
        <Section title="Why it failed" icon={OctagonX}>
          <Finding
            title={detail.failure.kopiaErrorClass ?? "Failure"}
            what={detail.failure.message ?? "The operator recorded a failure with no message."}
            why={
              detail.failure.op !== null && detail.failure.op !== undefined
                ? `The ${detail.failure.op} step of the mover Job is what failed.`
                : undefined
            }
            fix={
              detail.failure.retryRecommended === true
                ? "A retry is likely to succeed: create the restore again."
                : detail.failure.retryRecommended === false
                  ? "A retry will not help until the cause above is fixed."
                  : undefined
            }
            lamp={healthLamp("failed")}
            meta={
              detail.failure.exitCode !== null && detail.failure.exitCode !== undefined ? (
                <span className="mono">mover exit code {detail.failure.exitCode}</span>
              ) : undefined
            }
          />
        </Section>
      ) : null}

      <div className="page__pair">
        <Section title="What it reads" icon={Database}>
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
            Resolved once at creation; later edits to <span className="mono">spec.source</span> do
            not change it.
          </p>
        </Section>

        <Section title="Where it writes" icon={HardDrive}>
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
        </Section>
      </div>

      {row.claims.length > 0 ? (
        <Section title="Claims" icon={ListTree}>
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
        </Section>
      ) : null}

      <Section title="Conditions" icon={ScrollText}>
        <Conditions conditions={detail.conditions} now={now} />
      </Section>

      <Section title="Log tail" icon={TerminalSquare}>
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
      </Section>
    </div>
  );
}

interface SectionProps {
  title: string;
  icon: LucideIcon;
  children: ReactNode;
}

function Section({ title, icon: Icon, children }: SectionProps) {
  return (
    <section className="page__section" aria-label={title}>
      <div className="page__section-head">
        <h2>
          <Icon size={16} strokeWidth={2} aria-hidden="true" />
          {title}
        </h2>
      </div>
      {children}
    </section>
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

function Conditions({ conditions, now }: { conditions: readonly ConditionView[]; now: Date }) {
  if (conditions.length === 0) {
    return (
      <p className="page__section-note">
        No conditions yet: the operator has not reconciled this restore.
      </p>
    );
  }
  return (
    <div className="ledger-scroll">
      <table className="ledger" aria-label="Conditions">
        <thead>
          <tr>
            <th scope="col">Type</th>
            <th scope="col">Status</th>
            <th scope="col">Reason</th>
            <th scope="col">Message</th>
            <th scope="col" className="num">
              Since
            </th>
          </tr>
        </thead>
        <tbody>
          {conditions.map((condition) => (
            <tr key={condition.type}>
              <td className="mono">{condition.type}</td>
              <td className="mono">{condition.status}</td>
              <td className="mono">{condition.reason ?? EMPTY_CELL}</td>
              <td>{condition.message ?? EMPTY_CELL}</td>
              <td className="num">
                {condition.lastTransitionTime !== null &&
                condition.lastTransitionTime !== undefined ? (
                  <time dateTime={condition.lastTransitionTime}>
                    {relativeTime(condition.lastTransitionTime, now)}
                  </time>
                ) : (
                  EMPTY_CELL
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
