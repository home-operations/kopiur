import { Link } from "@tanstack/react-router";
import {
  ArrowLeftRight,
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
  ConditionView,
  MaintenanceRow,
  RepositoryDetail as RepositoryDetailData,
  RunStatusView,
  SessionInfo,
} from "../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../util/format";
import { Facts, type Fact } from "./Facts";
import { Finding } from "./Finding";
import { LastObserved } from "./RepositoryTable";
import { gateSeverityLamp } from "./gates";
import { accessLabel, repositoryVerdict } from "./repository";
import { NotReported } from "./NotReported";

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
}

export function RepositoryDetail({
  detail,
  actions,
  renderSessionAction,
  now = new Date(),
}: RepositoryDetailProps) {
  const { summary } = detail;
  const verdict = repositoryVerdict(summary);
  const Lamp = verdict.lamp.icon;
  const namespaceScope = summary.namespace ?? undefined;
  const replicationSearch = namespaceScope !== undefined ? { namespace: namespaceScope } : {};

  return (
    <div className="page">
      <p className="verdict" role="status" aria-label="Repository verdict">
        <span className="verdict__lamp" data-health={verdict.lamp.key}>
          <Lamp size={18} strokeWidth={2} aria-hidden="true" />
          <span>{verdict.lamp.word}</span>
        </span>
        <span className="verdict__text">{verdict.text}</span>
        <span className="verdict__meta mono">
          {summary.kind}
          {summary.namespace !== null && summary.namespace !== undefined
            ? ` · ${summary.namespace}`
            : ""}{" "}
          · {summary.name}
        </span>
      </p>

      {actions !== undefined && actions !== null ? (
        <section className="page__section" aria-label="Actions">
          {actions}
        </section>
      ) : null}

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
              ...(summary.allowedNamespaceCount !== null &&
              summary.allowedNamespaceCount !== undefined
                ? [
                    {
                      term: "Namespaces admitted",
                      value: String(summary.allowedNamespaceCount),
                    },
                  ]
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
              No catalog scan has been recorded. A scan walks the kopia repository and adopts the
              snapshots it finds as <span className="mono">Snapshot</span> resources, so backups
              taken before kopiur — or by another cluster — become visible here.
            </p>
          ) : (
            <Facts
              label="Catalog"
              facts={[
                { term: "Discovered backups", value: count(detail.catalog.discoveredBackupCount) },
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
              No probe result has been recorded. The probe is what turns an unreachable backend into
              a Degraded repository before a backup fails on it.
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
              No repository server is running for this repository, so every mover connects to the
              storage backend itself with the repository&apos;s own credentials.
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

      <div className="page__pair">
        <Section title="Policies writing here" icon={ScrollText}>
          {detail.policies.length === 0 ? (
            <p className="page__section-note">
              No SnapshotPolicy names this repository, so nothing is scheduled to write into it.
              Snapshots may still exist here from before, or from another cluster.
            </p>
          ) : (
            <ul className="ref-list" aria-label="Policies writing here">
              {detail.policies.map((policy) => (
                <li key={`${policy.namespace}/${policy.name}`} className="label-strip">
                  <span className="label-strip__kind">{policy.namespace}</span>
                  <span className="label-strip__name">{policy.name}</span>
                </li>
              ))}
            </ul>
          )}
        </Section>

        <Section title="Replication" icon={ArrowLeftRight}>
          <Facts
            label="Replication"
            facts={[
              { term: "Copies out of here", value: refs(detail.replicationsOut) },
              { term: "Copies into here", value: refs(detail.replicationsIn) },
            ]}
          />
          <p className="page__section-note">
            <Link to="/replications" search={replicationSearch}>
              Replications
            </Link>{" "}
            shows each one&apos;s schedule, phase and lag.
          </p>
        </Section>
      </div>

      <Section title="Browse sessions" icon={TerminalSquare}>
        {detail.sessions.length === 0 ? (
          <p className="page__section-note">
            No browse session is running. A session is a mover pod that mounts this
            repository&apos;s credentials so a snapshot&apos;s files can be listed and downloaded;
            it is started from a snapshot and reaped when it goes idle.
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
        No Maintenance resource governs this repository. Without one, kopia&apos;s indexes are never
        compacted and deleted content is never dropped, so the repository grows without bound. A
        repository&apos;s <span className="mono">spec.maintenance</span> projects one by default;
        this repository either disabled it or has not been reconciled.
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
              : "a user — the operator honors it and never rewrites it",
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
        The operator has written no conditions on this repository yet, which means it has not been
        reconciled — not that it is healthy.
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

/** An instant with a direction, or the empty cell. */
function instant(at: string | null | undefined, now: Date): ReactNode {
  if (at === null || at === undefined || at.length === 0) {
    return EMPTY_CELL;
  }
  return <time dateTime={at}>{relativeTime(at, now)}</time>;
}

/** A list of referenced names, or the empty cell. */
function refs(names: readonly string[]): ReactNode {
  if (names.length === 0) {
    return "none";
  }
  return <span className="mono">{names.join(", ")}</span>;
}
