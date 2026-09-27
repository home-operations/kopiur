import { Link, createFileRoute } from "@tanstack/react-router";
import { Camera } from "lucide-react";

import { useSnapshot, useSnapshotRetention } from "../api/hooks";
import { isNotPermitted, problemKind } from "../api/problem";
import type { Problem, SnapshotDetail as SnapshotDetailData } from "../api/types";
import { ErrorState, NotPermittedState } from "../components/ErrorState";
import { Finding } from "../components/Finding";
import { LoadingState } from "../components/LoadingState";
import { RetentionPreview } from "../components/RetentionPreview";
import { SnapshotActions } from "../components/SnapshotActions";
import { SnapshotDetail } from "../components/SnapshotDetail";
import { healthLamp } from "../components/health";
import { snapshotPhaseLamp } from "../components/snapshot";
import { relativeTime } from "../util/format";

/**
 * One snapshot — what it is, what it did, and whether retention is about to
 * take it away.
 *
 * The route file is named with the **escaped** form,
 * `snapshots_.$namespace.$name.tsx`. Without the underscore the list route
 * becomes this route's layout, and since the list renders no `<Outlet/>` the
 * detail would render nothing at all, with no error anywhere — a blank page.
 * The generated tree was read back to confirm this route's parent is the root.
 *
 * **The full retention plan is opt-in, and the opt-in is in the URL.** The
 * plan is a different question with a different cost: for a fan-out policy it
 * is every child of the policy, and `GET …/retention` says so in its own doc —
 * "the detail screen should not pay for it on every open". So the detail always
 * shows the cheap single verdict that rides on `SnapshotDetail`, and
 * `?retention=open` asks for the bucketed plan. Being a search key rather than
 * component state means the expanded view is a link, which is what an operator
 * wants to paste when they are arguing about a restore point.
 */
export interface SnapshotDetailSearch {
  retention?: string;
}

export const Route = createFileRoute("/snapshots_/$namespace/$name")({
  validateSearch: (search: Record<string, unknown>): SnapshotDetailSearch =>
    search.retention === "open" ? { retention: "open" } : {},
  component: SnapshotDetailRoute,
});

function SnapshotDetailRoute() {
  const { namespace, name } = Route.useParams();
  const search: Record<string, unknown> = Route.useSearch();
  const planOpen = search.retention === "open";
  const snapshot = useSnapshot(namespace, name);

  if (snapshot.isPending) {
    return (
      <div className="page">
        <LoadingState what={`the snapshot ${name}`} rows={8} />
      </div>
    );
  }

  if (snapshot.isError) {
    return (
      <div className="page">
        <ErrorState
          problem={snapshot.error.problem}
          what={`the snapshot ${name}`}
          onRetry={() => void snapshot.refetch()}
          actions={
            <Link className="button" to="/snapshots" search={{}}>
              <Camera size={14} strokeWidth={2} aria-hidden="true" />
              All snapshots
            </Link>
          }
        />
      </div>
    );
  }

  return (
    <SnapshotDetail
      detail={snapshot.data}
      actions={<SnapshotActions row={snapshot.data.row} />}
      retention={
        <Retention detail={snapshot.data} namespace={namespace} name={name} planOpen={planOpen} />
      }
    />
  );
}

interface RetentionProps {
  detail: SnapshotDetailData;
  namespace: string;
  name: string;
  planOpen: boolean;
}

/**
 * The cheap verdict, then the full plan on request.
 *
 * `SnapshotDetail.retentionPreview` is a single kept/pruned answer for this one
 * snapshot and rides on the detail payload; the bucketed view of its
 * competitors is a separate read (addenda item 21). The two never disagree —
 * both come from `kopiur_api::retention` — so the verdict here is shown
 * whether or not the plan is open.
 */
function Retention({ detail, namespace, name, planOpen }: RetentionProps) {
  const plan = useSnapshotRetention(namespace, name, { enabled: planOpen });
  const { retentionPreview: preview } = detail;

  return (
    <>
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

      {planOpen ? (
        <PlanSection plan={plan} namespace={namespace} name={name} />
      ) : (
        <p className="page__section-note">
          <Link
            to="/snapshots/$namespace/$name"
            params={{ namespace, name }}
            search={{ retention: "open" }}
          >
            Show the full retention plan
          </Link>{" "}
          — every snapshot in the same buckets, and whether each is kept.
        </p>
      )}
    </>
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
      fix="open the full retention plan below to see which case applies"
      lamp={lamp}
    />
  );
}

interface PlanSectionProps {
  plan: ReturnType<typeof useSnapshotRetention>;
  namespace: string;
  name: string;
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
function PlanSection({ plan, namespace, name }: PlanSectionProps) {
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
  return (
    <>
      <RetentionPreview plan={plan.data} />
      <p className="page__section-note">
        <Link to="/snapshots/$namespace/$name" params={{ namespace, name }} search={{}}>
          Hide the plan
        </Link>
      </p>
    </>
  );
}

/** The three 422s that explain rather than fail. */
function isExplanation(problem: Problem): boolean {
  const kind = problemKind(problem);
  return (
    kind === "no-retention-policy" || kind === "not-retention-governed" || kind === "plan-too-large"
  );
}
