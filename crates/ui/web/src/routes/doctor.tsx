import { Link, createFileRoute, useNavigate } from "@tanstack/react-router";
import { Play, Stethoscope } from "lucide-react";
import { type SubmitEvent, useState } from "react";

import { useDoctor } from "../api/hooks";
import { ActionButton } from "../components/ActionButton";
import { DoctorChecks } from "../components/DoctorChecks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { NamespaceField } from "../components/NamespaceField";
import { Popover } from "../components/Popover";
import { DOCTOR_DEFAULTS, summarizeDoctor } from "../components/doctor";
import { type HealthKey, healthLamp } from "../components/health";
import { parseGoDuration } from "../util/duration";
import { relativeTime } from "../util/format";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Doctor — the checks `kubectl kopiur doctor` runs, as the signed-in user.
 *
 * The URL is the state: `?namespace=` (shared with every route) scopes the
 * checks that can be scoped, and `?stuckThreshold=` / `?failureLookback=`
 * are the two windows, spelled as the CLI spells them (`1h`, `24h`) and sent
 * to the server in seconds. They belong here, not on the overview (addenda
 * item 12). A value the server would refuse never leaves the form.
 */
export interface DoctorSearch {
  stuckThreshold?: string;
  failureLookback?: string;
}

/**
 * One window as the reader wrote it — **total over `unknown`**, and it keeps
 * a value the server would refuse.
 *
 * Two separate reasons for both halves.
 *
 * Total, because the declared `DoctorSearch` type is a claim about
 * `validateSearch`'s output and not about what the component is handed: the
 * router parses `?stuckThreshold=0` into the **number** `0` and the component
 * sees that number even though the validator dropped the key. Typing this
 * `string` and calling `.trim()` on it threw inside render and took the whole
 * route to its error boundary — a crash any shared link could cause.
 *
 * Keeping a refused value, because dropping it was silent:
 * `/doctor?stuckThreshold=1d` — the exact value the form rejects with a
 * message — rendered an empty control and ran with the server default, so a
 * bookmarked link reported on a window its reader never chose. Kept, the form
 * can show it and say what is wrong with it. [`windowFor`] is the one gate on
 * what actually reaches the server.
 */
function windowParam(value: unknown): string | undefined {
  if (typeof value === "string") {
    const text = value.trim();
    return text.length > 0 ? text : undefined;
  }
  // A bare number or `true` is what the reader typed, parsed by the router.
  // Rendered back as they wrote it so the error names their value.
  return typeof value === "number" || typeof value === "boolean" ? String(value) : undefined;
}

/**
 * The seconds to send for a window, or `undefined` when the server would
 * refuse it — the one gate, so the URL cannot get past what the form
 * enforces. `parseGoDuration` alone is not that gate: it accepts `0`, and
 * `withQuery` keeps a `0`, so `?stuckThreshold=0` used to reach a handler
 * that answers 400 ("must be a positive number of seconds"). Same for a
 * window past the year `window()` caps at.
 */
function windowFor(value: string | undefined): number | undefined {
  if (value === undefined || windowError(value) !== null) {
    return undefined;
  }
  return parseGoDuration(value) ?? undefined;
}

/** A window the URL asked for that the report could not use. */
interface IgnoredWindow {
  label: string;
  value: string;
  fallback: string;
}

/** **Pure.** That window, or `null` when the value is usable or absent. */
function ignoredWindow(
  label: string,
  value: string | undefined,
  fallback: string,
): IgnoredWindow | null {
  return value !== undefined && windowError(value) !== null ? { label, value, fallback } : null;
}

export const Route = createFileRoute("/doctor")({
  validateSearch: (search: Record<string, unknown>): DoctorSearch => {
    const out: DoctorSearch = {};
    const stuck = windowParam(search.stuckThreshold);
    const lookback = windowParam(search.failureLookback);
    if (stuck !== undefined) {
      out.stuckThreshold = stuck;
    }
    if (lookback !== undefined) {
      out.failureLookback = lookback;
    }
    return out;
  },
  component: Doctor,
});

const DURATION_HINT = "a duration like 90s, 30m or 1h";

function Doctor() {
  const namespace = useCurrentNamespace();
  // Re-normalised here, not just in `validateSearch`: the router hands the
  // component the raw parsed value for a key the validator dropped, so the
  // declared `string` type is not something render may rely on.
  const search: Record<string, unknown> = Route.useSearch();
  const stuckThreshold = windowParam(search.stuckThreshold);
  const failureLookback = windowParam(search.failureLookback);
  const doctor = useDoctor({
    namespace,
    stuckThreshold: windowFor(stuckThreshold),
    failureLookback: windowFor(failureLookback),
  });
  const now = new Date();
  // A window the URL named that the report could not use. Said out loud: the
  // control shows the value and what is wrong with it, and this says which
  // window the run actually used instead.
  const ignored = [
    ignoredWindow("Stuck threshold", stuckThreshold, DOCTOR_DEFAULTS.stuckThreshold),
    ignoredWindow("Failure lookback", failureLookback, DOCTOR_DEFAULTS.failureLookback),
  ].filter((w): w is IgnoredWindow => w !== null);

  return (
    <div className="page">
      <div className="doctor-head">
        {doctor.data !== undefined ? (
          <Summary
            checks={doctor.data.checks}
            ranAt={doctor.data.ranAt}
            now={now}
            namespace={namespace}
          />
        ) : null}
        <RunDoctor
          key={`${namespace ?? ""}|${stuckThreshold ?? ""}|${failureLookback ?? ""}`}
          namespace={namespace}
          stuckThreshold={stuckThreshold}
          failureLookback={failureLookback}
          running={doctor.isFetching}
          onRerun={() => void doctor.refetch()}
        />
      </div>

      {ignored.length > 0 ? (
        <p className="page__prose" role="status">
          {ignored.map((w) => (
            <span key={w.label}>
              This link asked for a {w.label.toLowerCase()} of{" "}
              <span className="mono">{w.value}</span>, which is not valid, so the report ran with
              the default <span className="mono">{w.fallback}</span>.{" "}
            </span>
          ))}
        </p>
      ) : null}

      <section className="page__section" aria-label="Doctor checks">
        {doctor.isPending ? (
          <LoadingState what="the doctor report" rows={10} />
        ) : doctor.isError ? (
          <ErrorState
            problem={doctor.error.problem}
            what="the doctor report"
            onRetry={() => void doctor.refetch()}
          />
        ) : doctor.data.checks.length === 0 ? (
          <EmptyState title="No checks ran" icon={Stethoscope}>
            Doctor returned an empty report: the server ran no checks, not that all passed.
          </EmptyState>
        ) : (
          <DoctorChecks checks={doctor.data.checks} namespace={namespace} />
        )}
      </section>

      <p className="page__prose">
        Checks run as you: a check your permissions cannot run is a warning naming the missing
        grant. The Scope column shows what each check read; installation checks such as{" "}
        <span className="mono">crds-installed</span> ignore the namespace. A blocked object names
        its gate; the{" "}
        <Link to="/gates" search={namespace !== undefined ? { namespace } : {}}>
          gate registry
        </Link>{" "}
        explains every gate the operator can raise.
      </p>
    </div>
  );
}

interface SummaryProps {
  checks: Parameters<typeof summarizeDoctor>[0];
  ranAt: string;
  now: Date;
  namespace: string | undefined;
}

function Summary({ checks, ranAt, now, namespace }: SummaryProps) {
  const summary = summarizeDoctor(checks);
  // A check the viewer's RBAC blocked is not a verdict on the cluster, so it
  // does not light the degraded lamp — but it did not pass either, and a run
  // that could not perform part of itself is `unknown`, not green.
  const health: HealthKey =
    summary.fail > 0
      ? "failed"
      : summary.warn > 0 || summary.other > 0
        ? "degraded"
        : summary.total === 0 || summary.rbac > 0
          ? "unknown"
          : "healthy";
  const lamp = healthLamp(health);
  const Icon = lamp.icon;
  const parts = [
    `${summary.pass} pass`,
    `${summary.warn} warn`,
    `${summary.fail} fail`,
    ...(summary.rbac > 0 ? [`${summary.rbac} not permitted`] : []),
    ...(summary.other > 0 ? [`${summary.other} unreadable`] : []),
  ];
  return (
    // A named live region, not a heading: as an `h2` the run's answer read as
    // a peer of the section headings rather than as the page's answer.
    <p className="verdict" role="status" aria-label="Doctor verdict">
      <span className="verdict__lamp" data-health={lamp.key}>
        <Icon size={18} strokeWidth={2} aria-hidden="true" />
        <span>{lamp.word}</span>
      </span>
      <span className="verdict__text">
        {summary.total} {summary.total === 1 ? "check" : "checks"}: {parts.join(", ")}
      </span>
      <span className="verdict__meta">
        ran {relativeTime(ranAt, now)} · {namespace ?? "whole installation"}
      </span>
    </p>
  );
}

interface RunDoctorProps {
  namespace: string | undefined;
  stuckThreshold: string | undefined;
  failureLookback: string | undefined;
  running: boolean;
  onRerun: () => void;
}

/**
 * The one way to run the report: a button under the verdict whose options —
 * the namespace and the two windows, all optional — open beside it in an
 * anchored popover, filled in with what the current report ran with.
 *
 * Running with the settings unchanged runs the report again; changing any of
 * them writes the new ones into the URL, which runs the report for those.
 * While a run is under way the button says so and starts nothing.
 */
function RunDoctor({ running, ...current }: RunDoctorProps) {
  const [open, setOpen] = useState(false);
  return (
    <Popover
      label="Doctor options"
      open={open}
      onOpenChange={setOpen}
      className="doctor-options"
      trigger={(props) => (
        <ActionButton
          variant="primary"
          {...props}
          disabledReason={running ? "A run is already in progress." : undefined}
          reasonShown={false}
        >
          <Stethoscope size={14} strokeWidth={2} aria-hidden="true" />
          {running ? "Running…" : "Run doctor"}
        </ActionButton>
      )}
    >
      {(close) => <DoctorOptions {...current} onRun={close} />}
    </Popover>
  );
}

interface DoctorOptionsProps {
  namespace: string | undefined;
  stuckThreshold: string | undefined;
  failureLookback: string | undefined;
  onRerun: () => void;
  /** Called once the run has been asked for; closes the popover. */
  onRun: () => void;
}

/**
 * The three options, submitted into the URL. Validation is the server's rule
 * repeated in the client so a refused value never costs a round trip: a
 * window is a Go-style duration, positive, at most a year.
 */
function DoctorOptions({
  namespace,
  stuckThreshold,
  failureLookback,
  onRerun,
  onRun,
}: DoctorOptionsProps) {
  const navigate = useNavigate();
  const [ns, setNs] = useState(namespace ?? "");
  const [stuck, setStuck] = useState(stuckThreshold ?? "");
  const [lookback, setLookback] = useState(failureLookback ?? "");
  // A window that arrived invalid from the URL is already wrong, so its
  // message shows as soon as the options open rather than waiting for a
  // submit the reader of a shared link has no reason to perform.
  const [submitted, setSubmitted] = useState(
    () => windowError(stuckThreshold ?? "") !== null || windowError(failureLookback ?? "") !== null,
  );

  const stuckError = submitted ? windowError(stuck) : null;
  const lookbackError = submitted ? windowError(lookback) : null;

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setSubmitted(true);
    if (windowError(stuck) !== null || windowError(lookback) !== null) {
      return;
    }
    const search: DoctorSearch & { namespace?: string } = {};
    const trimmedNs = ns.trim();
    if (trimmedNs.length > 0) {
      search.namespace = trimmedNs;
    }
    if (stuck.trim().length > 0) {
      search.stuckThreshold = stuck.trim();
    }
    if (lookback.trim().length > 0) {
      search.failureLookback = lookback.trim();
    }
    const unchanged =
      search.namespace === namespace &&
      search.stuckThreshold === stuckThreshold &&
      search.failureLookback === failureLookback;
    if (unchanged) {
      onRerun();
    } else {
      void navigate({ to: "/doctor", search });
    }
    onRun();
  };

  return (
    <form className="doctor-options__form" onSubmit={submit} aria-label="Doctor options">
      <NamespaceField
        id="doctor-namespace"
        label="Namespace"
        value={ns}
        onChange={setNs}
        emptyLabel="all namespaces"
        hint="empty runs the whole installation"
      />
      <div className="controls__field">
        <label htmlFor="doctor-stuck">Stuck threshold</label>
        <input
          id="doctor-stuck"
          className="controls__input"
          name="stuckThreshold"
          value={stuck}
          onChange={(event) => {
            setStuck(event.target.value);
          }}
          placeholder={DOCTOR_DEFAULTS.stuckThreshold}
          aria-invalid={stuckError !== null ? "true" : undefined}
          aria-describedby="doctor-stuck-hint"
          autoComplete="off"
          spellCheck={false}
        />
        <span
          className="controls__hint"
          id="doctor-stuck-hint"
          data-invalid={stuckError !== null ? "true" : undefined}
        >
          {stuckError ??
            `non-terminal this long is stuck; default ${DOCTOR_DEFAULTS.stuckThreshold}`}
        </span>
      </div>
      <div className="controls__field">
        <label htmlFor="doctor-lookback">Failure lookback</label>
        <input
          id="doctor-lookback"
          className="controls__input"
          name="failureLookback"
          value={lookback}
          onChange={(event) => {
            setLookback(event.target.value);
          }}
          placeholder={DOCTOR_DEFAULTS.failureLookback}
          aria-invalid={lookbackError !== null ? "true" : undefined}
          aria-describedby="doctor-lookback-hint"
          autoComplete="off"
          spellCheck={false}
        />
        <span
          className="controls__hint"
          id="doctor-lookback-hint"
          data-invalid={lookbackError !== null ? "true" : undefined}
        >
          {lookbackError ??
            `a failure this recent is current; default ${DOCTOR_DEFAULTS.failureLookback}`}
        </span>
      </div>
      <div className="doctor-options__actions">
        <ActionButton variant="primary" type="submit">
          <Play size={14} strokeWidth={2} aria-hidden="true" />
          Run
        </ActionButton>
      </div>
    </form>
  );
}

const MAX_WINDOW_SECONDS = 365 * 24 * 60 * 60;

/** The server's `window()` rule, so the message arrives before the request. */
function windowError(value: string): string | null {
  const text = value.trim();
  if (text.length === 0) {
    return null;
  }
  const seconds = parseGoDuration(text);
  if (seconds === null) {
    return `expected ${DURATION_HINT}`;
  }
  if (seconds === 0) {
    return "must be more than zero seconds";
  }
  if (seconds > MAX_WINDOW_SECONDS) {
    return "must be at most a year";
  }
  return null;
}
