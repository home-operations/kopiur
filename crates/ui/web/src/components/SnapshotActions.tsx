import { Camera, Trash2 } from "lucide-react";
import { useState } from "react";

import { useDeleteSnapshot, useSnapshotNow } from "../api/hooks";
import type { SnapshotRow } from "../api/types";
import { ActionButton } from "./ActionButton";
import { ActionResult } from "./ActionResult";
import { useConfirmFocus } from "./actions/useConfirmFocus";
import { deletionConsequence, deletionPolicyLabel } from "./snapshot";
import { useCapabilityReason } from "./useCapabilityReason";

/**
 * The two things an operator does to a snapshot from this screen: ask for
 * another backup under the same policy, and ask for this one to be deleted.
 *
 * Both obey the console's action rules — never hidden, judged in the namespace
 * of the object being written, confirmed before they run, and answered by the
 * server's own `ActionReceipt` with `note` intact. Two things are specific to
 * snapshots and both are about not destroying data by accident.
 *
 * **The delete confirmation names what happens to the kopia snapshot, not to
 * the resource.** Deleting the `Snapshot` is reversible in the sense that
 * matters — you can scan the catalog and find the backup again — *unless*
 * `deletionPolicy` is `Delete`, in which case the finalizer removes the kopia
 * manifest and the restore point is gone. An absent policy is not filled in
 * with a default: `deletionConsequence` reports that the operator decides and
 * names both outcomes (addenda item 19).
 *
 * **The delete is *requested*.** `DELETE …/snapshots/{ns}/{name}` answers 202,
 * and the receipt's `note` is where the repository's mass-deletion breaker
 * says it is holding the request. The button says "Request deletion" and the
 * receipt says "requested", because "Deleted" would be a claim the API never
 * made.
 *
 * **`pin` is asked for, never defaulted.** `SnapshotNowBody.pin` is a required
 * boolean whose consequence is permanent: a pinned snapshot is exempt from GFS
 * pruning entirely, for as long as it exists, so a policy that keeps 7 days
 * keeps this one forever (addenda item 23). It is a checkbox that starts
 * cleared, with the consequence beside it.
 */
export interface SnapshotActionsProps {
  row: SnapshotRow;
}

/** Which confirmation is open. */
type ActionId = "snapshot-now" | "delete";

export function SnapshotActions({ row }: SnapshotActionsProps) {
  const snapshotNow = useSnapshotNow();
  const remove = useDeleteSnapshot();
  const [open, setOpen] = useState<ActionId | null>(null);
  const [pin, setPin] = useState(false);
  // This bar is hand-rolled rather than built from `ActionPanel` (it predates
  // it), so it borrows the shared focus behaviour directly: opening moves
  // focus into the panel, Escape closes it, and closing returns focus to the
  // trigger instead of dropping the reader on `<body>`.
  const close = () => {
    setOpen(null);
  };
  const snapshotNowRef = useConfirmFocus(open === "snapshot-now", close);
  const deleteRef = useConfirmFocus(open === "delete", close);

  const policy = row.policy;
  const hasPolicy = policy !== null && policy !== undefined && policy.length > 0;

  // Both writes land in the snapshot's own namespace: the new `Snapshot` is
  // created beside the policy, and the delete patches this resource. `/me`'s
  // flags are namespace-scoped (addenda item 17), so that is the namespace
  // they are judged in.
  const createReason = useCapabilityReason(row.namespace, "createSnapshots");
  const deleteReason = useCapabilityReason(row.namespace, "deleteSnapshots");
  const noPolicy = hasPolicy
    ? undefined
    : `${row.name} names no SnapshotPolicy, so there is no recipe to run again. A discovered or hand-written snapshot has no policy to snapshot under.`;

  const consequence = deletionConsequence(row.deletionPolicy);

  return (
    <>
      <div className="action-bar">
        <ActionButton
          disabledReason={noPolicy ?? createReason}
          aria-expanded={open === "snapshot-now"}
          onClick={() => {
            setOpen((was) => (was === "snapshot-now" ? null : "snapshot-now"));
          }}
        >
          <Camera size={14} strokeWidth={2} aria-hidden="true" />
          Snapshot now
        </ActionButton>
        <ActionButton
          variant="danger"
          disabledReason={deleteReason}
          aria-expanded={open === "delete"}
          onClick={() => {
            setOpen((was) => (was === "delete" ? null : "delete"));
          }}
        >
          <Trash2 size={14} strokeWidth={2} aria-hidden="true" />
          Delete
        </ActionButton>
      </div>

      {open === "snapshot-now" && hasPolicy ? (
        <div
          className="action__confirm"
          role="group"
          aria-label="Snapshot now"
          ref={snapshotNowRef}
          tabIndex={-1}
        >
          <div className="action__prose">
            <p>
              This creates a new <span className="mono">Snapshot</span> under SnapshotPolicy{" "}
              <span className="mono">
                {row.namespace}/{policy}
              </span>{" "}
              and the operator runs it in a mover Job. It does not touch this snapshot. A
              multi-repository policy fans out to every repository it names.
            </p>
            <div className="action__switch">
              <label htmlFor="snapshot-now-pin">
                <input
                  id="snapshot-now-pin"
                  type="checkbox"
                  checked={pin}
                  onChange={(event) => {
                    setPin(event.target.checked);
                  }}
                />
                Pin the new snapshot
              </label>
              <p className="controls__hint" id="snapshot-now-pin-hint">
                A pin is{" "}
                <strong>permanent and exempts the snapshot from GFS pruning entirely</strong>:
                retention will never remove it, however old it gets and whatever the policy&apos;s
                rules say. Leave it cleared for an ordinary backup; set it for one you intend to
                keep indefinitely, such as a pre-upgrade checkpoint.
              </p>
            </div>
          </div>
          <div className="action__actions">
            <ActionButton
              variant="primary"
              disabledReason={snapshotNow.isPending ? "The request is in flight." : createReason}
              onClick={() => {
                snapshotNow.mutate({
                  namespace: row.namespace,
                  policy,
                  tags: [],
                  pin,
                });
                setOpen(null);
              }}
            >
              {pin ? "Take a pinned snapshot" : "Take a snapshot"}
            </ActionButton>
            <ActionButton
              variant="quiet"
              onClick={() => {
                setOpen(null);
              }}
            >
              Cancel
            </ActionButton>
          </div>
        </div>
      ) : null}

      {open === "delete" ? (
        <div
          className="action__confirm"
          role="group"
          aria-label="Delete"
          ref={deleteRef}
          tabIndex={-1}
        >
          <div className="action__prose">
            <p>
              This requests deletion of the <span className="mono">Snapshot</span> resource{" "}
              <span className="mono">
                {row.namespace}/{row.name}
              </span>
              . What happens to the backup itself is decided by its deletion policy, which is{" "}
              <span className="mono">{deletionPolicyLabel(row.deletionPolicy)}</span>.
            </p>
            <p data-consequence={consequence.known ? "known" : "unknown"}>
              {consequence.destroys ? <strong>{consequence.text}</strong> : consequence.text}
            </p>
            <p>
              The API answers <span className="mono">202</span>: the deletion is <em>requested</em>,
              not performed. The operator batches it into a mover Job, and the repository&apos;s
              mass-deletion breaker may hold it — if it does, the receipt below says so.
            </p>
          </div>
          <div className="action__actions">
            <ActionButton
              variant="danger"
              disabledReason={remove.isPending ? "The request is in flight." : deleteReason}
              onClick={() => {
                remove.mutate({ namespace: row.namespace, name: row.name });
                setOpen(null);
              }}
            >
              Request deletion
            </ActionButton>
            <ActionButton
              variant="quiet"
              onClick={() => {
                setOpen(null);
              }}
            >
              Cancel
            </ActionButton>
          </div>
        </div>
      ) : null}

      <ActionResult
        label="Snapshot now"
        receipt={snapshotNow.data}
        problem={snapshotNow.error?.problem}
      />
      <ActionResult label="Delete" receipt={remove.data} problem={remove.error?.problem} />
    </>
  );
}
