/**
 * Focus for a confirmation panel — the one implementation, shared.
 *
 * Three panels exist in this console: `ActionPanel`, and the two bespoke
 * action bars on the snapshot and repository details that predate it
 * (`SnapshotActions`, `RepositoryActions`). They are the same shape — a
 * trigger, a `role="group"` panel below it, a result — and they must behave
 * the same under a keyboard, so the behaviour lives here rather than three
 * times. (The panels themselves should be folded into `ActionPanel`; that is
 * a refactor, this is the part that is a correctness bug.)
 *
 * **Managed, not trapped.** Nothing here is modal: the page behind the panel
 * stays visible and usable, so capturing the tab order would strand a reader
 * who wants to go back and check what they are about to confirm — exactly
 * what a destructive confirmation should invite. What a non-modal disclosure
 * owes instead is that focus never goes missing:
 *
 * 1. Opening moves focus into the panel. The `group` takes it rather than its
 *    first control, so a screen reader announces *what* was opened before the
 *    reader starts walking the fields.
 * 2. Escape closes it — listened for on the panel node, so it only fires
 *    while focus is inside, and so a keyboard handler is not bound to a
 *    non-interactive role in JSX.
 * 3. Closing — by Escape, by Cancel, or by confirming — returns focus to
 *    whatever opened it. Without this the confirm button unmounts under the
 *    reader's cursor and focus falls back to `<body>`: a keyboard user is
 *    dropped at the top of the document every time they act.
 */

import { type RefObject, useEffect, useRef } from "react";

/**
 * Attach the returned ref to the panel element and give it `tabIndex={-1}`.
 *
 * `onClose` is read at keypress time, so it need not be stable.
 */
export function useConfirmFocus(
  isOpen: boolean,
  onClose: () => void,
): RefObject<HTMLDivElement | null> {
  const panelRef = useRef<HTMLDivElement>(null);
  const returnFocus = useRef<HTMLElement | null>(null);
  const wasOpen = useRef(false);
  const closeRef = useRef(onClose);

  useEffect(() => {
    closeRef.current = onClose;
  });

  useEffect(() => {
    if (isOpen && !wasOpen.current) {
      // Read before moving it: React has not touched focus, so this is still
      // the control the reader activated.
      returnFocus.current = document.activeElement as HTMLElement | null;
      panelRef.current?.focus();
    } else if (!isOpen && wasOpen.current) {
      const target = returnFocus.current;
      if (target?.isConnected === true) {
        target.focus();
      }
      returnFocus.current = null;
    }
    wasOpen.current = isOpen;
  }, [isOpen]);

  useEffect(() => {
    const node = panelRef.current;
    if (!isOpen || node === null) {
      return;
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        closeRef.current();
      }
    };
    node.addEventListener("keydown", onKeyDown);
    return () => {
      node.removeEventListener("keydown", onKeyDown);
    };
  }, [isOpen]);

  return panelRef;
}
