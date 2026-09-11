import { Link, createFileRoute } from "@tanstack/react-router";
import { ArchiveRestore } from "lucide-react";

import { useRestore } from "../api/hooks";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { RestoreDetailView } from "../components/RestoreDetailView";

/**
 * One restore — what it pinned, where it is writing, and why it failed if it
 * did.
 *
 * The route file is `restores_.$namespace.$name.tsx`, with the underscore, so
 * `restores.tsx` does not become a layout this renders inside: the list has
 * no `<Outlet/>`, and without the escape the page is silently blank. The
 * generated route tree is where that is verified, not the browser.
 *
 * There is no refetch interval. A running restore's counters do advance, but
 * the operator's own heartbeat decides how often they land and the list's
 * 15-second stale time already brings them in on a revisit; polling a detail
 * page hard would multiply impersonated reads against the apiserver for a
 * number that moves in minutes.
 */
export const Route = createFileRoute("/restores_/$namespace/$name")({
  component: RestoreDetailRoute,
});

function RestoreDetailRoute() {
  const { namespace, name } = Route.useParams();
  const restore = useRestore(namespace, name);

  if (restore.isPending) {
    return (
      <div className="page">
        <LoadingState what={`the restore ${name}`} rows={8} />
      </div>
    );
  }

  if (restore.isError) {
    return (
      <div className="page">
        <ErrorState
          problem={restore.error.problem}
          what={`the restore ${name}`}
          onRetry={() => void restore.refetch()}
          actions={
            <Link className="button" to="/restores" search={{}}>
              <ArchiveRestore size={14} strokeWidth={2} aria-hidden="true" />
              All restores
            </Link>
          }
        />
      </div>
    );
  }

  return <RestoreDetailView detail={restore.data} />;
}
