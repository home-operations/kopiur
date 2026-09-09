import { Link, createFileRoute } from "@tanstack/react-router";
import { Database } from "lucide-react";

import { useRepositories } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { RepositoryTable } from "../components/RepositoryTable";
import { StatusCards } from "../components/StatusCards";
import { HEALTH_ORDER, type HealthKey, healthLamp } from "../components/health";
import { filterByHealth, isHealthKey } from "../components/repository";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Repositories — every `Repository` and `ClusterRepository` the caller may
 * see, in one ledger.
 *
 * `?health=` is a live contract, not a nicety: the overview's health strip
 * links straight here with it (`StatusCards` → `RepositoriesFilter`), so this
 * route has to accept the key, filter on it, and — when the value is not one
 * of the six lamps — say so rather than 404 or quietly ignore it.
 *
 * The strip is repeated at the top of this page as the filter's own control.
 * Its counts are always the whole fleet's, never the filtered set's: a strip
 * whose numbers moved with the filter could not be used to get back out of
 * one.
 */
export interface RepositoriesSearch {
  /**
   * The lamp to filter to — kept as a plain `string`, not narrowed to
   * `HealthKey`, so a stale or hand-edited link keeps its value and can be
   * quoted back to the reader. `isHealthKey` is the gate on what actually
   * filters; the same split doctor's window controls use.
   */
  health?: string;
}

/** One `?health=` as the reader wrote it. */
function healthParam(value: unknown): string | undefined {
  if (typeof value === "string") {
    const text = value.trim();
    return text.length > 0 ? text : undefined;
  }
  // The router parses a bare number or `true` before this sees it; rendered
  // back as written so the message names the reader's own value.
  return typeof value === "number" || typeof value === "boolean" ? String(value) : undefined;
}

export const Route = createFileRoute("/repositories")({
  validateSearch: (search: Record<string, unknown>): RepositoriesSearch => {
    const health = healthParam(search.health);
    return health === undefined ? {} : { health };
  },
  component: Repositories,
});

function Repositories() {
  const namespace = useCurrentNamespace();
  // Re-read raw: the router hands the component the parsed value for a key
  // `validateSearch` dropped, so the declared `string` is not something render
  // may rely on (the same trap doctor's windows fell into).
  const search: Record<string, unknown> = Route.useSearch();
  const asked = healthParam(search.health);
  const health: HealthKey | undefined =
    asked !== undefined && isHealthKey(asked) ? asked : undefined;
  const unusable = asked !== undefined && health === undefined ? asked : undefined;

  const repositories = useRepositories(namespace);
  const scope = namespace ?? "all namespaces";
  const all = repositories.data ?? [];
  const rows = filterByHealth(all, health);
  const clear = namespace !== undefined ? { namespace } : {};

  return (
    <div className="page">
      <p className="page__prose">
        A repository is where backups actually live — a kopia repository on object storage, a
        filesystem or a repository server. <span className="mono">Repository</span> is namespaced
        and <span className="mono">ClusterRepository</span> is cluster-scoped; both are listed here
        because &ldquo;my repositories&rdquo; is one question. Health is the operator&apos;s own
        verdict over phase, suspension and any gate holding the repository back; open a row to see
        which gate.
      </p>

      {repositories.data !== undefined && repositories.data.length > 0 ? (
        <section className="page__section" aria-label="Repositories by health">
          <StatusCards repositories={all} namespace={namespace} />
          {health !== undefined ? (
            <p className="page__section-note" role="status">
              Showing the {healthLamp(health).word.toLowerCase()} repositories in {scope}.{" "}
              <Link to="/repositories" search={clear}>
                Show all {all.length}
              </Link>
            </p>
          ) : null}
        </section>
      ) : null}

      {unusable !== undefined ? (
        <p className="page__prose" role="status">
          This link asked for repositories whose health is <span className="mono">{unusable}</span>,
          which is not one of the six this console knows ({HEALTH_ORDER.join(", ")}), so nothing was
          filtered out.
        </p>
      ) : null}

      <section className="page__section" aria-label="Repositories">
        {repositories.isPending ? (
          <LoadingState what="repositories" rows={6} />
        ) : repositories.isError ? (
          <ErrorState
            problem={repositories.error.problem}
            what="repositories"
            onRetry={() => void repositories.refetch()}
          />
        ) : all.length === 0 ? (
          <EmptyState title={`No repositories in ${scope}`} icon={Database}>
            A Repository or ClusterRepository is the kopia repository backups land in. Create one
            and it appears here with its health; a SnapshotPolicy then names it as the target its
            snapshots are written to.
          </EmptyState>
        ) : rows.length === 0 ? (
          <EmptyState
            title={`No ${healthLamp(health ?? "unknown").word.toLowerCase()} repositories in ${scope}`}
            icon={Database}
            action={
              <Link className="button" to="/repositories" search={clear}>
                Show all {all.length}
              </Link>
            }
          >
            The fleet has {all.length} {all.length === 1 ? "repository" : "repositories"} in this
            scope, none of them on this lamp. Nothing is wrong with the filter — there is simply
            nothing under it.
          </EmptyState>
        ) : (
          <RepositoryTable repositories={rows} />
        )}
      </section>
    </div>
  );
}
