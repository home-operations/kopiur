import { PauseCircle, PlayCircle } from "lucide-react";
import type { ReactNode } from "react";

import { useSuspend } from "../../api/hooks";
import { useCapabilityReason } from "../useCapabilityReason";
import { ActionPanel } from "./ActionPanel";
import { type SuspendableKind, suspendReviewNamespace, suspendable } from "./suspendable";

/**
 * Suspend or resume one object.
 *
 * The value is explicit, never a toggle of whatever the page last read: the
 * body carries `suspend: true` or `suspend: false` (the handler's own words —
 * "two people clicking at once converge instead of undoing each other"), and
 * a flip that asks for the value already in place writes nothing and says so
 * in the receipt's `note`.
 *
 * Everything kind-specific comes from `suspendable.ts`: the token the body
 * sends, the `/me` flag the control is judged against, the namespace that
 * review is asked about, and the spec path the confirmation quotes.
 *
 * `200`, not `202` — the patch is the whole action and it is done when the
 * request returns. That is why this is the one control here whose receipt may
 * honestly be read as an outcome.
 */
export interface SuspendToggleProps {
  kind: SuspendableKind;
  name: string;
  /** The object's own namespace; omitted for a cluster-scoped kind. */
  namespace?: string | undefined;
  /** Its current `spec.suspend` — what the button offers the opposite of. */
  suspended: boolean;
  /**
   * What stops while this object is suspended, in one clause, e.g. "no
   * schedule will fire it". Rendered inside the confirmation's sentence.
   */
  consequence: ReactNode;
  /** Controlled open state, for a bar that allows one open question at a time. */
  open?: boolean | undefined;
  onOpenChange?: ((open: boolean) => void) | undefined;
}

export function SuspendToggle({
  kind,
  name,
  namespace,
  suspended,
  consequence,
  open,
  onOpenChange,
}: SuspendToggleProps) {
  const meta = suspendable(kind);
  const suspend = useSuspend();
  const reason = useCapabilityReason(suspendReviewNamespace(kind, namespace), meta.capability);

  // A cluster-scoped kind has no namespace, whatever the page's scope is: the
  // body must not carry one, the review must not be asked about one, and the
  // confirmation must not claim the object lives in one.
  const scope =
    meta.namespaced && namespace !== undefined && namespace.length > 0 ? namespace : undefined;
  const label = suspended ? "Resume" : "Suspend";

  return (
    <ActionPanel
      label={label}
      icon={suspended ? PlayCircle : PauseCircle}
      // Resuming starts backups again; suspending stops them. Only one of the
      // two is the destructive direction.
      variant={suspended ? "default" : "danger"}
      disabledReason={reason}
      confirmLabel={suspended ? `Resume ${name}` : `Suspend ${name}`}
      running={suspend.isPending}
      onConfirm={() => {
        suspend.mutate({
          kind,
          name,
          suspend: !suspended,
          ...(scope !== undefined ? { namespace: scope } : {}),
        });
      }}
      receipt={suspend.data}
      problem={suspend.error?.problem}
      open={open}
      onOpenChange={onOpenChange}
    >
      <p>
        This sets <span className="mono">{meta.path}</span> to{" "}
        <span className="mono">{suspended ? "false" : "true"}</span> on the {meta.crd}{" "}
        <span className="mono">{name}</span>
        {scope !== undefined ? (
          <>
            {" "}
            in <span className="mono">{scope}</span>
          </>
        ) : null}
        .
      </p>
      {suspended ? (
        <p>
          Work resumes at the next slot. Nothing is caught up retroactively — the windows missed
          while it was suspended are simply gone.
        </p>
      ) : (
        <p>
          While it is suspended, {consequence}. A window that comes and goes meanwhile is not made
          up later. Data already written is untouched.
        </p>
      )}
    </ActionPanel>
  );
}
