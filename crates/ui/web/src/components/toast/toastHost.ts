/**
 * Where toasts must render to be seen and touched.
 *
 * The resource drawer is a modal `<dialog>`: while it is open it sits in the
 * top layer and the browser makes everything outside it inert. A toast
 * answering an action taken in the drawer would sit under it, unclickable.
 * So the open drawer registers its dialog here and the toaster portals into
 * it; with no drawer open, the toaster renders where the shell mounts it.
 */
function createToastHost() {
  let host: HTMLElement | null = null;
  const listeners = new Set<() => void>();
  return {
    get: (): HTMLElement | null => host,
    set: (next: HTMLElement | null): void => {
      if (next === host) return;
      host = next;
      for (const listener of listeners) listener();
    },
    /** Unregister `node`, if it is still the host. */
    release: (node: HTMLElement): void => {
      if (host !== node) return;
      host = null;
      for (const listener of listeners) listener();
    },
    subscribe: (listener: () => void): (() => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

export const toastHost = createToastHost();
