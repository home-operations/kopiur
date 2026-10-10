import { X } from "lucide-react";
import {
  type CSSProperties,
  type ReactNode,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";

import type { ObjectKind } from "../api/types";
import { ActionButton } from "./ActionButton";
import {
  DRAWER_STEP,
  clampDrawerWidth,
  drawerMaxWidth,
  readDrawerWidth,
  saveDrawerWidth,
} from "./drawerWidth";
import { KindChip, KindName } from "./KindMark";
import { KIND_META } from "./kind";
import { toastHost } from "./toast/toastHost";

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
 *
 * A resize handle hangs just outside its left edge: drag it, or focus it and
 * use the arrow keys (Home and End for narrowest and widest). The width is
 * remembered for the next panel, whichever resource it shows
 * (`drawerWidth.ts`).
 *
 * Closing is the caller's to start and the panel's to finish: the caller
 * passes `leaving` (see `useExiting`), the panel slides out, and `onExited`
 * says it has gone so the caller can unmount it. With no exit animation to
 * wait for — reduced motion, or a test — it goes at once.
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
  /** It is on its way out: slide away, ignore further close requests. */
  leaving?: boolean | undefined;
  /** Called once the exit has finished. */
  onExited?: (() => void) | undefined;
  /**
   * `padded` (the default) pads the body and stacks its children; `fill`
   * hands the whole body to the content to divide — a head, then tabs whose
   * panel is the scroller.
   */
  layout?: "padded" | "fill" | undefined;
  footer?: ReactNode;
  children: ReactNode;
}

/** The longest an exit may take before it is treated as done anyway. */
const EXIT_TIMEOUT_MS = 400;

const noop = () => undefined;

export function SidePanel({
  label,
  kind,
  kindWord,
  status,
  onClose,
  leaving = false,
  onExited,
  layout = "padded",
  footer,
  children,
}: SidePanelProps) {
  const dialog = useRef<HTMLDialogElement | null>(null);
  const titleId = useId();
  const close = useRef(onClose);
  const grip = useRef<HTMLDivElement | null>(null);
  const [width, setWidth] = useState(() => readDrawerWidth(window.innerWidth));
  const widthRef = useRef(width);

  const exited = useRef(onExited);

  useEffect(() => {
    close.current = leaving ? noop : onClose;
    exited.current = onExited;
  });

  // Slide out, then say so. The exit animation is read off the dialog itself:
  // none (reduced motion collapses it; a test has no stylesheet) means done.
  useLayoutEffect(() => {
    const node = dialog.current;
    if (!leaving || node === null) return;
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      exited.current?.();
    };
    const name = window.getComputedStyle(node).animationName;
    if (name === "" || name === "none") {
      finish();
      return;
    }
    const onEnd = (event: AnimationEvent) => {
      if (event.target === node) finish();
    };
    node.addEventListener("animationend", onEnd);
    const timer = window.setTimeout(finish, EXIT_TIMEOUT_MS);
    return () => {
      node.removeEventListener("animationend", onEnd);
      window.clearTimeout(timer);
    };
  }, [leaving]);

  useLayoutEffect(() => {
    const node = dialog.current;
    if (node === null) return;
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    let unmounting = false;

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
      if (!unmounting && !node.open) close.current();
    };

    document.addEventListener("keydown", onKey);
    node.addEventListener("click", onClick);
    node.addEventListener("cancel", onCancel);
    node.addEventListener("close", onNativeClose);
    if (!node.open) node.showModal();
    // Toasts render inside the open dialog; outside it they would be inert.
    toastHost.set(node);
    node.focus({ preventScroll: true });
    return () => {
      unmounting = true;
      document.removeEventListener("keydown", onKey);
      node.removeEventListener("click", onClick);
      node.removeEventListener("cancel", onCancel);
      node.removeEventListener("close", onNativeClose);
      toastHost.release(node);
      if (node.open) node.close();
      if (opener?.isConnected) opener.focus({ preventScroll: true });
    };
  }, []);

  // The resize handle. Listeners go on the element directly: a drag moves the
  // pointer off the 8px handle at once, so it captures the pointer, and the
  // width is saved once, when the drag ends.
  useEffect(() => {
    const node = grip.current;
    if (node === null) return;
    // A focusable separator is a widget (it takes the arrow keys), which the
    // a11y lint's role table does not yet know; the tab stop is set here.
    node.tabIndex = 0;
    const resize = (next: number) => {
      const clamped = clampDrawerWidth(next, window.innerWidth);
      widthRef.current = clamped;
      setWidth(clamped);
      return clamped;
    };
    let drag: { x: number; width: number } | null = null;
    const onDown = (event: PointerEvent) => {
      if (event.button !== 0) return;
      event.preventDefault();
      drag = { x: event.clientX, width: widthRef.current };
      node.setPointerCapture(event.pointerId);
    };
    const onMove = (event: PointerEvent) => {
      if (drag === null) return;
      // The panel hangs off the right edge: moving left widens it.
      resize(drag.width + (drag.x - event.clientX));
    };
    const onUp = (event: PointerEvent) => {
      if (drag === null) return;
      drag = null;
      node.releasePointerCapture(event.pointerId);
      saveDrawerWidth(widthRef.current);
    };
    const onKey = (event: KeyboardEvent) => {
      const current = widthRef.current;
      const next =
        event.key === "ArrowLeft"
          ? current + DRAWER_STEP
          : event.key === "ArrowRight"
            ? current - DRAWER_STEP
            : event.key === "Home"
              ? 0
              : event.key === "End"
                ? Number.POSITIVE_INFINITY
                : null;
      if (next === null) return;
      event.preventDefault();
      saveDrawerWidth(resize(next));
    };
    node.addEventListener("pointerdown", onDown);
    node.addEventListener("pointermove", onMove);
    node.addEventListener("pointerup", onUp);
    node.addEventListener("pointercancel", onUp);
    node.addEventListener("keydown", onKey);
    return () => {
      node.removeEventListener("pointerdown", onDown);
      node.removeEventListener("pointermove", onMove);
      node.removeEventListener("pointerup", onUp);
      node.removeEventListener("pointercancel", onUp);
      node.removeEventListener("keydown", onKey);
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
      data-leaving={leaving ? "" : undefined}
      tabIndex={-1}
      style={{ "--drawer-width": `${String(width)}px` } as CSSProperties}
    >
      <div
        ref={grip}
        className="side-panel__grip"
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize details"
        aria-valuemin={clampDrawerWidth(0, window.innerWidth)}
        aria-valuemax={drawerMaxWidth(window.innerWidth)}
        aria-valuenow={width}
      />
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
          <ActionButton
            variant="quiet"
            aria-label="Close details"
            onClick={() => {
              close.current();
            }}
          >
            <X size={16} strokeWidth={2} aria-hidden="true" />
          </ActionButton>
        </header>
        <div className="side-panel__body" data-layout={layout === "fill" ? "fill" : undefined}>
          {children}
        </div>
        {footer !== undefined && footer !== null ? (
          <footer className="side-panel__foot">{footer}</footer>
        ) : null}
      </div>
    </dialog>
  );
}
