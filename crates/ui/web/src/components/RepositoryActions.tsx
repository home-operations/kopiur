import { FolderSearch, PauseCircle, PlayCircle, Wrench } from "lucide-react";
import { type ReactNode, useState } from "react";

import { useMaintenanceRun, useScanCatalog, useSuspend } from "../api/hooks";
import type {
  MaintenanceRow,
  RepositoryDetail as RepositoryDetailData,
  RepositorySummary,
} from "../api/types";
import { ActionButton } from "./ActionButton";
import { ActionResult } from "./ActionResult";
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
 * **Confirmed, then answered.** Each control opens a panel that says what the
 * action will do before it runs, and the server's `ActionReceipt` — `note`
 * included — is what reports the outcome. Two of these answer `202`: the
 * request was accepted, not performed.
 */
export interface RepositoryActionsProps {
  detail: RepositoryDetailData;
}

export function RepositoryActions({ detail }: RepositoryActionsProps) {
  return (
    <div className="action-bar">
      <SuspendAction summary={detail.summary} />
      <MaintenanceAction summary={detail.summary} maintenance={detail.maintenance} />
      <ScanAction summary={detail.summary} />
    </div>
  );
}

/**
 * One action: a trigger, a confirmation panel that says what will happen, and
 * the receipt or the problem underneath.
 */
interface ActionProps {
  label: string;
  icon: typeof Wrench;
  /** The button's variant; `danger` for the one that stops backups. */
  variant?: "default" | "primary" | "danger" | undefined;
  disabledReason: string | undefined;
  /** What the confirmation says will happen. */
  children: ReactNode;
  confirmLabel: string;
  running: boolean;
  result: ReactNode;
  onConfirm: () => void;
}

function Action({
  label,
  icon: Icon,
  variant = "default",
  disabledReason,
  children,
  confirmLabel,
  running,
  result,
  onConfirm,
}: ActionProps) {
  const [open, setOpen] = useState(false);
  return (
    <div className="action">
      <ActionButton
        variant={variant}
        disabledReason={disabledReason}
        aria-expanded={open}
        onClick={() => {
          setOpen((was) => !was);
        }}
      >
        <Icon size={14} strokeWidth={2} aria-hidden="true" />
        {label}
      </ActionButton>
      {open ? (
        <div className="action__confirm" role="group" aria-label={label}>
          <div className="action__prose">{children}</div>
          <div className="action__actions">
            <ActionButton
              variant="primary"
              disabledReason={running ? "The request is in flight." : disabledReason}
              onClick={() => {
                onConfirm();
                setOpen(false);
              }}
            >
              {confirmLabel}
            </ActionButton>
            <ActionButton
              variant="quiet"
              onClick={() => {
                setOpen(false);
              }}
            >
              Cancel
            </ActionButton>
          </div>
        </div>
      ) : null}
      {result}
    </div>
  );
}

function SuspendAction({ summary }: { summary: RepositorySummary }) {
  const suspend = useSuspend();
  // A ClusterRepository's review is cluster-scoped whatever namespace is
  // asked, so it is asked with none.
  const reason = useCapabilityReason(
    isClusterScoped(summary) ? undefined : actionNamespace(summary),
    repositoryPatchCapability(summary),
  );
  const resuming = summary.suspended;
  const label = resuming ? "Resume" : "Suspend";
  return (
    <Action
      label={label}
      icon={resuming ? PlayCircle : PauseCircle}
      variant={resuming ? "default" : "danger"}
      disabledReason={reason}
      confirmLabel={resuming ? "Resume this repository" : "Suspend this repository"}
      running={suspend.isPending}
      onConfirm={() => {
        const namespace = actionNamespace(summary);
        suspend.mutate({
          kind: suspendKindToken(summary),
          name: summary.name,
          suspend: !resuming,
          ...(namespace !== undefined ? { namespace } : {}),
        });
      }}
      result={
        <ActionResult label={label} receipt={suspend.data} problem={suspend.error?.problem} />
      }
    >
      {resuming ? (
        <p>
          Resuming sets <span className="mono">spec.suspend: false</span>. Schedules that name this
          repository start firing again on their next slot; nothing is caught up retroactively.
        </p>
      ) : (
        <p>
          Suspending sets <span className="mono">spec.suspend: true</span>. No new backup will be
          written here until it is resumed — a schedule that fires meanwhile simply does not run,
          and the missed window is not made up later. Snapshots already in the repository are
          untouched.
        </p>
      )}
    </Action>
  );
}

function MaintenanceAction({
  summary,
  maintenance,
}: {
  summary: RepositorySummary;
  maintenance: MaintenanceRow | null | undefined;
}) {
  const run = useMaintenanceRun();
  const [mode, setMode] = useState("quick");
  // The Maintenance resource is what gets patched, and it need not live in the
  // repository's namespace — a ClusterRepository's lives in the operator's.
  const reason = useCapabilityReason(maintenance?.namespace, "patchMaintenances");
  const missing =
    maintenance === null || maintenance === undefined
      ? `No Maintenance resource governs ${summary.name}, so there is no run to request. A repository's spec.maintenance projects one by default.`
      : undefined;
  return (
    <Action
      label="Run maintenance"
      icon={Wrench}
      disabledReason={missing ?? reason}
      confirmLabel="Request the run"
      running={run.isPending}
      onConfirm={() => {
        if (maintenance === null || maintenance === undefined) {
          return;
        }
        run.mutate({ namespace: maintenance.namespace, name: maintenance.name, mode });
      }}
      result={
        <ActionResult label="Run maintenance" receipt={run.data} problem={run.error?.problem} />
      }
    >
      <p>
        This stamps a run request on{" "}
        <span className="mono">
          {maintenance?.namespace ?? "?"}/{maintenance?.name ?? "?"}
        </span>
        . The operator honors it on its next pass — the run itself happens in a mover Job, not here.
      </p>
      <div className="controls__field">
        <label htmlFor="maintenance-mode">Mode</label>
        <select
          id="maintenance-mode"
          className="controls__input"
          value={mode}
          onChange={(event) => {
            setMode(event.target.value);
          }}
        >
          <option value="quick">quick — compact indexes, cheap and frequent</option>
          <option value="full">full — also drop unreferenced content, slow and heavy</option>
        </select>
      </div>
    </Action>
  );
}

function ScanAction({ summary }: { summary: RepositorySummary }) {
  const scan = useScanCatalog();
  const reason = useCapabilityReason(
    isClusterScoped(summary) ? undefined : actionNamespace(summary),
    repositoryPatchCapability(summary),
  );
  return (
    <Action
      label="Scan catalog"
      icon={FolderSearch}
      disabledReason={reason}
      confirmLabel="Request a scan"
      running={scan.isPending}
      onConfirm={() => {
        const namespace = actionNamespace(summary);
        scan.mutate({
          kind: summary.kindPath,
          name: summary.name,
          ...(namespace !== undefined ? { namespace } : {}),
        });
      }}
      result={
        <ActionResult label="Scan catalog" receipt={scan.data} problem={scan.error?.problem} />
      }
    >
      <p>
        A scan walks the kopia repository and adopts every snapshot it finds as a{" "}
        <span className="mono">Snapshot</span> resource — including backups taken before kopiur or
        by another cluster. Discovered snapshots are forced to{" "}
        <span className="mono">deletionPolicy: Retain</span>, so nothing a scan finds can be deleted
        by kopiur on your behalf. Repeated clicks collapse onto one honored scan.
      </p>
    </Action>
  );
}
