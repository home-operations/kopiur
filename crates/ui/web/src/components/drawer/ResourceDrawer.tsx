import { SearchX } from "lucide-react";
import { useState } from "react";

import type { ApiProblemError } from "../../api/client";
import {
  useMaintenance,
  usePolicies,
  usePolicy,
  useReplications,
  useRepositories,
  useRepository,
  useRestore,
  useSchedules,
  useSnapshot,
} from "../../api/hooks";
import type { Problem } from "../../api/types";
import { EmptyState } from "../EmptyState";
import { ErrorState } from "../ErrorState";
import { LampBadge } from "../HealthBadge";
import { type InspectTarget, inspectToken, useInspect } from "../inspect";
import { KIND_META } from "../kind";
import { LoadingState } from "../LoadingState";
import { SidePanel } from "../SidePanel";
import { Tabs } from "../Tabs";
import { useExiting } from "../useExiting";
import type { DrawerData } from "./drawerData";
import { drawerView } from "./views";

/**
 * The resource drawer: whatever `?inspect=` names, over whatever page is open.
 *
 * Mounted once, by the shell. The panel stays mounted while the resource
 * changes — walking from a schedule to the policy it fires swaps the body,
 * not the panel, so focus still goes home to the row that first opened it.
 */
export function ResourceDrawer() {
  const { target, close } = useInspect();
  // Once the URL drops `?inspect=`, the last resource stays on screen while
  // the panel slides away.
  const { shown, leaving, exited } = useExiting(target, sameTarget);
  if (shown === null) return null;
  return <Drawer target={shown} onClose={close} leaving={leaving} onExited={exited} />;
}

function sameTarget(a: InspectTarget, b: InspectTarget): boolean {
  return inspectToken(a) === inspectToken(b);
}

type Loaded =
  | { state: "loading" }
  | { state: "missing" }
  | { state: "error"; problem: Problem }
  | { state: "ready"; data: DrawerData };

interface Query<T> {
  isPending: boolean;
  isError: boolean;
  error: ApiProblemError | null;
  data: T | undefined;
}

/** A read's state as the drawer's; a 404 is "gone", like a name missing from a list. */
function settle<T>(query: Query<T>, pick: (data: T) => DrawerData | undefined): Loaded {
  if (query.isError && query.error !== null) {
    return query.error.problem.status === 404
      ? { state: "missing" }
      : { state: "error", problem: query.error.problem };
  }
  if (query.isPending || query.data === undefined) return { state: "loading" };
  const data = pick(query.data);
  return data === undefined ? { state: "missing" } : { state: "ready", data };
}

function named<T extends { name: string }>(rows: readonly T[], name: string): T | undefined {
  return rows.find((row) => row.name === name);
}

/**
 * What the drawer shows, read the cheapest honest way: a kind with a detail
 * read asks for that one object, all of it; a kind without one reads its
 * namespace's list — already cached when the drawer was opened from that
 * list — and picks the row out by name.
 *
 * Every hook is called on every render (hooks cannot be conditional); only
 * the one for this kind is enabled.
 */
function useDrawerData(target: InspectTarget): Loaded {
  const { kind, name } = target;
  const ns = target.namespace ?? "";
  const isRepo = kind === "repository" || kind === "clusterRepository";
  const isReplication = kind === "repositoryReplication" || kind === "snapshotReplication";
  const repository = useRepository(
    KIND_META[kind === "clusterRepository" ? "clusterRepository" : "repository"].slug,
    name,
    kind === "clusterRepository" ? undefined : ns,
    { enabled: isRepo },
  );
  // The rows that give a repository's chain its live pills: cluster-wide,
  // because what writes into or copies a repository can live anywhere.
  const policyRows = usePolicies(undefined, { enabled: isRepo });
  const replicationRows = useReplications(undefined, { enabled: isRepo });
  const repositoryRows = useRepositories(undefined, { enabled: kind === "snapshotPolicy" });
  const policy = usePolicy(ns, name, { enabled: kind === "snapshotPolicy" });
  const snapshot = useSnapshot(ns, name, { enabled: kind === "snapshot" });
  const restore = useRestore(ns, name, { enabled: kind === "restore" });
  const schedules = useSchedules(ns, { enabled: kind === "snapshotSchedule" });
  const maintenance = useMaintenance(ns, { enabled: kind === "maintenance" });
  const replications = useReplications(ns, { enabled: isReplication });

  switch (kind) {
    case "repository":
    case "clusterRepository":
      return settle(repository, (detail) => ({
        kind,
        detail,
        policies: policyRows.data,
        replications: replicationRows.data,
      }));
    case "snapshotPolicy":
      return settle(policy, (detail) => ({ kind, detail, repositories: repositoryRows.data }));
    case "snapshot":
      return settle(snapshot, (detail) => ({ kind, detail }));
    case "restore":
      return settle(restore, (detail) => ({ kind, detail }));
    case "snapshotSchedule":
      return settle(schedules, (rows) => {
        const row = named(rows, name);
        return row && { kind, row };
      });
    case "maintenance":
      return settle(maintenance, (rows) => {
        const row = named(rows, name);
        return row && { kind, row };
      });
    case "repositoryReplication":
      return settle(replications, (view) => {
        const row = named(view.repository, name);
        return row && { kind, row };
      });
    case "snapshotReplication":
      return settle(replications, (view) => {
        const row = named(view.snapshot, name);
        return row && { kind, row };
      });
  }
}

interface DrawerProps {
  target: InspectTarget;
  onClose: () => void;
  leaving: boolean;
  onExited: () => void;
}

function Drawer({ target, onClose, leaving, onExited }: DrawerProps) {
  const loaded = useDrawerData(target);
  const meta = KIND_META[target.kind];
  const token = inspectToken(target);
  // The open tab belongs to the resource on screen: walking to another one
  // starts it on its first tab.
  const [tab, setTab] = useState<{ token: string; id: string }>({ token, id: "" });
  const selected = tab.token === token ? tab.id : "";
  const view = loaded.state === "ready" ? drawerView(loaded.data, new Date()) : undefined;
  const where = target.namespace !== undefined ? ` in ${target.namespace}` : "";
  return (
    <SidePanel
      kind={target.kind}
      label={
        <>
          {target.namespace !== undefined ? (
            <span className="side-panel__ns">{target.namespace}/</span>
          ) : null}
          {target.name}
        </>
      }
      status={view !== undefined ? <LampBadge lamp={view.lamp} /> : undefined}
      onClose={onClose}
      leaving={leaving}
      onExited={onExited}
      layout="fill"
      footer={view?.actions}
    >
      {view !== undefined ? (
        <>
          {view.head}
          {view.tabs.length > 1 ? (
            <Tabs
              label={view.tabsLabel}
              tabs={view.tabs}
              selected={selected}
              onSelect={(id) => {
                setTab({ token, id });
              }}
            />
          ) : (
            <div className="drawer__panel">{view.tabs[0]?.render()}</div>
          )}
        </>
      ) : (
        <div className="drawer__panel">
          {loaded.state === "loading" ? (
            <LoadingState what={`${meta.label} ${target.name}`} rows={4} />
          ) : loaded.state === "error" ? (
            <ErrorState problem={loaded.problem} what={`${meta.label} ${target.name}`} />
          ) : (
            <EmptyState title="Not found" icon={SearchX}>
              No {meta.label} named {target.name}
              {where}. It may have been deleted.
            </EmptyState>
          )}
        </div>
      )}
    </SidePanel>
  );
}
