/**
 * Where a two-column split sits: the start side's share of the width, half
 * and half until someone drags it, then whatever they left it at —
 * remembered per browser, like the drawer's width.
 *
 * Kept as a share rather than pixels so it survives a different window. Each
 * side keeps at least {@link SPLIT_MIN_PANE}; narrower than both of those and
 * the gutter, the two sides stack and the share is not used. Storage can be
 * blocked; the split then opens at half and forgets.
 */

/** The narrowest either side may be. */
export const SPLIT_MIN_PANE = 440;
/** The gutter between the sides, holding the handle. */
export const SPLIT_GUTTER = 8;
export const SPLIT_DEFAULT = 0.5;
/** One arrow-key press. */
export const SPLIT_STEP = 0.04;
/** When the width is not known (not laid out yet), the furthest either way. */
const UNMEASURED_MIN = 0.2;

/** The share clamped so both sides keep their minimum in a pane this wide. */
export function clampSplit(ratio: number, width: number): number {
  const usable = width - SPLIT_GUTTER;
  const min = usable > 2 * SPLIT_MIN_PANE ? SPLIT_MIN_PANE / usable : UNMEASURED_MIN;
  return Math.min(1 - min, Math.max(min, ratio));
}

export function readSplit(key: string): number {
  try {
    const raw = window.localStorage.getItem(key);
    const parsed = raw === null || raw.trim() === "" ? Number.NaN : Number(raw);
    return Number.isFinite(parsed) && parsed > 0 && parsed < 1 ? parsed : SPLIT_DEFAULT;
  } catch {
    return SPLIT_DEFAULT;
  }
}

export function saveSplit(key: string, ratio: number): void {
  try {
    window.localStorage.setItem(key, String(Math.round(ratio * 100) / 100));
  } catch {
    // Not remembered this time; the split still sits where it was put.
  }
}
