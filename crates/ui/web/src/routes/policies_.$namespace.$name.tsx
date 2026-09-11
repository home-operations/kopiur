import { Link, createFileRoute } from "@tanstack/react-router";
import { ScrollText } from "lucide-react";
import { useState } from "react";

import { usePolicy } from "../api/hooks";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { PolicyDetail } from "../components/PolicyDetail";
import { SnapshotNowDialog } from "../components/actions/SnapshotNowDialog";
import { SuspendToggle } from "../components/actions/SuspendToggle";

/**
 * One policy — the recipe, what fires it, and what it has produced.
 *
 * # The route file's name is load-bearing
 *
 * `policies_.$namespace.$name.tsx`, with the underscore. Without it,
 * `policies.tsx` becomes a *layout* that this route renders inside — and the
 * list has no `<Outlet/>`, so the detail would render nothing at all, with no
 * error anywhere. The repositories detail hit this first; the generated route
 * tree is the thing to check, not the page.
 *
 * # One open question at a time
 *
 * The two controls share an `open` state so a reader is never asked two
 * questions at once — the same rule the repository action bar keeps, for the
 * same reason: being asked two questions at once is how the wrong one gets
 * answered.
 */
export const Route = createFileRoute("/policies_/$namespace/$name")({
  component: PolicyDetailRoute,
});

/** Which confirmation is open, if any. */
type OpenAction = "snapshot" | "suspend" | null;

function PolicyDetailRoute() {
  const { namespace, name } = Route.useParams();
  const policy = usePolicy(namespace, name);
  const [open, setOpen] = useState<OpenAction>(null);

  if (policy.isPending) {
    return (
      <div className="page">
        <LoadingState what={`the policy ${name}`} rows={8} />
      </div>
    );
  }

  if (policy.isError) {
    return (
      <div className="page">
        <ErrorState
          problem={policy.error.problem}
          what={`the policy ${name}`}
          onRetry={() => void policy.refetch()}
          actions={
            <Link className="button" to="/policies" search={{}}>
              <ScrollText size={14} strokeWidth={2} aria-hidden="true" />
              All policies
            </Link>
          }
        />
      </div>
    );
  }

  const { row } = policy.data;

  return (
    <PolicyDetail
      detail={policy.data}
      actions={
        <>
          <SnapshotNowDialog
            namespace={row.namespace}
            policy={row.name}
            repositories={row.repositories}
            open={open === "snapshot"}
            onOpenChange={(next) => {
              setOpen(next ? "snapshot" : null);
            }}
          />
          <SuspendToggle
            kind="policy"
            name={row.name}
            namespace={row.namespace}
            suspended={row.suspended}
            consequence="no schedule will fire it, and the sources it names go unbacked-up"
            open={open === "suspend"}
            onOpenChange={(next) => {
              setOpen(next ? "suspend" : null);
            }}
          />
        </>
      }
    />
  );
}
