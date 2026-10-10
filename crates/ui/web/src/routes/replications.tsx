import { createFileRoute } from "@tanstack/react-router";
import { ArrowLeftRight } from "lucide-react";

import { useReplications } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { ReplicationTable } from "../components/ReplicationTable";
import { replicationRows } from "../components/replication";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Replications — every copy out of a repository, of either kind, in one
 * ledger.
 *
 * `RepositoryReplication` syncs a repository's blobs to a bare backend;
 * `SnapshotReplication` migrates selected snapshots between repository CRs.
 * The API keeps them apart because their rows genuinely differ, but the
 * operator's question — "is anything falling behind?" — is one question, so
 * they are read together here and the kind survives as the label strip's
 * KIND.
 */
export const Route = createFileRoute("/replications")({
  component: Replications,
});

function Replications() {
  const namespace = useCurrentNamespace();
  const replications = useReplications(namespace);
  const scope = namespace ?? "all namespaces";
  const rows = replications.data === undefined ? [] : replicationRows(replications.data);

  return (
    <div className="page">
      <p className="page__prose">
        A replication copies a repository to a second backend, or selected snapshots into another
        repository, on a schedule. Watch <strong>Last replicated</strong>: a copy that is overdue is
        a copy you do not have. Its bar is the age of the last copy, on one scale for every row.
      </p>

      <section className="page__section" aria-label="Replications">
        {replications.isPending ? (
          <LoadingState what="replications" rows={5} />
        ) : replications.isError ? (
          <ErrorState
            problem={replications.error.problem}
            what="replications"
            onRetry={() => void replications.refetch()}
          />
        ) : rows.length === 0 ? (
          <EmptyState title={`No replications in ${scope}`} icon={ArrowLeftRight}>
            Nothing is copying a repository elsewhere. Create a RepositoryReplication or a
            SnapshotReplication to keep a second copy of your backups.
          </EmptyState>
        ) : (
          <ReplicationTable rows={rows} />
        )}
      </section>

      <p className="page__prose">
        &ldquo;Next run&rdquo; reads <em>not reported</em> because the operator does not publish a
        next-run time for replications. The cron expression says when the next copy is due.
      </p>
    </div>
  );
}
