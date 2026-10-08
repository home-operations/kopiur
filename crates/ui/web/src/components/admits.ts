import type { AdmittedNamespacesView } from "../api/types";
import { unknownVariant } from "../util/assertNever";

/**
 * Which namespaces a `ClusterRepository` admits, in words.
 *
 * Read from the server's `admits`, which names the spec's admission. The
 * controller's own `allowedNamespaceCount` uses `-1` for "all" and `0` for a
 * selector it never resolves; neither is a count, so it is never shown.
 */
export function admitsText(admits: AdmittedNamespacesView): string {
  if (typeof admits === "string") {
    switch (admits) {
      case "all":
        return "admits all namespaces";
      case "none":
        return "admits no namespaces";
      default:
        return `admits ${unknownVariant(admits, "AdmittedNamespacesView")}`;
    }
  }
  if ("listed" in admits) {
    const { count } = admits.listed;
    return `admits ${String(count)} namespace${count === 1 ? "" : "s"}`;
  }
  return `admits namespaces matching ${admits.selector.selector}`;
}
