import { Camera } from "lucide-react";
import { useId, useState } from "react";

import { useSnapshotNow } from "../../api/hooks";
import type { SnapshotNowBody } from "../../api/types";
import { useCapabilityReason } from "../useCapabilityReason";
import { repositoryName } from "../policy";
import { ActionPanel } from "./ActionPanel";

/**
 * Take a snapshot now, under an existing `SnapshotPolicy`.
 *
 * The policy is the recipe and this is one invocation of it: the server reads
 * the policy live and mints the same fan-out cells a schedule slot would, so
 * a multi-repository or `pvcSelector` policy creates several `Snapshot`
 * resources from one click. `201` with every one of them, which the receipt
 * names.
 *
 * # `pin` is asked, never assumed
 *
 * `SnapshotNowBody.pin` is a required boolean, and the consequence of `true`
 * is **permanent**: the kopia manifest carries a retention pin and GFS
 * pruning skips it for good. A checkbox that starts unticked is a default; a
 * checkbox that starts ticked is worse. So it is an unanswered pair of radios
 * and the confirm button is blocked, with the reason, until one is chosen
 * (addenda item 23).
 *
 * # What is deliberately not on the form
 *
 * `tags` is sent empty. The field is required on the wire and takes kopia
 * `(key, value)` pairs that become part of what the snapshot is addressed by;
 * an editor for them is a screen of its own, and an empty list is the honest
 * "none" rather than a guess. `deletionPolicy` is not offered at all — the
 * handler leaves it `None` on purpose so the operator's origin-aware default
 * decides the kopia object's fate, and a form that overrode it would be
 * choosing for a snapshot nobody has looked at yet.
 */
export interface SnapshotNowDialogProps {
  /** The policy's namespace — also the `Snapshot`'s, and the review's. */
  namespace: string;
  /** `metadata.name` of the `SnapshotPolicy`. */
  policy: string;
  /**
   * The repositories the policy writes into, as `PolicyRow.repositories`
   * spells them (`Repository/ns/name`). A select appears when there is more
   * than one, because that is exactly when the fan-out can be narrowed.
   */
  repositories?: readonly string[] | undefined;
  open?: boolean | undefined;
  onOpenChange?: ((open: boolean) => void) | undefined;
}

/** The pin question's three states: unanswered, and the two answers. */
type PinChoice = "" | "prune" | "pin";

export function SnapshotNowDialog({
  namespace,
  policy,
  repositories = [],
  open,
  onOpenChange,
}: SnapshotNowDialogProps) {
  const fieldId = useId();
  const snapshotNow = useSnapshotNow();
  const reason = useCapabilityReason(namespace, "createSnapshots");

  const [pin, setPin] = useState<PinChoice>("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [repository, setRepository] = useState("");

  const choices = repositories.map(repositoryName);
  const multi = choices.length > 1;

  const body: SnapshotNowBody = {
    namespace,
    policy,
    tags: [],
    pin: pin === "pin",
    ...(name.trim().length > 0 ? { name: name.trim() } : {}),
    ...(description.trim().length > 0 ? { description: description.trim() } : {}),
    ...(multi && repository.length > 0 ? { repository } : {}),
  };

  return (
    <ActionPanel
      label="Snapshot now"
      icon={Camera}
      disabledReason={reason}
      confirmLabel="Take the snapshot"
      blockedReason={
        pin === ""
          ? "Say whether this snapshot may be pruned. Pinning is permanent, so it is not something to get by omission."
          : undefined
      }
      running={snapshotNow.isPending}
      onConfirm={() => {
        snapshotNow.mutate(body);
      }}
      receipt={snapshotNow.data}
      problem={snapshotNow.error?.problem}
      open={open}
      onOpenChange={onOpenChange}
    >
      <p>
        This creates a <span className="mono">Snapshot</span> under{" "}
        <span className="mono">{policy}</span> in <span className="mono">{namespace}</span> — one
        per repository and source the policy expands to, the same fan-out a scheduled run mints. The
        backup itself happens in a mover Job the operator schedules, so the list below is what was
        created, not what has finished.
      </p>

      <fieldset className="action__choice">
        <legend>Retention</legend>
        <label htmlFor={`${fieldId}-prune`}>
          <input
            type="radio"
            id={`${fieldId}-prune`}
            name={`${fieldId}-pin`}
            value="prune"
            checked={pin === "prune"}
            onChange={() => {
              setPin("prune");
            }}
          />
          <span>
            Prune it under the policy&apos;s retention, like every other snapshot it takes.
          </span>
        </label>
        <label htmlFor={`${fieldId}-pin`}>
          <input
            type="radio"
            id={`${fieldId}-pin`}
            name={`${fieldId}-pin`}
            value="pin"
            checked={pin === "pin"}
            onChange={() => {
              setPin("pin");
            }}
          />
          <span>
            Pin it — <strong>permanently</strong> exempt from GFS pruning. The kopia manifest keeps
            the pin, so this snapshot goes on occupying the repository until someone deletes it by
            hand.
          </span>
        </label>
      </fieldset>

      {multi ? (
        <div className="controls__field">
          <label htmlFor={`${fieldId}-repo`}>Repository</label>
          <select
            id={`${fieldId}-repo`}
            className="controls__input"
            value={repository}
            onChange={(event) => {
              setRepository(event.target.value);
            }}
          >
            <option value="">Every repository the policy names ({choices.length})</option>
            {choices.map((choice) => (
              <option key={choice} value={choice}>
                {choice}
              </option>
            ))}
          </select>
        </div>
      ) : null}

      <div className="controls__field">
        <label htmlFor={`${fieldId}-name`}>Name (optional)</label>
        <input
          id={`${fieldId}-name`}
          className="controls__input"
          value={name}
          placeholder="the server generates one"
          onChange={(event) => {
            setName(event.target.value);
          }}
        />
      </div>

      <div className="controls__field">
        <label htmlFor={`${fieldId}-why`}>Description (optional)</label>
        <input
          id={`${fieldId}-why`}
          className="controls__input"
          value={description}
          placeholder="why you took it"
          onChange={(event) => {
            setDescription(event.target.value);
          }}
        />
      </div>
    </ActionPanel>
  );
}
