import { Trash2 } from "lucide-react";

import { useDeleteSnapshot } from "../../api/hooks";
import { useCapabilityReason } from "../useCapabilityReason";
import { ActionPanel } from "./ActionPanel";
import { deletionConsequence } from "./deletion";

/**
 * Delete one `Snapshot` resource.
 *
 * # Requested, not done
 *
 * `DELETE /snapshots/{ns}/{name}` answers `202`, never `204`: the object
 * usually still exists when the request returns, because the kopia
 * manifest's fate is decided by the CR's finalizer rather than by the
 * apiserver's cascade. So the confirmation is worded as a request and the
 * receipt says "requested".
 *
 * # The receipt's silence is not evidence
 *
 * The server re-reads the object once and, if the repository's mass-deletion
 * breaker is holding it, puts that in the receipt's `note`. But the re-read
 * races the controller and usually loses — `DeletionHeld` is stamped by a
 * *later* reconcile — so on a first delete the note is almost always absent
 * and means nothing at all. A present note is trustworthy; an absent one is
 * not a statement that the deletion will proceed. The panel says so, and
 * points at the snapshot's own conditions for the real answer.
 *
 * # What the deletion costs is `deletionPolicy`'s answer, not ours
 *
 * `deletion.ts` turns the field into the sentence, including the case where
 * it is unset — which must not be rendered as `Delete`.
 */
export interface ConfirmDeleteProps {
  namespace: string;
  name: string;
  /** `spec.deletionPolicy`; absent means the operator decides at delete time. */
  deletionPolicy?: string | null | undefined;
  /** `status.pinned` — a pinned manifest is exempt from GFS pruning, not from this. */
  pinned?: boolean | undefined;
  open?: boolean | undefined;
  onOpenChange?: ((open: boolean) => void) | undefined;
}

export function ConfirmDelete({
  namespace,
  name,
  deletionPolicy,
  pinned = false,
  open,
  onOpenChange,
}: ConfirmDeleteProps) {
  const remove = useDeleteSnapshot();
  const reason = useCapabilityReason(namespace, "deleteSnapshots");
  const consequence = deletionConsequence(deletionPolicy);

  return (
    <ActionPanel
      label="Delete"
      icon={Trash2}
      variant="danger"
      disabledReason={reason}
      confirmLabel="Request the deletion"
      running={remove.isPending}
      onConfirm={() => {
        remove.mutate({ namespace, name });
      }}
      receipt={remove.data}
      problem={remove.error?.problem}
      open={open}
      onOpenChange={onOpenChange}
    >
      <p>
        This deletes the <span className="mono">Snapshot</span> resource{" "}
        <span className="mono">
          {namespace}/{name}
        </span>
        .
      </p>
      <p data-destructive={consequence.destructive ? "true" : undefined}>
        <span className="mono">deletionPolicy: {consequence.policy}</span> — {consequence.text}
      </p>
      {pinned ? (
        <p>
          The kopia manifest is pinned. A pin exempts a snapshot from GFS pruning; it does not
          protect it from a deletion you ask for here.
        </p>
      ) : null}
      <p>
        The request is accepted, not completed: the finalizer does the work afterwards, and a
        repository&apos;s mass-deletion breaker can hold it. If the receipt says nothing about a
        hold, that is not a promise there is none — the snapshot&apos;s own conditions are the
        answer.
      </p>
    </ActionPanel>
  );
}
