/** Short fixed lists a `PickerField` offers, shared by the forms that ask. */

/** The two maintenance modes. */
export const MAINTENANCE_MODES = [
  { value: "quick", label: "quick" },
  { value: "full", label: "full" },
] as const;

/**
 * **Pure.** A repository the way a snapshot row shows it, and the way the
 * snapshot list's `?repository=` reads one back: `Repository/<ns>/<name>` or
 * `ClusterRepository/<name>`.
 */
export function repositoryKey(
  kindPath: string,
  namespace: string | null | undefined,
  name: string,
): string {
  return kindPath === "cluster-repository"
    ? `ClusterRepository/${name}`
    : `Repository/${namespace ?? ""}/${name}`;
}
