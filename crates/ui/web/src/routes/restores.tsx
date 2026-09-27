import { createFileRoute } from "@tanstack/react-router";
import { ArchiveRestore } from "lucide-react";

import { useRestores } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { RestoreTable } from "../components/RestoreTable";
import { RestoreDialog } from "../components/actions/RestoreDialog";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Restores — every `Restore` in scope, and the button that creates one.
 *
 * # Why the create control needs a namespace and the list does not
 *
 * The ledger is happy cluster-wide: it reads whatever the caller may see. A
 * restore, though, is *created* somewhere — it writes into a claim its mover
 * Job can mount, and that Job runs where the `Restore` is — so the dialog
 * cannot be offered without a namespace to create in. With none chosen the
 * page says which scope to pick rather than offering a control that would
 * 400, and the scope is the shell's own `?namespace=`.
 */
export const Route = createFileRoute("/restores")({
  component: Restores,
});

function Restores() {
  const namespace = useCurrentNamespace();
  const restores = useRestores(namespace);
  const scope = namespace ?? "all namespaces";

  return (
    <div className="page">
      <p className="page__prose">
        A <span className="mono">Restore</span> writes one snapshot into a{" "}
        <span className="mono">PersistentVolumeClaim</span>. Its source is fixed when it is created;
        open one to see which snapshot it uses.
      </p>

      <section className="page__section" aria-label="Actions">
        {namespace !== undefined ? (
          <div className="action-bar">
            <RestoreDialog namespace={namespace} />
          </div>
        ) : (
          <p className="page__section-note">
            A restore lives in the namespace of the claim it writes. Scope this page to a namespace
            to create one.
          </p>
        )}
      </section>

      <section className="page__section" aria-label="Restores">
        {restores.isPending ? (
          <LoadingState what="restores" rows={6} />
        ) : restores.isError ? (
          <ErrorState
            problem={restores.error.problem}
            what="restores"
            onRetry={() => void restores.refetch()}
          />
        ) : restores.data.length === 0 ? (
          <EmptyState title={`No restores in ${scope}`} icon={ArchiveRestore}>
            Nothing has been restored here. A Restore names a snapshot and the claim to write it
            into.
          </EmptyState>
        ) : (
          <RestoreTable restores={restores.data} />
        )}
      </section>
    </div>
  );
}
