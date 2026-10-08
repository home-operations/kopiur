import { useState } from "react";

/**
 * A value that lingers while its view animates out.
 *
 * The side panel is shown for whatever the URL names; when the URL stops
 * naming anything, the panel must stay mounted — still showing what it showed
 * — long enough to slide away. `shown` is the last value, `leaving` says it is
 * on its way out, and `exited` (called when the animation ends) lets it go.
 *
 * `same` decides whether a new value is really new: a target parsed afresh
 * from the URL on every render is a new object each time, and storing each
 * copy would re-render forever.
 */
export function useExiting<T>(
  value: T | null,
  same: (a: T, b: T) => boolean,
): { shown: T | null; leaving: boolean; exited: () => void } {
  const [shown, setShown] = useState<T | null>(value);
  const fresh = value !== null && (shown === null || !same(value, shown));
  if (fresh) {
    // Adopting the new value during render, not in an effect, so the panel
    // never paints a frame of the old one.
    setShown(value);
  }
  return {
    // A value that is present is always the one shown, so a refetched copy of
    // the same object shows its fresh data; the stored one only fills in
    // while leaving.
    shown: value ?? shown,
    leaving: value === null && shown !== null,
    exited: () => {
      setShown(null);
    },
  };
}
