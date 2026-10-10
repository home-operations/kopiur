import { useEffect, useRef } from "react";

/**
 * What is left of each toast's time, by toast id, across remounts: the
 * toaster moves between the page and an open drawer (`toastHost.ts`), and a
 * portal that moves remounts what it holds. Without this a toast would get a
 * fresh full duration every time a drawer opened or closed.
 */
const remaining = new Map<string, number>();

/**
 * Call `onDone` once `ms` of *unpaused* time has passed for toast `id`. `ms`
 * null never ends.
 *
 * The clock is JS, not a CSS `animationend`: the reduced-motion rule in the
 * stylesheet collapses every animation to an instant, and a toast must still
 * last its time for a reader who asked for less motion. Pausing keeps what is
 * left, so resuming runs out the remainder rather than a fresh duration.
 */
export function useDecay(id: string, ms: number | null, paused: boolean, onDone: () => void): void {
  const done = useRef(onDone);
  useEffect(() => {
    done.current = onDone;
  });
  useEffect(() => {
    if (paused || ms === null) return undefined;
    const left = remaining.get(id) ?? ms;
    const started = Date.now();
    const timer = window.setTimeout(() => {
      remaining.delete(id);
      done.current();
    }, left);
    return () => {
      window.clearTimeout(timer);
      const elapsed = Date.now() - started;
      if (elapsed < left) remaining.set(id, left - elapsed);
    };
  }, [id, ms, paused]);
}

/** How much of a toast's time has already run, for its line to start part-way. */
export function decayElapsed(id: string, ms: number): number {
  return ms - (remaining.get(id) ?? ms);
}

/** Forget a toast's clock once it is gone. */
export function forgetDecay(id: string): void {
  remaining.delete(id);
}
