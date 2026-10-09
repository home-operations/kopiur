import { Link, createFileRoute } from "@tanstack/react-router";
import { Activity, Stethoscope, Wrench } from "lucide-react";
import type { ReactNode } from "react";

import {
  useDoctor,
  useMaintenance,
  useOverview,
  usePolicies,
  useReplications,
  useRepositories,
  useRestores,
  useSchedules,
  useSnapshots,
  useStatus,
} from "../api/hooks";
import { type StatusReportView, narrowStatusReport } from "../api/statusReport";
import type { KindTally, DoctorCheckView, RepositorySummary, StatusOverview } from "../api/types";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { ActivityList } from "../components/ActivityList";
import { SplitPane } from "../components/SplitPane";
import { activity } from "../components/activity";
import { AttentionList } from "../components/AttentionList";
import { attention } from "../components/attention";
import { OVERVIEW_DOCTOR_CHECKS, summarizeDoctor } from "../components/doctor";
import { countByHealth, healthLamp } from "../components/health";
import { overviewVerdict, tallyPhrases } from "../components/verdict";
import { relativeTime } from "../util/format";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Overview — the screen an operator opens first when they suspect something
 * is wrong. One sentence answers "is my data safe?"; under it, the fleet by
 * kind, then one list of what needs someone: each object once, whichever read
 * noticed it, with the fix on its plate (`components/attention.ts`).
 *
 * Three reads, each with its own loading / empty / error / not-permitted
 * state so one refused read does not blank the page:
 *
 * - `/repositories` for the health strip — the only place the typed `Health`
 *   lives, so the counts are the server's, not a phase table here.
 * - `/status` (`?namespace=` only, addenda item 12) for the CLI's own report:
 *   in-flight counts and the stalled objects. Its `report` is `unknown` on
 *   the wire by design and is narrowed defensively (addenda item 15).
 * - `/doctor` for the failing checks — the one source of `fix` text. It asks
 *   for `OVERVIEW_DOCTOR_CHECKS`, not the whole suite: this page is the one
 *   an operator opens most and it refetches on window focus, so it must not
 *   drag a dryRun admission create, a Secret read per credential reference
 *   and a cluster-wide Events list along with it.
 */
export const Route = createFileRoute("/")({
  component: Overview,
});

function Overview() {
  const namespace = useCurrentNamespace();
  const overview = useOverview(namespace);
  const repositories = useRepositories(namespace);
  const policies = usePolicies(namespace);
  const schedules = useSchedules(namespace);
  const maintenance = useMaintenance(namespace);
  const status = useStatus(namespace);
  const doctor = useDoctor({ namespace, checks: OVERVIEW_DOCTOR_CHECKS });
  const snapshots = useSnapshots({ namespace, limit: RECENT_SNAPSHOTS });
  const restores = useRestores(namespace);
  const replications = useReplications(namespace);

  const report = status.data !== undefined ? narrowStatusReport(status.data.report) : null;
  const now = status.data !== undefined ? new Date(status.data.now) : new Date();
  const search = namespace !== undefined ? { namespace } : {};
  const needs = attention({
    repositories: repositories.data,
    policies: policies.data,
    schedules: schedules.data,
    maintenance: maintenance.data,
    stalled: report?.stalled,
    checks: doctor.data?.checks,
  });

  return (
    <div className="page page--fill">
      <VerdictLine
        repositories={repositories.data}
        report={report}
        doctor={doctor.data?.checks}
        unavailable={[
          repositories.isError ? "the repositories" : null,
          status.isError ? "the status report" : null,
          doctor.isError ? "doctor" : null,
          overview.isError ? "the fleet overview" : null,
          policies.isError ? "the policies" : null,
          schedules.isError ? "the schedules" : null,
          maintenance.isError ? "maintenance" : null,
        ].filter((source): source is string => source !== null)}
        pending={
          repositories.isPending ||
          status.isPending ||
          doctor.isPending ||
          overview.isPending ||
          policies.isPending ||
          schedules.isPending ||
          maintenance.isPending
        }
        at={status.data?.now}
        tallies={overview.data?.kinds}
      />

      <SplitPane
        label="Needs attention and recent activity"
        storageKey="kopiur-ui.overview-split"
        start={
          <Section
            title="Needs attention"
            icon={Wrench}
            note={
              <Link to="/doctor" search={search}>
                full doctor report
              </Link>
            }
          >
            {status.isPending || doctor.isPending ? (
              <LoadingState what="what needs attention" rows={2} />
            ) : (
              <AttentionList attention={needs} namespace={namespace} now={now} />
            )}
            {repositories.isError ? (
              <ErrorState
                problem={repositories.error.problem}
                what="repositories"
                onRetry={() => void repositories.refetch()}
              />
            ) : null}
            {policies.isError ? (
              <ErrorState
                problem={policies.error.problem}
                what="policies"
                onRetry={() => void policies.refetch()}
              />
            ) : null}
            {schedules.isError ? (
              <ErrorState
                problem={schedules.error.problem}
                what="schedules"
                onRetry={() => void schedules.refetch()}
              />
            ) : null}
            {maintenance.isError ? (
              <ErrorState
                problem={maintenance.error.problem}
                what="maintenance"
                onRetry={() => void maintenance.refetch()}
              />
            ) : null}
            {status.isError ? (
              <ErrorState
                problem={status.error.problem}
                what="the status report"
                onRetry={() => void status.refetch()}
              />
            ) : null}
            {doctor.isError ? (
              <ErrorState
                problem={doctor.error.problem}
                what="the doctor report"
                onRetry={() => void doctor.refetch()}
              />
            ) : null}
            {overview.isError ? (
              <ErrorState
                problem={overview.error.problem}
                what="the fleet overview"
                onRetry={() => void overview.refetch()}
              />
            ) : null}
            {nothingNeedsYou({
              // Every read must have answered: an errored read is not pending,
              // and "none" from a refused read is not "none failing".
              settled:
                repositories.isSuccess &&
                policies.isSuccess &&
                schedules.isSuccess &&
                maintenance.isSuccess &&
                overview.isSuccess &&
                status.isSuccess &&
                doctor.isSuccess,
              fleetFailing: tallyPhrases(overview.data?.kinds ?? []).failed.length,
              needs: needs.items.length + needs.checks.length,
            }) ? (
              <EmptyState title="Nothing needs you" icon={Stethoscope}>
                No repository, policy, schedule or maintenance is failing, nothing is stalled, and
                the doctor checks found nothing to fix.
              </EmptyState>
            ) : null}
          </Section>
        }
        end={
          <Section title="Recent activity" icon={Activity}>
            <RecentActivity
              namespace={namespace}
              now={now}
              maintenance={maintenance}
              snapshots={snapshots}
              restores={restores}
              replications={replications}
            />
          </Section>
        }
      />

      {report !== null && !report.complete ? (
        <p className="page__prose">
          kopiur-ui could not read part of the status report; the rest is shown. The console and
          server may be different releases. <span className="mono">kubectl kopiur status</span>{" "}
          prints the full report.
        </p>
      ) : null}
    </div>
  );
}

function nothingNeedsYou(state: {
  settled: boolean;
  needs: number;
  fleetFailing: number;
}): boolean {
  return state.settled && state.needs === 0 && state.fleetFailing === 0;
}

interface SectionProps {
  title: string;
  icon: typeof Wrench;
  note?: ReactNode;
  children: ReactNode;
}

function Section({ title, icon: Icon, note, children }: SectionProps) {
  return (
    <section className="page__section page__card" aria-label={title}>
      <div className="page__section-head">
        <h2>
          <Icon size={16} strokeWidth={1.75} aria-hidden="true" />
          <span>{title}</span>
        </h2>
        {note !== undefined && note !== null ? (
          <span className="page__section-note">{note}</span>
        ) : null}
      </div>
      {children}
    </section>
  );
}

interface VerdictLineProps {
  repositories: RepositorySummary[] | undefined;
  report: StatusReportView | null;
  doctor: DoctorCheckView[] | undefined;
  unavailable: string[];
  pending: boolean;
  at: StatusOverview["now"] | undefined;
  tallies: readonly KindTally[] | undefined;
}

/**
 * The one sentence. Never green while anything is still loading or failed to.
 *
 * A named live region, not a heading: as an `h2` it sat in the outline as a
 * peer of "Repositories by health" and every other section, so a reader
 * navigating by heading heard four equal h2s and no answer. The page's answer
 * belongs under the shell's `h1`, and `role="status"` is also what announces
 * it when it changes.
 */
function VerdictLine({
  repositories,
  report,
  doctor,
  unavailable,
  pending,
  at,
  tallies,
}: VerdictLineProps) {
  if (pending && unavailable.length === 0) {
    const lamp = healthLamp("pending");
    const Icon = lamp.icon;
    return (
      <p className="verdict" role="status" aria-label="Vault verdict">
        <span className="verdict__lamp" data-health="pending">
          <Icon size={18} strokeWidth={2} aria-hidden="true" />
          <span>Checking</span>
        </span>
        <span className="verdict__text">Checking the vault…</span>
      </p>
    );
  }
  const verdict = overviewVerdict({
    repositories: countByHealth(repositories ?? []),
    stalled: report?.stalled.length ?? 0,
    doctor: summarizeDoctor(doctor ?? []),
    unavailable,
    tallies,
  });
  const lamp = healthLamp(verdict.health);
  const Icon = lamp.icon;
  return (
    <p className="verdict" role="status" aria-label="Vault verdict">
      <span className="verdict__lamp" data-health={lamp.key}>
        <Icon size={18} strokeWidth={2} aria-hidden="true" />
        <span>{lamp.word}</span>
      </span>
      <span className="verdict__text">{verdict.text}</span>
      {at !== undefined ? (
        <span className="verdict__meta">as of {relativeTime(at, new Date())}</span>
      ) : null}
    </p>
  );
}

/** The newest snapshots the timeline reads; the snapshot list pages the rest. */
const RECENT_SNAPSHOTS = 25;

interface RecentActivityProps {
  namespace: string | undefined;
  now: Date;
  snapshots: ReturnType<typeof useSnapshots>;
  restores: ReturnType<typeof useRestores>;
  replications: ReturnType<typeof useReplications>;
  maintenance: ReturnType<typeof useMaintenance>;
}

/**
 * Every run, newest first, from four reads. Each read keeps its own state: a
 * refused one is named where its runs would be, never silently missing from
 * a timeline that would then look complete.
 */
function RecentActivity({
  now,
  snapshots,
  restores,
  replications,
  maintenance,
}: RecentActivityProps) {
  const reads = [
    { read: snapshots, what: "snapshots" },
    { read: restores, what: "restores" },
    { read: replications, what: "replications" },
    { read: maintenance, what: "maintenance" },
  ] as const;
  if (reads.every(({ read }) => read.isPending)) {
    return <LoadingState what="recent activity" rows={4} />;
  }
  const items = activity({
    snapshots: snapshots.data?.items,
    restores: restores.data,
    replications: replications.data,
    maintenance: maintenance.data,
  });
  const settled = reads.every(({ read }) => read.isSuccess);
  return (
    <>
      {items.length > 0 ? <ActivityList items={items} now={now} /> : null}
      {reads.map(({ read, what }) =>
        read.isError ? (
          <ErrorState
            key={what}
            problem={read.error.problem}
            what={what}
            onRetry={() => void read.refetch()}
          />
        ) : null,
      )}
      {settled && items.length === 0 ? (
        <EmptyState title="No runs yet" icon={Activity}>
          Nothing in this scope has run yet: no snapshot, restore, replication or maintenance.
        </EmptyState>
      ) : null}
    </>
  );
}
