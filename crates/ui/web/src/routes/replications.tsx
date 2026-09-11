import { createFileRoute } from "@tanstack/react-router";
import { ArrowLeftRight, Hourglass } from "lucide-react";

import { useReplications } from "../api/hooks";
import { ReplicationLagChart } from "../charts/ReplicationLagChart";
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
        A replication is a scheduled copy out of a repository — either the whole repository&apos;s
        blobs synced to a second backend, or selected snapshots migrated into another repository.
        The column that matters is <strong>Last replicated</strong>: a copy that has not run since
        its schedule says it should have is a second copy you do not actually have.
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
            Nothing is copying a repository elsewhere. A RepositoryReplication mirrors a
            repository&apos;s blobs to a second backend; a SnapshotReplication copies chosen
            snapshots into another repository. Either one turns a single repository into two places
            the data lives.
          </EmptyState>
        ) : (
          <ReplicationTable rows={rows} />
        )}
      </section>

      {rows.length > 0 ? (
        <section className="page__section" aria-label="Replication lag">
          <div className="page__section-head">
            <h2>
              <Hourglass size={16} strokeWidth={2} aria-hidden="true" />
              Replication lag
            </h2>
          </div>
          <p className="page__section-note">
            The same rows, ordered by how long it has been since each one last finished a copy.
          </p>
          <ReplicationLagChart rows={rows} />
        </section>
      ) : null}

      <p className="page__prose">
        &ldquo;Next run&rdquo; reads <em>not reported</em> for every row, and that is the
        operator&apos;s gap rather than this screen&apos;s:{" "}
        <span className="mono">RepositoryReplication.status.nextScheduledAt</span> is declared and
        written by nothing, and <span className="mono">SnapshotReplication</span> publishes no
        next-run time at all. The cron expression is what says when the next copy is due.
      </p>
    </div>
  );
}
