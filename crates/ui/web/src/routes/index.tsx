import { Link, createFileRoute } from "@tanstack/react-router";
import { LayoutGrid, Stethoscope, Wrench } from "lucide-react";
import type { ReactNode } from "react";

import {
  useDoctor,
  useMaintenance,
  useOverview,
  usePolicies,
  useRepositories,
  useSchedules,
  useStatus,
} from "../api/hooks";
import { type StatusReportView, narrowStatusReport } from "../api/statusReport";
import type {
  KindTally,
  DoctorCheckView,
  MaintenanceRow,
  PolicyRow,
  RepositorySummary,
  ScheduleRow,
  StatusOverview,
} from "../api/types";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { Finding } from "../components/Finding";
import { LoadingState } from "../components/LoadingState";
import { KindTiles } from "../components/KindTiles";
import { ObjectCard } from "../components/ObjectCard";
import type { CardRow } from "../components/objectCard";
import { WorkTable, type WorkRow } from "../components/WorkTable";
import { OVERVIEW_DOCTOR_CHECKS, doctorOutcomeLamp, summarizeDoctor } from "../components/doctor";
import { countByHealth, healthLamp } from "../components/health";
import { overviewVerdict, tallyPhrases } from "../components/verdict";
import { relativeTime } from "../util/format";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Overview — the screen an operator opens first when they suspect something
 * is wrong. One sentence answers "is my data safe?"; under it, the fleet by
 * health, the work in flight, what is stalled, and what needs fixing with the
 * fix on its plate.
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

  const report = status.data !== undefined ? narrowStatusReport(status.data.report) : null;
  const now = status.data !== undefined ? new Date(status.data.now) : new Date();
  const search = namespace !== undefined ? { namespace } : {};

  return (
    <div className="page">
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

      <Section title="Fleet by kind" icon={LayoutGrid}>
        {overview.isPending ? (
          <LoadingState what="the fleet overview" rows={2} />
        ) : overview.isError ? (
          <ErrorState
            problem={overview.error.problem}
            what="the fleet overview"
            onRetry={() => void overview.refetch()}
          />
        ) : (
          <KindTiles overview={overview.data} namespace={namespace} />
        )}
      </Section>

      <Section
        title="Needs attention"
        icon={Wrench}
        note={
          <Link to="/doctor" search={search}>
            full doctor report
          </Link>
        }
      >
        <Attention
          cards={attentionCards({
            repositories: repositories.data,
            policies: policies.data,
            schedules: schedules.data,
            maintenance: maintenance.data,
          })}
          now={now}
        />
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
        {status.isPending ? (
          <LoadingState what="stalled objects" rows={1} />
        ) : status.isError ? (
          <ErrorState
            problem={status.error.problem}
            what="the status report"
            onRetry={() => void status.refetch()}
          />
        ) : stalledRows(report).length > 0 ? (
          <WorkTable
            caption="Stalled objects"
            rows={stalledRows(report)}
            now={now}
            ageLabel="Since"
          />
        ) : null}
        {doctor.isPending ? (
          <LoadingState what="the doctor report" rows={1} />
        ) : doctor.isError ? (
          <ErrorState
            problem={doctor.error.problem}
            what="the doctor report"
            onRetry={() => void doctor.refetch()}
          />
        ) : (
          <Fixes checks={doctor.data.checks} ranAt={doctor.data.ranAt} now={now} />
        )}
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
          cards: attentionCards({
            repositories: repositories.data,
            policies: policies.data,
            schedules: schedules.data,
            maintenance: maintenance.data,
          }).length,
          stalled: stalledRows(report).length,
          failing: doctor.data?.checks.filter((c) => c.outcome === "Fail").length ?? 0,
        }) ? (
          <EmptyState title="Nothing needs you" icon={Stethoscope}>
            No repository, policy, schedule or maintenance is failing, nothing is stalled, and the
            doctor checks found nothing to fix.
          </EmptyState>
        ) : null}
      </Section>

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

/** At most this many objects lead "Needs attention"; the lists hold the rest. */
const ATTENTION_MAX = 6;

interface AttentionInputs {
  repositories: RepositorySummary[] | undefined;
  policies: PolicyRow[] | undefined;
  schedules: ScheduleRow[] | undefined;
  maintenance: MaintenanceRow[] | undefined;
}

/**
 * The objects that need someone, worst first: failed or degraded or unknown
 * repositories, maintenance and schedules with failed runs, and policies that
 * have never succeeded or never been verified.
 */
export function attentionCards({
  repositories,
  policies,
  schedules,
  maintenance,
}: AttentionInputs): CardRow[] {
  const ranked: { rank: number; card: CardRow }[] = [];
  for (const r of repositories ?? []) {
    const rank =
      r.health === "failed" ? 0 : r.health === "degraded" ? 1 : r.health === "unknown" ? 2 : -1;
    if (rank >= 0) {
      ranked.push({
        rank,
        card: { kind: r.kind === "ClusterRepository" ? "clusterRepository" : "repository", row: r },
      });
    }
  }
  for (const m of maintenance ?? []) {
    if (m.quick.consecutiveFailures + m.full.consecutiveFailures > 0) {
      ranked.push({ rank: 0, card: { kind: "maintenance", row: m } });
    }
  }
  for (const s of schedules ?? []) {
    if (!s.suspended && s.consecutiveFailures > 0) {
      ranked.push({ rank: 0, card: { kind: "snapshotSchedule", row: s } });
    }
  }
  for (const p of policies ?? []) {
    if (p.suspended) continue;
    if (!p.lastSuccessfulSnapshot)
      ranked.push({ rank: 1, card: { kind: "snapshotPolicy", row: p } });
    else if (!p.lastVerified) ranked.push({ rank: 3, card: { kind: "snapshotPolicy", row: p } });
  }
  return ranked
    .sort((a, b) => a.rank - b.rank)
    .slice(0, ATTENTION_MAX)
    .map((r) => r.card);
}

function Attention({ cards, now }: { cards: CardRow[]; now: Date }) {
  if (cards.length === 0) return null;
  return (
    <ul className="attention" aria-label="Objects needing attention">
      {cards.map((card) => (
        <li key={`${card.kind}/${card.row.namespace ?? ""}/${card.row.name}`}>
          <ObjectCard card={card} variant="stats" now={now} />
        </li>
      ))}
    </ul>
  );
}

function nothingNeedsYou(state: {
  settled: boolean;
  cards: number;
  stalled: number;
  failing: number;
  fleetFailing: number;
}): boolean {
  return (
    state.settled &&
    state.cards === 0 &&
    state.stalled === 0 &&
    state.failing === 0 &&
    state.fleetFailing === 0
  );
}

interface SectionProps {
  title: string;
  icon: typeof Wrench;
  note?: ReactNode;
  children: ReactNode;
}

function Section({ title, icon: Icon, note, children }: SectionProps) {
  return (
    <section className="page__section" aria-label={title}>
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

/** `namespace/name` → the two halves; a name with no slash is cluster-scoped. */
function splitObject(object: string): { namespace: string | null; name: string } {
  const slash = object.indexOf("/");
  if (slash < 0) {
    return { namespace: null, name: object };
  }
  return { namespace: object.slice(0, slash), name: object.slice(slash + 1) };
}

function stalledRows(report: StatusReportView | null): WorkRow[] {
  if (report === null) {
    return [];
  }
  return report.stalled.map((row) => {
    const { namespace, name } = splitObject(row.object);
    return {
      id: `${row.kind}/${row.object}`,
      kind: row.kind,
      namespace,
      name,
      health: "failed",
      stateWord: "Stalled",
      detail: row.message,
    };
  });
}

interface FixesProps {
  checks: DoctorCheckView[];
  ranAt: string;
  now: Date;
}

/** The failing doctor checks, each with its fix; warnings belong on the doctor page. */
function Fixes({ checks, ranAt, now }: FixesProps) {
  const failing = checks.filter((check) => check.outcome === "Fail");
  if (failing.length === 0) {
    return null;
  }
  return (
    <ul className="finding-list" aria-label="Failing checks">
      {failing.map((check) => (
        <li key={check.check}>
          <Finding
            title={check.title}
            what={check.what ?? "The check failed without saying what it found."}
            why={check.why}
            fix={check.fix}
            lamp={doctorOutcomeLamp(check.outcome)}
            meta={
              <>
                {check.check} · ran {relativeTime(ranAt, now)}
              </>
            }
          />
        </li>
      ))}
    </ul>
  );
}
