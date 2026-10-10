import { createFileRoute } from "@tanstack/react-router";
import { CalendarClock } from "lucide-react";
import { useState } from "react";

import { useSchedules, useSuspend } from "../api/hooks";
import type { ScheduleRow, SuspendBody } from "../api/types";
import { ActionResult } from "../components/ActionResult";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { ScheduleTable } from "../components/ScheduleTable";
import { SuspendToggle } from "../components/actions/SuspendToggle";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Schedules — every `SnapshotSchedule`, with the one control an operator
 * reaches for at 3am: stop this thing firing.
 *
 * The suspend control is per row and judged per row. `/me`'s capability flags
 * are answers *for a namespace* (addenda item 17), so a schedule in `media`
 * and one in `prod` are two different questions, and a user holding a single
 * namespaced RoleBinding sees their own row enabled and the other refused
 * with the reason — rather than everything disabled, which is what a
 * page-scoped review would produce.
 *
 * One confirmation is open at a time, across the whole ledger. Two open
 * questions in a table of near-identical rows is how the wrong schedule gets
 * suspended. It opens in a popover pinned to the row's button (the table's
 * card clips, so it is pinned on screen rather than hung inside it), and the
 * answer appears once, under the table, at full width.
 */
export const Route = createFileRoute("/schedules")({
  component: Schedules,
});

/** `namespace/name` of the row whose confirmation is open. */
type OpenRow = string | null;

function Schedules() {
  const namespace = useCurrentNamespace();
  const schedules = useSchedules(namespace);
  const [open, setOpen] = useState<OpenRow>(null);
  // One request for the whole ledger, so its receipt sits once under the
  // table at full width rather than squeezed into a row's cell.
  const suspend = useSuspend();
  const scope = namespace ?? "all namespaces";

  return (
    <div className="page">
      <p className="page__prose">
        A <span className="mono">SnapshotSchedule</span> runs a policy on a cron, in a timezone. The
        cron is shown exactly as written, including any <span className="mono">H</span> jitter
        token.
      </p>

      <section className="page__section" aria-label="Schedules">
        {schedules.isPending ? (
          <LoadingState what="schedules" rows={6} />
        ) : schedules.isError ? (
          <ErrorState
            problem={schedules.error.problem}
            what="schedules"
            onRetry={() => void schedules.refetch()}
          />
        ) : schedules.data.length === 0 ? (
          <EmptyState title={`No schedules in ${scope}`} icon={CalendarClock}>
            A SnapshotSchedule fires a SnapshotPolicy on a cron. Without one, a policy only runs
            when started from its own page.
          </EmptyState>
        ) : (
          <>
            <ScheduleTable
              schedules={schedules.data}
              renderAction={(schedule) => (
                <SuspendToggle
                  kind="schedule"
                  name={schedule.name}
                  namespace={schedule.namespace}
                  suspended={schedule.suspended}
                  consequence="this cron is not evaluated at all, so the policy it fires goes unrun"
                  inLedger
                  mutation={suspend}
                  showResult={false}
                  align="end"
                  strategy="fixed"
                  open={open === rowId(schedule)}
                  onOpenChange={(next) => {
                    setOpen((current) =>
                      next ? rowId(schedule) : current === rowId(schedule) ? null : current,
                    );
                  }}
                />
              )}
            />
            <ActionResult
              label={resultLabel(suspend.variables)}
              receipt={suspend.data}
              problem={suspend.error?.problem}
            />
          </>
        )}
      </section>

      <p className="page__prose">
        A schedule is suspended with <span className="mono">spec.schedule.suspend</span>, not{" "}
        <span className="mono">spec.suspend</span>. Runs missed while suspended are not made up.
      </p>
    </div>
  );
}

/** The row's stable key, and the value `open` holds while its panel is up. */
function rowId(schedule: ScheduleRow): string {
  return `${schedule.namespace}/${schedule.name}`;
}

/** What the receipt under the ledger answers: the last row asked about. */
function resultLabel(body: SuspendBody | undefined): string {
  if (body === undefined) return "Suspend";
  return `${body.suspend ? "Suspend" : "Resume"} ${body.name}`;
}
