import { useRouter, useRouterState } from "@tanstack/react-router";
import { ChevronsUpDown } from "lucide-react";
import { useState } from "react";

import { PickerOptions } from "../PickerOptions";
import { useNamespaceOptions } from "../namespaceOptions";
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
 * The list is `PickerOptions` over `useNamespaceOptions`, the same one every
 * namespace field in a form opens. Choosing one keeps the current page and its filters (but not its
 * page offset).
 */
export function NamespaceSwitcher() {
  const router = useRouter();
  const location = useRouterState({ select: (s) => s.location });
  const current = useCurrentNamespace();
  const [open, setOpen] = useState(false);
  const { options, total } = useNamespaceOptions(open);

  const choose = (namespace: string | undefined) => {
    void router.navigate({
      href: `${location.pathname}${withNamespace(location.search as Record<string, unknown>, namespace)}`,
    });
  };

  const scope = current ?? "all namespaces";

  return (
    <div className="ns-switcher">
      <Popover
        label="Choose a namespace"
        open={open}
        onOpenChange={setOpen}
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
          <PickerOptions
            options={options}
            current={current}
            empty={{ label: "all namespaces", meta: total, initial: "*" }}
            search={{ noun: "namespaces" }}
            onChoose={(namespace) => {
              choose(namespace);
              close();
            }}
          />
        )}
      </Popover>
    </div>
  );
}
