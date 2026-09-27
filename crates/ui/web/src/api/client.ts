/**
 * The typed HTTP client — the only place in the SPA that calls `fetch`
 * (eslint's `no-restricted-globals` enforces it everywhere else).
 *
 * What it owns, so no route has to:
 *
 * - **Problems.** A non-2xx with `application/problem+json` becomes an
 *   `ApiProblemError` carrying the server's `Problem`; anything else — a
 *   proxy's HTML 502, an empty 504, a dropped connection, a 2xx that is not
 *   JSON — becomes an `ApiProblemError` carrying a *synthetic* problem with
 *   what / why / fix. Callers never see a bare status or a raw `Response`.
 * - **The CSRF marker.** Every mutation carries `X-Kopiur-Request: 1` and, when
 *   it has a body, `Content-Type: application/json`; the server refuses a
 *   mutation without them (`crates/ui/src/auth/csrf.rs`). Reads never carry
 *   the marker.
 * - **Empty bodies.** A `204`, or any 2xx with an empty body, resolves to
 *   `undefined` — both session deletes answer that way (addenda item 18), and a
 *   blanket `JSON.parse` would throw on the first "stop session" click.
 * - **Cancellation.** An `AbortSignal` is passed straight to `fetch` and an
 *   abort is rethrown untouched, so TanStack Query can cancel a query without
 *   it surfacing as a problem.
 *
 * Mutation status codes differ per endpoint (201 / 200 / 202 / 204); the
 * client treats every 2xx alike and lets the body decide.
 */

import {
  isProblemResponse,
  problemForBadJson,
  problemForHttpStatus,
  problemForNetworkFailure,
} from "./problem";
import type { Problem } from "./types";

/** Header the server requires on every mutating request. */
export const CSRF_HEADER = "X-Kopiur-Request";

/** The only value the server accepts for {@link CSRF_HEADER}. */
export const CSRF_HEADER_VALUE = "1";

/** What the client will accept back: JSON, or a problem explaining why not. */
const ACCEPT = "application/json, application/problem+json";

/**
 * A failed request, always carrying a `Problem`. `message` is the what and
 * the fix, so an unhandled rejection in the console still reads as a sentence.
 */
export class ApiProblemError extends Error {
  readonly problem: Problem;

  constructor(problem: Problem) {
    super(`${problem.what} ${problem.fix}`);
    this.name = "ApiProblemError";
    this.problem = problem;
  }
}

/** Narrow a caught value to the client's error type. */
export function isApiProblemError(value: unknown): value is ApiProblemError {
  return value instanceof ApiProblemError;
}

/** Per-request options every client function accepts. */
export interface ApiInit {
  /** TanStack Query's cancellation signal, passed straight to `fetch`. */
  signal?: AbortSignal;
}

/** Query-string values; `null` and `undefined` mean "leave the key out". */
export type QueryParams = Record<string, string | number | boolean | null | undefined>;

/**
 * Append a query string, omitting keys with no value.
 *
 * Every backend query struct is `deny_unknown_fields` and an empty
 * `namespace=` is not the same as no namespace, so absent keys stay absent.
 */
export function withQuery(path: string, params?: QueryParams): string {
  if (params === undefined) {
    return path;
  }
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null) {
      continue;
    }
    search.set(key, String(value));
  }
  const encoded = search.toString();
  return encoded.length === 0 ? path : `${path}?${encoded}`;
}

/** `GET` a JSON resource. */
export function apiFetch<T>(path: string, init: ApiInit = {}): Promise<T> {
  return request<T>("GET", path, init);
}

/** `POST` a JSON body with the CSRF marker. */
export function apiPost<T>(path: string, body: unknown, init: ApiInit = {}): Promise<T> {
  return request<T>("POST", path, { ...init, body });
}

/** `DELETE` with the CSRF marker. Resolves to `undefined` on an empty body. */
export function apiDelete<T = undefined>(path: string, init: ApiInit = {}): Promise<T> {
  return request<T>("DELETE", path, init);
}

type Method = "GET" | "POST" | "DELETE";

interface RequestOptions extends ApiInit {
  body?: unknown;
}

function isAbort(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

async function request<T>(method: Method, path: string, options: RequestOptions): Promise<T> {
  const headers: Record<string, string> = { Accept: ACCEPT };
  const init: RequestInit = { method, headers, credentials: "same-origin" };
  if (method !== "GET") {
    headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
  }
  if (options.body !== undefined) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(options.body);
  }
  if (options.signal !== undefined) {
    init.signal = options.signal;
  }

  let response: Response;
  try {
    response = await fetch(path, init);
  } catch (cause: unknown) {
    if (isAbort(cause)) {
      throw cause;
    }
    throw new ApiProblemError(problemForNetworkFailure({ method, path, cause }));
  }

  // Read the body as text first: a 204 has none, a 202 may have none, and a
  // problem body that is not JSON must still produce a useful problem.
  let text: string;
  try {
    text = await response.text();
  } catch (cause: unknown) {
    if (isAbort(cause)) {
      throw cause;
    }
    throw new ApiProblemError(problemForNetworkFailure({ method, path, cause }));
  }

  if (!response.ok) {
    throw new ApiProblemError(problemFor(response, text, method, path));
  }

  if (response.status === 204 || text.length === 0) {
    return undefined as T;
  }

  try {
    return JSON.parse(text) as T;
  } catch (cause: unknown) {
    throw new ApiProblemError(problemForBadJson({ method, path, status: response.status, cause }));
  }
}

/**
 * The problem a failed response carries — the server's own when the media
 * type says so and the body parses, otherwise a synthetic one that still
 * names the request and the status.
 */
function problemFor(response: Response, text: string, method: Method, path: string): Problem {
  const base = { method, path, status: response.status, statusText: response.statusText };
  if (!isProblemResponse(response)) {
    return problemForHttpStatus(base);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return problemForHttpStatus({
      ...base,
      detail: "The body claimed to be a problem but was malformed (not valid JSON).",
    });
  }
  if (!looksLikeProblem(parsed)) {
    return problemForHttpStatus({
      ...base,
      detail: "The body claimed to be a problem but was malformed (missing what/why/fix).",
    });
  }
  // The server's status wins over the body's if they ever disagree: the
  // status is what the browser saw.
  return { ...parsed, status: response.status };
}

/**
 * The minimum a parsed problem body must carry to be rendered as one. This is
 * a guard against a malformed body *after* the media type classified the
 * response — the shape never decides whether something is a problem.
 */
function looksLikeProblem(value: unknown): value is Problem {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const record = value as Record<string, unknown>;
  return (
    typeof record.type === "string" &&
    typeof record.title === "string" &&
    typeof record.what === "string" &&
    typeof record.why === "string" &&
    typeof record.fix === "string"
  );
}
