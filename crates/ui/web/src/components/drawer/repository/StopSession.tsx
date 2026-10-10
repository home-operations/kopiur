import { Square } from "lucide-react";

import { useEndRepositorySession } from "../../../api/hooks";
import type { SessionInfo } from "../../../api/types";
import { ActionButton } from "../../ActionButton";
import { useCapabilityReason } from "../../useCapabilityReason";

export interface StopSessionProps {
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
export function StopSession({ session, kindPath, name, namespace }: StopSessionProps) {
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
