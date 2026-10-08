import { Link } from "@tanstack/react-router";
import {
  Database,
  FolderSearch,
  HeartPulse,
  ScrollText,
  Server,
  ShieldAlert,
  Sprout,
  TerminalSquare,
  Wrench,
} from "lucide-react";
import type { ReactNode } from "react";

import type {
  CatalogView,
  ConditionView,
  MaintenanceRow,
  PolicyRow,
  ReplicationsView,
  RepositoryDetail as RepositoryDetailData,
  RunStatusView,
  SessionInfo,
} from "../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../util/format";
import { DetailHeader } from "./DetailHeader";
import { FlowLanes, type Lane, type LaneItem } from "./FlowLanes";
import { Facts, type Fact } from "./Facts";
import { Finding } from "./Finding";
import { LastObserved } from "./RepositoryTable";
import { gateSeverityLamp } from "./gates";
import { accessLabel, catalogCoverageNote, repositoryVerdict } from "./repository";
import { NotReported } from "./NotReported";
import { admitsText } from "./admits";

/**
 * One repository, in the order an operator needs it when it is unhealthy.
 *
 * The verdict first — what this repository is doing, in a sentence, on the
 * server's own lamp. Then the gates, which are the reasons a human has to act
 * before anything else will move. Then what is actually in it, whether the
 * operator can reach it, how it is maintained, and who writes into it. The
 * raw conditions are last: they are the evidence for everything above, and a
 * reader who got that far wants them verbatim.
 *
 * Presentation only. Every mutating control is passed in by the route, which
 * owns the hooks — so this renders identically in a test with no query client.
 */
export interface RepositoryDetailProps {
  detail: RepositoryDetailData;
  /** The suspend / maintenance / scan controls, built by the route. */
  actions?: ReactNode;
  /** The "stop session" control for one browse session, built by the route. */
  renderSessionAction?: ((session: SessionInfo) => ReactNode) | undefined;
  /** The clock ages are measured against. */
  now?: Date | undefined;
  /** Policy rows, to show the policies writing here as cards; names alone otherwise. */
  policyRows?: readonly PolicyRow[] | undefined;
  /** Replication rows, to show what copies this repository as cards. */
  replications?: ReplicationsView | undefined;
}

/**
 * Fired by → written by → this repository → copies to. Names the server sent
 * with no row loaded stay references, never dropped and never guessed.
 */
function repositoryLanes(
  detail: RepositoryDetailData,
  selfKind: "repository" | "clusterRepository",
  policyRows: readonly PolicyRow[] | undefined,
  replications: ReplicationsView | undefined,
): Lane[] {
  const { summary } = detail;
  const ns = summary.namespace ?? undefined;
  const written: LaneItem[] = detail.policies.map((p) => {
    const row = policyRows?.find((r) => r.namespace === p.namespace && r.name === p.name);
    return row !== undefined
      ? { card: { kind: "snapshotPolicy", row } }
      : { ref: { kind: "snapshotPolicy", name: p.name, namespace: p.namespace } };
  });
  for (const name of detail.replicationsIn) {
    const row = replications?.snapshot.find((r) => r.name === name);
    written.push(
      row !== undefined
        ? { card: { kind: "snapshotReplication", row } }
        : { ref: { kind: "snapshotReplication", name, namespace: ns } },
    );
  }
  const copies: LaneItem[] = detail.replicationsOut.map((name) => {
    const snap = replications?.snapshot.find((r) => r.name === name);
    if (snap !== undefined) return { card: { kind: "snapshotReplication", row: snap } };
    const repo = replications?.repository.find((r) => r.name === name);
    if (repo !== undefined) return { card: { kind: "repositoryReplication", row: repo } };
    return { ref: { kind: "snapshotReplication", name, namespace: ns } };
  });
  return [
    {
      title: "Fired by",
      label: "Fired by",
      items: detail.schedules.map((row) => ({ card: { kind: "snapshotSchedule", row } })),
      empty: "No schedule fires the policies writing here.",
    },
    {
      title: "Written by",
      label: "Policies writing here",
      items: written,
      empty: "No SnapshotPolicy writes into this repository. Older snapshots may still be here.",
    },
    {
      title: "This repository",
      label: "This repository",
      items: [{ card: { kind: selfKind, row: summary } }],
      empty: "",
      variant: "stats",
    },
    {
      title: "Copies to",
      label: "Copies to",
      items: copies,
      empty: "Nothing copies this repository.",
    },
  ];
}

export function RepositoryDetail({
  detail,
  actions,
  renderSessionAction,
  now = new Date(),
  policyRows,
  replications,
}: RepositoryDetailProps) {
  const { summary } = detail;
  const verdict = repositoryVerdict(summary);
  const selfKind = summary.kind === "ClusterRepository" ? "clusterRepository" : "repository";
  const lanes = repositoryLanes(detail, selfKind, policyRows, replications);
  const namespaceScope = summary.namespace ?? undefined;
  const replicationSearch = namespaceScope !== undefined ? { namespace: namespaceScope } : {};

  return (
    <div className="page">
      <DetailHeader
        kind={selfKind}
        name={summary.name}
        namespace={summary.namespace ?? undefined}
        lamp={verdict.lamp}
        verdictLabel="Repository verdict"
        verdict={verdict.text}
        actions={actions}
        stats={[
          {
            label: "Snapshots",
            value:
              summary.snapshotCount !== null && summary.snapshotCount !== undefined
                ? summary.snapshotCount.toLocaleString()
                : { absent: "na" },
          },
          { label: "Stored", value: humanBytes(summary.totalSizeBytes) },
          {
            label: "Index blobs",
            value:
              summary.indexBlobCount !== null && summary.indexBlobCount !== undefined
                ? summary.indexBlobCount.toLocaleString()
                : { absent: "na" },
          },
          {
            label: "Last observed",
            value:
              summary.lastObservedAt !== null && summary.lastObservedAt !== undefined
                ? relativeTime(summary.lastObservedAt, now)
                : { absent: "unreported", field: "repositoryLastObserved" },
          },
        ]}
      />

      <FlowLanes label="Relationships" lanes={lanes} />

      {detail.gates.length > 0 ? (
        <Section title="Gates holding this repository" icon={ShieldAlert}>
          <ul className="finding-list">
            {detail.gates.map((gate) => {
              const lamp = gateSeverityLamp(gate.severity);
              return (
                <li key={`${gate.condition}/${gate.reason}`}>
                  <Finding
                    title={gate.reason}
                    what={gate.message}
                    lamp={lamp}
                    meta={
                      <span className="mono">
                        condition {gate.condition} · {lamp.word}
                      </span>
                    }
                  />
                </li>
              );
            })}
          </ul>
          <p className="page__section-note">
            A gate never self-heals — the repository stays parked until a human acts. The{" "}
            <Link to="/gates" search={replicationSearch}>
              gate registry
            </Link>{" "}
            explains every gate the operator can raise.
          </p>
        </Section>
      ) : null}

      <div className="page__pair">
        <Section title="Storage" icon={Database}>
          <Facts
            label="Storage"
            facts={[
              { term: "Backend", value: summary.backend ?? EMPTY_CELL },
              { term: "Mode", value: summary.mode },
              { term: "Snapshots", value: count(summary.snapshotCount) },
              { term: "Size", value: humanBytes(summary.totalSizeBytes) },
              { term: "Index blobs", value: count(summary.indexBlobCount) },
              { term: "Last observed", value: <LastObserved at={summary.lastObservedAt} /> },
              ...(summary.admits !== null && summary.admits !== undefined
                ? [{ term: "Namespaces", value: admitsText(summary.admits) }]
                : []),
              ...(detail.identityCluster !== null && detail.identityCluster !== undefined
                ? [
                    {
                      term: "Identity cluster",
                      value: <span className="mono">{detail.identityCluster}</span>,
                    },
                  ]
                : []),
            ]}
          />
        </Section>

        <Section title="Catalog" icon={FolderSearch}>
          {detail.catalog === null || detail.catalog === undefined ? (
            <p className="page__section-note">
              No catalog scan has been recorded. A scan lists backups already in the repository,
              such as ones from another cluster.
            </p>
          ) : (
            <Facts
              label="Catalog"
              facts={[
                {
                  term: "Discovered backups",
                  value: discoveredWithCoverage(detail.catalog),
                },
                { term: "Foreign snapshots", value: count(detail.catalog.foreignSnapshotCount) },
                {
                  term: "Last scan",
                  value: instant(detail.catalog.lastRefreshAt, now),
                },
              ]}
            />
          )}
        </Section>
      </div>

      <div className="page__pair">
        <Section title="Health probe" icon={HeartPulse}>
          {detail.health === null || detail.health === undefined ? (
            <p className="page__section-note">
              No probe result has been recorded. The probe flags an unreachable backend before a
              backup fails on it.
            </p>
          ) : (
            <Facts
              label="Health probe"
              facts={[
                { term: "Last probe", value: instant(detail.health.lastProbeAt, now) },
                { term: "Last healthy", value: instant(detail.health.lastHealthyAt, now) },
                {
                  term: "Failures since success",
                  value: count(detail.health.consecutiveProbeFailures),
                },
              ]}
            />
          )}
        </Section>

        <Section title="Access" icon={Server}>
          <Facts
            label="Access"
            facts={[
              { term: "Reached", value: accessLabel(summary) },
              ...(detail.server !== null && detail.server !== undefined
                ? [
                    {
                      term: "Endpoint",
                      value: <span className="mono">{detail.server.endpoint ?? EMPTY_CELL}</span>,
                    },
                    {
                      term: "Writes",
                      value:
                        detail.server.readOnly === null || detail.server.readOnly === undefined
                          ? EMPTY_CELL
                          : detail.server.readOnly
                            ? "Refused — the server is read-only"
                            : "Accepted",
                    },
                    { term: "Auth", value: detail.server.authMode ?? EMPTY_CELL },
                  ]
                : []),
            ]}
          />
          {detail.server === null || detail.server === undefined ? (
            <p className="page__section-note">
              No repository server is running; movers connect to the storage backend directly.
            </p>
          ) : null}
        </Section>
      </div>

      {/* Seed rides beside maintenance rather than alone across the width: it
          is four facts, and a section with three quarters of the row empty
          reads as a gap in the page rather than as a short answer. With no
          seed, maintenance keeps the first column. */}
      <div className="page__pair">
        {detail.seed !== null && detail.seed !== undefined ? (
          <Section title="Seed" icon={Sprout}>
            <Facts
              label="Seed"
              facts={[
                { term: "Mode", value: detail.seed.mode ?? EMPTY_CELL },
                {
                  term: "Seeded from",
                  value: <span className="mono">{detail.seed.source ?? EMPTY_CELL}</span>,
                },
                { term: "Completed", value: instant(detail.seed.seededAt, now) },
                { term: "Snapshots copied", value: count(detail.seed.snapshotsCopied) },
              ]}
            />
          </Section>
        ) : null}

        <Section title="Maintenance" icon={Wrench}>
          <MaintenanceCoverage maintenance={detail.maintenance} now={now} />
        </Section>
      </div>

      <Section title="Browse sessions" icon={TerminalSquare}>
        {detail.sessions.length === 0 ? (
          <p className="page__section-note">
            No browse session is running. Start one from a snapshot to list and download its files.
          </p>
        ) : (
          <ul className="ref-list ref-list--stacked" aria-label="Browse sessions">
            {detail.sessions.map((session) => (
              <li key={`${session.namespace}/${session.job}`}>
                <span className="label-strip">
                  <span className="label-strip__kind">{session.namespace}</span>
                  <span className="label-strip__name">{session.job}</span>
                </span>
                <span className="page__section-note">
                  {session.expiresAt !== null && session.expiresAt !== undefined
                    ? `reaped ${relativeTime(session.expiresAt, now)}`
                    : "no expiry reported"}
                </span>
                {renderSessionAction?.(session)}
              </li>
            ))}
          </ul>
        )}
      </Section>

      <Section title="Conditions" icon={ScrollText}>
        <Conditions conditions={detail.conditions} now={now} />
      </Section>
    </div>
  );
}

interface SectionProps {
  title: string;
  icon: typeof Database;
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

function MaintenanceCoverage({
  maintenance,
  now,
}: {
  maintenance: MaintenanceRow | null | undefined;
  now: Date;
}) {
  if (maintenance === null || maintenance === undefined) {
    return (
      <p className="page__section-note">
        No Maintenance resource governs this repository, so it grows without bound. Check that{" "}
        <span className="mono">spec.maintenance</span> is not disabled.
      </p>
    );
  }
  const manual = maintenance.manualRun;
  return (
    <>
      <Facts
        label="Maintenance"
        facts={[
          {
            term: "Resource",
            value: (
              <span className="mono">
                {maintenance.namespace}/{maintenance.name}
              </span>
            ),
          },
          {
            term: "Authored by",
            value: maintenance.managedByRepository
              ? "the operator, from the repository's spec.maintenance"
              : "a user",
          },
          ...trackFacts("Quick", maintenance.quick, now, "maintenanceQuickNextRun"),
          ...trackFacts("Full", maintenance.full, now, "maintenanceFullNextRun"),
          ...(manual !== null && manual !== undefined
            ? [
                {
                  term: "Manual run",
                  value: `${manual.mode ?? "run"} ${manual.phase ?? "requested"}${
                    manual.requestedAt !== null && manual.requestedAt !== undefined
                      ? `, asked for ${relativeTime(manual.requestedAt, now)}`
                      : ""
                  }`,
                },
              ]
            : []),
        ]}
      />
    </>
  );
}

/** One maintenance track's three facts. */
function trackFacts(
  label: string,
  track: RunStatusView,
  now: Date,
  nextField: "maintenanceQuickNextRun" | "maintenanceFullNextRun",
): Fact[] {
  return [
    { term: `${label} last run`, value: instant(track.lastRunAt, now) },
    {
      term: `${label} next run`,
      value:
        track.nextScheduledAt !== null && track.nextScheduledAt !== undefined ? (
          instant(track.nextScheduledAt, now)
        ) : (
          // Never written by any controller — see `unwired.tsx`.
          <NotReported field={nextField} />
        ),
    },
    {
      term: `${label} failures since success`,
      value: String(track.consecutiveFailures),
    },
    ...(track.lastContentReclaimedBytes !== null && track.lastContentReclaimedBytes !== undefined
      ? [{ term: `${label} reclaimed`, value: humanBytes(track.lastContentReclaimedBytes) }]
      : []),
  ];
}

function Conditions({ conditions, now }: { conditions: readonly ConditionView[]; now: Date }) {
  if (conditions.length === 0) {
    return (
      <p className="page__section-note">
        No conditions yet: the operator has not reconciled this repository.
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

/** A count, where zero is a measurement and absent is not one. */
function count(value: number | null | undefined): string {
  return value === null || value === undefined ? EMPTY_CELL : String(value);
}

/** The discovered count, qualified when the scan saw only part of the repository. */
function discoveredWithCoverage(catalog: CatalogView): string {
  const base = count(catalog.discoveredBackupCount);
  const note = catalogCoverageNote(catalog.coverage);
  return note === null || base === EMPTY_CELL ? base : `${base} (${note})`;
}

/** An instant with a direction, or the empty cell. */
function instant(at: string | null | undefined, now: Date): ReactNode {
  if (at === null || at === undefined || at.length === 0) {
    return EMPTY_CELL;
  }
  return <time dateTime={at}>{relativeTime(at, now)}</time>;
}
