import { FolderSearch, PauseCircle, PlayCircle, Wrench, type LucideIcon } from "lucide-react";
import { type ReactNode, useState } from "react";

import { useMaintenanceRun, useScanCatalog, useSuspend } from "../api/hooks";
import type { RepositoryDetail as RepositoryDetailData } from "../api/types";
import { ActionButton } from "./ActionButton";
import { Popover } from "./Popover";
import { PickerField } from "./PickerField";
import { MAINTENANCE_MODES } from "./pickerChoices";
import {
  actionNamespace,
  isClusterScoped,
  repositoryPatchCapability,
  suspendKindToken,
} from "./repository";
import { useCapabilityReason } from "./useCapabilityReason";

/**
 * The three things an operator does to a repository from this screen:
 * suspend or resume it, ask for a maintenance run, and rescan its catalog.
 *
 * The rules every one of them obeys.
 *
 * **Never hidden.** A control the caller's RBAC forbids stays visible, reads
 * as disabled and says why on hover and on focus (`ActionButton`), because
 * the console is also how an operator learns what their bindings allow.
 *
 * **Judged in the right namespace.** `/me`'s flags are answers *for a
 * namespace* (addenda item 17), so each control asks about the namespace the
 * object it writes to lives in — the repository's own for a `Repository`,
 * none at all for a `ClusterRepository` (whose review is cluster-scoped
 * whatever is asked), and the `Maintenance` resource's for a run, which may
 * be a different namespace entirely when a `ClusterRepository`'s maintenance
 * lives in the operator's.
 *
 * **Confirmed, then answered.** Opening one asks what it will do before it
 * runs, and the server's `ActionReceipt` — `note` included — is what reports
 * the outcome. Two of these answer `202`: the request was accepted, not
 * performed.
 *
 * At most one confirmation is open at a time and it sits *below* the row of
 * triggers rather than inside one. Two reasons: a panel inside a trigger
 * widened it and shoved the other buttons across the bar, and being asked two
 * questions at once is how the wrong one gets answered.
 */
export interface RepositoryActionsProps {
  detail: RepositoryDetailData;
}

/** Which confirmation is open. */
type ActionId = "suspend" | "maintenance" | "scan";

interface ActionSpec {
  id: ActionId;
  label: string;
  icon: LucideIcon;
  variant: "default" | "danger";
  /** Disabled with this reason, or enabled when absent. */
  reason: string | undefined;
  confirmLabel: string;
  /** What the confirmation says will happen. */
  prose: ReactNode;
  running: boolean;
  run: () => void;
}

export function RepositoryActions({ detail }: RepositoryActionsProps) {
  const { summary, maintenance } = detail;
  const suspend = useSuspend();
  const maintenanceRun = useMaintenanceRun();
  const scan = useScanCatalog();
  const [mode, setMode] = useState("quick");
  const [open, setOpen] = useState<ActionId | null>(null);

  // A ClusterRepository's review is cluster-scoped whatever namespace is
  // asked, so it is asked with none.
  const patchReason = useCapabilityReason(
    isClusterScoped(summary) ? undefined : actionNamespace(summary),
    repositoryPatchCapability(summary),
  );
  // The Maintenance resource is what gets patched, and it need not live in the
  // repository's namespace — a ClusterRepository's lives in the operator's.
  const maintenanceReason = useCapabilityReason(maintenance?.namespace, "patchMaintenances");
  const noMaintenance =
    maintenance === null || maintenance === undefined
      ? `No Maintenance resource governs ${summary.name}. Check that its spec.maintenance is not disabled.`
      : undefined;

  const resuming = summary.suspended;
  const suspendLabel = resuming ? "Resume" : "Suspend";

  const actions: ActionSpec[] = [
    {
      id: "suspend",
      label: suspendLabel,
      icon: resuming ? PlayCircle : PauseCircle,
      variant: resuming ? "default" : "danger",
      reason: patchReason,
      confirmLabel: resuming ? "Resume this repository" : "Suspend this repository",
      prose: resuming ? (
        <p>
          Resuming sets <span className="mono">spec.suspend: false</span>. Backups resume at the
          next scheduled slot; missed ones are not made up.
        </p>
      ) : (
        <p>
          Suspending sets <span className="mono">spec.suspend: true</span>. No new backups run here
          until it is resumed, and missed ones are not made up. Existing snapshots are untouched.
        </p>
      ),
      running: suspend.isPending,
      run: () => {
        const namespace = actionNamespace(summary);
        suspend.mutate({
          kind: suspendKindToken(summary),
          name: summary.name,
          suspend: !resuming,
          ...(namespace !== undefined ? { namespace } : {}),
        });
      },
    },
    {
      id: "maintenance",
      label: "Run maintenance",
      icon: Wrench,
      variant: "default",
      reason: noMaintenance ?? maintenanceReason,
      confirmLabel: "Request the run",
      prose: (
        <>
          <p>
            This stamps a run request on{" "}
            <span className="mono">
              {maintenance?.namespace ?? "?"}/{maintenance?.name ?? "?"}
            </span>
            . <strong>Quick</strong> compacts kopia&apos;s indexes and is cheap;{" "}
            <strong>full</strong> also reclaims unused space, and is slow.
          </p>
          <PickerField
            id="maintenance-mode"
            label="Mode"
            value={mode}
            onChange={setMode}
            options={MAINTENANCE_MODES}
            strategy="fixed"
          />
        </>
      ),
      running: maintenanceRun.isPending,
      run: () => {
        if (maintenance === null || maintenance === undefined) {
          return;
        }
        maintenanceRun.mutate({
          namespace: maintenance.namespace,
          name: maintenance.name,
          mode,
        });
      },
    },
    {
      id: "scan",
      label: "Scan catalog",
      icon: FolderSearch,
      variant: "default",
      reason: patchReason,
      confirmLabel: "Request a scan",
      prose: (
        <p>
          A scan adds every snapshot in the kopia repository as a{" "}
          <span className="mono">Snapshot</span> resource, including ones from another cluster.
          Discovered snapshots use <span className="mono">deletionPolicy: Retain</span>, so kopiur
          never deletes them.
        </p>
      ),
      running: scan.isPending,
      run: () => {
        const namespace = actionNamespace(summary);
        scan.mutate({
          kind: summary.kindPath,
          name: summary.name,
          ...(namespace !== undefined ? { namespace } : {}),
        });
      },
    },
  ];

  return (
    <>
      <div className="action-bar">
        {actions.map((action) => {
          const Icon = action.icon;
          return (
            <Popover
              key={action.id}
              label={action.label}
              open={open === action.id}
              onOpenChange={(next) => {
                setOpen((was) => (next ? action.id : was === action.id ? null : was));
              }}
              className="action__popover"
              trigger={(props) => (
                <ActionButton variant={action.variant} disabledReason={action.reason} {...props}>
                  <Icon size={14} strokeWidth={2} aria-hidden="true" />
                  {action.label}
                </ActionButton>
              )}
            >
              {(close) => (
                <>
                  <div className="action__prose">{action.prose}</div>
                  <div className="action__actions">
                    <ActionButton
                      variant="primary"
                      disabledReason={action.running ? "The request is in flight." : action.reason}
                      onClick={() => {
                        action.run();
                        close();
                      }}
                    >
                      {action.confirmLabel}
                    </ActionButton>
                    <ActionButton variant="quiet" onClick={close}>
                      Cancel
                    </ActionButton>
                  </div>
                </>
              )}
            </Popover>
          );
        })}
      </div>
    </>
  );
}
