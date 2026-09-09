/**
 * Theme preference: follow the OS, or override it.
 *
 * The server sends no theme. The page follows `prefers-color-scheme` until the
 * user picks light or dark, which is stored in `localStorage` and applied as
 * `data-theme` on `<html>` — `styles.css` turns that into `color-scheme`, and
 * every colour token resolves through `light-dark()`. `initTheme()` runs in
 * `main.tsx` before the first render; an inline `<script>` in `index.html`
 * would have been earlier, but the backend's `default-src 'self'` CSP refuses
 * inline scripts.
 */

import { useSyncExternalStore } from "react";

export type ThemePreference = "system" | "light" | "dark";

export const THEME_STORAGE_KEY = "kopiur-ui.theme";

const THEME_ATTRIBUTE = "data-theme";

function isPreference(value: unknown): value is ThemePreference {
  return value === "system" || value === "light" || value === "dark";
}

/** The stored preference, or `system` when none is stored or storage is unavailable. */
export function readThemePreference(): ThemePreference {
  try {
    const stored: unknown = window.localStorage.getItem(THEME_STORAGE_KEY);
    return isPreference(stored) ? stored : "system";
  } catch {
    return "system";
  }
}

/** Reflect a preference on `<html>`; `system` removes the override. */
export function applyTheme(preference: ThemePreference): void {
  const root = document.documentElement;
  if (preference === "system") {
    root.removeAttribute(THEME_ATTRIBUTE);
  } else {
    root.setAttribute(THEME_ATTRIBUTE, preference);
  }
}

const listeners = new Set<() => void>();

/** Store and apply a preference, and tell every `useThemePreference` about it. */
export function setThemePreference(preference: ThemePreference): void {
  try {
    if (preference === "system") {
      window.localStorage.removeItem(THEME_STORAGE_KEY);
    } else {
      window.localStorage.setItem(THEME_STORAGE_KEY, preference);
    }
  } catch {
    // Storage may be unavailable (private mode, a locked-down browser); the
    // override then lasts for this page load only, which is still useful.
  }
  applyTheme(preference);
  for (const listener of listeners) {
    listener();
  }
}

/** Apply the stored preference. Call once, before the first render. */
export function initTheme(): void {
  applyTheme(readThemePreference());
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** The current preference and its setter, for the theme switch. */
export function useThemePreference(): [ThemePreference, (preference: ThemePreference) => void] {
  const preference = useSyncExternalStore(subscribe, readThemePreference, () => "system" as const);
  return [preference, setThemePreference];
}
