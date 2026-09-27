/**
 * The namespace the console is currently scoped to, read from the URL.
 *
 * `?namespace=` is the scope: the shell re-asks `/me` with it (capabilities
 * are namespace-scoped, addenda item 17) and every list route passes it on.
 * `undefined` means cluster-wide. The URL is the state so a scoped view is
 * shareable and survives a reload.
 */

import { useRouterState } from "@tanstack/react-router";

/** Read `namespace` out of any search object; empty and non-string are "none". */
export function namespaceFromSearch(search: unknown): string | undefined {
  if (typeof search !== "object" || search === null) {
    return undefined;
  }
  const value: unknown = (search as Record<string, unknown>).namespace;
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

/** The current `?namespace=`, or `undefined` for cluster-wide. */
export function useCurrentNamespace(): string | undefined {
  return useRouterState({ select: (state) => namespaceFromSearch(state.location.search) });
}
