import { Check, Search } from "lucide-react";
import { useState } from "react";

import { useNamespaces } from "../api/hooks";

export interface NamespaceOptionsProps {
  /** The namespace chosen now; `undefined` is the empty choice. */
  current: string | undefined;
  /** Called with the namespace picked, or `undefined` for the empty choice. */
  onChoose: (namespace: string | undefined) => void;
  /** What the empty choice means here: "all namespaces", "the listing's namespace". */
  emptyLabel: string;
  /**
   * Offer what was typed as a choice of its own when no listed namespace has
   * that name — the list holds only the namespaces the caller may list, and a
   * field may still name another one.
   */
  freeform?: boolean | undefined;
}

/**
 * The body of every namespace picker: a filter field over the namespaces the
 * caller may see, with their object counts, below the empty choice.
 *
 * Loading, a refusal or an empty list leave the empty choice, which is always
 * valid. Rendered inside a `Popover`, so it fetches only once opened.
 */
export function NamespaceOptions({
  current,
  onChoose,
  emptyLabel,
  freeform = false,
}: NamespaceOptionsProps) {
  const [filter, setFilter] = useState("");
  const namespaces = useNamespaces();

  const listed = namespaces.isSuccess ? namespaces.data : [];
  const needle = filter.trim().toLowerCase();
  const shown = listed.filter((ns) => ns.name.toLowerCase().includes(needle));
  const total = listed.reduce((sum, ns) => sum + ns.objects, 0);
  const typed = filter.trim();
  const offerTyped = freeform && typed.length > 0 && !listed.some((ns) => ns.name === typed);

  return (
    <>
      <label className="ns-switcher__filter">
        <Search size={14} strokeWidth={2} aria-hidden="true" />
        <input
          type="search"
          aria-label="Filter namespaces"
          placeholder={freeform ? "Filter or type a namespace…" : "Filter namespaces…"}
          value={filter}
          autoComplete="off"
          spellCheck={false}
          onChange={(event) => {
            setFilter(event.target.value);
          }}
          onKeyDown={(event) => {
            // Enter takes what was typed — the one listed match, else the text
            // itself where a field allows it — and keeps the form from submitting.
            if (event.key !== "Enter") return;
            event.preventDefault();
            const only = shown.length === 1 ? shown[0] : undefined;
            if (only !== undefined) onChoose(only.name);
            else if (offerTyped) onChoose(typed);
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
              onChoose(undefined);
            }}
          >
            <span
              className="ns-switcher__avatar ns-switcher__avatar--all"
              data-initial="*"
              aria-hidden="true"
            />
            {emptyLabel}
            {namespaces.isSuccess ? <span className="ns-switcher__count">{total}</span> : null}
          </button>
        </li>
        {offerTyped ? (
          <li>
            <button
              type="button"
              className="ns-switcher__option"
              onClick={() => {
                onChoose(typed);
              }}
            >
              <span
                className="ns-switcher__avatar"
                data-initial={typed.charAt(0)}
                aria-hidden="true"
              />
              {typed}
              <span className="ns-switcher__count">not listed</span>
            </button>
          </li>
        ) : null}
        {shown.map((ns) => (
          <li key={ns.name}>
            <button
              type="button"
              className="ns-switcher__option"
              aria-current={current === ns.name ? "true" : undefined}
              onClick={() => {
                onChoose(ns.name);
              }}
            >
              <span
                className="ns-switcher__avatar"
                data-initial={ns.name.charAt(0)}
                aria-hidden="true"
              />
              {ns.name}
              <span className="ns-switcher__count">{ns.objects}</span>
              {current === ns.name ? <Check size={14} strokeWidth={2} aria-hidden="true" /> : null}
            </button>
          </li>
        ))}
      </ul>
    </>
  );
}
