import { Wrench } from "lucide-react";
import type { ReactNode } from "react";

import type { MaintenanceRow, RunStatusView } from "../api/types";
import { humanBytes, relativeTime } from "../util/format";
import { Facts, type Fact } from "./Facts";
import { healthLamp, loudLamp } from "./health";
import { LampBadge } from "./HealthBadge";
import { NotReported } from "./NotReported";

/**
 * Every `Maintenance` in scope, one region each: the two run tracks, any
 * manual run, and the control that asks for another.
 *
 * # Why regions rather than one ledger
 *
 * A `Maintenance` is two tracks and a pending manual run, which is four to
 * six values per object — a table would need either six columns of mostly
 * absent cells or a nested row, and the console's `Facts` unit is exactly the
 * shape for "a handful of named values". There is one of these per repository,
 * so the page stays short by construction.
 *
 * # Managed and hand-authored look identical, and the difference matters
 *
 * kopiur projects a `Maintenance` from a repository's `spec.maintenance`, and
 * honors a hand-authored one it finds instead. Their specs are
 * indistinguishable — the difference is a controller `ownerReference` — so
 * `managedByRepository` is the field that tells a reader whether editing this
 * object will stick or be reconciled away, and it is stated on every one.
 *
 * Presentation only: the run control is built by the route, which owns the
 * hooks.
 */
export interface MaintenanceListProps {
  rows: readonly MaintenanceRow[];
  /** The "run now" control for one row, built by the route. */
  renderAction?: ((row: MaintenanceRow) => ReactNode) | undefined;
  now?: Date | undefined;
}

export function MaintenanceList({ rows, renderAction, now = new Date() }: MaintenanceListProps) {
  return (
    <>
      {rows.map((row) => (
        <section
          className="page__section"
          key={`${row.namespace}/${row.name}`}
          aria-label={`Maintenance ${row.namespace}/${row.name}`}
        >
          <div className="page__section-head">
            <h2>
              <Wrench size={16} strokeWidth={2} aria-hidden="true" />
              {row.repository}
            </h2>
            <span className="maintenance__resource mono">
              {row.namespace}/{row.name}
            </span>
          </div>

          <Facts
            label={`Maintenance ${row.name}`}
            facts={[
              {
                term: "Authored by",
                value: row.managedByRepository
                  ? `the operator, from ${row.owner ?? "the repository"}'s spec.maintenance — edits here are reconciled away`
                  : "a user — the operator honors it and never rewrites it",
              },
              ...trackFacts("Quick", row.quick, now, "maintenanceQuickNextRun"),
              ...trackFacts("Full", row.full, now, "maintenanceFullNextRun"),
              ...manualFacts(row, now),
            ]}
          />

          {renderAction !== undefined ? (
            <div className="action-bar">{renderAction(row)}</div>
          ) : null}
        </section>
      ))}
    </>
  );
}

/**
 * One track's facts.
 *
 * The next run is always "not reported": `Maintenance.status.{quick,full}
 * .nextScheduledAt` is declared on the CRD and written by no controller
 * (`components/unwired.ts`, from the wiring ratchet's own list). A blank would
 * read as "nothing to say" and a dash as "not applicable"; neither is true, so
 * the absence is written as one. The value is read first and only falls back,
 * so the day a controller writes it the number simply appears.
 */
function trackFacts(
  label: string,
  track: RunStatusView,
  now: Date,
  nextField: "maintenanceQuickNextRun" | "maintenanceFullNextRun",
): Fact[] {
  return [
    {
      term: `${label} last run`,
      value:
        track.lastRunAt !== null && track.lastRunAt !== undefined ? (
          <time dateTime={track.lastRunAt} title={track.lastRunAt}>
            {relativeTime(track.lastRunAt, now)}
          </time>
        ) : (
          <LampBadge lamp={loudLamp("never run")} />
        ),
    },
    {
      term: `${label} next run`,
      value:
        track.nextScheduledAt !== null && track.nextScheduledAt !== undefined ? (
          <time dateTime={track.nextScheduledAt}>{relativeTime(track.nextScheduledAt, now)}</time>
        ) : (
          <NotReported field={nextField} />
        ),
    },
    {
      term: `${label} failures since success`,
      // The one place a bare number carried the alarm: a red "2" beside a
      // plain "0" differed by hue and nothing else, and with the icon
      // `aria-hidden` a screen reader heard only the digit. This is the case
      // `LampBadge`'s `label` was written for — the count stays the visible
      // word, and the lamp's own word is spoken after it.
      value:
        track.consecutiveFailures > 0 ? (
          <LampBadge lamp={healthLamp("failed")} label={String(track.consecutiveFailures)} />
        ) : (
          "0"
        ),
    },
    ...(track.lastContentReclaimedBytes !== null && track.lastContentReclaimedBytes !== undefined
      ? [
          {
            term: `${label} reclaimed`,
            value: humanBytes(track.lastContentReclaimedBytes),
          },
        ]
      : []),
  ];
}

/**
 * The manual run, when there is one.
 *
 * `requestedAt` is the token the operator echoes back on the run it produced,
 * which is how a previous run's outcome is told apart from the one just asked
 * for — so it is shown rather than collapsed into "a run was requested".
 */
function manualFacts(row: MaintenanceRow, now: Date): Fact[] {
  const manual = row.manualRun;
  if (manual === null || manual === undefined) {
    return [];
  }
  return [
    {
      term: "Manual run",
      value: (
        <>
          <span className="mono">{manual.mode ?? "run"}</span> — {manual.phase ?? "requested"}
          {manual.requestedAt !== null && manual.requestedAt !== undefined ? (
            <>
              , asked for{" "}
              <time dateTime={manual.requestedAt} title={manual.requestedAt}>
                {relativeTime(manual.requestedAt, now)}
              </time>
            </>
          ) : null}
          {manual.completedAt !== null && manual.completedAt !== undefined ? (
            <>
              , finished{" "}
              <time dateTime={manual.completedAt}>{relativeTime(manual.completedAt, now)}</time>
            </>
          ) : null}
        </>
      ),
    },
  ];
}
