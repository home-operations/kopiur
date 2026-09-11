import {
  ArchiveRestore,
  ArrowLeftRight,
  CalendarClock,
  Camera,
  Database,
  LayoutDashboard,
  ScrollText,
  ShieldAlert,
  Stethoscope,
  Waypoints,
  Wrench,
  type LucideIcon,
} from "lucide-react";

/** The paths the rail links to — one per section, each a route file — plus the off-rail pages. */
export type NavPath =
  | "/"
  | "/topology"
  | "/repositories"
  | "/snapshots"
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
}

const OVERVIEW: NavItem = { to: "/", label: "Overview", icon: LayoutDashboard };

/** The ten sections, in reading order: the health question first, the tools last. */
export const NAV_ITEMS: readonly NavItem[] = [
  OVERVIEW,
  { to: "/topology", label: "Topology", icon: Waypoints },
  { to: "/repositories", label: "Repositories", icon: Database },
  { to: "/snapshots", label: "Snapshots", icon: Camera },
  { to: "/policies", label: "Policies", icon: ScrollText },
  { to: "/schedules", label: "Schedules", icon: CalendarClock },
  { to: "/restores", label: "Restores", icon: ArchiveRestore },
  { to: "/maintenance", label: "Maintenance", icon: Wrench },
  { to: "/replications", label: "Replications", icon: ArrowLeftRight },
  { to: "/doctor", label: "Doctor", icon: Stethoscope },
];

/**
 * Pages with a title of their own that are not rail sections: reached from
 * a section (doctor links to the gate registry), named in the header, never
 * counted among the ten.
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
