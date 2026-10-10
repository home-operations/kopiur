import { Camera, Database, FolderTree, GitBranch, ScrollText, Sigma, Timer } from "lucide-react";
import type { ReactNode } from "react";

import { useSnapshotRetention, useSnapshots } from "../../../api/hooks";
import { SnapshotSizeChart } from "../../../charts/SnapshotSizeChart";
import { isNotPermitted, problemKind } from "../../../api/problem";
import type { Problem, SnapshotDetail as SnapshotDetailData } from "../../../api/types";
import {
  EMPTY_CELL,
  formatTimestamp,
  humanBytes,
  humanDuration,
  relativeTime,
} from "../../../util/format";
import { ErrorState, NotPermittedState } from "../../ErrorState";
import { type Fact, Facts } from "../../Facts";
import { Finding } from "../../Finding";
import { LampBadge } from "../../HealthBadge";
import { healthLamp } from "../../health";
import { LineageTrail } from "../../LineageTrail";
import { LoadingState } from "../../LoadingState";
import { NotReported } from "../../NotReported";
import { RetentionPreview } from "../../RetentionPreview";
import {
  deletionPolicyLabel,
  durationSeconds,
  originLabel,
  snapshotPhaseLamp,
} from "../../snapshot";
import { DrawerSection } from "../DrawerSection";

export function RunTab({ detail, now }: { detail: SnapshotDetailData; now: Date }) {
  return (
    <DrawerSection title="This run" icon={Camera}>
      <Facts label="This run" facts={runFacts(detail, now)} />
    </DrawerSection>
  );
}

export function StorageTab({ detail }: { detail: SnapshotDetailData }) {
  return (
    <>
      <DrawerSection title="In the repository" icon={Database}>
        <Facts label="In the repository" facts={storageFacts(detail)} />
      </DrawerSection>
      <DrawerSection title="Sources" icon={FolderTree}>
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
            No resolved sources recorded (<span className="mono">status.resolved.sources</span>).
            Discovered snapshots often have none.
          </p>
        )}
      </DrawerSection>
      <DrawerSection title="Size over time" icon={Sigma}>
        <SizeHistory detail={detail} />
      </DrawerSection>
    </>
  );
}

/** "The 13 runs", or "The newest 100 of 240 runs" when the history is cut. */
function runsText(shown: number, total: number): string {
  const runs = (n: number) => `${String(n)} run${n === 1 ? "" : "s"}`;
  return total > shown ? `The newest ${String(shown)} of ${runs(total)}` : `The ${runs(shown)}`;
}

/** How many runs the size history reads: the newest, which is what a trend is. */
const HISTORY_RUNS = 100;

/**
 * This snapshot's size among its policy's other runs into the same
 * repository, with its own point marked. Read only while the Storage tab is
 * open — a policy's history is a list call the drawer must not pay for
 * unopened. A fan-out policy writes one series per repository, so the
 * repository narrows it: sizes in two repositories are two different trends.
 */
function SizeHistory({ detail }: { detail: SnapshotDetailData }) {
  const { row } = detail;
  const policy = row.policy ?? "";
  const history = useSnapshots(
    {
      namespace: row.namespace,
      policy,
      repository: row.repository ?? undefined,
      limit: HISTORY_RUNS,
    },
    { enabled: policy.length > 0 },
  );
  if (policy.length === 0) {
    return (
      <p className="page__section-note">
        No <span className="mono">SnapshotPolicy</span> governs this snapshot, so it has no runs to
        trend against.
      </p>
    );
  }
  if (history.isPending) {
    return <LoadingState what="this policy's earlier runs" rows={3} />;
  }
  if (history.isError) {
    const { problem } = history.error;
    if (isNotPermitted(problem)) {
      return <NotPermittedState problem={problem} what="this policy's earlier runs" />;
    }
    return (
      <ErrorState
        problem={problem}
        what="this policy's earlier runs"
        onRetry={() => void history.refetch()}
      />
    );
  }
  return (
    <>
      <p className="page__section-note">
        {runsText(history.data.items.length, history.data.total)} of{" "}
        <span className="mono">{policy}</span>
        {row.repository !== null && row.repository !== undefined ? (
          <>
            {" "}
            into <span className="mono">{row.repository}</span>
          </>
        ) : null}
        ; this snapshot&apos;s point is the one shown.
      </p>
      <SnapshotSizeChart
        rows={history.data.items}
        maxPolicies={1}
        focus={{ namespace: row.namespace, name: row.name }}
      />
    </>
  );
}

/**
 * Will this restore point still be here? The cheap verdict that rides on the
 * detail first, then the whole bucketed plan — read only once this tab is
 * open. The two never disagree: both come from `kopiur_api::retention`.
 */
export function RetentionTab({ detail }: { detail: SnapshotDetailData }) {
  const { retentionPreview: preview, row } = detail;
  return (
    <DrawerSection title="Retention" icon={Timer}>
      {preview !== null && preview !== undefined ? (
        <Finding
          what={
            preview.kept
              ? `Today's retention keeps this snapshot: ${preview.reasons.join(", ")}.`
              : "Today's retention does not keep this snapshot; the next retention run removes it."
          }
          why={
            preview.kept
              ? "The slot is its position in that rule's window (keepDaily slot 3 is the third-newest day kept), which shows how close it is to ageing out."
              : "It is older than every rule's window and is not pinned."
          }
          lamp={healthLamp(preview.kept ? "healthy" : "degraded")}
          meta={<span className="mono">computed {relativeTime(preview.computedAt)}</span>}
        />
      ) : (
        <NoPreview detail={detail} />
      )}
      <PlanSection namespace={row.namespace} name={row.name} />
    </DrawerSection>
  );
}

export function LineageTab({ detail }: { detail: SnapshotDetailData }) {
  return (
    <DrawerSection title="Lineage" icon={GitBranch}>
      <LineageTrail
        lineage={detail.lineage}
        name={detail.row.name}
        namespace={detail.row.namespace}
      />
    </DrawerSection>
  );
}

export function LogTab({ detail }: { detail: SnapshotDetailData }) {
  return (
    <DrawerSection title="Log tail" icon={ScrollText}>
      <pre className="log-tail" aria-label="Last lines of the mover log">
        {detail.logTail.join("\n")}
      </pre>
      <p className="page__section-note">
        The last lines of the mover log, with credentials redacted. The full log is in the
        Job&apos;s pod while it exists.
      </p>
    </DrawerSection>
  );
}

/**
 * Why there is no verdict — narrowed to the reason that actually applies.
 *
 * `retentionPreview` is null in four different situations and none of them
 * means "we could not work it out" (the wire doc is explicit). Two of the four
 * are decidable from what the detail already carries: the phase, and whether
 * the snapshot names a policy at all. For the other two the plan endpoint
 * answers precisely, which is what the wire doc tells a reader to do.
 */
function NoPreview({ detail }: { detail: SnapshotDetailData }) {
  const { row } = detail;
  const hasPolicy = row.policy !== null && row.policy !== undefined && row.policy.length > 0;
  const lamp = healthLamp("unknown");

  if (!hasPolicy) {
    return (
      <Finding
        what="GFS retention does not evaluate this snapshot, because no SnapshotPolicy governs it."
        why="Retention rules live on a SnapshotPolicy, and this snapshot names none."
        fix="look at the repository's catalog settings, or set spec.policyRef"
        lamp={lamp}
      />
    );
  }

  const phase = snapshotPhaseLamp(row.phase).word;
  return (
    <Finding
      what={`No retention verdict is available for this snapshot, which is ${phase}.`}
      why="Only succeeded snapshots count toward retention, and a policy with no retention rules prunes nothing."
      fix="read the full retention plan below to see which case applies"
      lamp={lamp}
    />
  );
}

/**
 * The bucketed plan, and the several honest answers that are not failures.
 *
 * `GET …/retention` answers 422 for three different situations that are all
 * *explanations*, not errors: no governing policy, a snapshot the prune does
 * not evaluate, and a population too large to assemble into one plan. Each
 * carries a what / why / fix, so each renders as a `Finding` rather than as a
 * red error state — the matching is on the problem's kind by prefix, never on
 * the status (addenda item 24).
 */
function PlanSection({ namespace, name }: { namespace: string; name: string }) {
  // Read here, in a component mounted only while the Retention tab is open:
  // for a fan-out policy the plan is every child of the policy, and opening
  // the drawer must not pay for it.
  const plan = useSnapshotRetention(namespace, name);
  if (plan.isPending) {
    return <LoadingState what="the retention plan" rows={5} />;
  }
  if (plan.isError) {
    const { problem } = plan.error;
    if (isNotPermitted(problem)) {
      return <NotPermittedState problem={problem} what="this snapshot's retention plan" />;
    }
    if (isExplanation(problem)) {
      return (
        <Finding
          what={problem.what}
          why={problem.why}
          fix={problem.fix}
          lamp={healthLamp("unknown")}
        />
      );
    }
    return (
      <ErrorState problem={problem} what="the retention plan" onRetry={() => void plan.refetch()} />
    );
  }
  return <RetentionPreview plan={plan.data} />;
}

/** The three 422s that explain rather than fail. */
function isExplanation(problem: Problem): boolean {
  const kind = problemKind(problem);
  return (
    kind === "no-retention-policy" || kind === "not-retention-governed" || kind === "plan-too-large"
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
      value: row.pinned ? "yes — never pruned by retention" : "no — retention decides",
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
