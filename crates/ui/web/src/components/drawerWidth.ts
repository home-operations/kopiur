/**
 * How wide the side panel is: half the window until someone drags it, then
 * whatever they dragged it to — remembered per browser, like the theme.
 *
 * The width is kept in pixels and clamped against the window it is shown in,
 * so a width chosen on a wide monitor reopens as wide as a laptop allows, not
 * wider. Storage can be blocked (private windows, policies); the panel then
 * opens at the default and forgets, rather than failing to open.
 */

export const DRAWER_WIDTH_KEY = "kopiur-ui.drawer-width";
/** Narrower than this and a facts list stops fitting its labels. */
export const DRAWER_MIN_WIDTH = 360;
export const DRAWER_MAX_SHARE = 0.85;
export const DRAWER_DEFAULT_SHARE = 0.5;
/** One arrow-key press. */
export const DRAWER_STEP = 24;

/** The widest the panel may be in a window this wide. */
export function drawerMaxWidth(viewport: number): number {
  return Math.floor(viewport * DRAWER_MAX_SHARE);
}

export function clampDrawerWidth(width: number, viewport: number): number {
  const max = drawerMaxWidth(viewport);
  const min = Math.min(DRAWER_MIN_WIDTH, max);
  return Math.round(Math.min(max, Math.max(min, width)));
}

export function readDrawerWidth(viewport: number): number {
  return clampDrawerWidth(storedWidth() ?? viewport * DRAWER_DEFAULT_SHARE, viewport);
}

/** The remembered width, or `null` when there is none worth trusting. */
function storedWidth(): number | null {
  try {
    const raw = window.localStorage.getItem(DRAWER_WIDTH_KEY);
    const parsed = raw === null || raw.trim() === "" ? Number.NaN : Number(raw);
    return Number.isFinite(parsed) && parsed > 0 ? parsed : null;
  } catch {
    return null;
  }
}

export function saveDrawerWidth(width: number): void {
  try {
    window.localStorage.setItem(DRAWER_WIDTH_KEY, String(Math.round(width)));
  } catch {
    // Not remembered this time; the panel still has the width it was given.
  }
}
