import { CircleAlert, CircleCheck, ShieldOff, X } from "lucide-react";
import { type CSSProperties, useEffect, useState, useSyncExternalStore } from "react";
import { createPortal } from "react-dom";

import { isNotPermitted } from "../../api/problem";
import type { ActionReceipt } from "../../api/types";
import { relativeTime } from "../../util/format";
import { kindOfLabel } from "../kind";
import { ObjectRef } from "../ObjectRef";
import { ProblemBody } from "../ProblemBanner";
import { EXIT_MS, RECEIPT_MS, RECEIPT_WITH_NOTE_MS } from "./durations";
import { type Toast, toasts } from "./toasts";
import { toastHost } from "./toastHost";
import { decayElapsed, forgetDecay, useDecay } from "./useDecay";

const NONE: readonly Toast[] = [];

/**
 * Toasts that have already slid in. The toaster remounts its toasts when it
 * moves into or out of an open drawer, and a toast already on screen must not
 * arrive a second time.
 */
const entered = new Set<string>();

/**
 * Every action's answer, in one place: a stack of toasts at the bottom right
 * of the screen, newest at the bottom, each sliding up as it arrives.
 *
 * The answers come from the mutation cache (`api/queryClient.ts`), not from
 * the button that asked, so a request whose drawer was closed mid-flight is
 * still answered. A toast never takes focus.
 *
 * An accepted action decays: a line along its foot runs down as its time
 * does, and the pointer resting on it — or focus inside it — stops both the
 * clock and the line. A refusal does not decay: its fix is the thing to read,
 * so it stays until it is closed.
 */
export function Toaster() {
  const list = useSyncExternalStore(toasts.subscribe, toasts.get, () => NONE);
  const host = useSyncExternalStore(toastHost.subscribe, toastHost.get, () => null);
  const region = (
    <section className="toaster" aria-label="Notifications">
      <ol className="toaster__list" aria-live="polite" aria-relevant="additions">
        {list.map((toast) => (
          <ToastItem key={toast.id} toast={toast} />
        ))}
      </ol>
    </section>
  );
  // Inside an open drawer's dialog, or the page is inert under it.
  return host !== null ? createPortal(region, host) : region;
}

function durationOf(toast: Toast): number | null {
  switch (toast.kind) {
    case "receipt": {
      const note = toast.receipt.note;
      return note !== null && note !== undefined && note.length > 0
        ? RECEIPT_WITH_NOTE_MS
        : RECEIPT_MS;
    }
    case "problem":
      return null;
    default:
      return toast satisfies never;
  }
}

function ToastItem({ toast }: { toast: Toast }) {
  const ms = durationOf(toast);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [hidden, setHidden] = useState(() => document.visibilityState === "hidden");
  const [leaving, setLeaving] = useState(false);
  const [arrived] = useState(() => entered.has(toast.id));
  // Read once, at mount: a toast remounted part-way (moved into or out of a
  // drawer) starts its line where its clock is. Read on every render it would
  // change on each hover, and a changed delay moves a running line forward by
  // the time already spent.
  const [startedAt] = useState(() => (ms === null ? 0 : decayElapsed(toast.id, ms)));
  useEffect(() => {
    entered.add(toast.id);
  }, [toast.id]);
  const paused = hovered || focused || hidden;

  // A tab in the background is not being read.
  useEffect(() => {
    const onVisibility = () => {
      setHidden(document.visibilityState === "hidden");
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);

  useEffect(() => {
    if (!leaving) return undefined;
    const timer = window.setTimeout(() => {
      entered.delete(toast.id);
      forgetDecay(toast.id);
      toasts.dismiss(toast.id);
    }, EXIT_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, [leaving, toast.id]);

  const leave = () => {
    setLeaving(true);
  };
  useDecay(toast.id, ms, paused, leave);

  const refused = toast.kind === "problem";
  const forbidden = toast.kind === "problem" && isNotPermitted(toast.problem);
  const Icon = refused ? (forbidden ? ShieldOff : CircleAlert) : CircleCheck;

  return (
    <li
      className="toast"
      role={refused ? "alert" : "status"}
      data-tone={refused ? "failed" : "healthy"}
      data-paused={paused ? "true" : undefined}
      data-leaving={leaving ? "true" : undefined}
      data-arrived={arrived ? "true" : undefined}
      style={
        ms !== null
          ? ({
              "--toast-ms": `${String(ms)}ms`,
              // A remounted toast's line picks up where its clock is.
              "--toast-elapsed": `${String(-startedAt)}ms`,
            } as CSSProperties)
          : undefined
      }
      onPointerEnter={() => {
        setHovered(true);
      }}
      onPointerLeave={() => {
        setHovered(false);
      }}
      onFocus={() => {
        setFocused(true);
      }}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setFocused(false);
      }}
    >
      <span className="toast__icon" aria-hidden="true">
        <Icon size={16} strokeWidth={2} />
      </span>
      <div className="toast__body">
        <ToastBody toast={toast} />
      </div>
      <button
        type="button"
        className="toast__close"
        aria-label="Dismiss notification"
        onClick={leave}
      >
        <X size={14} strokeWidth={2} aria-hidden="true" />
      </button>
      {ms !== null ? <span className="toast__timer" aria-hidden="true" /> : null}
    </li>
  );
}

function ToastBody({ toast }: { toast: Toast }) {
  switch (toast.kind) {
    case "receipt":
      return <ReceiptBody label={toast.label} receipt={toast.receipt} />;
    case "problem":
      return <ProblemBody problem={toast.problem} source={toast.label} />;
    default:
      return toast satisfies never;
  }
}

/**
 * An accepted action, as its receipt says it.
 *
 * **`note` is not decoration.** It is where an action that was accepted but
 * not performed explains itself — a suspend that changed nothing because the
 * object was already suspended, a delete the mass-deletion breaker is holding
 * (addenda item 19). Leaving it out turns "held" into "done".
 *
 * **Accepted is not done.** Three of the actions answer `202`: the request
 * stamped an annotation a reconciler will honor. The wording says
 * *requested*, and `requestedAt` says when, because that instant is also the
 * token the operator echoes back on the run it produced.
 */
function ReceiptBody({ label, receipt }: { label: string; receipt: ActionReceipt }) {
  const note = receipt.note;
  return (
    <>
      <p className="toast__what">{label} requested.</p>
      {note !== null && note !== undefined && note.length > 0 ? (
        <p className="toast__note">{note}</p>
      ) : null}
      {receipt.created.length > 0 ? (
        <ul className="toast__created" aria-label="Created">
          {receipt.created.map((ref) => (
            <li key={`${ref.namespace}/${ref.name}`}>
              <CreatedRef kind={receipt.kind} namespace={ref.namespace} name={ref.name} />
            </li>
          ))}
        </ul>
      ) : null}
      {receipt.requestedAt !== null && receipt.requestedAt !== undefined ? (
        <p className="toast__meta mono">
          <time dateTime={receipt.requestedAt}>{relativeTime(receipt.requestedAt)}</time>
        </p>
      ) : null}
    </>
  );
}

/**
 * One created object, as a reference to its page in its own kind. A kind
 * this bundle does not know is still named — a newer server's object must
 * not vanish from the receipt that says it was made.
 */
function CreatedRef({ kind, namespace, name }: { kind: string; namespace: string; name: string }) {
  const known = kindOfLabel(kind);
  if (known === null) {
    return (
      <span className="label-strip">
        <span className="label-strip__kind">{namespace}</span>
        <span className="label-strip__name">{name}</span>
      </span>
    );
  }
  return <ObjectRef kind={known} name={name} namespace={namespace} />;
}
