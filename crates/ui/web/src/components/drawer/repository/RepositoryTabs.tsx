import { Database, FolderSearch, HeartPulse, Server, Sprout, TerminalSquare } from "lucide-react";

import type {
  CatalogView,
  MaintenanceRow,
  RepositoryDetail,
  RunStatusView,
} from "../../../api/types";
import { EMPTY_CELL, humanBytes, relativeTime } from "../../../util/format";
import { admitsText } from "../../admits";
import { type Fact, Facts } from "../../Facts";
import { NotReported } from "../../NotReported";
import { accessLabel, catalogCoverageNote } from "../../repository";
import { LastObserved } from "../../RepositoryTable";
import { DrawerSection, Instant } from "../DrawerSection";
import { StopSession } from "./StopSession";

/** A count, where zero is a measurement and absent is not one. */
function count(value: number | null | undefined): string {
  return value === null || value === undefined ? EMPTY_CELL : String(value);
}

export function StorageTab({ detail, now }: { detail: RepositoryDetail; now: Date }) {
  const { summary } = detail;
  return (
    <>
      <DrawerSection title="Storage" icon={Database}>
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
      </DrawerSection>

      <DrawerSection title="Access" icon={Server}>
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
      </DrawerSection>

      {detail.seed !== null && detail.seed !== undefined ? (
        <DrawerSection title="Seed" icon={Sprout}>
          <Facts
            label="Seed"
            facts={[
              { term: "Mode", value: detail.seed.mode ?? EMPTY_CELL },
              {
                term: "Seeded from",
                value: <span className="mono">{detail.seed.source ?? EMPTY_CELL}</span>,
              },
              { term: "Completed", value: <Instant at={detail.seed.seededAt} now={now} /> },
              { term: "Snapshots copied", value: count(detail.seed.snapshotsCopied) },
            ]}
          />
        </DrawerSection>
      ) : null}
    </>
  );
}

/** The discovered count, qualified when the scan saw only part of the repository. */
function discoveredWithCoverage(catalog: CatalogView): string {
  const base = count(catalog.discoveredBackupCount);
  const note = catalogCoverageNote(catalog.coverage);
  return note === null || base === EMPTY_CELL ? base : `${base} (${note})`;
}

export function CatalogTab({ detail, now }: { detail: RepositoryDetail; now: Date }) {
  return (
    <>
      <DrawerSection title="Catalog" icon={FolderSearch}>
        {detail.catalog === null || detail.catalog === undefined ? (
          <p className="page__section-note">
            No catalog scan has been recorded. A scan lists backups already in the repository, such
            as ones from another cluster.
          </p>
        ) : (
          <Facts
            label="Catalog"
            facts={[
              { term: "Discovered backups", value: discoveredWithCoverage(detail.catalog) },
              { term: "Foreign snapshots", value: count(detail.catalog.foreignSnapshotCount) },
              { term: "Last scan", value: <Instant at={detail.catalog.lastRefreshAt} now={now} /> },
            ]}
          />
        )}
      </DrawerSection>

      <DrawerSection title="Health probe" icon={HeartPulse}>
        {detail.health === null || detail.health === undefined ? (
          <p className="page__section-note">
            No probe result has been recorded. The probe flags an unreachable backend before a
            backup fails on it.
          </p>
        ) : (
          <Facts
            label="Health probe"
            facts={[
              { term: "Last probe", value: <Instant at={detail.health.lastProbeAt} now={now} /> },
              {
                term: "Last healthy",
                value: <Instant at={detail.health.lastHealthyAt} now={now} />,
              },
              {
                term: "Failures since success",
                value: count(detail.health.consecutiveProbeFailures),
              },
            ]}
          />
        )}
      </DrawerSection>
    </>
  );
}

export function SessionsTab({ detail, now }: { detail: RepositoryDetail; now: Date }) {
  const { summary } = detail;
  return (
    <DrawerSection title="Browse sessions" icon={TerminalSquare}>
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
              <StopSession
                session={session}
                kindPath={summary.kindPath}
                name={summary.name}
                namespace={summary.namespace ?? undefined}
              />
            </li>
          ))}
        </ul>
      )}
    </DrawerSection>
  );
}

export function MaintenanceCoverage({
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
  );
}

/** One maintenance track's facts. */
function trackFacts(
  label: string,
  track: RunStatusView,
  now: Date,
  nextField: "maintenanceQuickNextRun" | "maintenanceFullNextRun",
): Fact[] {
  return [
    { term: `${label} last run`, value: <Instant at={track.lastRunAt} now={now} /> },
    {
      term: `${label} next run`,
      value:
        track.nextScheduledAt !== null && track.nextScheduledAt !== undefined ? (
          <Instant at={track.nextScheduledAt} now={now} />
        ) : (
          // Never written by any controller — see `unwired.tsx`.
          <NotReported field={nextField} />
        ),
    },
    { term: `${label} failures since success`, value: String(track.consecutiveFailures) },
    ...(track.lastContentReclaimedBytes !== null && track.lastContentReclaimedBytes !== undefined
      ? [{ term: `${label} reclaimed`, value: humanBytes(track.lastContentReclaimedBytes) }]
      : []),
  ];
}
