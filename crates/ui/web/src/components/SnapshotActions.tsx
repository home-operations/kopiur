import { Camera, Trash2 } from "lucide-react";
import { type ReactNode, useState } from "react";

import { useDeleteSnapshot, useSnapshotNow } from "../api/hooks";
import type { SnapshotRow } from "../api/types";
import { ActionButton } from "./ActionButton";
import { Popover } from "./Popover";
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
  /** More controls for the end of the bar — the drawer's "Browse files". */
  extra?: ReactNode;
}

/** Which confirmation is open. */
type ActionId = "snapshot-now" | "delete";

export function SnapshotActions({ row, extra }: SnapshotActionsProps) {
  const snapshotNow = useSnapshotNow();
  const remove = useDeleteSnapshot();
  const [open, setOpen] = useState<ActionId | null>(null);
  // Unanswered until the reader chooses: a pin is permanent, so neither
  // answer is a default (rule 7 of the kopiur-ui-design skill).
  const [pin, setPin] = useState<"" | "pin" | "prune">("");
  // Each question is a popover on its button: opening moves focus into it,
  // Escape or a click elsewhere closes it, and closing hands focus back to
  // the button. Only one is open at a time.
  const openChange = (id: ActionId) => (next: boolean) => {
    setOpen((was) => (next ? id : was === id ? null : was));
  };

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
        <Popover
          label="Snapshot now"
          open={open === "snapshot-now" && hasPolicy}
          onOpenChange={openChange("snapshot-now")}
          className="action__popover"
          trigger={(props) => (
            <ActionButton disabledReason={noPolicy ?? createReason} {...props}>
              <Camera size={14} strokeWidth={2} aria-hidden="true" />
              Snapshot now
            </ActionButton>
          )}
        >
          {(close) => (
            <>
              <div className="action__prose">
                <p>
                  This creates a new <span className="mono">Snapshot</span> under SnapshotPolicy{" "}
                  <span className="mono">
                    {row.namespace}/{policy}
                  </span>
                  . It does not touch this snapshot.
                </p>
                <fieldset className="action__choice">
                  <legend>Retention</legend>
                  <label htmlFor="snapshot-now-prune">
                    <input
                      type="radio"
                      id="snapshot-now-prune"
                      name="snapshot-now-pin"
                      value="prune"
                      checked={pin === "prune"}
                      onChange={() => {
                        setPin("prune");
                      }}
                    />
                    <span>
                      Prune it under the policy&apos;s retention, like any other snapshot (
                      <span className="mono">spec.pin</span> unset).
                    </span>
                  </label>
                  <label htmlFor="snapshot-now-pin" data-danger="true">
                    <input
                      type="radio"
                      id="snapshot-now-pin"
                      name="snapshot-now-pin"
                      value="pin"
                      checked={pin === "pin"}
                      onChange={() => {
                        setPin("pin");
                      }}
                    />
                    <span>
                      Pin it — a pin is <strong>permanent</strong>: retention never removes a pinned
                      snapshot (<span className="mono">spec.pin: true</span>).
                    </span>
                  </label>
                </fieldset>
              </div>
              <div className="action__actions">
                <ActionButton
                  variant="primary"
                  disabledReason={
                    snapshotNow.isPending
                      ? "The request is in flight."
                      : (createReason ??
                        (pin === "" ? "Choose whether to pin it first." : undefined))
                  }
                  reasonKind={createReason !== undefined ? "refused" : "blocked"}
                  onClick={() => {
                    if (policy === null || policy === undefined) return;
                    snapshotNow.mutate({
                      namespace: row.namespace,
                      policy,
                      tags: [],
                      pin: pin === "pin",
                    });
                    close();
                  }}
                >
                  {pin === "pin" ? "Take a pinned snapshot" : "Take a snapshot"}
                </ActionButton>
                <ActionButton
                  variant="quiet"
                  onClick={() => {
                    close();
                  }}
                >
                  Cancel
                </ActionButton>
              </div>
            </>
          )}
        </Popover>
        <Popover
          label="Delete"
          open={open === "delete"}
          onOpenChange={openChange("delete")}
          className="action__popover"
          trigger={(props) => (
            <ActionButton variant="danger" disabledReason={deleteReason} {...props}>
              <Trash2 size={14} strokeWidth={2} aria-hidden="true" />
              Delete
            </ActionButton>
          )}
        >
          {(close) => (
            <>
              <div className="action__prose">
                <p>
                  This requests deletion of the <span className="mono">Snapshot</span> resource{" "}
                  <span className="mono">
                    {row.namespace}/{row.name}
                  </span>
                  . Its deletion policy is{" "}
                  <span className="mono">{deletionPolicyLabel(row.deletionPolicy)}</span>.
                </p>
                <p data-consequence={consequence.known ? "known" : "unknown"}>
                  {consequence.destroys ? <strong>{consequence.text}</strong> : consequence.text}
                </p>
                <p>
                  The deletion is <em>requested</em>, not done yet. If the repository&apos;s
                  mass-deletion breaker holds it, the answer says so.
                </p>
              </div>
              <div className="action__actions">
                <ActionButton
                  variant="danger"
                  disabledReason={remove.isPending ? "The request is in flight." : deleteReason}
                  onClick={() => {
                    remove.mutate({ namespace: row.namespace, name: row.name });
                    close();
                  }}
                >
                  Request deletion
                </ActionButton>
                <ActionButton
                  variant="quiet"
                  onClick={() => {
                    close();
                  }}
                >
                  Cancel
                </ActionButton>
              </div>
            </>
          )}
        </Popover>
        {extra}
      </div>
    </>
  );
}
