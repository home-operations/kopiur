/**
 * Shared test harness for routes and router-dependent components.
 *
 * Two mounts, because the two kinds of test want different things:
 *
 * - `renderWithRouter(element)` gives a component that renders `<Link>`s a
 *   router to build hrefs against, and nothing else — no shell, no query
 *   client, no fetch.
 * - `mountApp(path)` mounts the real route tree under a fresh query client
 *   at `path`, exactly as `main.tsx` does, so a route test exercises its
 *   file route, the shell around it and the hooks it calls.
 *
 * `mockApi` answers `fetch` per `/api/v1` path so one test can give `/me`,
 * `/status` and `/doctor` different bodies (or different failures). It
 * matches on the pathname alone; query parameters are the caller's to
 * assert through `calledPaths()`.
 */

import { QueryClientProvider } from "@tanstack/react-query";
import {
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRouter,
} from "@tanstack/react-router";
import { render } from "@testing-library/react";
import type { ReactElement } from "react";
import { vi } from "vitest";
import createFetchMock from "vitest-fetch-mock";

import { createQueryClient } from "./api/queryClient";
import type { Me, Problem } from "./api/types";
import { routeTree } from "./routeTree.gen";

export const fetchMock = createFetchMock(vi);
fetchMock.enableMocks();

/** A minimal router whose only route renders `element`. */
export function renderWithRouter(element: ReactElement, path = "/") {
  const root = createRootRoute({ component: () => element });
  const router = createRouter({
    routeTree: root,
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  return render(<RouterProvider router={router} />);
}

/** The real app at `path`, with a fresh query client. */
export function mountApp(path: string) {
  const router = createRouter({
    routeTree,
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  const client = createQueryClient();
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { router, client };
}

/** A JSON body with the content type the client requires. */
export function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** An RFC 9457 problem body, as the backend sends one. */
export function problemResponse(problem: Problem): Response {
  return new Response(JSON.stringify(problem), {
    status: problem.status,
    headers: { "content-type": "application/problem+json" },
  });
}

/** The backend's 403 for a read the impersonated identity may not perform. */
export function forbiddenProblem(what: string, instance: string): Problem {
  return {
    type: "urn:kopiur:problem:forbidden",
    title: "Forbidden",
    status: 403,
    detail: what,
    what,
    why: "The apiserver refused the request for the identity kopiur-ui impersonated.",
    fix: "ask a cluster admin to bind kopiur-ui-user, kopiur-ui-editor or kopiur-ui-viewer to your user or group",
    instance,
    kubeReason: "Forbidden",
  };
}

/** A signed-in identity that may do everything, for tests that are not about capabilities. */
export const ME: Me = {
  user: "alice",
  groups: ["platform"],
  email: null,
  source: "trustedHeaders",
  namespace: null,
  can: {
    createSnapshots: true,
    deleteSnapshots: true,
    createRestores: true,
    patchPolicies: true,
    patchSchedules: true,
    patchRepositories: true,
    patchClusterRepositories: true,
    patchMaintenances: true,
    patchRepositoryReplications: true,
    patchSnapshotReplications: true,
    createSessionJobs: true,
    deleteSessionJobs: true,
    execSessions: true,
  },
};

/** Responses keyed by `/api/v1/...` pathname; a function may inspect the URL. */
export type ApiRoutes = Record<string, Response | ((url: URL) => Response)>;

/**
 * Answer each request from `routes` by pathname. `/api/v1/me` defaults to
 * `ME`; an unrouted path answers 404 so a test cannot pass on a request it
 * never described.
 */
export function mockApi(routes: ApiRoutes): void {
  fetchMock.resetMocks();
  fetchMock.mockResponse((request) => {
    const url = new URL(request.url, "http://localhost");
    const handler = routes[url.pathname];
    if (handler !== undefined) {
      return Promise.resolve(typeof handler === "function" ? handler(url) : handler.clone());
    }
    if (url.pathname === "/api/v1/me") {
      return Promise.resolve(jsonResponse(ME));
    }
    return Promise.resolve(
      new Response(`no mock for ${url.pathname}`, {
        status: 404,
        headers: { "content-type": "text/plain" },
      }),
    );
  });
}

/**
 * The `i`th item, or a failed test: `noUncheckedIndexedAccess` makes
 * `rows[0]` possibly undefined, and a bare `!` is forbidden by lint.
 */
export function nth<T>(items: readonly T[], i: number): T {
  const item = items[i];
  if (item === undefined) {
    throw new Error(`expected at least ${i + 1} items, found ${items.length}`);
  }
  return item;
}

/** The body rows of a ledger, without the header row. */
export function bodyRows(table: HTMLElement): HTMLElement[] {
  const [, ...rows] = Array.from(table.querySelectorAll("tr"));
  return rows;
}

/** Every URL (path + query) `fetch` was called with, in order. */
export function calledPaths(): string[] {
  return fetchMock.mock.calls.map(([input]) => {
    if (typeof input === "string") {
      return input;
    }
    return input instanceof URL ? input.pathname + input.search : input.url;
  });
}

/**
 * A component under a fresh query client and a bare router.
 *
 * The third mount, for the thing neither of the other two fits: a control
 * that calls hooks (`/me`, a mutation) and renders `<Link>`s but is not a
 * route — every dialog in `components/actions/` is one. `mountApp` would drag
 * in a whole page's reads; `renderWithRouter` has no query client for the
 * hooks to use.
 */
export function renderWithClient(element: ReactElement, path = "/") {
  const root = createRootRoute({ component: () => element });
  const router = createRouter({
    routeTree: root,
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  const client = createQueryClient();
  const result = render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { ...result, router, client };
}

/**
 * The JSON body of the request sent to `path`, parsed.
 *
 * Asserting the *body* is the point for a mutating control: a dialog that
 * posts to the right URL with the wrong fields is the failure mode the
 * generated request types exist to catch, and `toEqual` against a typed
 * fixture is what catches it.
 */
export function sentBody(path: string): unknown {
  const call = fetchMock.mock.calls.find(([input]) => input === path);
  if (call === undefined) {
    throw new Error(`no request to ${path}; sent ${calledPaths().join(", ")}`);
  }
  const body = call[1]?.body;
  if (typeof body !== "string") {
    throw new Error(`request to ${path} carried no JSON body`);
  }
  return JSON.parse(body) as unknown;
}

/** `ME` with only the named capabilities granted — everything else refused. */
export function meWith(allowed: Partial<Me["can"]>): Response {
  return jsonResponse({
    ...ME,
    can: Object.fromEntries(
      Object.keys(ME.can).map((key) => [key, allowed[key as keyof Me["can"]] ?? false]),
    ),
  });
}
