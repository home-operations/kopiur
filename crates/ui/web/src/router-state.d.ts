// History entries the console marks for itself. `inspect` is set on the entry a
// resource drawer was opened onto, so closing it can step back over that entry
// rather than stack a second copy of the page beneath it (`components/inspect.ts`).
import "@tanstack/react-router";

declare module "@tanstack/react-router" {
  interface HistoryState {
    inspect?: true;
  }
}
