import { Wrench } from "lucide-react";
import { useId, useState } from "react";

import { useMaintenanceRun, useReplicationRun } from "../../api/hooks";
import type { Capabilities, MaintenanceRunBody, ReplicationRunBody } from "../../api/types";
import { assertNever } from "../../util/assertNever";
import { useCapabilityReason } from "../useCapabilityReason";
import { ActionPanel } from "./ActionPanel";

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
  /** Distinguish one row's trigger from another's for assistive technology. */
  labelSuffix?: string | undefined;
  open?: boolean | undefined;
  onOpenChange?: ((open: boolean) => void) | undefined;
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
  labelSuffix,
  open,
  onOpenChange,
}: RunDialogProps) {
  const fieldId = useId();
  const maintenance = useMaintenanceRun();
  const replication = useReplicationRun();
  const [mode, setMode] = useState("quick");

  // The namespace that decides the grant is the namespace of the object being
  // patched: a ClusterRepository's Maintenance lives in the operator's
  // namespace, not the repository's, and each replication row is judged in
  // its own (addenda item 17).
  const reason = useCapabilityReason(target.namespace, runCapability(target));

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
      disabledReason={unavailable ?? reason}
      confirmLabel="Request the run"
      running={running}
      onConfirm={run}
      receipt={receipt}
      problem={problem}
      open={open}
      onOpenChange={onOpenChange}
    >
      <p>
        This stamps a run request on{" "}
        <span className="mono">
          {target.namespace}/{target.name}
        </span>
        . The operator honors it on its next pass and the work happens in a mover Job — so this
        answers &ldquo;asked for&rdquo;, never &ldquo;finished&rdquo;. Watch the object&apos;s own
        status for the outcome.
      </p>

      {target.kind === "maintenance" ? (
        <>
          <p>
            It maintains <span className="mono">{target.repository}</span>. <strong>Quick</strong>{" "}
            compacts kopia&apos;s indexes and is cheap enough to run often; <strong>full</strong>{" "}
            also drops content nothing references any more, which is what actually reclaims space,
            and is slow and heavy.
          </p>
          <div className="controls__field">
            <label htmlFor={`${fieldId}-mode`}>Mode</label>
            <select
              id={`${fieldId}-mode`}
              className="controls__input"
              value={mode}
              onChange={(event) => {
                setMode(event.target.value);
              }}
            >
              <option value="quick">quick</option>
              <option value="full">full</option>
            </select>
          </div>
        </>
      ) : (
        <p>
          A replication run copies what the source holds to the destination. Nothing at either end
          is deleted by asking for one, and a run already in flight is not restarted.
        </p>
      )}
    </ActionPanel>
  );
}
