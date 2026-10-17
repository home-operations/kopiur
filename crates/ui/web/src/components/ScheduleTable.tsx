import type { ReactNode } from "react";

import type { ScheduleRow } from "../api/types";
import { relativeTime } from "../util/format";
import { ColumnLedger } from "./ColumnLedger";
import { LampBadge } from "./HealthBadge";
import { healthLamp, healthRank, loudLamp } from "./health";
import { firesBySelector, scheduleCron, scheduleFires } from "./schedule";
import { KindChip } from "./KindMark";
import { ObjectRef } from "./ObjectRef";
import { InspectLink } from "./InspectLink";
import { type SortValue, nameValue, timeValue } from "./tableColumns";
import { SCHEDULE_COLUMNS, SCHEDULE_COLUMNS_WITH_ACTION, type ScheduleColumn } from "./listColumns";

/**
 * Every `SnapshotSchedule` in scope: the cron, what it fires, when it last
 * fired and when it fires next.
 *
 * As on the policies ledger there is no health column, because a schedule
 * publishes none. What it does publish is a run of failures, and that number
 * is rendered loudly rather than turned into a lamp — the count is the fact,
 * and a lamp would be this bundle inventing a verdict.
 *
 * Presentation only: the suspend control is built by the route, which owns
 * the hooks, so this table renders with no query client.
 */
export interface ScheduleTableProps {
  schedules: readonly ScheduleRow[];
  /** The suspend/resume control for one row, built by the route. */
  renderAction?: ((schedule: ScheduleRow) => ReactNode) | undefined;
  now?: Date | undefined;
}

export function ScheduleTable({ schedules, renderAction, now = new Date() }: ScheduleTableProps) {
  return (
    <ColumnLedger
      id="schedules"
      rowLinks
      label="Schedules"
      className="schedule-table"
      columns={renderAction === undefined ? SCHEDULE_COLUMNS : SCHEDULE_COLUMNS_WITH_ACTION}
      rows={schedules}
      rowKey={(s) => `${s.namespace}/${s.name}`}
      rowProps={() => ({ "data-kind": "snapshot-schedule" })}
      cell={(schedule, id) => scheduleCell(schedule, id, now, renderAction)}
      sortValue={scheduleSortValue}
    />
  );
}

function scheduleSortValue(schedule: ScheduleRow, id: ScheduleColumn): SortValue {
  switch (id) {
    case "schedule":
      return nameValue(schedule.name, schedule.namespace);
    case "state":
      // The State cell's own reading: suspended, failing, or active.
      return healthRank(
        schedule.suspended ? "suspended" : schedule.consecutiveFailures > 0 ? "failed" : "healthy",
      );
    case "lastFire":
      return timeValue(schedule.lastFire);
    case "nextFire":
      return timeValue(schedule.nextFire);
    case "cron":
    case "fires":
    case "action":
      return null;
    default:
      return id satisfies never;
  }
}

function scheduleCell(
  schedule: ScheduleRow,
  id: ScheduleColumn,
  now: Date,
  renderAction: ScheduleTableProps["renderAction"],
): ReactNode {
  switch (id) {
    case "schedule":
      return (
        <div className="table__object">
          <KindChip kind="snapshotSchedule" size="sm" />
          <div className="schedule-table__object">
            <span className="label-strip">
              <span className="label-strip__name">
                <InspectLink
                  className="row-link"
                  target={{
                    kind: "snapshotSchedule",
                    namespace: schedule.namespace,
                    name: schedule.name,
                  }}
                >
                  {schedule.name}
                </InspectLink>
              </span>
            </span>
            <span className="schedule-table__namespace mono">{schedule.namespace}</span>
          </div>
        </div>
      );
    case "cron":
      return scheduleCron(schedule);
    case "fires":
      return <Fires schedule={schedule} />;
    case "state":
      return <State schedule={schedule} />;
    case "lastFire":
      return <Fire at={schedule.lastFire} now={now} never="never fired" />;
    case "nextFire":
      return <Fire at={schedule.nextFire} now={now} never="none computed" />;
    case "action":
      return renderAction?.(schedule) ?? null;
    default:
      return id satisfies never;
  }
}

/**
 * What the schedule fires, and how it picked it.
 *
 * A selector is marked as one, because the two are answered differently: a
 * name is a name, while a selector's membership changes whenever a policy's
 * labels do — and a policy that drops the label stops being backed up with no
 * change to the schedule at all.
 */
function Fires({ schedule }: { schedule: ScheduleRow }) {
  const bySelector = firesBySelector(schedule);
  const named = schedule.policy;
  const fires = scheduleFires(schedule);
  if (named === null || named === undefined || named.length === 0) {
    if (!bySelector) {
      return <span className="schedule-table__never">{fires}</span>;
    }
    return (
      <div className="schedule-table__fires">
        <span className="mono">{fires}</span>
        <span className="schedule-table__note">
          by selector — membership follows the policies&apos; labels
        </span>
      </div>
    );
  }
  return (
    <div className="schedule-table__fires">
      <ObjectRef
        kind="snapshotPolicy"
        name={named}
        namespace={schedule.namespace}
        contextNamespace={schedule.namespace}
      />
    </div>
  );
}

/**
 * Suspended lamp, a loud failure run, or the quiet word for "it is firing".
 *
 * All three are one column, so all three are read the same way: two lamps and
 * one deliberately quiet word. The failure run used to be the failed ink on
 * bare text while the row beside it carried a suspended *lamp* — one column,
 * two encodings, and the louder of the two was the one a colour-blind reader
 * could not pick out. It keeps its own count-and-noun (`loudLamp`), because a
 * schedule publishes no health for this bundle to name.
 */
function State({ schedule }: { schedule: ScheduleRow }) {
  if (schedule.suspended) {
    return <LampBadge lamp={healthLamp("suspended")} />;
  }
  if (schedule.consecutiveFailures > 0) {
    const runs = schedule.consecutiveFailures === 1 ? "run" : "runs";
    return <LampBadge lamp={loudLamp(`${schedule.consecutiveFailures} failed ${runs}`)} />;
  }
  return <span className="schedule-table__active">Active</span>;
}

/**
 * A fire time, or the word for its absence.
 *
 * "Never fired" and "no next fire computed" are different absences and are
 * worded differently: the first is a schedule that has never done its job,
 * the second usually a suspended one the operator has stopped computing slots
 * for.
 */
function Fire({ at, now, never }: { at: string | null | undefined; now: Date; never: string }) {
  if (at === null || at === undefined || at.length === 0) {
    return <span className="schedule-table__absent">{never}</span>;
  }
  return (
    <time dateTime={at} title={at}>
      {relativeTime(at, now)}
    </time>
  );
}
