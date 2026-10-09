/**
 * Which columns a list table shows, in what order, and how wide — the rules,
 * kept apart from storage and from React so each one can be pinned by a test.
 *
 * A table declares its columns once ({@link ColumnSpec}); a person's changes
 * are layered on top as {@link TablePrefs}. Only what someone changed is ever
 * stored, so a column added to a table later simply appears in its declared
 * place, and one removed is ignored wherever it was remembered — there is no
 * schema to migrate.
 *
 * The first column of every table is `locked`: it carries the kind stripe,
 * the chip and the link that makes the whole row open the object, so it can
 * neither be hidden nor moved off the left edge. Action columns are locked at
 * the other end for the same reason — a row's button stays where the hand
 * expects it.
 */

/** No column grows past this, however far it is dragged. */
export const MAX_COLUMN_WIDTH = 800;
/** One arrow-key press on a resize handle. */
export const COLUMN_STEP = 16;

export interface ColumnSpec<Id extends string = string> {
  /** Stable: stored preferences key on it. */
  id: Id;
  /** The header's words, and the column picker's. */
  label: string;
  /** Pixels, or `"auto"` for the column that takes the space left over. */
  width: number | "auto";
  /** The narrowest a drag may make it; enough for the header word. */
  min: number;
  /** Right-aligned tabular figures. */
  numeric?: boolean;
  /** Cannot be hidden or moved. */
  locked?: boolean;
  /** Defaults to true; false for a column of buttons. */
  resizable?: boolean;
  /** Off until someone turns it on. */
  defaultHidden?: boolean;
  /** Carries the row's kind stripe (the identity column). */
  stripe?: boolean;
  /** Extra classes on this column's cells. */
  className?: string;
}

/** What one person changed about one table. */
export interface TablePrefs {
  /** `true` hides a column, `false` shows one that is hidden by default. */
  hidden?: Readonly<Record<string, boolean>>;
  /** Widths someone dragged to, in pixels. */
  width?: Readonly<Record<string, number>>;
  /** The movable columns, in the order someone put them. */
  order?: readonly string[];
}

export function isVisible(spec: ColumnSpec, prefs: TablePrefs): boolean {
  if (spec.locked === true) return true;
  const stored = prefs.hidden?.[spec.id];
  return stored === undefined ? spec.defaultHidden !== true : !stored;
}

/**
 * The movable columns in their current order: the stored order with ids that
 * no longer exist (or repeat) dropped, then any column it does not mention, in
 * declaration order.
 */
export function movableOrder(specs: readonly ColumnSpec[], prefs: TablePrefs): string[] {
  const movable = specs.filter((s) => s.locked !== true).map((s) => s.id);
  const known = new Set(movable);
  const seen = new Set<string>();
  const order: string[] = [];
  for (const id of prefs.order ?? []) {
    if (known.has(id) && !seen.has(id)) {
      seen.add(id);
      order.push(id);
    }
  }
  return [...order, ...movable.filter((id) => !seen.has(id))];
}

/**
 * Every column in display order, hidden ones included (so a column shown
 * again comes back where it was). Locked columns keep their declared index;
 * the movable ones fill the other slots in their current order.
 */
export function orderedColumns<C extends ColumnSpec>(specs: readonly C[], prefs: TablePrefs): C[] {
  const byId = new Map(specs.map((s) => [s.id, s]));
  const queue = movableOrder(specs, prefs);
  return specs.map((spec) => {
    if (spec.locked === true) return spec;
    const next = queue.shift();
    return (next === undefined ? undefined : byId.get(next)) ?? spec;
  });
}

/** The columns on screen, in order. Never empty: a table always has its first column. */
export function visibleColumns<C extends ColumnSpec>(specs: readonly C[], prefs: TablePrefs): C[] {
  const shown = orderedColumns(specs, prefs).filter((s) => isVisible(s, prefs));
  const first = specs[0];
  return shown.length > 0 || first === undefined ? shown : [first];
}

/** Hidden columns a person could turn back on. */
export function hiddenCount(specs: readonly ColumnSpec[], prefs: TablePrefs): number {
  return specs.filter((s) => s.locked !== true && !isVisible(s, prefs)).length;
}

/** `order` with `id` moved to position `to` (clamped to the ends). */
export function moveColumn(order: readonly string[], id: string, to: number): string[] {
  const from = order.indexOf(id);
  if (from === -1) return [...order];
  const rest = order.filter((o) => o !== id);
  const at = Math.max(0, Math.min(rest.length, to));
  return [...rest.slice(0, at), id, ...rest.slice(at)];
}

export function clampWidth(spec: ColumnSpec, px: number): number {
  if (!Number.isFinite(px)) return spec.min;
  return Math.max(spec.min, Math.min(MAX_COLUMN_WIDTH, Math.round(px)));
}

/** The width a column is drawn at, or `null` for the flexible column left alone. */
export function columnWidth(spec: ColumnSpec, prefs: TablePrefs): number | null {
  const stored = prefs.width?.[spec.id];
  if (stored !== undefined) return clampWidth(spec, stored);
  return spec.width === "auto" ? null : spec.width;
}

/**
 * The narrowest the table may be: every visible column at its width (the
 * flexible one at its floor). Wider than its card, it scrolls inside it.
 */
export function tableMinWidth(columns: readonly ColumnSpec[], prefs: TablePrefs): number {
  return columns.reduce((sum, c) => sum + (columnWidth(c, prefs) ?? c.min), 0);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function pick<T>(value: unknown, keep: (v: unknown) => v is T): Record<string, T> | undefined {
  if (!isRecord(value)) return undefined;
  const out: Record<string, T> = {};
  for (const [key, v] of Object.entries(value)) {
    if (keep(v)) out[key] = v;
  }
  return out;
}

const isBoolean = (v: unknown): v is boolean => typeof v === "boolean";
const isWidth = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v) && v > 0;

function parseTable(value: unknown): TablePrefs | undefined {
  if (!isRecord(value)) return undefined;
  const prefs: { -readonly [K in keyof TablePrefs]: TablePrefs[K] } = {};
  const hidden = pick(value.hidden, isBoolean);
  const width = pick(value.width, isWidth);
  if (hidden !== undefined) prefs.hidden = hidden;
  if (width !== undefined) prefs.width = width;
  if (Array.isArray(value.order)) {
    prefs.order = value.order.filter((id): id is string => typeof id === "string");
  }
  return prefs;
}

/** Every table's stored preferences; anything malformed is dropped, never thrown. */
export function parsePrefs(raw: string | null): Record<string, TablePrefs> {
  if (raw === null || raw.trim() === "") return {};
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return {};
  }
  if (!isRecord(parsed)) return {};
  const out: Record<string, TablePrefs> = {};
  for (const [table, value] of Object.entries(parsed)) {
    const prefs = parseTable(value);
    if (prefs !== undefined) out[table] = prefs;
  }
  return out;
}
