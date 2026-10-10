import type { ActionReceipt, Problem } from "../../api/types";

/**
 * What an action answered, as a toast. A discriminated union so a new kind
 * of answer cannot be pushed without the toaster rendering it.
 */
export type ToastEntry =
  /** Accepted: the receipt, worded as requested, never as done. */
  | { kind: "receipt"; label: string; receipt: ActionReceipt }
  /** Refused: the problem's what / why / fix. Stays until closed. */
  | { kind: "problem"; label: string; problem: Problem };

export type Toast = ToastEntry & { id: string };

/** How many toasts are kept at once. */
export const MAX_TOASTS = 5;

/**
 * The toast list: the one place every action's answer goes
 * (`api/queryClient.ts` pushes from the mutation cache), read by `Toaster`
 * through `useSyncExternalStore`.
 *
 * Framework-free, like `problemBanner`, so the mutation cache can drive it
 * and tests can read it without React. Past the cap the oldest *timed* toast
 * goes; a refusal is never pushed out before someone has closed it.
 */
function createToasts() {
  let list: readonly Toast[] = [];
  let next = 0;
  const listeners = new Set<() => void>();
  const set = (value: readonly Toast[]) => {
    list = value;
    for (const listener of listeners) listener();
  };
  // Arrow properties: `useSyncExternalStore` passes them detached.
  return {
    get: (): readonly Toast[] => list,
    push: (entry: ToastEntry): string => {
      next += 1;
      const id = `toast-${String(next)}`;
      let value: Toast[] = [...list, { ...entry, id }];
      while (value.length > MAX_TOASTS) {
        const oldestTimed = value.findIndex((toast) => toast.kind !== "problem");
        if (oldestTimed === -1) break;
        value = value.filter((_, index) => index !== oldestTimed);
      }
      set(value);
      return id;
    },
    dismiss: (id: string): void => {
      if (!list.some((toast) => toast.id === id)) return;
      set(list.filter((toast) => toast.id !== id));
    },
    clear: (): void => {
      if (list.length > 0) set([]);
    },
    subscribe: (listener: () => void): (() => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

export const toasts = createToasts();
