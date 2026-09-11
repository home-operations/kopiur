import { Link } from "@tanstack/react-router";
import {
  Camera,
  Database,
  FolderTree,
  GitBranch,
  ScrollText,
  ShieldAlert,
  Timer,
  type LucideIcon,
} from "lucide-react";
import type { ReactNode } from "react";

import type { SnapshotDetail as SnapshotDetailData } from "../api/types";
import {
  EMPTY_CELL,
  formatTimestamp,
  humanBytes,
  humanDuration,
  relativeTime,
} from "../util/format";
import { ConditionsTable } from "./ConditionsTable";
import { Facts, type Fact } from "./Facts";
import { Finding } from "./Finding";
import { LampBadge } from "./HealthBadge";
import { LineageTrail } from "./LineageTrail";
import { NotReported } from "./NotReported";
import { gateSeverityLamp } from "./gates";
import { healthLamp } from "./health";
import {
  deletionPolicyLabel,
  durationSeconds,
  originLabel,
  snapshotPhaseLamp,
  snapshotVerdict,
} from "./snapshot";

/**
 * One snapshot, in the order an operator needs it.
 *
 * The verdict first — what this snapshot is, in a sentence, on its phase's own
 * lamp. Then the gates and the failure, which are the reasons a human has to
 * act. Then retention, because "will I still have this restore point?" is the
 * question this screen exists to answer. Then what the run actually did, where
 * the snapshot lives, where it came from, and finally the log tail and the raw
 * conditions — the evidence for everything above.
 *
 * Presentation only. Every mutating control and the retention plan itself are
 * passed in by the route, which owns the hooks — so this renders identically
 * in a test with no query client, and the plan (which can be an expensive read
 * over a whole fan-out policy) is not paid for by merely opening the page.
 */
export interface SnapshotDetailProps {
  detail: SnapshotDetailData;
  /** The delete / snapshot-now controls, built by the route. */
  actions?: ReactNode;
  /** The retention plan section, built by the route. */
  retention?: ReactNode;
  /** The clock ages are measured against. */
  now?: Date | undefined;
}

export function SnapshotDetail({
  detail,
  actions,
  retention,
  now = new Date(),
}: SnapshotDetailProps) {
  const { row } = detail;
  const verdict = snapshotVerdict(row);
  const Lamp = verdict.lamp.icon;

  return (
    <div className="page">
      <p className="verdict" role="status" aria-label="Snapshot verdict">
        <span className="verdict__lamp" data-health={verdict.lamp.key}>
          <Lamp size={18} strokeWidth={2} aria-hidden="true" />
          <span>{verdict.lamp.word}</span>
        </span>
        <span className="verdict__text">{verdict.text}</span>
        <span className="verdict__meta mono">
          Snapshot · {row.namespace} · {row.name}
        </span>
      </p>

      {actions !== undefined && actions !== null ? (
        <section className="page__section" aria-label="Actions">
          {actions}
        </section>
      ) : null}

      {detail.gates.length > 0 ? (
        <Section title="Gates holding this snapshot" icon={ShieldAlert}>
          <ul className="finding-list">
            {detail.gates.map((gate) => {
              const lamp = gateSeverityLamp(gate.severity);
              return (
                <li key={`${gate.condition}/${gate.reason}`}>
                  <Finding
                    title={gate.reason}
                    what={gate.message}
                    lamp={lamp}
                    meta={
                      <span className="mono">
                        condition {gate.condition} · {lamp.word}
                      </span>
                    }
                  />
                </li>
              );
            })}
          </ul>
          <p className="page__section-note">
            A gate never self-heals — the snapshot stays parked until a human acts. The{" "}
            <Link to="/gates" search={{}}>
              gate registry
            </Link>{" "}
            explains every gate the operator can raise.
          </p>
        </Section>
      ) : null}

      {detail.failure !== null && detail.failure !== undefined ? (
        <Section title="Why this run failed" icon={Camera}>
          <Finding
            title={detail.failure.kopiaErrorClass ?? "Failure"}
            what={detail.failure.message ?? "The operator recorded a failure with no message."}
            why={
              detail.failure.retryRecommended === true
                ? "The operator classified this as worth retrying: the cause looks transient rather than a misconfiguration."
                : detail.failure.retryRecommended === false
                  ? "The operator classified this as not worth retrying on its own — the same run would fail the same way until something changes."
                  : undefined
            }
            lamp={healthLamp("failed")}
            meta={
              <span className="mono">
                {[
                  detail.failure.op !== null && detail.failure.op !== undefined
                    ? `op ${detail.failure.op}`
                    : undefined,
                  detail.failure.exitCode !== null && detail.failure.exitCode !== undefined
                    ? `exit ${String(detail.failure.exitCode)}`
                    : undefined,
                ]
                  .filter((part) => part !== undefined)
                  .join(" · ")}
              </span>
            }
          />
        </Section>
      ) : null}

      <Section title="Retention" icon={Timer}>
        {retention}
      </Section>

      <div className="page__pair">
        <Section title="This run" icon={Camera}>
          <Facts label="This run" facts={runFacts(detail, now)} />
        </Section>

        <Section title="In the repository" icon={Database}>
          <Facts label="In the repository" facts={storageFacts(detail)} />
        </Section>
      </div>

      <Section title="Sources" icon={FolderTree}>
        {detail.sources.length > 0 ? (
          <ul className="ref-list" aria-label="Source paths">
            {detail.sources.map((source) => (
              <li key={source} className="label-strip">
                <span className="label-strip__kind">path</span>
                <span className="label-strip__name mono">{source}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="page__section-note">
            The operator recorded no resolved sources on this snapshot. A produced backup pins the
            paths it covered in <span className="mono">status.resolved.sources</span>; a discovered
            one may carry none, because the paths are kopia&apos;s, not the policy&apos;s.
          </p>
        )}
      </Section>

      <Section title="Lineage" icon={GitBranch}>
        <LineageTrail lineage={detail.lineage} name={row.name} namespace={row.namespace} />
      </Section>

      {detail.logTail.length > 0 ? (
        <Section title="Log tail" icon={ScrollText}>
          <pre className="log-tail" aria-label="Last lines of the mover log">
            {detail.logTail.join("\n")}
          </pre>
          <p className="page__section-note">
            The last lines the mover Job wrote, with anything that looks like a credential or a
            presigned URL redacted by the server before it left the cluster. The full log is in the
            Job&apos;s pod, for as long as the cluster keeps it.
          </p>
        </Section>
      ) : null}

      <Section title="Conditions" icon={ScrollText}>
        <ConditionsTable
          conditions={detail.conditions}
          now={now}
          emptyNote="The operator has written no conditions on this snapshot yet, which means it has not been reconciled — not that it is healthy."
        />
      </Section>
    </div>
  );
}

/** What the run did. */
function runFacts(detail: SnapshotDetailData, now: Date): Fact[] {
  const { row, stats } = detail;
  const duration = detail.durationSeconds ?? durationSeconds(row);
  return [
    { term: "Phase", value: <LampBadge lamp={snapshotPhaseLamp(row.phase)} /> },
    { term: "Origin", value: originLabel(row.origin) },
    {
      term: "Started",
      value: instant(row.startTime, now),
    },
    {
      term: "Finished",
      value: instant(row.endTime, now),
    },
    { term: "Took", value: humanDuration(duration) },
    { term: "Size", value: humanBytes(stats?.sizeBytes ?? row.sizeBytes) },
    {
      term: "New bytes",
      value:
        stats?.bytesNew !== null && stats?.bytesNew !== undefined ? (
          humanBytes(stats.bytesNew)
        ) : (
          // Declared on the CRD and written by no controller — see `unwired.ts`.
          // A blank would read as "nothing new"; a 0 as perfect deduplication.
          <NotReported field="snapshotBytesNew" />
        ),
    },
    { term: "Files new", value: count(stats?.filesNew) },
    { term: "Files modified", value: count(stats?.filesModified) },
    { term: "Files unchanged", value: count(stats?.filesUnchanged) },
    {
      term: "Files failed",
      value:
        stats?.filesFailed !== null && stats?.filesFailed !== undefined && stats.filesFailed > 0 ? (
          <span className="snapshot-table__failed">
            {stats.filesFailed.toLocaleString()} could not be read
          </span>
        ) : (
          count(stats?.filesFailed ?? row.filesFailed)
        ),
    },
  ];
}

/** Where the snapshot lives and what happens to it. */
function storageFacts(detail: SnapshotDetailData): Fact[] {
  const { row } = detail;
  return [
    { term: "Repository", value: mono(row.repository) },
    { term: "Policy", value: mono(row.policy) },
    { term: "kopia manifest", value: mono(row.kopiaSnapshotId) },
    { term: "kopia identity", value: mono(row.identity) },
    {
      term: "Pinned",
      value: row.pinned
        ? "yes — exempt from GFS pruning entirely"
        : "no — GFS retention decides whether it is kept",
    },
    {
      term: "Deletion policy",
      value: deletionPolicyLabel(row.deletionPolicy),
    },
    {
      term: "Browsable",
      value: detail.browsable
        ? "yes"
        : (detail.browseBlocker ?? "no — the operator did not say why"),
    },
  ];
}

/** A count, where zero is a measurement and absent is not one. */
function count(value: number | null | undefined): string {
  return value === null || value === undefined ? EMPTY_CELL : value.toLocaleString();
}

/** An identifier, or the empty cell. */
function mono(value: string | null | undefined): ReactNode {
  return value !== null && value !== undefined && value.length > 0 ? (
    <span className="mono">{value}</span>
  ) : (
    EMPTY_CELL
  );
}

/** An exact instant with its age beside it — retention buckets on these. */
function instant(at: string | null | undefined, now: Date): ReactNode {
  if (at === null || at === undefined || at.length === 0) {
    return EMPTY_CELL;
  }
  return (
    <time dateTime={at} className="mono">
      {formatTimestamp(at)} <span className="facts__age">({relativeTime(at, now)})</span>
    </time>
  );
}

interface SectionProps {
  title: string;
  icon: LucideIcon;
  children: ReactNode;
}

function Section({ title, icon: Icon, children }: SectionProps) {
  return (
    <section className="page__section" aria-label={title}>
      <div className="page__section-head">
        <h2>
          <Icon size={16} strokeWidth={2} aria-hidden="true" />
          {title}
        </h2>
      </div>
      {children}
    </section>
  );
}
