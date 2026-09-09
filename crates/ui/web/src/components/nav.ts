import {
  ArchiveRestore,
  ArrowLeftRight,
  CalendarClock,
  Camera,
  Database,
  LayoutDashboard,
  ScrollText,
  Stethoscope,
  Waypoints,
  Wrench,
  type LucideIcon,
} from "lucide-react";

/** The paths the rail links to — one per section, each a route file. */
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
  | "/doctor";

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

/** The section a pathname belongs to, by its first segment; unknown paths read as Overview. */
export function sectionFor(pathname: string): NavItem {
  const first = `/${pathname.split("/")[1] ?? ""}`;
  return NAV_ITEMS.find((item) => item.to === first) ?? OVERVIEW;
}
