import { Link, createFileRoute } from "@tanstack/react-router";
import { Activity, Database, PauseCircle, Stethoscope, Wrench } from "lucide-react";
import type { ReactNode } from "react";

import { useDoctor, useRepositories, useStatus } from "../api/hooks";
import { type StatusReportView, narrowStatusReport } from "../api/statusReport";
import type { DoctorCheckView, RepositorySummary, StatusOverview } from "../api/types";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { Finding } from "../components/Finding";
import { LoadingState } from "../components/LoadingState";
import { StatusCards } from "../components/StatusCards";
import { WorkTable, type WorkRow } from "../components/WorkTable";
import { doctorOutcomeLamp, summarizeDoctor } from "../components/doctor";
import { countByHealth, healthLamp } from "../components/health";
import { overviewVerdict } from "../components/verdict";
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
 * - `/doctor` for the failing checks — the one source of `fix` text.
 */
export const Route = createFileRoute("/")({
  component: Overview,
});

function Overview() {
  const namespace = useCurrentNamespace();
  const repositories = useRepositories(namespace);
  const status = useStatus(namespace);
  const doctor = useDoctor({ namespace });

  const report = status.data !== undefined ? narrowStatusReport(status.data.report) : null;
  const now = status.data !== undefined ? new Date(status.data.now) : new Date();
  const scope = namespace ?? "all namespaces";
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
        ].filter((source): source is string => source !== null)}
        pending={repositories.isPending || status.isPending || doctor.isPending}
        at={status.data?.now}
      />

      <Section title="Repositories by health" icon={Database}>
        {repositories.isPending ? (
          <LoadingState what="repositories" rows={1} />
        ) : repositories.isError ? (
          <ErrorState
            problem={repositories.error.problem}
            what="repositories"
            onRetry={() => void repositories.refetch()}
          />
        ) : repositories.data.length === 0 ? (
          <EmptyState title={`No repositories in ${scope}`} icon={Database}>
            A Repository or ClusterRepository is the kopia repository backups land in. Create one
            and it appears here with its health; policies then name it as their target.
          </EmptyState>
        ) : (
          <StatusCards repositories={repositories.data} namespace={namespace} />
        )}
      </Section>

      {status.isError ? (
        // Both halves of the pair come from the one report; a refusal is
        // said once, across the width, rather than twice side by side.
        <Section title="Status report" icon={Activity}>
          <ErrorState
            problem={status.error.problem}
            what="the status report"
            onRetry={() => void status.refetch()}
          />
        </Section>
      ) : (
        <div className="page__pair">
          <Section
            title="Work in flight"
            icon={Activity}
            note={report !== null && !report.complete ? "report incomplete" : undefined}
          >
            {status.isPending ? (
              <LoadingState what="the status report" rows={2} />
            ) : (
              <InFlight report={report} search={search} />
            )}
          </Section>

          <Section title="Stalled objects" icon={PauseCircle}>
            {status.isPending ? (
              <LoadingState what="stalled objects" rows={2} />
            ) : (
              <WorkTable
                caption="Stalled objects"
                rows={stalledRows(report)}
                now={now}
                ageLabel="Since"
                empty={
                  <EmptyState title="Nothing is stalled" icon={PauseCircle}>
                    An object reports Stalled when its reconciler has given up and is waiting for a
                    human — a parked snapshot, a restore with no credentials. It would be listed
                    here with the condition&apos;s message.
                  </EmptyState>
                }
              />
            )}
          </Section>
        </div>
      )}

      <Section
        title="What needs fixing"
        icon={Wrench}
        note={
          <Link to="/doctor" search={search}>
            full doctor report
          </Link>
        }
      >
        {doctor.isPending ? (
          <LoadingState what="the doctor report" rows={2} />
        ) : doctor.isError ? (
          <ErrorState
            problem={doctor.error.problem}
            what="the doctor report"
            onRetry={() => void doctor.refetch()}
          />
        ) : (
          <Fixes checks={doctor.data.checks} ranAt={doctor.data.ranAt} now={now} />
        )}
      </Section>

      {report !== null && !report.complete ? (
        <p className="page__prose">
          kopiur-ui could not read part of the status report this server sent; what it could read is
          shown. The console and the server may be different releases — the CLI&apos;s
          <span className="mono"> kubectl kopiur status</span> prints the report in full.
        </p>
      ) : null}
    </div>
  );
}

interface SectionProps {
  title: string;
  icon: typeof Database;
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
}

/** The one sentence. Never green while anything is still loading or failed to. */
function VerdictLine({ repositories, report, doctor, unavailable, pending, at }: VerdictLineProps) {
  if (pending && unavailable.length === 0) {
    const lamp = healthLamp("pending");
    const Icon = lamp.icon;
    return (
      <h2 className="verdict" aria-live="polite">
        <span className="verdict__lamp" data-health="pending">
          <Icon size={18} strokeWidth={2} aria-hidden="true" />
          <span>Checking</span>
        </span>
        <span className="verdict__text">Checking the vault…</span>
      </h2>
    );
  }
  const verdict = overviewVerdict({
    repositories: countByHealth(repositories ?? []),
    stalled: report?.stalled.length ?? 0,
    doctor: summarizeDoctor(doctor ?? []),
    unavailable,
  });
  const lamp = healthLamp(verdict.health);
  const Icon = lamp.icon;
  return (
    <h2 className="verdict" aria-live="polite">
      <span className="verdict__lamp" data-health={lamp.key}>
        <Icon size={18} strokeWidth={2} aria-hidden="true" />
        <span>{lamp.word}</span>
      </span>
      <span className="verdict__text">{verdict.text}</span>
      {at !== undefined ? (
        <span className="verdict__meta">as of {relativeTime(at, new Date())}</span>
      ) : null}
    </h2>
  );
}

interface InFlightProps {
  report: StatusReportView | null;
  search: { namespace?: string };
}

function Count({ value }: { value: number | null }) {
  return value === null ? (
    <span className="in-flight__count" data-unknown="true">
      unknown
    </span>
  ) : (
    <span className="in-flight__count">{value}</span>
  );
}

/** Snapshots and restores in a non-terminal phase, from the report's counts. */
function InFlight({ report, search }: InFlightProps) {
  const snapshots = report?.inFlight.snapshots ?? null;
  const restores = report?.inFlight.restores ?? null;
  return (
    <dl className="in-flight">
      <dt>Snapshots</dt>
      <dd>
        <Count value={snapshots} />
        <Link to="/snapshots" search={search}>
          {snapshots === 1 ? "snapshot running" : "snapshots running"}
        </Link>
      </dd>
      <dt>Restores</dt>
      <dd>
        <Count value={restores} />
        <Link to="/restores" search={search}>
          {restores === 1 ? "restore running" : "restores running"}
        </Link>
      </dd>
      <dt>Policies</dt>
      <dd>
        <Count value={report?.policies ?? null} />
        <Link to="/policies" search={search}>
          in scope
        </Link>
      </dd>
      <dt>Schedules</dt>
      <dd>
        <Count value={report?.schedules ?? null} />
        <Link to="/schedules" search={search}>
          in scope
        </Link>
      </dd>
    </dl>
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
  const summary = summarizeDoctor(checks);
  if (failing.length === 0) {
    // A check the viewer's RBAC blocked is said in its own words, not added
    // to the warning count: the two are different facts and this screen shows
    // neither in detail, so the sentence is the only place they can be told
    // apart.
    const notes: string[] = [];
    if (summary.warn > 0) {
      notes.push(`raised ${summary.warn} ${summary.warn === 1 ? "warning" : "warnings"}`);
    }
    if (summary.rbac > 0) {
      notes.push(
        `could not run ${summary.rbac} ${summary.rbac === 1 ? "check" : "checks"} with your permissions`,
      );
    }
    return (
      <EmptyState title="Nothing to fix" icon={Stethoscope}>
        Doctor ran {summary.total} {summary.total === 1 ? "check" : "checks"}{" "}
        {relativeTime(ranAt, now)}
        {notes.length > 0 ? ` and ${notes.join(", and ")}` : " and found nothing failing"}; a
        failing check would be listed here with what to do about it.
      </EmptyState>
    );
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
