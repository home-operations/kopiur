import { useState } from "react";

import type { PolicyRow } from "../../../api/types";
import { SnapshotNowDialog } from "../../actions/SnapshotNowDialog";
import { SuspendToggle } from "../../actions/SuspendToggle";

/** Snapshot now and suspend, one question open at a time. */
export function PolicyActions({ row }: { row: PolicyRow }) {
  const [open, setOpen] = useState<"snapshot" | "suspend" | null>(null);
  return (
    <div className="drawer-actions">
      <div className="action-bar">
        <SnapshotNowDialog
          namespace={row.namespace}
          policy={row.name}
          repositories={row.repositories}
          open={open === "snapshot"}
          onOpenChange={(next) => {
            setOpen(next ? "snapshot" : null);
          }}
        />
        <SuspendToggle
          kind="policy"
          name={row.name}
          namespace={row.namespace}
          suspended={row.suspended}
          consequence="no schedule will fire it, and the sources it names go unbacked-up"
          open={open === "suspend"}
          onOpenChange={(next) => {
            setOpen(next ? "suspend" : null);
          }}
        />
      </div>
    </div>
  );
}
