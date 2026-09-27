import { Link, createFileRoute } from "@tanstack/react-router";
import { ShieldAlert } from "lucide-react";

import { useGates } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { GateList } from "../components/GateList";
import { LoadingState } from "../components/LoadingState";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Gates — the operator's registry of structural gates, so a parked object
 * can be explained in the operator's own words, including a gate this
 * bundle has never seen fire. Reached from the doctor page; not a rail
 * section. The registry is a compile-time constant on the server, so it is
 * cached for five minutes and never 403s today (addenda item 23) — the
 * states are still all wired so the route degrades like every other read.
 */
export const Route = createFileRoute("/gates")({
  component: Gates,
});

function Gates() {
  const namespace = useCurrentNamespace();
  const gates = useGates();
  return (
    <div className="page">
      <p className="page__prose">
        A structural gate is a condition the operator writes on an object it cannot proceed with — a
        mover the namespace has not permitted, a credential Secret that is missing, a mass-deletion
        breaker that has tripped. A gate never self-heals: the object parks with the condition below
        until a human acts. Each row is the condition&apos;s type, the status value that means
        blocked (some gates block on <span className="mono">False</span>, some on{" "}
        <span className="mono">True</span>), the reason the reconciler stamps, and the kinds it
        appears on. The{" "}
        <Link to="/doctor" search={namespace !== undefined ? { namespace } : {}}>
          doctor report
        </Link>{" "}
        names any object currently parked on one.
      </p>
      <section className="page__section" aria-label="Structural gates">
        {gates.isPending ? (
          <LoadingState what="the gate registry" rows={8} />
        ) : gates.isError ? (
          <ErrorState
            problem={gates.error.problem}
            what="the gate registry"
            onRetry={() => void gates.refetch()}
          />
        ) : gates.data.length === 0 ? (
          <EmptyState title="No gates registered" icon={ShieldAlert}>
            The server published an empty registry. Every gate the operator can raise would be
            listed here; an empty list means this server has none to explain, not that nothing is
            parked.
          </EmptyState>
        ) : (
          <GateList gates={gates.data} />
        )}
      </section>
    </div>
  );
}
