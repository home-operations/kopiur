import {
  type CSSProperties,
  type ReactNode,
  type Ref,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";

/** What the trigger must carry so the popover can open, close and be found. */
export interface PopoverTriggerProps {
  ref: Ref<HTMLButtonElement>;
  "aria-expanded": boolean;
  "aria-haspopup": "dialog";
  "aria-controls": string | undefined;
  onClick: () => void;
}

export interface PopoverProps {
  /** The panel's accessible name. */
  label: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The button that opens it, handed the props it needs. */
  trigger: (props: PopoverTriggerProps) => ReactNode;
  /** Which edge of the trigger the panel lines up with. */
  align?: "start" | "end" | undefined;
  /** Below the trigger, or above it (a trigger at the bottom of something). */
  side?: "bottom" | "top" | undefined;
  /**
   * `absolute` (the default) hangs off the trigger in the page's flow.
   * `fixed` pins it to where the trigger is on screen, following it as the
   * page scrolls — for a trigger inside a box that clips, such as a table's
   * scrolling card.
   */
  strategy?: "absolute" | "fixed" | undefined;
  /** Extra classes on the panel. */
  className?: string | undefined;
  /** `data-*` attributes on the panel (a state the content styles by). */
  panelData?: Readonly<Record<`data-${string}`, string | undefined>> | undefined;
  /** Where focus goes on opening (a selector inside the panel); the first control otherwise. */
  initialFocus?: string | undefined;
  /** Escape goes here first; return true to keep it (and stay open). */
  onEscape?: (() => boolean) | undefined;
  /** The content, or a function of `close` (closes and refocuses the trigger). */
  children: ReactNode | ((close: () => void) => ReactNode);
}

/** Space between the trigger and the panel, as in the stylesheet. */
const GAP = 6;

const FOCUSABLE =
  'input:not([disabled]), select:not([disabled]), textarea:not([disabled]), button:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])';

/**
 * A panel anchored to the button that opened it — options for an action, a
 * menu of a table's columns — floating over the page just below the button.
 *
 * Not modal: the page stays live, focus moves into the panel when it opens
 * (to its first control that can take it), and it goes away the way a menu
 * does — Escape closes it and puts focus back on the button, a click
 * anywhere else closes it where you clicked, a second click on the button
 * closes it. It hangs below its trigger or above it (`side`), and inside a
 * box that clips — a table's scrolling card — it pins itself to the trigger
 * on screen instead (`strategy="fixed"`). Content that needs Escape for itself (cancelling a drag) can
 * keep it with `onEscape`.
 */
export function Popover({
  label,
  open,
  onOpenChange,
  trigger,
  align = "start",
  side = "bottom",
  strategy = "absolute",
  className,
  panelData,
  initialFocus,
  onEscape,
  children,
}: PopoverProps) {
  const [button, setButton] = useState<HTMLButtonElement | null>(null);
  const panel = useRef<HTMLDivElement | null>(null);
  const root = useRef<HTMLDivElement | null>(null);
  const panelId = useId();
  const escape = useRef(onEscape);
  const change = useRef(onOpenChange);
  useEffect(() => {
    escape.current = onEscape;
    change.current = onOpenChange;
  });

  // Where a fixed panel sits: measured from the trigger, again on every
  // scroll (of anything) and resize while open.
  const [place, setPlace] = useState<CSSProperties | null>(null);
  useLayoutEffect(() => {
    if (!open || strategy !== "fixed" || button === null) return undefined;
    const measure = () => {
      const box = button.getBoundingClientRect();
      setPlace({
        position: "fixed",
        ...(side === "top"
          ? { bottom: `${String(window.innerHeight - box.top + GAP)}px` }
          : { top: `${String(box.bottom + GAP)}px` }),
        ...(align === "end"
          ? { right: `${String(window.innerWidth - box.right)}px` }
          : { left: `${String(box.left)}px` }),
      });
    };
    measure();
    window.addEventListener("scroll", measure, true);
    window.addEventListener("resize", measure);
    return () => {
      window.removeEventListener("scroll", measure, true);
      window.removeEventListener("resize", measure);
    };
  }, [open, strategy, button, side, align]);

  const close = () => {
    onOpenChange(false);
    button?.focus();
  };

  useEffect(() => {
    if (!open) return undefined;
    const first =
      (initialFocus !== undefined
        ? panel.current?.querySelector<HTMLElement>(initialFocus)
        : undefined) ?? panel.current?.querySelector<HTMLElement>(FOCUSABLE);
    first?.focus();
    const onPointer = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!panel.current?.contains(target) && button?.contains(target) !== true) {
        change.current(false);
      }
    };
    // Escape is heard on the popover itself, on its way out to the page, and
    // taken there: a popover inside a drawer answers it, and the drawer —
    // which ignores a key already handled — stays open.
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      if (escape.current?.() === true) return;
      change.current(false);
      button?.focus();
    };
    const node = root.current;
    document.addEventListener("mousedown", onPointer);
    node?.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointer);
      node?.removeEventListener("keydown", onKey);
    };
  }, [open, button, initialFocus]);

  return (
    <div ref={root} className="popover">
      {trigger({
        ref: setButton,
        "aria-expanded": open,
        "aria-haspopup": "dialog",
        "aria-controls": open ? panelId : undefined,
        onClick: () => {
          onOpenChange(!open);
        },
      })}
      {open ? (
        <div
          id={panelId}
          ref={panel}
          className={["popover__panel", className].filter(Boolean).join(" ")}
          role="dialog"
          aria-label={label}
          data-align={align}
          data-side={side}
          data-strategy={strategy}
          style={
            strategy === "fixed"
              ? (place ?? { position: "fixed", visibility: "hidden" })
              : undefined
          }
          {...panelData}
        >
          {typeof children === "function" ? children(close) : children}
        </div>
      ) : null}
    </div>
  );
}
