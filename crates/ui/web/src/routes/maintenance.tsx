import { Link, createFileRoute } from "@tanstack/react-router";
import { Wrench } from "lucide-react";
import { useState } from "react";

import { useMaintenance } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { MaintenanceList } from "../components/MaintenanceList";
import { RunDialog } from "../components/actions/RunDialog";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Maintenance — every `Maintenance` resource, its two run tracks, and the
 * control that asks for another run now.
 *
 * # The namespace that decides the grant is this object's
 *
 * A `Maintenance` need not live where the repository it governs does: a
 * `ClusterRepository`'s is placed in `spec.maintenance.namespace` or else in
 * the operator's own namespace. `patchMaintenances` is therefore reviewed for
 * the **row's** namespace, not for the page's scope and not for the
 * repository's (addenda item 17) — which is also why each row asks `/me`
 * separately and TanStack collapses the repeats.
 *
 * # Why this page matters even when everything is green
 *
 * Without maintenance, kopia's indexes are never compacted and content
 * nothing references is never dropped, so a repository grows without bound
 * while every backup on it succeeds. A repository with no `Maintenance` at
 * all is the failure this page exists to make visible, which is why the empty
 * state says so rather than reading as "nothing to do".
 */
export const Route = createFileRoute("/maintenance")({
  component: Maintenance,
});

/** `namespace/name` of the row whose confirmation is open. */
type OpenRow = string | null;

function Maintenance() {
  const namespace = useCurrentNamespace();
  const maintenance = useMaintenance(namespace);
  const [open, setOpen] = useState<OpenRow>(null);
  const scope = namespace ?? "all namespaces";
  const search = namespace !== undefined ? { namespace } : {};

  return (
    <div className="page">
      <p className="page__prose">
        Maintenance is what keeps a kopia repository from growing without bound.{" "}
        <strong>Quick</strong> compacts the indexes and is cheap enough to run often;{" "}
        <strong>full</strong> also drops content nothing references any more, which is what actually
        reclaims space, and is slow and heavy. A repository&apos;s{" "}
        <span className="mono">spec.maintenance</span> projects one of these by default; a
        hand-authored one is honored instead and never rewritten.
      </p>

      {maintenance.isPending ? (
        <section className="page__section" aria-label="Maintenance">
          <LoadingState what="maintenance" rows={6} />
        </section>
      ) : maintenance.isError ? (
        <section className="page__section" aria-label="Maintenance">
          <ErrorState
            problem={maintenance.error.problem}
            what="maintenance"
            onRetry={() => void maintenance.refetch()}
          />
        </section>
      ) : maintenance.data.length === 0 ? (
        <section className="page__section" aria-label="Maintenance">
          <EmptyState
            title={`No maintenance in ${scope}`}
            icon={Wrench}
            action={
              <Link className="button" to="/repositories" search={search}>
                Repositories
              </Link>
            }
          >
            Nothing is compacting indexes or reclaiming space here. That is not a quiet state: a
            repository with no Maintenance grows without bound while every backup on it succeeds. A
            repository&apos;s spec.maintenance projects one by default, so an empty page usually
            means there are no repositories in this scope — or that one has disabled it.
          </EmptyState>
        </section>
      ) : (
        <MaintenanceList
          rows={maintenance.data}
          renderAction={(row) => {
            const id = `${row.namespace}/${row.name}`;
            return (
              <RunDialog
                target={{
                  kind: "maintenance",
                  namespace: row.namespace,
                  name: row.name,
                  repository: row.repository,
                }}
                labelSuffix={row.repository}
                open={open === id}
                onOpenChange={(next) => {
                  setOpen(next ? id : null);
                }}
              />
            );
          }}
        />
      )}
    </div>
  );
}
