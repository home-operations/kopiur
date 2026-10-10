import { useEffect, useRef } from "react";

/**
 * Each toast's clock, by toast id, outside React: the toaster moves between
 * the page and an open drawer (`toastHost.ts`), and a portal that moves
 * remounts what it holds. The new copy renders before the old one's effects
 * clean up, so the clock must be right whenever it is read — hence what is
 * left plus when it last started running, rather than a value saved on pause.
 */
interface Clock {
  /** Time left as of `since` (or now, when stopped). */
  left: number;
  /** When it last started running; null while stopped. */
  since: number | null;
}

const clocks = new Map<string, Clock>();

function leftOf(clock: Clock, now: number): number {
  return clock.since === null ? clock.left : Math.max(0, clock.left - (now - clock.since));
}

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
    const now = Date.now();
    const clock = clocks.get(id) ?? { left: ms, since: null };
    const left = leftOf(clock, now);
    clocks.set(id, { left, since: now });
    const timer = window.setTimeout(() => {
      clocks.delete(id);
      done.current();
    }, left);
    return () => {
      window.clearTimeout(timer);
      const running = clocks.get(id);
      if (running !== undefined) clocks.set(id, { left: leftOf(running, Date.now()), since: null });
    };
  }, [id, ms, paused]);
}

/** How much of a toast's time has run, for its line to start part-way. */
export function decayElapsed(id: string, ms: number): number {
  const clock = clocks.get(id);
  return clock === undefined ? 0 : ms - leftOf(clock, Date.now());
}

/** Forget a toast's clock once it is gone. */
export function forgetDecay(id: string): void {
  clocks.delete(id);
}
