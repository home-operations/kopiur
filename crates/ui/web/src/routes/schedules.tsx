import { createFileRoute } from "@tanstack/react-router";
import { CalendarClock, PauseCircle, PlayCircle } from "lucide-react";
import { useState } from "react";

import { useSchedules } from "../api/hooks";
import type { ScheduleRow } from "../api/types";
import { ActionButton } from "../components/ActionButton";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { ScheduleTable } from "../components/ScheduleTable";
import { SuspendToggle } from "../components/actions/SuspendToggle";
import { useRefusal } from "../components/actions/reason";
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
 * suspended.
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
                <SuspendTrigger
                  schedule={schedule}
                  open={open === rowId(schedule)}
                  onOpen={(next) => {
                    setOpen(next ? rowId(schedule) : null);
                  }}
                />
              )}
            />
            <Confirmation schedules={schedules.data} open={open} onOpenChange={setOpen} />
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

/**
 * The trigger that lives in a row's Action cell.
 *
 * Bare, because the confirmation cannot live here: a cell is a column, and a
 * column turns a paragraph into a tall thin ribbon that shoves every other
 * column narrow. It opens the one panel below the ledger instead.
 *
 * The refusal is judged in this row's own namespace, and shown at both
 * lengths — a ledger suppresses `ActionButton`'s floating tooltip, so the
 * short word in the cell is the only reason a sighted mouse user can reach.
 */
function SuspendTrigger({
  schedule,
  open,
  onOpen,
}: {
  schedule: ScheduleRow;
  open: boolean;
  onOpen: (open: boolean) => void;
}) {
  const refusal = useRefusal(schedule.namespace, "patchSchedules");
  const label = schedule.suspended ? "Resume" : "Suspend";
  return (
    <div className="action">
      <ActionButton
        variant={schedule.suspended ? "default" : "danger"}
        disabledReason={refusal?.full}
        reasonShown={false}
        title={refusal?.full}
        aria-expanded={open}
        onClick={() => {
          onOpen(!open);
        }}
      >
        {schedule.suspended ? (
          <PlayCircle size={14} strokeWidth={2} aria-hidden="true" />
        ) : (
          <PauseCircle size={14} strokeWidth={2} aria-hidden="true" />
        )}
        {label}
      </ActionButton>
      {refusal !== undefined ? <span className="action__short">{refusal.short}</span> : null}
    </div>
  );
}

/**
 * The open row's confirmation and receipt, at the page's full width.
 *
 * One at a time by construction: there is one of these, and `open` names the
 * row it belongs to. A row that leaves the list while its panel is open takes
 * the panel with it.
 */
function Confirmation({
  schedules,
  open,
  onOpenChange,
}: {
  schedules: readonly ScheduleRow[];
  open: OpenRow;
  onOpenChange: (open: OpenRow) => void;
}) {
  const schedule = schedules.find((row) => rowId(row) === open);
  if (schedule === undefined) {
    return null;
  }
  return (
    <SuspendToggle
      kind="schedule"
      name={schedule.name}
      namespace={schedule.namespace}
      suspended={schedule.suspended}
      consequence="this cron is not evaluated at all, so the policy it fires goes unrun"
      hideTrigger
      open
      onOpenChange={(next) => {
        onOpenChange(next ? rowId(schedule) : null);
      }}
    />
  );
}
