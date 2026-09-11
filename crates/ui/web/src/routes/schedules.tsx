import { createFileRoute } from "@tanstack/react-router";
import { CalendarClock } from "lucide-react";
import { useState } from "react";

import { useSchedules } from "../api/hooks";
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
        A <span className="mono">SnapshotSchedule</span> is the clock: a cron, a timezone, and the
        policy it fires — named directly, or selected by label. The cron is shown exactly as
        written, <span className="mono">H</span> jitter token and all, because the slot{" "}
        <span className="mono">H</span> resolves to is the operator&apos;s and rewriting it here
        would show you a time you cannot find in your own manifest.
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
            when someone asks it to from the policy&apos;s own page — which is a perfectly good way
            to run a backup once, and a bad way to run one nightly.
          </EmptyState>
        ) : (
          <ScheduleTable
            schedules={schedules.data}
            renderAction={(schedule) => {
              const id = `${schedule.namespace}/${schedule.name}`;
              return (
                <SuspendToggle
                  kind="schedule"
                  name={schedule.name}
                  namespace={schedule.namespace}
                  suspended={schedule.suspended}
                  consequence="this cron is not evaluated at all, so the policy it fires goes unrun"
                  // A ledger suppresses the floating reason tooltip, so a
                  // refused control says so in the cell as well.
                  inLedger
                  open={open === id}
                  onOpenChange={(next) => {
                    setOpen(next ? id : null);
                  }}
                />
              );
            }}
          />
        )}
      </section>

      <p className="page__prose">
        Suspension lives at <span className="mono">spec.schedule.suspend</span> on a schedule, not
        at <span className="mono">spec.suspend</span> as it does on every other kind — so a
        suspended schedule is the clock stopping, while a suspended{" "}
        <span className="mono">SnapshotPolicy</span> is the recipe itself being held. Neither is
        caught up afterwards: a window that passes while suspended is gone.
      </p>
    </div>
  );
}
