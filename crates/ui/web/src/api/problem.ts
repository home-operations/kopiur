/**
 * Problems — the one error shape the SPA ever renders.
 *
 * The server answers every failure as `application/problem+json` carrying
 * kopiur's what/why/fix triple (`Problem`, generated from
 * `kopiur_ui_model::problem::Problem`). This module classifies responses,
 * parses the problem's type URN, and builds the *synthetic* problems the client
 * throws when the answer did not come from the API at all: a proxy's HTML 502,
 * a dropped connection, a body that is not JSON. Whatever went wrong, a route
 * renders what / why / fix — never a bare status, never `[object Object]`.
 */

import type { Problem } from "./types";

/** The media type that marks a response body as a `Problem`. */
export const PROBLEM_MEDIA_TYPE = "application/problem+json";

/**
 * Prefix of every `Problem.type` the server emits: `urn:kopiur:problem:<kind>`
 * (`crates/ui/src/api/problem.rs::PROBLEM_TYPE_PREFIX`). Callers match kinds by
 * this prefix and never hard-code the full set (addenda item 24).
 */
export const KOPIUR_PROBLEM_PREFIX = "urn:kopiur:problem:";

/**
 * Prefix of the problems this client fabricates when no server problem
 * arrived. A different URN namespace on purpose: `problemKind` must never
 * mistake a synthetic problem for a server one.
 */
export const CLIENT_PROBLEM_PREFIX = "urn:kopiur-ui:problem:";

/**
 * Whether a response carries a `Problem` body.
 *
 * Classification is by `Content-Type`, not by the shape of the body: the
 * server sets the media type on every problem it writes, and duck-typing a
 * proxy's error page into a problem is exactly the failure this avoids.
 * Parameters (`; charset=utf-8`) and case are ignored, per RFC 9110.
 */
export function isProblemResponse(response: Response): boolean {
  const contentType = response.headers.get("content-type");
  if (contentType === null) {
    return false;
  }
  const mediaType = contentType.split(";", 1)[0]?.trim().toLowerCase();
  return mediaType === PROBLEM_MEDIA_TYPE;
}

/**
 * The `<kind>` of a server problem — `forbidden`, `not-found`,
 * `session-required`, … — or `null` when the type is not a kopiur URN, in
 * which case the problem is generic and only its status and text mean
 * anything.
 */
export function problemKind(problem: Problem): string | null {
  if (!problem.type.startsWith(KOPIUR_PROBLEM_PREFIX)) {
    return null;
  }
  const kind = problem.type.slice(KOPIUR_PROBLEM_PREFIX.length);
  return kind.length > 0 ? kind : null;
}

/**
 * A problem the not-permitted state is derived from: the request was
 * understood and refused for who the caller is. Keyed on the status — a
 * `forbidden`, a `forbidden-principal` and a proxy's own 403 all mean the
 * same thing to the person looking at the screen.
 */
export function isNotPermitted(problem: Problem): boolean {
  return problem.status === 403;
}

/**
 * The browse endpoints' "start a session first" answer. `GET …/session`
 * sends it as a 404 while `…/tree` and `…/file` send it as a 409, all with
 * this one type — so this switches on the type and never on the status
 * (addenda item 14).
 */
export function isSessionRequired(problem: Problem): boolean {
  return problemKind(problem) === "session-required";
}

/** What produced a synthetic problem. */
export interface RequestDescription {
  method: string;
  path: string;
}

/**
 * Why a given status class arrived without a problem body, and what to do.
 * Anything the API itself produces carries a real problem, so a bare status
 * is always something in front of it — or the API mid-restart.
 */
function explainStatus(status: number): { why: string; fix: string } {
  if (status === 401) {
    return {
      why: "The authenticating proxy in front of kopiur-ui did not accept the request; kopiur-ui itself never asks for credentials.",
      fix: "sign in again through the proxy, then reload the page",
    };
  }
  if (status === 403) {
    return {
      why: "Something in front of kopiur-ui refused the request before it reached the API.",
      fix: "check the proxy or ingress policy that fronts kopiur-ui",
    };
  }
  if (status === 404) {
    return {
      why: "No API route answered, so the request may have reached a different service or a kopiur-ui older than this page.",
      fix: "reload the page; if it persists, check the ingress path routing and that kopiur-ui and the UI come from the same release",
    };
  }
  if (status === 429) {
    return {
      why: "A rate limit in front of the API, or the API's own concurrency limit, held the request.",
      fix: "wait a moment and retry",
    };
  }
  if (status === 502 || status === 503 || status === 504) {
    return {
      why: "A proxy or gateway in front of kopiur-ui answered instead of the API — kopiur-ui is not running, not ready, or too slow for the proxy's timeout.",
      fix: "check the kopiur-ui pod's readiness and logs, then reload",
    };
  }
  if (status >= 500) {
    return {
      why: "Something between the browser and the API failed without saying why.",
      fix: "reload the page; if it persists, check the kopiur-ui pod logs",
    };
  }
  return {
    why: "The response did not come from the kopiur-ui API, which always explains itself.",
    fix: "reload the page; if it persists, check what sits between the browser and kopiur-ui",
  };
}

/**
 * A problem for a response that carried no usable problem body — a gateway's
 * HTML page, an empty 504, a problem+json that was not JSON.
 */
export function problemForHttpStatus(
  input: RequestDescription & {
    status: number;
    statusText: string;
    detail?: string;
  },
): Problem {
  const { status, statusText, method, path } = input;
  const { why, fix } = explainStatus(status);
  const what = `${method} ${path} answered ${status}${statusText ? ` ${statusText}` : ""} without a problem body.`;
  const cause = input.detail ? `${why} ${input.detail}` : why;
  return {
    type: `${CLIENT_PROBLEM_PREFIX}http-status`,
    title: statusText || `HTTP ${status}`,
    status,
    detail: `${what} ${cause}`,
    what,
    why: cause,
    fix,
    instance: path,
  };
}

/** A problem for a 2xx whose body could not be parsed as JSON. */
export function problemForBadJson(
  input: RequestDescription & { status: number; cause: unknown },
): Problem {
  const { status, method, path } = input;
  const what = `${method} ${path} answered ${status} with a body that is not JSON.`;
  const why = `The response could not be parsed (${describeCause(input.cause)}); a proxy or a sign-in page may have answered in the API's place.`;
  return {
    type: `${CLIENT_PROBLEM_PREFIX}bad-json`,
    title: "Unparseable response",
    status,
    detail: `${what} ${why}`,
    what,
    why,
    fix: "reload the page; if it persists, check what sits between the browser and kopiur-ui",
    instance: path,
  };
}

/** A problem for a request that never got a response. */
export function problemForNetworkFailure(input: RequestDescription & { cause: unknown }): Problem {
  const { method, path } = input;
  const what = `${method} ${path} got no response.`;
  const why = `The browser could not reach kopiur-ui (${describeCause(input.cause)}).`;
  return {
    type: `${CLIENT_PROBLEM_PREFIX}network`,
    title: "No response",
    status: 0,
    detail: `${what} ${why}`,
    what,
    why,
    fix: "check the network connection and that kopiur-ui is reachable, then retry",
    instance: path,
  };
}

function describeCause(cause: unknown): string {
  if (cause instanceof Error) {
    return cause.message;
  }
  if (typeof cause === "string") {
    return cause;
  }
  return "unknown cause";
}

/**
 * The global problem banner's store: the one problem the shell shows above
 * every route. Mutations report here (a failed action must be seen wherever
 * the user is); queries render their own `ErrorState` inline instead.
 *
 * Framework-free so it can be driven from TanStack's `MutationCache` and
 * tested without React; `ProblemBanner.tsx` subscribes through
 * `useSyncExternalStore`.
 */
export interface ReportedProblem {
  problem: Problem;
  /** What the user was doing, e.g. "Suspend policy nightly". */
  source?: string;
}

function createProblemBanner() {
  let current: ReportedProblem | null = null;
  const listeners = new Set<() => void>();
  const notify = () => {
    for (const listener of listeners) {
      listener();
    }
  };
  // Arrow properties, not methods: `useSyncExternalStore(store.subscribe,
  // store.get)` passes them detached, and they must not depend on `this`.
  return {
    get: (): ReportedProblem | null => current,
    report: (problem: Problem, options: { source?: string } = {}): void => {
      current = options.source === undefined ? { problem } : { problem, source: options.source };
      notify();
    },
    dismiss: (): void => {
      if (current === null) {
        return;
      }
      current = null;
      notify();
    },
    subscribe: (listener: () => void): (() => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

export const problemBanner = createProblemBanner();
