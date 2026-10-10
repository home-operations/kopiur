import { Wrench } from "lucide-react";
import { useId, useState } from "react";

import { useMaintenanceRun, useReplicationRun } from "../../api/hooks";
import type { Capabilities, MaintenanceRunBody, ReplicationRunBody } from "../../api/types";
import { assertNever } from "../../util/assertNever";
import { PickerField } from "../PickerField";
import { MAINTENANCE_MODES } from "../pickerChoices";
import { ActionPanel } from "./ActionPanel";
import { useRefusal } from "./reason";

/**
 * Ask for a run right now — maintenance, or a replication copy.
 *
 * Both endpoints answer `202`, and the two share this component because they
 * share a shape that is easy to render wrongly: neither one *runs* anything.
 * Each stamps a run-request annotation that a reconciler honors on its next
 * pass, and the `requestedAt` in the receipt is the token the operator echoes
 * back on the run it eventually produces — which is how a previous run's
 * outcome is told apart from this one's. So the wording is *requested*, and
 * the object's own status is where the answer arrives.
 *
 * The target is a discriminated union rather than a bag of optional fields,
 * so a third run kind cannot be added without the capability, the body and
 * the prose all being named for it.
 */
export type RunTarget =
  | {
      kind: "maintenance";
      /** The `Maintenance` resource's namespace — NOT the repository's. */
      namespace: string;
      name: string;
      /** The repository it governs, for the confirmation's prose. */
      repository: string;
    }
  | {
      kind: "replication";
      namespace: string;
      name: string;
      /** The token `ReplicationRunBody.kind` takes. */
      replicationKind: "replication" | "snapshot-replication";
    };

export interface RunDialogProps {
  target: RunTarget;
  /** Refuse the run for a reason of the caller's — a suspended object, say. */
  unavailable?: string | undefined;
  /** The few words for `unavailable`, when this sits inside a ledger. */
  unavailableShort?: string | undefined;
  /**
   * Set when this dialog sits inside a `.ledger-scroll`, which suppresses
   * `ActionButton`'s floating tooltip — the refusal then also appears as a
   * short word in the cell. See `reason.ts`.
   */
  inLedger?: boolean | undefined;
  /** Distinguish one row's trigger from another's for assistive technology. */
  labelSuffix?: string | undefined;
  open?: boolean | undefined;
  onOpenChange?: ((open: boolean) => void) | undefined;
  /** How the confirmation opens; see `ActionPanel`. */
  presentation?: "inline" | "popover" | undefined;
  /** Shorter trigger words for a table cell; see `ActionPanel.shortLabel`. */
  shortLabel?: string | undefined;
  /** For the popover: which edge of the trigger it lines up with. */
  align?: "start" | "end" | undefined;
  /** For the popover: `fixed` inside a box that clips, such as a table. */
  strategy?: "absolute" | "fixed" | undefined;
  /**
   * The maintenance request to send through, when the page shows the receipt
   * itself (a table: one receipt under it, not one per cell). Its own otherwise.
   */
  maintenanceMutation?: ReturnType<typeof useMaintenanceRun> | undefined;
  /** Show the receipt beside the trigger; off when the page shows it. */
  showResult?: boolean | undefined;
}

/** The `/me` flag that decides whether this run may be asked for. */
function runCapability(target: RunTarget): keyof Capabilities {
  switch (target.kind) {
    case "maintenance":
      return "patchMaintenances";
    case "replication":
      return target.replicationKind === "replication"
        ? "patchRepositoryReplications"
        : "patchSnapshotReplications";
    default:
      return assertNever(target, "RunTarget");
  }
}

export function RunDialog({
  target,
  unavailable,
  unavailableShort,
  inLedger = false,
  labelSuffix,
  open,
  onOpenChange,
  presentation,
  shortLabel,
  align,
  strategy,
  maintenanceMutation,
  showResult,
}: RunDialogProps) {
  const fieldId = useId();
  const ownMaintenance = useMaintenanceRun();
  const maintenance = maintenanceMutation ?? ownMaintenance;
  const replication = useReplicationRun();
  const [mode, setMode] = useState("quick");

  // The namespace that decides the grant is the namespace of the object being
  // patched: a ClusterRepository's Maintenance lives in the operator's
  // namespace, not the repository's, and each replication row is judged in
  // its own (addenda item 17).
  const refusal = useRefusal(target.namespace, runCapability(target));
  // The caller's own refusal wins: "this object is suspended" is a better
  // answer than "you may not", and it is the one the reader can act on.
  const reason = unavailable ?? refusal?.full;
  const short = unavailable !== undefined ? unavailableShort : refusal?.short;

  const maintenanceRun = target.kind === "maintenance";
  const label = maintenanceRun ? "Run maintenance" : "Run now";
  const running = maintenanceRun ? maintenance.isPending : replication.isPending;
  const receipt = maintenanceRun ? maintenance.data : replication.data;
  const problem = maintenanceRun ? maintenance.error?.problem : replication.error?.problem;

  const run = () => {
    if (target.kind === "maintenance") {
      const body: MaintenanceRunBody = {
        namespace: target.namespace,
        name: target.name,
        mode,
      };
      maintenance.mutate(body);
      return;
    }
    const body: ReplicationRunBody = {
      namespace: target.namespace,
      name: target.name,
      // Always explicit. Detection reads both kinds and refuses a name that
      // exists in both; the row already knows which one it is, so saying so
      // removes an ambiguity the server would otherwise have to raise.
      kind: target.replicationKind,
    };
    replication.mutate(body);
  };

  return (
    <ActionPanel
      label={labelSuffix === undefined ? label : `${label} for ${labelSuffix}`}
      icon={Wrench}
      disabledReason={reason}
      shortReason={inLedger ? short : undefined}
      confirmLabel="Request the run"
      running={running}
      onConfirm={run}
      receipt={receipt}
      problem={problem}
      open={open}
      onOpenChange={onOpenChange}
      presentation={presentation}
      shortLabel={shortLabel}
      align={align}
      strategy={strategy}
      showResult={showResult}
    >
      <p>
        This stamps a run request on{" "}
        <span className="mono">
          {target.namespace}/{target.name}
        </span>
        . The operator runs it in a mover Job on its next pass; watch the object&apos;s status for
        the outcome.
      </p>

      {target.kind === "maintenance" ? (
        <>
          <p>
            It maintains <span className="mono">{target.repository}</span>. <strong>Quick</strong>{" "}
            compacts indexes and is cheap; <strong>full</strong> also reclaims unused space, and is
            slow.
          </p>
          <PickerField
            id={`${fieldId}-mode`}
            label="Mode"
            value={mode}
            onChange={setMode}
            options={MAINTENANCE_MODES}
            strategy="fixed"
          />
        </>
      ) : (
        <p>
          A replication run copies the source to the destination. Nothing is deleted, and a run
          already in flight is not restarted.
        </p>
      )}
    </ActionPanel>
  );
}
