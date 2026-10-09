import { useSyncExternalStore } from "react";

import { type TablePrefs, parsePrefs } from "./tableColumns";

/**
 * Every list table's column layout, remembered per browser under one key.
 *
 * Kept in memory and written through: a hide or a move is stored at once; a
 * width changes on every frame of a drag, so it lives in memory until
 * {@link commitWidths} (the drag's end, or a key press). Storage can be
 * blocked; the layout then holds for the visit and is forgotten. Another tab
 * changing it is picked up from the `storage` event.
 *
 * Each table's preferences keep the same object until that table changes —
 * `useSyncExternalStore` compares snapshots by identity, so a fresh object per
 * read would re-render forever.
 */
export const COLUMNS_KEY = "kopiur-ui.columns";

const EMPTY: TablePrefs = Object.freeze({});

let memory: Record<string, TablePrefs> | null = null;
const listeners = new Set<() => void>();

function load(): Record<string, TablePrefs> {
  if (memory === null) {
    let raw: string | null = null;
    try {
      raw = window.localStorage.getItem(COLUMNS_KEY);
    } catch {
      // Blocked: start from the declared layout.
    }
    memory = parsePrefs(raw);
  }
  return memory;
}

function persist(): void {
  try {
    window.localStorage.setItem(COLUMNS_KEY, JSON.stringify(load()));
  } catch {
    // Not remembered this time; the layout still holds for this visit.
  }
}

function notify(): void {
  for (const listener of listeners) listener();
}

function update(table: string, change: (prefs: TablePrefs) => TablePrefs, save: boolean): void {
  const all = load();
  const before = all[table] ?? EMPTY;
  const after = change(before);
  if (JSON.stringify(after) === JSON.stringify(before)) return;
  const others = Object.entries(all).filter(([name]) => name !== table);
  memory = Object.fromEntries(
    Object.keys(after).length === 0 ? others : [...others, [table, after]],
  );
  if (save) persist();
  notify();
}

/** `record` with `key` set to `value`, or removed when `value` is undefined. */
function withKey<T>(
  record: Readonly<Record<string, T>> | undefined,
  key: string,
  value: T | undefined,
): Record<string, T> | undefined {
  const others = Object.entries(record ?? {}).filter(([k]) => k !== key);
  const entries = value === undefined ? others : [...others, [key, value] as const];
  return entries.length > 0 ? Object.fromEntries(entries) : undefined;
}

/** `prefs` with one field replaced, the field dropped when it is empty. */
function withField<K extends keyof TablePrefs>(
  prefs: TablePrefs,
  field: K,
  value: TablePrefs[K] | undefined,
): TablePrefs {
  const rest = Object.fromEntries(Object.entries(prefs).filter(([k]) => k !== field)) as TablePrefs;
  return value === undefined ? rest : { ...rest, [field]: value };
}

function onStorage(event: StorageEvent): void {
  if (event.key !== COLUMNS_KEY) return;
  memory = parsePrefs(event.newValue);
  notify();
}

export function subscribeColumnPrefs(listener: () => void): () => void {
  if (listeners.size === 0) window.addEventListener("storage", onStorage);
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) window.removeEventListener("storage", onStorage);
  };
}

export function tablePrefs(table: string): TablePrefs {
  return load()[table] ?? EMPTY;
}

export function useColumnPrefs(table: string): TablePrefs {
  return useSyncExternalStore(
    subscribeColumnPrefs,
    () => tablePrefs(table),
    () => EMPTY,
  );
}

export function setHidden(table: string, id: string, hidden: boolean): void {
  update(table, (p) => withField(p, "hidden", withKey(p.hidden, id, hidden)), true);
}

/** While dragging: in memory only. */
export function setWidth(table: string, id: string, px: number): void {
  update(table, (p) => withField(p, "width", withKey(p.width, id, px)), false);
}

/** Store whatever widths are in memory. */
export function commitWidths(): void {
  persist();
}

export function resetWidth(table: string, id: string): void {
  update(table, (p) => withField(p, "width", withKey(p.width, id, undefined)), true);
}

export function setOrder(table: string, order: readonly string[]): void {
  update(table, (p) => withField(p, "order", order.length > 0 ? [...order] : undefined), true);
}

/** Order, widths and visibility back to the table's declared layout. */
export function resetColumns(table: string): void {
  update(table, () => EMPTY, true);
}

/** Drop the in-memory copy so the next read comes from storage, as a reload would. */
export function forgetColumnPrefs(): void {
  memory = null;
}
