import { Link, useRouter, useRouterState } from "@tanstack/react-router";
import type { ReactNode } from "react";

import {
  type InspectTarget,
  inspectPushEntry,
  inspectStackParam,
  inspectStepsBack,
  parseInspectStack,
  pushInspect,
} from "./inspect";

export interface InspectLinkProps {
  target: InspectTarget;
  className?: string | undefined;
  "aria-label"?: string | undefined;
  "data-kind"?: string | undefined;
  /** More `data-*` hooks for the anchor (a topology plate's node id, health, …). */
  data?: Readonly<Record<`data-${string}`, string | undefined>> | undefined;
  onClick?: (() => void) | undefined;
  children: ReactNode;
}

/**
 * A link that opens a resource in the side panel, on the page you are on.
 *
 * A real link — it has an address, so it opens in a new tab and copies — that
 * keeps every other parameter (the scope, a list's filters and page) and adds
 * the resource to `?inspect=`. Opened from the page it opens the first drawer;
 * opened from inside a drawer it opens another on top. Either way it pushes
 * one history entry marked with the new depth, so Back — like clicking out —
 * closes one drawer. A resource already open lower down is backed down to
 * instead (`pushInspect`) by stepping back through the entries those drawers
 * pushed, so Back never reopens one; when they are not behind this entry (a
 * deep link opened them together) the URL is replaced in place.
 *
 * The page stays where it is: the router scrolls to the top on a navigation
 * by default, and a drawer opened from halfway down a list must not throw the
 * reader back to its head.
 */
export function InspectLink({
  target,
  className,
  "aria-label": ariaLabel,
  "data-kind": dataKind,
  data,
  onClick,
  children,
}: InspectLinkProps) {
  const router = useRouter();
  const location = useRouterState({ select: (s) => s.location });
  const search = location.search as Record<string, unknown>;
  const open = parseInspectStack(search.inspect);
  const next = pushInspect(open, target);
  const stepsBack = next.truncated
    ? inspectStepsBack(location.state, open.length, next.stack.length)
    : null;
  return (
    <Link
      to={location.pathname}
      search={{ ...search, inspect: inspectStackParam(next.stack) } as never}
      state={(prev) => ({
        ...prev,
        ...(next.truncated
          ? { inspectDepth: undefined, inspectBase: undefined }
          : inspectPushEntry(location.state, open.length)),
      })}
      replace={next.truncated}
      resetScroll={false}
      className={className}
      aria-label={ariaLabel}
      data-kind={dataKind}
      {...data}
      onClick={(event) => {
        onClick?.();
        // A plain click backs down through history; a modified one (a new
        // tab) still follows the address.
        if (
          stepsBack === null ||
          stepsBack === 0 ||
          event.button !== 0 ||
          event.metaKey ||
          event.ctrlKey ||
          event.shiftKey ||
          event.altKey
        ) {
          return;
        }
        event.preventDefault();
        router.history.go(-stepsBack);
      }}
    >
      {children}
    </Link>
  );
}
