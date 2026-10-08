import { useRouter, useRouterState } from "@tanstack/react-router";

import type { ObjectKind } from "../api/types";
import { KIND_META } from "./kind";

/**
 * Which resource the side panel is showing — the `?inspect=` parameter.
 *
 * The URL is the state, as it is for the namespace scope: a drawer that is
 * open survives a reload, can be linked to, and Back closes it. The value is
 * the kind's slug and the object's coordinates, `snapshot-policy/media/app`,
 * or `cluster-repository/shared` for the one cluster-scoped kind. Namespaces
 * and names are DNS labels, so `/` can never be part of either.
 */
export interface InspectTarget {
  kind: ObjectKind;
  name: string;
  namespace?: string | undefined;
}

const BY_SLUG: ReadonlyMap<string, ObjectKind> = new Map(
  Object.values(KIND_META).map((m) => [m.slug, m.kind]),
);

export function inspectToken(target: InspectTarget): string {
  const slug = KIND_META[target.kind].slug;
  return target.kind === "clusterRepository"
    ? `${slug}/${target.name}`
    : `${slug}/${target.namespace ?? ""}/${target.name}`;
}

/**
 * A token back into a target, or `null` for anything that does not name one
 * exactly: an unknown kind, a missing or extra part, a namespace on the
 * cluster-scoped kind or none on a namespaced one. A bad link opens nothing.
 */
export function parseInspect(raw: unknown): InspectTarget | null {
  if (typeof raw !== "string") return null;
  const [slug, ...rest] = raw.split("/");
  const kind = BY_SLUG.get(slug ?? "");
  if (kind === undefined || rest.some((part) => part.length === 0)) return null;
  if (kind === "clusterRepository") {
    return rest.length === 1 && rest[0] !== undefined ? { kind, name: rest[0] } : null;
  }
  const [namespace, name] = rest;
  return rest.length === 2 && namespace !== undefined && name !== undefined
    ? { kind, namespace, name }
    : null;
}

/**
 * The resource the drawer is showing, and how to close it.
 *
 * Closing undoes the opening: when the drawer was opened onto this history
 * entry (a link marked it), close steps back over it, so the entry is gone and
 * Back afterwards leaves the page as it would have before. A deep link has no
 * such entry to step over, so it drops the parameter in place.
 */
export function useInspect(): { target: InspectTarget | null; close: () => void } {
  const router = useRouter();
  const location = useRouterState({ select: (s) => s.location });
  const search = location.search as Record<string, unknown>;
  const target = parseInspect(search.inspect);
  const close = () => {
    if (location.state.inspect === true && router.history.canGoBack()) {
      router.history.back();
      return;
    }
    const rest = Object.fromEntries(Object.entries(search).filter(([key]) => key !== "inspect"));
    void router.navigate({ to: location.pathname, search: rest as never, replace: true });
  };
  return { target, close };
}
