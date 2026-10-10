import {
  ArchiveRestore,
  ArrowLeftRight,
  CalendarClock,
  Camera,
  Database,
  FolderOpen,
  LayoutDashboard,
  ScrollText,
  ShieldAlert,
  Stethoscope,
  Waypoints,
  Wrench,
  type LucideIcon,
} from "lucide-react";

import type { ObjectKind } from "../api/types";

/** The paths the sidebar links to — one per section, each a route file — plus the off-nav pages. */
export type NavPath =
  | "/"
  | "/topology"
  | "/repositories"
  | "/snapshots"
  | "/browse"
  | "/policies"
  | "/schedules"
  | "/restores"
  | "/maintenance"
  | "/replications"
  | "/doctor"
  | "/gates";

export interface NavItem {
  to: NavPath;
  label: string;
  icon: LucideIcon;
  /** The kind the section lists, which tints its chip; none for the tool pages. */
  kind?: ObjectKind | undefined;
}

/** A labelled run of sections; `label: null` is an unlabelled run. */
export interface NavGroup {
  label: string | null;
  items: readonly NavItem[];
}

const OVERVIEW: NavItem = { to: "/", label: "Overview", icon: LayoutDashboard };

/**
 * The eleven sections in reading order, grouped by the part of the system they
 * list: the health question first, storage, the data itself, the recipes
 * that protect it, then the tools.
 */
export const NAV_GROUPS: readonly NavGroup[] = [
  { label: null, items: [OVERVIEW, { to: "/topology", label: "Topology", icon: Waypoints }] },
  {
    label: "Storage",
    items: [
      { to: "/repositories", label: "Repositories", icon: Database, kind: "repository" },
      { to: "/maintenance", label: "Maintenance", icon: Wrench, kind: "maintenance" },
      {
        to: "/replications",
        label: "Replications",
        icon: ArrowLeftRight,
        kind: "snapshotReplication",
      },
    ],
  },
  {
    label: "Data",
    items: [
      { to: "/snapshots", label: "Snapshots", icon: Camera, kind: "snapshot" },
      { to: "/browse", label: "Browse", icon: FolderOpen },
      { to: "/restores", label: "Restores", icon: ArchiveRestore, kind: "restore" },
    ],
  },
  {
    label: "Protection",
    items: [
      { to: "/policies", label: "Policies", icon: ScrollText, kind: "snapshotPolicy" },
      { to: "/schedules", label: "Schedules", icon: CalendarClock, kind: "snapshotSchedule" },
    ],
  },
  { label: null, items: [{ to: "/doctor", label: "Doctor", icon: Stethoscope }] },
];

/** The eleven sections, flat, in sidebar order. */
export const NAV_ITEMS: readonly NavItem[] = NAV_GROUPS.flatMap((group) => group.items);

/**
 * Pages with a title of their own that are not rail sections: reached from
 * a section (doctor links to the gate registry), named in the header, never
 * counted among the eleven.
 */
export const OFF_RAIL_ITEMS: readonly NavItem[] = [
  { to: "/gates", label: "Gates", icon: ShieldAlert },
];

/** The section a pathname belongs to, by its first segment; unknown paths read as Overview. */
export function sectionFor(pathname: string): NavItem {
  const first = `/${pathname.split("/")[1] ?? ""}`;
  return (
    NAV_ITEMS.find((item) => item.to === first) ??
    OFF_RAIL_ITEMS.find((item) => item.to === first) ??
    OVERVIEW
  );
}
