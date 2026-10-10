import { useRouter, useRouterState } from "@tanstack/react-router";
import { Check, ChevronsUpDown, Search } from "lucide-react";
import { useState } from "react";

import { useNamespaces } from "../../api/hooks";
import { Popover } from "../Popover";
import { useCurrentNamespace } from "../../util/namespace";

/** The current search with `namespace` set (or removed for cluster-wide), as a query string. */
function withNamespace(search: Record<string, unknown>, namespace: string | undefined): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(search)) {
    if (key === "namespace" || key === "offset" || value === undefined || value === null) continue;
    params.set(key, typeof value === "string" ? value : JSON.stringify(value));
  }
  if (namespace !== undefined) params.set("namespace", namespace);
  const query = params.toString();
  return query.length > 0 ? `?${query}` : "";
}

/**
 * The namespace scope, switched like a workspace: it frames the whole app.
 * A `Popover` under the button, opening onto its filter field.
 *
 * Lists `/api/v1/namespaces` — only namespaces the caller may see, with their
 * object counts — behind a filter field. Loading, a refusal or an empty list
 * all leave "all namespaces", which is always a valid scope. Choosing one keeps
 * the current page and its filters (but not its page offset).
 */
export function NamespaceSwitcher() {
  const router = useRouter();
  const location = useRouterState({ select: (s) => s.location });
  const current = useCurrentNamespace();
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState("");
  const namespaces = useNamespaces({ enabled: open });
  const setOpenState = (next: boolean) => {
    setOpen(next);
    if (!next) setFilter("");
  };

  const choose = (namespace: string | undefined) => {
    void router.navigate({
      href: `${location.pathname}${withNamespace(location.search as Record<string, unknown>, namespace)}`,
    });
  };

  const listed = namespaces.isSuccess ? namespaces.data : [];
  const needle = filter.trim().toLowerCase();
  const shown = listed.filter((ns) => ns.name.toLowerCase().includes(needle));
  const total = listed.reduce((sum, ns) => sum + ns.objects, 0);
  const scope = current ?? "all namespaces";

  return (
    <div className="ns-switcher">
      <Popover
        label="Choose a namespace"
        open={open}
        onOpenChange={setOpenState}
        className="ns-switcher__panel"
        initialFocus="input[type=search]"
        trigger={(props) => (
          <button
            type="button"
            className="ns-switcher__button"
            {...props}
            aria-label={`Namespace: ${scope}`}
          >
            <span
              className="ns-switcher__avatar"
              data-initial={current === undefined ? "*" : current.charAt(0)}
              aria-hidden="true"
            />
            <span className="ns-switcher__text">
              <span className="ns-switcher__scope">{scope}</span>
              <span className="ns-switcher__hint">namespace scope</span>
            </span>
            <ChevronsUpDown size={14} strokeWidth={2} aria-hidden="true" />
          </button>
        )}
      >
        {(close) => (
          <>
            <label className="ns-switcher__filter">
              <Search size={14} strokeWidth={2} aria-hidden="true" />
              <input
                type="search"
                aria-label="Filter namespaces"
                placeholder="Filter namespaces…"
                value={filter}
                onChange={(event) => {
                  setFilter(event.target.value);
                }}
              />
            </label>
            <ul className="ns-switcher__list">
              <li>
                <button
                  type="button"
                  className="ns-switcher__option"
                  aria-current={current === undefined ? "true" : undefined}
                  onClick={() => {
                    choose(undefined);
                    close();
                  }}
                >
                  <span
                    className="ns-switcher__avatar ns-switcher__avatar--all"
                    data-initial="*"
                    aria-hidden="true"
                  />
                  all namespaces
                  {namespaces.isSuccess ? (
                    <span className="ns-switcher__count">{total}</span>
                  ) : null}
                </button>
              </li>
              {shown.map((ns) => (
                <li key={ns.name}>
                  <button
                    type="button"
                    className="ns-switcher__option"
                    aria-current={current === ns.name ? "true" : undefined}
                    onClick={() => {
                      choose(ns.name);
                      close();
                    }}
                  >
                    <span
                      className="ns-switcher__avatar"
                      data-initial={ns.name.charAt(0)}
                      aria-hidden="true"
                    />
                    {ns.name}
                    <span className="ns-switcher__count">{ns.objects}</span>
                    {current === ns.name ? (
                      <Check size={14} strokeWidth={2} aria-hidden="true" />
                    ) : null}
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
      </Popover>
    </div>
  );
}
