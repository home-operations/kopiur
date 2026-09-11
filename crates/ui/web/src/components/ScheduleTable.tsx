import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";

import type { ScheduleRow } from "../api/types";
import { relativeTime } from "../util/format";
import { LampBadge } from "./HealthBadge";
import { healthLamp } from "./health";
import { firesBySelector, scheduleCron, scheduleFires } from "./schedule";

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
    <div className="ledger-scroll">
      <table className="ledger schedule-table" aria-label="Schedules">
        <thead>
          <tr>
            <th scope="col">Schedule</th>
            <th scope="col">Cron</th>
            <th scope="col">Fires</th>
            <th scope="col">State</th>
            <th scope="col" className="num">
              Last fire
            </th>
            <th scope="col" className="num">
              Next fire
            </th>
            {renderAction !== undefined ? <th scope="col">Action</th> : null}
          </tr>
        </thead>
        <tbody>
          {schedules.map((schedule) => (
            <tr key={`${schedule.namespace}/${schedule.name}`}>
              <td>
                <div className="schedule-table__object">
                  <span className="label-strip">
                    <span className="label-strip__kind">SnapshotSchedule</span>
                    <span className="label-strip__name">{schedule.name}</span>
                  </span>
                  <span className="schedule-table__namespace mono">{schedule.namespace}</span>
                </div>
              </td>
              <td className="mono schedule-table__cron">{scheduleCron(schedule)}</td>
              <td>
                <Fires schedule={schedule} />
              </td>
              <td>
                <State schedule={schedule} />
              </td>
              <td className="num">
                <Fire at={schedule.lastFire} now={now} never="never fired" />
              </td>
              <td className="num">
                <Fire at={schedule.nextFire} now={now} never="none computed" />
              </td>
              {renderAction !== undefined ? (
                <td className="schedule-table__action">{renderAction(schedule)}</td>
              ) : null}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
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
      <span className="label-strip">
        <span className="label-strip__kind">SnapshotPolicy</span>
        <span className="label-strip__name">
          <Link
            to="/policies/$namespace/$name"
            params={{ namespace: schedule.namespace, name: named }}
          >
            {named}
          </Link>
        </span>
      </span>
    </div>
  );
}

/** Suspended lamp, a loud failure run, or the quiet word for "it is firing". */
function State({ schedule }: { schedule: ScheduleRow }) {
  if (schedule.suspended) {
    return <LampBadge lamp={healthLamp("suspended")} />;
  }
  if (schedule.consecutiveFailures > 0) {
    const runs = schedule.consecutiveFailures === 1 ? "run" : "runs";
    return (
      <span className="schedule-table__never">
        {schedule.consecutiveFailures} failed {runs}
      </span>
    );
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
