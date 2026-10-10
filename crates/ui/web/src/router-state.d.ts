// History entries the console marks for itself. `inspectDepth` is set on the
// entry a resource drawer was opened onto — how many drawers are open there —
// so closing the top one can step back over that entry rather than stack a
// second copy of the page beneath it. `inspectBase` is how many drawers were
// open where that chain of opened entries began: stepping back further than
// that would leave the drawers' own entries (`components/inspect.ts`).
import "@tanstack/react-router";

declare module "@tanstack/react-router" {
  interface HistoryState {
    inspectDepth?: number | undefined;
    inspectBase?: number | undefined;
  }
}
