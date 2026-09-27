import { Link } from "@tanstack/react-router";
import { Pin } from "lucide-react";

import type { RetentionBucket, RetentionCandidate, RetentionPlan } from "../api/types";
import { formatTimestamp, humanAge, relativeTime } from "../util/format";
import { LampBadge } from "./HealthBadge";
import {
  bucketLabel,
  candidateVerdict,
  isFanOutBucket,
  planTotals,
  planVerdict,
} from "./retention";

/**
 * The retention plan: every snapshot competing for the same buckets as this
 * one, and what today's selection does to each.
 *
 * This is the screen where a misreading costs a restore point, so it is built
 * on four rules.
 *
 * **One group per bucket, and each bucket whole.** Buckets are independent —
 * a fan-out policy over seven PVCs has seven, and no snapshot in one can
 * displace a snapshot in another — so they are never merged into one list, and
 * a bucket is never paged. (The server refuses an oversized plan rather than
 * truncating one, for the same reason: half a bucket is not half an answer, it
 * is a different answer.)
 *
 * **The rule is on the row.** `keepDaily slot 3` is the third-newest day that
 * rule holds, which is how a reader sees a restore point ageing out. Putting
 * that behind a hover would hide the only thing that explains the verdict — so
 * it is a column, in text, always visible.
 *
 * **The order is the kernel's.** Candidates arrive newest first, ties broken
 * by name, exactly as `select_kept` walks them; nothing here re-sorts, because
 * a different order would suggest a different competition.
 *
 * **`unbounded` is not a prune.** With no `spec.retention` the operator runs
 * no selection at all. Every row comes back kept with no rule, and the empty
 * rule list means "nothing is dropped", not "nothing holds it" — the inversion
 * addenda item 21 exists to prevent.
 */
export interface RetentionPreviewProps {
  plan: RetentionPlan;
  /** The clock ages are measured against. */
  now?: Date | undefined;
}

export function RetentionPreview({ plan, now = new Date() }: RetentionPreviewProps) {
  const verdict = planVerdict(plan);
  const Lamp = verdict.lamp.icon;
  const totals = planTotals(plan);

  return (
    <div className="retention">
      <p className="verdict" role="status" aria-label="Retention verdict">
        <span className="verdict__lamp" data-health={verdict.lamp.key}>
          <Lamp size={18} strokeWidth={2} aria-hidden="true" />
          <span>{verdict.lamp.word}</span>
        </span>
        <span className="verdict__text">{verdict.text}</span>
        <span className="verdict__meta mono">computed {relativeTime(plan.computedAt, now)}</span>
      </p>

      {plan.unbounded ? (
        <p className="page__prose" data-retention="unbounded">
          <strong>No GFS retention is configured</strong> on SnapshotPolicy{" "}
          <span className="mono">
            {plan.policy.namespace}/{plan.policy.name}
          </span>
          . The operator only runs a selection when a policy declares retention rules, so none of
          these {totals.candidates} snapshots is dropped by retention — they are listed to show what
          the policy governs, not what it will remove. Nothing here is at risk; the repository is
          instead bounded by whatever else prunes it, and grows until something does.
        </p>
      ) : (
        <p className="page__prose">
          {totals.kept} of {totals.candidates}{" "}
          {totals.candidates === 1 ? "snapshot is" : "snapshots are"} kept by SnapshotPolicy{" "}
          <span className="mono">
            {plan.policy.namespace}/{plan.policy.name}
          </span>
          {totals.pruned > 0 ? (
            <>
              ; the other {totals.pruned} {totals.pruned === 1 ? "is" : "are"} removed by the next
              retention run
            </>
          ) : null}
          .{" "}
          {totals.buckets > 1 ? (
            <>
              The population splits into {totals.buckets} independent buckets — one per backup
              source, and per repository while the policy is multi-repository. A snapshot in one
              bucket never competes with a snapshot in another, so each is counted on its own.
            </>
          ) : null}{" "}
          A verdict is an answer about a moment: these rules are evaluated against the clock, so a
          snapshot kept today can age out tomorrow with nothing changing but the date.
        </p>
      )}

      {plan.buckets.map((bucket) => (
        <Bucket key={bucket.key} bucket={bucket} unbounded={plan.unbounded} now={now} />
      ))}
    </div>
  );
}

interface BucketProps {
  bucket: RetentionBucket;
  unbounded: boolean;
  now: Date;
}

function Bucket({ bucket, unbounded, now }: BucketProps) {
  const kept = bucket.candidates.filter((c) => c.kept).length;
  const label = bucketLabel(bucket.key);
  return (
    <section className="page__section retention__bucket" aria-label={`Bucket ${label}`}>
      <div className="page__section-head">
        <h3 className={isFanOutBucket(bucket.key) ? "mono" : undefined}>{label}</h3>
        <span className="retention__count">
          {kept} of {bucket.candidates.length} kept
        </span>
      </div>
      <div className="ledger-scroll">
        <table className="ledger retention-table" aria-label={`Candidates in ${label}`}>
          <thead>
            <tr>
              <th scope="col">Snapshot</th>
              <th scope="col">Backed up</th>
              <th scope="col">Verdict</th>
              <th scope="col">Held by</th>
            </tr>
          </thead>
          <tbody>
            {bucket.candidates.map((candidate) => (
              <Candidate
                key={`${candidate.namespace}/${candidate.name}`}
                candidate={candidate}
                unbounded={unbounded}
                now={now}
              />
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

interface CandidateProps {
  candidate: RetentionCandidate;
  unbounded: boolean;
  now: Date;
}

function Candidate({ candidate, unbounded, now }: CandidateProps) {
  const verdict = candidateVerdict(candidate, unbounded);
  return (
    <tr
      data-subject={candidate.subject ? "true" : undefined}
      data-kept={verdict.kept ? "true" : "false"}
      aria-current={candidate.subject ? "true" : undefined}
    >
      <td>
        <div className="retention-table__object">
          <Link
            className="mono"
            to="/snapshots/$namespace/$name"
            params={{ namespace: candidate.namespace, name: candidate.name }}
          >
            {candidate.name}
          </Link>
          {candidate.subject ? (
            <span className="retention-table__subject">this snapshot</span>
          ) : null}
          {candidate.pinned ? (
            <span className="snapshot-table__pin">
              <Pin size={12} strokeWidth={2} aria-hidden="true" />
              pinned
            </span>
          ) : null}
        </div>
      </td>
      <td className="mono retention-table__when">
        <time dateTime={candidate.endTime}>{formatTimestamp(candidate.endTime)}</time>
        <span className="retention-table__age">{humanAge(candidate.endTime, now)} old</span>
      </td>
      <td>
        <LampBadge lamp={verdict.lamp} />
      </td>
      <td className="retention-table__rule">{verdict.rule}</td>
    </tr>
  );
}
