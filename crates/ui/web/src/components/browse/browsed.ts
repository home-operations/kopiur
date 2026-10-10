import { parseSnapshotParam } from "./browse";

/**
 * What Browse is on, for the rest of the shell: the snapshot the tab last
 * browsed, and whether the last attempt to start a session on it failed.
 *
 * The snapshot is kept in the tab's `sessionStorage` so Browse from the
 * sidebar comes back to it; the failed start is not, since a fresh tab has
 * not failed anything. Both are conveniences: storage that refuses to be read
 * or written leaves the page working, starting on the picker.
 */
export interface Browsed {
  /** `namespace/name` of the snapshot last browsed. */
  snapshot: string | null;
  /** `namespace/name` of the snapshot whose last session start failed. */
  failedStart: string | null;
}

const STORED = "kopiur.browse.snapshot";
const EMPTY: Browsed = { snapshot: null, failedStart: null };

function createBrowsed() {
  let state: Browsed = EMPTY;
  const listeners = new Set<() => void>();
  const set = (next: Browsed) => {
    if (next.snapshot === state.snapshot && next.failedStart === state.failedStart) return;
    state = next;
    for (const listener of listeners) listener();
  };
  const stored = (): string | null => {
    try {
      const value = window.sessionStorage.getItem(STORED);
      return parseSnapshotParam(value) !== null ? value : null;
    } catch {
      return null;
    }
  };
  return {
    get: (): Browsed => state,
    /** The snapshot the tab stored, if it is still a snapshot address. */
    stored,
    /** Browse is on `snapshot` now. */
    browse: (snapshot: string): void => {
      try {
        window.sessionStorage.setItem(STORED, snapshot);
      } catch {
        // Refused (a private window, blocked site data): remembered for this
        // page load only.
      }
      set({ ...state, snapshot });
    },
    /** A session start on `snapshot` answered, `ok` or not. */
    started: (snapshot: string, ok: boolean): void => {
      if (ok) {
        set({ ...state, failedStart: state.failedStart === snapshot ? null : state.failedStart });
      } else {
        set({ ...state, failedStart: snapshot });
      }
    },
    subscribe: (listener: () => void): (() => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    /** For tests: forget everything held in memory. */
    reset: (): void => {
      set(EMPTY);
    },
  };
}

export const browsed = createBrowsed();

/** How Browse stands, for the sidebar's line under it. */
export interface BrowseStatus {
  state: "idle" | "active" | "failed";
  /** The same, in words, for the link's description. */
  words: string;
}

/**
 * Browse's state from what it is on, whether that snapshot's session is
 * running, and whether reading the session failed.
 *
 * Running wins over a failure before it: a start that failed and was retried
 * is a running session now. A failure counts only for the snapshot Browse is
 * on.
 */
export function browseStatus(on: Browsed, running: boolean, unreadable: boolean): BrowseStatus {
  const snapshot = on.snapshot;
  if (snapshot === null) {
    return { state: "idle", words: "No session running" };
  }
  if (running) {
    return { state: "active", words: `A browse session is running on ${snapshot}` };
  }
  if (unreadable || on.failedStart === snapshot) {
    return { state: "failed", words: `The last browse session on ${snapshot} failed` };
  }
  return { state: "idle", words: "No session running" };
}
