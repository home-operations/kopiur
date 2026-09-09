import { Link, createFileRoute } from "@tanstack/react-router";
import { Database, Square } from "lucide-react";

import { useEndRepositorySession, useRepository } from "../api/hooks";
import type { SessionInfo } from "../api/types";
import { ActionButton } from "../components/ActionButton";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { RepositoryActions } from "../components/RepositoryActions";
import { RepositoryDetail } from "../components/RepositoryDetail";
import { useCapabilityReason } from "../components/useCapabilityReason";
import { namespaceFromSearch } from "../util/namespace";

/**
 * One repository — the screen an operator opens when the fleet list says this
 * one is unhealthy and they need to know why and what to do.
 *
 * The `{kind}` segment is the summary's `kindPath` (`repository` /
 * `cluster-repository`), the same token the read route, the session-delete
 * route and the two action bodies all take (addenda item 16). It is never
 * derived from the display kind, and it is not re-spelled here: whatever the
 * URL carries is passed to the API, so a link built from a row and a link
 * pasted by hand behave identically.
 *
 * `?namespace=` is the repository's own namespace, and a namespaced
 * `Repository` genuinely needs it: without one the handler answers a 400
 * `namespace-required` explaining that a name alone does not identify one
 * object. That refusal is rendered as the problem it is — what, why and the
 * fix — rather than short-circuited here into a blank page, because the
 * server's sentence is better than anything this route could invent.
 */
export const Route = createFileRoute("/repositories_/$kind/$name")({
  component: RepositoryDetailRoute,
});

function RepositoryDetailRoute() {
  const { kind, name } = Route.useParams();
  // The namespace is the root route's shared `?namespace=`; read through the
  // same helper every other route uses so an empty value means "none".
  const search: unknown = Route.useSearch();
  const namespace = namespaceFromSearch(search);
  const repository = useRepository(kind, name, namespace);

  if (repository.isPending) {
    return (
      <div className="page">
        <LoadingState what={`the repository ${name}`} rows={8} />
      </div>
    );
  }

  if (repository.isError) {
    return (
      <div className="page">
        <ErrorState
          problem={repository.error.problem}
          what={`the repository ${name}`}
          onRetry={() => void repository.refetch()}
          actions={
            <Link className="button" to="/repositories" search={{}}>
              <Database size={14} strokeWidth={2} aria-hidden="true" />
              All repositories
            </Link>
          }
        />
      </div>
    );
  }

  return (
    <RepositoryDetail
      detail={repository.data}
      actions={<RepositoryActions detail={repository.data} />}
      renderSessionAction={(session) => (
        <StopSession session={session} kindPath={kind} name={name} namespace={namespace} />
      )}
    />
  );
}

interface StopSessionProps {
  session: SessionInfo;
  kindPath: string;
  name: string;
  namespace: string | undefined;
}

/**
 * Stop one browse session.
 *
 * The grant is judged in the **session Job's** namespace, not the
 * repository's: a `ClusterRepository`'s session lives wherever the snapshot
 * being browsed does, which may be neither the repository's namespace nor the
 * page's scope (addenda item 17).
 *
 * `DELETE …/session` answers `204` with no body, so there is no receipt to
 * render — the session simply leaves the list on the next read.
 */
function StopSession({ session, kindPath, name, namespace }: StopSessionProps) {
  const end = useEndRepositorySession();
  const reason = useCapabilityReason(session.namespace, "deleteSessionJobs");
  return (
    <>
      <ActionButton
        variant="quiet"
        disabledReason={end.isPending ? "The request is in flight." : reason}
        onClick={() => {
          end.mutate({
            kindPath,
            name,
            sessionNamespace: session.namespace,
            ...(namespace !== undefined ? { namespace } : {}),
          });
        }}
      >
        <Square size={14} strokeWidth={2} aria-hidden="true" />
        Stop session
      </ActionButton>
      {end.isError ? (
        <span className="page__section-note" role="alert">
          {end.error.problem.what} {end.error.problem.fix}
        </span>
      ) : null}
    </>
  );
}
