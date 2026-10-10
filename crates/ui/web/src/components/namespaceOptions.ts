import { useNamespaces } from "../api/hooks";
import type { PickerOption } from "./PickerOptions";

/**
 * The namespaces the caller may see, with their object counts, as picker
 * options — the one list the shell's switcher and every namespace field show.
 * Fetched only once `enabled`; loading or a refusal leaves it empty, and the
 * empty choice (all namespaces, or a field's default) is always valid.
 */
export function useNamespaceOptions(enabled: boolean): {
  options: PickerOption[];
  total: number | undefined;
} {
  const namespaces = useNamespaces({ enabled });
  const listed = namespaces.isSuccess ? namespaces.data : [];
  return {
    options: listed.map((ns) => ({
      value: ns.name,
      label: ns.name,
      initial: ns.name.charAt(0),
      meta: ns.objects,
    })),
    total: namespaces.isSuccess ? listed.reduce((sum, ns) => sum + ns.objects, 0) : undefined,
  };
}
