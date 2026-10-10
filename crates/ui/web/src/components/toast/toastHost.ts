/**
 * Where toasts must render to be seen and touched.
 *
 * The resource drawer is a modal `<dialog>`: while it is open it sits in the
 * top layer and the browser makes everything outside it inert. A toast
 * answering an action taken in the drawer would sit under it, unclickable.
 * So each open drawer registers its dialog here and the toaster portals into
 * the top one; with no drawer open, the toaster renders where the shell
 * mounts it.
 */
function createToastHost() {
  // Drawers stack: the top one is where toasts must be, and when it closes
  // the one beneath it is.
  const hosts: HTMLElement[] = [];
  const listeners = new Set<() => void>();
  const tell = () => {
    for (const listener of listeners) listener();
  };
  return {
    get: (): HTMLElement | null => hosts.at(-1) ?? null,
    /** `next` is the top drawer now; `null` forgets every host. */
    set: (next: HTMLElement | null): void => {
      if (next === null) {
        if (hosts.length === 0) return;
        hosts.length = 0;
        tell();
        return;
      }
      if (hosts.at(-1) === next) return;
      const at = hosts.indexOf(next);
      if (at !== -1) hosts.splice(at, 1);
      hosts.push(next);
      tell();
    },
    /** Unregister `node`, wherever it is in the stack. */
    release: (node: HTMLElement): void => {
      const at = hosts.indexOf(node);
      if (at === -1) return;
      hosts.splice(at, 1);
      tell();
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
