import { KeyRound, PlayCircle, Square } from "lucide-react";

import { useEndSession, useStartSession } from "../../api/hooks";
import type { SessionInfo } from "../../api/types";
import { ActionButton } from "../ActionButton";
import { ActionResult } from "../ActionResult";
import { Facts } from "../Facts";
import { useCapabilityReason } from "../useCapabilityReason";
import { sessionExpiry } from "./browse";

/**
 * Starting and stopping the browse session — and saying, in the place where
 * the decision is made, what starting one actually does.
 *
 * **The pod mounts the repository's credentials.** That sentence is on this
 * panel, above the button, not in documentation: a browse session is a mover
 * `Job` whose pod holds the kopia repository open, and the kubelet mounts the
 * repository's credential Secrets into it for as long as it runs. It is the
 * most consequential thing a reader can do from this console, and the console
 * has no business making it look like opening a folder.
 *
 * The companion fact, equally load-bearing and equally on the panel: it runs
 * **as the reader**. Every apiserver call the browse endpoints make — and
 * above all `pods/exec`, which in a namespace is effectively read access to
 * that namespace's repository credentials — goes through the impersonating
 * client for the signed-in identity. kopiur-ui's own ServiceAccount cannot
 * exec into anything, which is why starting takes two grants
 * (`createSessionJobs` *and* `execSessions`) and why a reader without them
 * sees the control disabled and explained rather than missing.
 *
 * Stopping is one click and idempotent: `DELETE …/session` answers `204` with
 * an empty body (addenda item 18), so there is no receipt — the panel simply
 * reports the prompt again on the next read.
 */
export interface SessionBarProps {
  namespace: string;
  name: string;
  /** The running session, or `null` when none is. */
  session: SessionInfo | null;
  /**
   * Set when a *read* just discovered the session had gone — the `409`
   * `session-required` from `…/tree`. The prompt then says the session ended
   * rather than pretending it was never started.
   */
  lapsed?: boolean | undefined;
  /** Injected by tests; the expiry is relative to it. */
  now?: Date | undefined;
}

export function SessionBar({ namespace, name, session, lapsed = false, now }: SessionBarProps) {
  const start = useStartSession();
  const stop = useEndSession();
  // Two grants, and the first refusal is the reason: a reader who may create
  // the Job but may not exec into it would start a pod that could answer
  // nothing.
  const startReason = useCapabilityReason(namespace, ["createSessionJobs", "execSessions"]);
  const stopReason = useCapabilityReason(namespace, "deleteSessionJobs");

  if (session === null) {
    return (
      <section className="browse-session" aria-label="Browse session">
        <h2 className="browse-session__title">
          {lapsed ? "The browse session has ended" : "No browse session is running"}
        </h2>
        <CredentialWarning namespace={namespace} />
        <p className="browse-session__prose">
          {lapsed
            ? "Sessions are reaped when their deadline passes, so a browse view left open outlives the pod behind it. Starting one again re-opens the repository; nothing about the snapshot changed while it was gone."
            : "Reading a snapshot's files needs that pod. kopiur-ui never starts one from a page load — a crawler, a link preview or a preloading browser would each spin up a mover otherwise — so it takes this deliberate click."}
        </p>
        <div className="action-bar">
          <ActionButton
            variant="primary"
            disabledReason={start.isPending ? "The request is in flight." : startReason}
            onClick={() => {
              start.mutate({ namespace, name, body: {} });
            }}
          >
            <PlayCircle size={14} strokeWidth={2} aria-hidden="true" />
            {start.isPending ? "Starting the session…" : "Start a browse session"}
          </ActionButton>
        </div>
        {start.error !== null ? (
          <ActionResult label="Start browse session" problem={start.error.problem} />
        ) : null}
      </section>
    );
  }

  const expiry = sessionExpiry(session, now);
  return (
    <section className="browse-session" data-running="true" aria-label="Browse session">
      <h2 className="browse-session__title">A browse session is running</h2>
      <p className="browse-session__prose">
        A pod in <span className="mono">{session.namespace}</span> is holding this repository open
        with its credentials mounted, and is being read as you. Stop it when you are done — it does
        not have to wait for its deadline.
      </p>
      <Facts
        label="Browse session"
        facts={[
          { term: "Namespace", value: <span className="mono">{session.namespace}</span> },
          { term: "Job", value: <span className="mono">{session.job}</span> },
          ...(session.pod !== null && session.pod !== undefined && session.pod.length > 0
            ? [{ term: "Pod", value: <span className="mono">{session.pod}</span> }]
            : []),
          {
            term: "Expires",
            value:
              expiry === null ? (
                // An absent deadline is not "never" and not "expired"; both
                // would be claims the server did not make.
                <span className="browse-session__no-expiry">
                  no deadline published for this session
                </span>
              ) : (
                <time
                  dateTime={expiry.at}
                  data-expired={expiry.expired ? "true" : undefined}
                  className={expiry.expired ? "browse-session__lapsed" : undefined}
                >
                  {expiry.expired ? `${expiry.label} — it may already be gone` : expiry.label}
                </time>
              ),
          },
        ]}
      />
      <div className="action-bar">
        <ActionButton
          variant="danger"
          disabledReason={stop.isPending ? "The request is in flight." : stopReason}
          onClick={() => {
            stop.mutate({ namespace, name });
          }}
        >
          <Square size={14} strokeWidth={2} aria-hidden="true" />
          {stop.isPending ? "Stopping the session…" : "Stop the session"}
        </ActionButton>
      </div>
      {stop.error !== null ? (
        <ActionResult label="Stop browse session" problem={stop.error.problem} />
      ) : null}
    </section>
  );
}

/**
 * The sentence this screen exists to say out loud, on its own plate so it is
 * read before the button beneath it is pressed.
 */
function CredentialWarning({ namespace }: { namespace: string }) {
  return (
    <p className="browse-session__warning">
      <KeyRound
        className="browse-session__warning-icon"
        size={16}
        strokeWidth={2}
        aria-hidden="true"
      />
      <span>
        Starting a session runs a pod in <span className="mono">{namespace}</span> that{" "}
        <strong>mounts this repository&rsquo;s credentials</strong> and holds the repository open
        until you stop it or its deadline passes. It runs as you: the cluster checks your own RBAC
        for <span className="mono">pods/exec</span>, not the console&rsquo;s.
      </span>
    </p>
  );
}
