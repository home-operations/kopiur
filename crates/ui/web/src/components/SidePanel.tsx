import { X } from "lucide-react";
import { type ReactNode, useEffect, useId, useLayoutEffect, useRef } from "react";

import type { ObjectKind } from "../api/types";
import { ActionButton } from "./ActionButton";
import { KindChip, KindName } from "./KindMark";
import { KIND_META } from "./kind";

/**
 * The side panel: a resource's details, floating over the right edge of the
 * page on a scrim.
 *
 * A native modal `<dialog>`, so the browser supplies what is hardest to get
 * right by hand: the top layer (nothing in the shell can stack over it), the
 * backdrop, and an inert page behind it that focus cannot wander into.
 *
 * Escape is handled here rather than left to the browser's `cancel`, for one
 * reason: an inline confirmation inside the panel owns Escape first and stops
 * it, and a stopped Escape must not also close the panel. The browser's own
 * close request would ignore that, so the key is taken (`preventDefault`) and
 * routed through `onClose` — the caller owns whether the panel exists, usually
 * by clearing a URL parameter. Only one panel is ever open.
 *
 * Focus moves into the panel when it opens and goes back to whatever had it
 * before — the row or reference that opened it — when it goes.
 */
export interface SidePanelProps {
  /** The title, which also names the dialog. */
  label: ReactNode;
  /** The resource kind: chip, kind name and stripe. */
  kind?: ObjectKind | undefined;
  /** The kind word for something that is not a resource (a backend, a namespace). */
  kindWord?: string | undefined;
  /** The status pill, beside the title. */
  status?: ReactNode;
  onClose: () => void;
  footer?: ReactNode;
  children: ReactNode;
}

export function SidePanel({
  label,
  kind,
  kindWord,
  status,
  onClose,
  footer,
  children,
}: SidePanelProps) {
  const dialog = useRef<HTMLDialogElement | null>(null);
  const titleId = useId();
  const close = useRef(onClose);

  useEffect(() => {
    close.current = onClose;
  });

  useLayoutEffect(() => {
    const node = dialog.current;
    if (node === null) return;
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    let leaving = false;

    // Escape is heard on the document, after the event has bubbled out of the
    // panel: a confirmation inside that answered it stopped it on the way, and
    // never gets here. Taking the key also stops the browser's own close.
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      event.preventDefault();
      close.current();
    };
    // A click on the backdrop lands on the dialog itself; anything inside
    // lands on the frame or deeper.
    const onClick = (event: MouseEvent) => {
      if (event.target === node) close.current();
    };
    const onCancel = (event: Event) => {
      event.preventDefault();
    };
    // A browser may close the dialog despite the refusal (repeated close
    // requests without user activation). Follow it, so a closed dialog never
    // sits under a URL that says it is open. `close` arrives on a later task,
    // so one queued by an earlier unmount (StrictMode mounts twice) can land
    // after the dialog reopened — an open dialog was not closed, whatever the
    // event says.
    const onNativeClose = () => {
      if (!leaving && !node.open) close.current();
    };

    document.addEventListener("keydown", onKey);
    node.addEventListener("click", onClick);
    node.addEventListener("cancel", onCancel);
    node.addEventListener("close", onNativeClose);
    if (!node.open) node.showModal();
    node.focus({ preventScroll: true });
    return () => {
      leaving = true;
      document.removeEventListener("keydown", onKey);
      node.removeEventListener("click", onClick);
      node.removeEventListener("cancel", onCancel);
      node.removeEventListener("close", onNativeClose);
      if (node.open) node.close();
      if (opener?.isConnected) opener.focus({ preventScroll: true });
    };
  }, []);

  // Content swapped under focus (another resource, a section that went away)
  // must not drop focus onto the inert page behind.
  useLayoutEffect(() => {
    const node = dialog.current;
    if (node === null) return;
    const active = document.activeElement;
    if (active === null || active === document.body || !node.contains(active)) {
      node.focus({ preventScroll: true });
    }
  });

  return (
    <dialog
      ref={dialog}
      className={kind !== undefined ? "side-panel has-stripe" : "side-panel"}
      data-kind={kind !== undefined ? KIND_META[kind].slug : undefined}
      aria-labelledby={titleId}
      tabIndex={-1}
    >
      <div className="side-panel__frame">
        <header className="side-panel__head">
          {kind !== undefined ? <KindChip kind={kind} /> : null}
          <div className="side-panel__heading">
            {kind !== undefined ? (
              <KindName kind={kind} />
            ) : kindWord !== undefined ? (
              <span className="kind-name label-strip__kind">{kindWord}</span>
            ) : null}
            <h2 className="side-panel__title" id={titleId}>
              {label}
            </h2>
            {status !== undefined && status !== null ? (
              <div className="side-panel__status">{status}</div>
            ) : null}
          </div>
          <ActionButton variant="quiet" aria-label="Close details" onClick={onClose}>
            <X size={16} strokeWidth={2} aria-hidden="true" />
          </ActionButton>
        </header>
        <div className="side-panel__body">{children}</div>
        {footer !== undefined && footer !== null ? (
          <footer className="side-panel__foot">{footer}</footer>
        ) : null}
      </div>
    </dialog>
  );
}
