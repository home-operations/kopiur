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
 * The drawers open, bottom first: `?inspect=` is their tokens joined by
 * commas (a token is slugs and DNS labels, so it never holds one). One token
 * is a stack of one. Reading stops at the first part that does not name a
 * resource, so a damaged link opens the drawers under the damage and no more.
 */
export function parseInspectStack(raw: unknown): InspectTarget[] {
  if (typeof raw !== "string") return [];
  const stack: InspectTarget[] = [];
  for (const part of raw.split(",")) {
    const target = parseInspect(part);
    if (target === null) break;
    stack.push(target);
  }
  return stack;
}

export function inspectStackParam(stack: readonly InspectTarget[]): string {
  return stack.map(inspectToken).join(",");
}

/**
 * The stack after opening `target` from its top drawer: on top of it — or,
 * when `target` is already open lower down, back down to that drawer, so
 * walking a loop (policy → repository → the same policy) never piles up
 * copies. `truncated` says which, since a step back down is not a new entry
 * in history.
 */
export function pushInspect(
  stack: readonly InspectTarget[],
  target: InspectTarget,
): { stack: InspectTarget[]; truncated: boolean } {
  const token = inspectToken(target);
  const at = stack.findIndex((open) => inspectToken(open) === token);
  return at === -1
    ? { stack: [...stack, target], truncated: false }
    : { stack: stack.slice(0, at + 1), truncated: true };
}

/** What a history entry says about the drawers opened onto it. */
export interface InspectEntry {
  inspectDepth?: number | undefined;
  inspectBase?: number | undefined;
}

/**
 * The mark for the entry a link pushes when it opens a drawer on top of the
 * `open` drawers of the current entry. An entry that was itself pushed at
 * this depth continues its chain, so the new one can be stepped back to its
 * base; anything else (the page, a deep link) starts a chain here.
 */
export function inspectPushEntry(current: InspectEntry, open: number): Required<InspectEntry> {
  const chained = current.inspectDepth === open;
  return {
    inspectDepth: open + 1,
    inspectBase: chained ? (current.inspectBase ?? open - 1) : open,
  };
}

/**
 * How many entries to step back to go from `open` drawers down to `keep`, or
 * `null` when the entries for those drawers are not behind this one — then
 * the URL is replaced instead. Stepping back is what keeps Back from
 * reopening a drawer that was closed by backing past it.
 */
export function inspectStepsBack(current: InspectEntry, open: number, keep: number): number | null {
  if (current.inspectDepth !== open) return null;
  const base = current.inspectBase ?? open - 1;
  return keep >= base ? open - keep : null;
}

/**
 * The drawers open, the top one, and how to close the top one.
 *
 * Closing undoes the opening, one drawer at a time: when the top drawer was
 * opened onto this history entry (a link marked it with its depth), close
 * steps back over it, so the entry is gone and Back afterwards leaves the page
 * as it would have before. A deep link has no such entry to step over, so the
 * top token is dropped in place. Neither moves the page: stepping back
 * restores where it was (the router's scroll restoration, `main.tsx`), and
 * dropping a token does not scroll.
 */
export function useInspect(): {
  stack: InspectTarget[];
  target: InspectTarget | null;
  close: () => void;
} {
  const router = useRouter();
  const location = useRouterState({ select: (s) => s.location });
  const search = location.search as Record<string, unknown>;
  const stack = parseInspectStack(search.inspect);
  const close = () => {
    if (location.state.inspectDepth === stack.length && router.history.canGoBack()) {
      router.history.back();
      return;
    }
    const below = stack.slice(0, -1);
    const rest = Object.fromEntries(Object.entries(search).filter(([key]) => key !== "inspect"));
    void router.navigate({
      to: location.pathname,
      search: (below.length > 0 ? { ...rest, inspect: inspectStackParam(below) } : rest) as never,
      replace: true,
      resetScroll: false,
    });
  };
  return { stack, target: stack.at(-1) ?? null, close };
}
