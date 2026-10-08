import { Link } from "@tanstack/react-router";
import { SearchX } from "lucide-react";

import type { ApiProblemError } from "../../api/client";
import {
  useMaintenance,
  usePolicy,
  useReplications,
  useRepository,
  useRestore,
  useSchedules,
  useSnapshot,
} from "../../api/hooks";
import type { Problem } from "../../api/types";
import { EmptyState } from "../EmptyState";
import { ErrorState } from "../ErrorState";
import { LampBadge } from "../HealthBadge";
import { type InspectTarget, useInspect } from "../inspect";
import { KIND_META } from "../kind";
import { LoadingState } from "../LoadingState";
import { ObjectRef } from "../ObjectRef";
import { type CardRow, cardFacts } from "../objectCard";
import { SidePanel } from "../SidePanel";
import { AbsenceText, StatStrip } from "../StatStrip";
import { drawerFacts } from "./drawerFacts";

/**
 * The resource drawer: whatever `?inspect=` names, over whatever page is open.
 *
 * Mounted once, by the shell. The panel stays mounted while the resource
 * changes — walking from a schedule to the policy it fires swaps the body,
 * not the panel, so focus still goes home to the row that first opened it.
 */
export function ResourceDrawer() {
  const { target, close } = useInspect();
  if (target === null) return null;
  return <Drawer target={target} onClose={close} />;
}

type Loaded =
  | { state: "loading" }
  | { state: "missing" }
  | { state: "error"; problem: Problem }
  | { state: "ready"; card: CardRow };

interface Query<T> {
  isPending: boolean;
  isError: boolean;
  error: ApiProblemError | null;
  data: T | undefined;
}

/** A read's state as the drawer's; a 404 is "gone", like a name missing from a list. */
function settle<T>(query: Query<T>, pick: (data: T) => CardRow | undefined): Loaded {
  if (query.isError && query.error !== null) {
    return query.error.problem.status === 404
      ? { state: "missing" }
      : { state: "error", problem: query.error.problem };
  }
  if (query.isPending || query.data === undefined) return { state: "loading" };
  const card = pick(query.data);
  return card === undefined ? { state: "missing" } : { state: "ready", card };
}

function named<T extends { name: string }>(rows: readonly T[], name: string): T | undefined {
  return rows.find((row) => row.name === name);
}

/**
 * The target's row, read the cheapest honest way: a kind with a detail read
 * asks for that one object; a kind without one reads its namespace's list —
 * already cached when the drawer was opened from that list — and picks the
 * row out by name.
 *
 * Every hook is called on every render (hooks cannot be conditional); only
 * the one for this kind is enabled.
 */
function useDrawerRow(target: InspectTarget): Loaded {
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
  const policy = usePolicy(ns, name, { enabled: kind === "snapshotPolicy" });
  const snapshot = useSnapshot(ns, name, { enabled: kind === "snapshot" });
  const restore = useRestore(ns, name, { enabled: kind === "restore" });
  const schedules = useSchedules(ns, { enabled: kind === "snapshotSchedule" });
  const maintenance = useMaintenance(ns, { enabled: kind === "maintenance" });
  const replications = useReplications(ns, { enabled: isReplication });

  switch (kind) {
    case "repository":
    case "clusterRepository":
      return settle(repository, (d) => ({ kind, row: d.summary }));
    case "snapshotPolicy":
      return settle(policy, (d) => ({ kind, row: d.row }));
    case "snapshot":
      return settle(snapshot, (d) => ({ kind, row: d.row }));
    case "restore":
      return settle(restore, (d) => ({ kind, row: d.row }));
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

function Drawer({ target, onClose }: { target: InspectTarget; onClose: () => void }) {
  const loaded = useDrawerRow(target);
  const meta = KIND_META[target.kind];
  const facts = loaded.state === "ready" ? cardFacts(loaded.card, new Date()) : undefined;
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
      status={facts !== undefined ? <LampBadge lamp={facts.lamp} /> : undefined}
      onClose={onClose}
      footer={
        facts?.to !== undefined ? (
          <Link className="button" to={facts.to}>
            Open full page
          </Link>
        ) : undefined
      }
    >
      {loaded.state === "loading" ? (
        <LoadingState what={`${meta.label} ${target.name}`} rows={4} />
      ) : loaded.state === "error" ? (
        <ErrorState problem={loaded.problem} what={`${meta.label} ${target.name}`} />
      ) : loaded.state === "missing" ? (
        <EmptyState title="Not found" icon={SearchX}>
          No {meta.label} named {target.name}
          {where}. It may have been deleted.
        </EmptyState>
      ) : (
        <DrawerBody card={loaded.card} />
      )}
    </SidePanel>
  );
}

/**
 * The card's three stats, then the facts a card has no room for, then what the
 * resource names. The card's one-line meta is not repeated: every part of it
 * is a fact below.
 */
function DrawerBody({ card }: { card: CardRow }) {
  const now = new Date();
  const { stats } = cardFacts(card, now);
  const { facts, related } = drawerFacts(card);
  const namespace = "namespace" in card.row ? (card.row.namespace ?? undefined) : undefined;
  return (
    <>
      <StatStrip stats={stats} variant="card" label="At a glance" />
      <section className="drawer__section" aria-label="Facts">
        <h3 className="drawer__title">Facts</h3>
        <dl className="drawer__facts">
          {facts.map((fact) => (
            <div className="drawer__fact" key={fact.label}>
              <dt>{fact.label}</dt>
              <dd className={fact.mono === true ? "mono" : undefined}>
                {typeof fact.value === "string" ? fact.value : <AbsenceText absence={fact.value} />}
              </dd>
            </div>
          ))}
        </dl>
      </section>
      {related.length > 0 ? (
        <section className="drawer__section" aria-label="Related">
          <h3 className="drawer__title">Related</h3>
          <ul className="drawer__related">
            {related.map((rel) => (
              <li
                key={`${rel.label}:${rel.target.kind}:${rel.target.namespace ?? ""}/${rel.target.name}`}
              >
                <span className="drawer__rel-label">{rel.label}</span>
                <ObjectRef
                  kind={rel.target.kind}
                  name={rel.target.name}
                  namespace={rel.target.namespace}
                  contextNamespace={namespace}
                  inspect
                />
              </li>
            ))}
          </ul>
        </section>
      ) : null}
    </>
  );
}
