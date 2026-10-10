import { useRouterState } from "@tanstack/react-router";
import { Search } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import {
  SEARCH_MIN_CHARS,
  useMaintenance,
  usePolicies,
  useReplications,
  useRepositories,
  useRestores,
  useSchedules,
  useSnapshotSearch,
} from "../../api/hooks";
import type { ObjectKind } from "../../api/types";
import { useCurrentNamespace } from "../../util/namespace";
import { ObjectRef } from "../ObjectRef";

interface Hit {
  kind: ObjectKind;
  name: string;
  namespace?: string | undefined;
}

const MAX_HITS = 8;

/** True while focus is somewhere a "/" is text, not a shortcut. */
function typingInField(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName);
}

/**
 * Find any object by name, across every kind, in the current scope.
 *
 * Nothing is asked until the query has {@link SEARCH_MIN_CHARS} characters.
 * The list reads are the ones every list page uses (and usually already
 * cached); snapshots, which are paged, are searched by the server's `?q=`.
 * `/` focuses the field from anywhere that is not itself a text field.
 */
export function ObjectSearch() {
  const namespace = useCurrentNamespace();
  const href = useRouterState({ select: (s) => s.location.href });
  // The query belongs to the page it was typed on: following a result (or
  // any navigation) leaves it behind, without an effect resetting state.
  const [typed, setTyped] = useState({ q: "", at: href });
  const q = typed.at === href ? typed.q : "";
  const setQ = (next: string) => {
    setTyped({ q: next, at: href });
  };
  const inputRef = useRef<HTMLInputElement>(null);
  const term = q.trim().toLowerCase();
  const active = term.length >= SEARCH_MIN_CHARS;
  const options = { enabled: active };

  const repositories = useRepositories(namespace, options);
  const policies = usePolicies(namespace, options);
  const schedules = useSchedules(namespace, options);
  const restores = useRestores(namespace, options);
  const maintenance = useMaintenance(namespace, options);
  const replications = useReplications(namespace, options);
  const snapshots = useSnapshotSearch(q, namespace);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "/" && !typingInField(event.target)) {
        event.preventDefault();
        inputRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  // What has not answered, and what refused to: absence is only claimed when
  // every read the search spans actually came back.
  const sources = [
    ["repositories", repositories],
    ["policies", policies],
    ["schedules", schedules],
    ["restores", restores],
    ["maintenance", maintenance],
    ["replications", replications],
    ["snapshots", snapshots],
  ] as const;
  const searching = active && sources.some(([, query]) => query.isPending);
  const unsearched = sources.filter(([, query]) => query.isError).map(([label]) => label);
  const matches = (name: string) => name.toLowerCase().includes(term);
  const hits: Hit[] = active
    ? [
        ...(repositories.data ?? [])
          .filter((r) => matches(r.name))
          .map((r): Hit => ({
            kind: r.kind === "ClusterRepository" ? "clusterRepository" : "repository",
            name: r.name,
            namespace: r.namespace ?? undefined,
          })),
        ...(policies.data ?? [])
          .filter((p) => matches(p.name))
          .map((p): Hit => ({
            kind: "snapshotPolicy",
            name: p.name,
            namespace: p.namespace,
          })),
        ...(schedules.data ?? [])
          .filter((s) => matches(s.name))
          .map((s): Hit => ({ kind: "snapshotSchedule", name: s.name, namespace: s.namespace })),
        ...(snapshots.data?.items ?? []).map((s): Hit => ({
          kind: "snapshot",
          name: s.name,
          namespace: s.namespace,
        })),
        ...(restores.data ?? [])
          .filter((r) => matches(r.name))
          .map((r): Hit => ({
            kind: "restore",
            name: r.name,
            namespace: r.namespace,
          })),
        ...(maintenance.data ?? [])
          .filter((m) => matches(m.name))
          .map((m): Hit => ({ kind: "maintenance", name: m.name, namespace: m.namespace })),
        ...(replications.data?.repository ?? [])
          .filter((r) => matches(r.name))
          .map((r): Hit => ({
            kind: "repositoryReplication",
            name: r.name,
            namespace: r.namespace,
          })),
        ...(replications.data?.snapshot ?? [])
          .filter((r) => matches(r.name))
          .map((r): Hit => ({ kind: "snapshotReplication", name: r.name, namespace: r.namespace })),
      ].slice(0, MAX_HITS)
    : [];

  return (
    <div className="search">
      <label className="search__field">
        <Search size={14} strokeWidth={2} aria-hidden="true" />
        <input
          ref={inputRef}
          type="search"
          aria-label="Find an object"
          placeholder="Find an object…"
          value={q}
          onChange={(event) => {
            setQ(event.target.value);
          }}
          onKeyDown={(event) => {
            if (event.key === "Escape") setQ("");
          }}
        />
        <kbd className="search__key" aria-hidden="true">
          /
        </kbd>
      </label>
      {active ? (
        <ul className="search__results" aria-label="Search results">
          {unsearched.length > 0 ? (
            <li className="search__empty">{joinWords(unsearched)} could not be searched.</li>
          ) : null}
          {hits.length === 0 ? (
            searching ? (
              <li className="search__empty">Searching…</li>
            ) : unsearched.length === 0 ? (
              <li className="search__empty">No object named like “{q.trim()}” in this scope.</li>
            ) : null
          ) : (
            hits.map((hit) => (
              <li key={`${hit.kind}/${hit.namespace ?? ""}/${hit.name}`}>
                <ObjectRef
                  kind={hit.kind}
                  name={hit.name}
                  namespace={hit.namespace}
                  contextNamespace={namespace}
                  onOpen={() => {
                    setQ("");
                  }}
                />
              </li>
            ))
          )}
        </ul>
      ) : null}
    </div>
  );
}

/** "a", "a and b", "a, b and c" — capitalised, as the start of a sentence. */
function joinWords(words: readonly string[]): string {
  const text =
    words.length <= 1
      ? words.join("")
      : `${words.slice(0, -1).join(", ")} and ${words[words.length - 1] ?? ""}`;
  return text.charAt(0).toUpperCase() + text.slice(1);
}
