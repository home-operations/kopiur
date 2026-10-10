import { useState } from "react";

import type { MaintenanceRow, ScheduleRow } from "../../api/types";
import { RunDialog } from "../actions/RunDialog";
import { SuspendToggle } from "../actions/SuspendToggle";
import type { ListOnlyCard } from "./drawerData";

/**
 * The actions of a kind with no page of its own — the same controls its list
 * row carries, so the drawer is never a place you cannot act from. One
 * question is open at a time; the bar is the drawer's foot, and an open
 * question stacks above it.
 */
export function ListActions({ card }: { card: ListOnlyCard }) {
  const [open, setOpen] = useState<string | null>(null);
  const toggle = (id: string) => (next: boolean) => {
    setOpen(next ? id : null);
  };
  return (
    <div className="drawer-actions">
      <div className="action-bar">
        {(() => {
          switch (card.kind) {
            case "snapshotSchedule":
              return (
                <ScheduleSuspend
                  row={card.row}
                  open={open === "suspend"}
                  onOpenChange={toggle("suspend")}
                />
              );
            case "maintenance":
              return (
                <MaintenanceRun row={card.row} open={open === "run"} onOpenChange={toggle("run")} />
              );
            case "repositoryReplication":
            case "snapshotReplication": {
              const replicationKind =
                card.kind === "repositoryReplication" ? "replication" : "snapshot-replication";
              const { namespace, name, suspended } = card.row;
              return (
                <>
                  <RunDialog
                    target={{ kind: "replication", namespace, name, replicationKind }}
                    unavailable={
                      suspended
                        ? `${name} is suspended; resume it before asking for a run.`
                        : undefined
                    }
                    open={open === "run"}
                    onOpenChange={toggle("run")}
                  />
                  <SuspendToggle
                    kind={replicationKind}
                    name={name}
                    namespace={namespace}
                    suspended={suspended}
                    consequence="its cron is not evaluated, so nothing is copied until it is resumed"
                    open={open === "suspend"}
                    onOpenChange={toggle("suspend")}
                  />
                </>
              );
            }
          }
        })()}
      </div>
    </div>
  );
}

interface Control<Row> {
  row: Row;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

function ScheduleSuspend({ row, open, onOpenChange }: Control<ScheduleRow>) {
  return (
    <SuspendToggle
      kind="schedule"
      name={row.name}
      namespace={row.namespace}
      suspended={row.suspended}
      consequence="this cron is not evaluated at all, so the policy it fires goes unrun"
      open={open}
      onOpenChange={onOpenChange}
    />
  );
}

function MaintenanceRun({ row, open, onOpenChange }: Control<MaintenanceRow>) {
  return (
    <RunDialog
      target={{
        kind: "maintenance",
        namespace: row.namespace,
        name: row.name,
        repository: row.repository,
      }}
      labelSuffix={row.repository}
      open={open}
      onOpenChange={onOpenChange}
    />
  );
}
