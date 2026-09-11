import { createFileRoute } from "@tanstack/react-router";
import { ScrollText } from "lucide-react";

import { usePolicies } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { PolicyTable } from "../components/PolicyTable";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Policies — every `SnapshotPolicy` the caller may see.
 *
 * A policy is the **recipe**: what to back up, where to write it, how long to
 * keep it. It is deliberately not the invocation (`Snapshot`) or the clock
 * (`SnapshotSchedule`), and the three screens stay separate for the same
 * reason the CRDs do — a recipe that has never run and a recipe that runs
 * nightly look identical in their spec, and only the ledger's "last snapshot"
 * column tells them apart.
 *
 * There is no health strip here. `RepositorySummary` carries a `Health` the
 * operator computed; `PolicyRow` carries no phase, no condition and no lamp,
 * so this page reports facts rather than deriving a verdict the operator
 * never published (`components/policy.ts`).
 */
export const Route = createFileRoute("/policies")({
  component: Policies,
});

function Policies() {
  const namespace = useCurrentNamespace();
  const policies = usePolicies(namespace);
  const scope = namespace ?? "all namespaces";

  return (
    <div className="page">
      <p className="page__prose">
        A <span className="mono">SnapshotPolicy</span> is the recipe: which sources to back up,
        which repositories to write them into, how long to keep them and how often to verify them.
        It does not run on its own — a <span className="mono">SnapshotSchedule</span> fires it on a
        cron, and &ldquo;snapshot now&rdquo; on a policy&apos;s own page runs it once. Open a policy
        to see its retention, the schedules that fire it, and what it has produced.
      </p>

      <section className="page__section" aria-label="Policies">
        {policies.isPending ? (
          <LoadingState what="policies" rows={6} />
        ) : policies.isError ? (
          <ErrorState
            problem={policies.error.problem}
            what="policies"
            onRetry={() => void policies.refetch()}
          />
        ) : policies.data.length === 0 ? (
          <EmptyState title={`No policies in ${scope}`} icon={ScrollText}>
            A SnapshotPolicy names the sources to back up and the repository to write them into.
            Create one and it appears here; a SnapshotSchedule then fires it on a cron, or you can
            run it once from its own page.
          </EmptyState>
        ) : (
          <PolicyTable policies={policies.data} />
        )}
      </section>
    </div>
  );
}
