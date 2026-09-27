import { beforeEach, describe, expect, it, vi } from "vitest";
import createFetchMock from "vitest-fetch-mock";

import {
  ApiProblemError,
  CSRF_HEADER,
  apiDelete,
  apiFetch,
  apiPost,
  isApiProblemError,
  withQuery,
} from "./client";
import { CLIENT_PROBLEM_PREFIX, KOPIUR_PROBLEM_PREFIX, isProblemResponse } from "./problem";
import type { ActionReceipt, Problem, ScanCatalogBody } from "./types";

const fetchMock = createFetchMock(vi);
fetchMock.enableMocks();

/**
 * A 403 the way `crates/ui/src/api/problem.rs` writes it for a refused
 * `GET /api/v1/repositories` (addenda item 23: `/gates` is a static registry
 * with no identity extractor and cannot 403, so it is the wrong fixture).
 */
const forbidden: Problem = {
  type: "urn:kopiur:problem:forbidden",
  title: "Forbidden",
  status: 403,
  detail:
    "Listing repositories in namespace prod was refused. The apiserver refused the request for the identity kopiur-ui impersonated.",
  what: "Listing repositories in namespace prod was refused.",
  why: "The apiserver refused the request for the identity kopiur-ui impersonated.",
  fix: "ask a cluster admin to bind kopiur-ui-user, kopiur-ui-editor or kopiur-ui-viewer to your user or group",
  instance: "/api/v1/repositories",
  kubeReason: "Forbidden",
};

const receipt: ActionReceipt = {
  kind: "scanCatalog",
  created: [],
  requestedAt: "2026-09-08T12:00:00Z",
  note: null,
};

/** The request init the client handed to fetch, as the test sees it. */
function sentInit(call = 0): RequestInit & { headers: Record<string, string> } {
  const init = fetchMock.mock.calls[call]?.[1];
  if (init === undefined) {
    throw new Error(`fetch call ${call} was not made`);
  }
  return init as RequestInit & { headers: Record<string, string> };
}

beforeEach(() => {
  fetchMock.resetMocks();
});

describe("apiFetch", () => {
  it("surfaces a problem body as what/why/fix", async () => {
    fetchMock.mockResponseOnce(JSON.stringify(forbidden), {
      status: 403,
      headers: { "content-type": "application/problem+json" },
    });
    await expect(apiFetch("/api/v1/repositories?namespace=prod")).rejects.toMatchObject({
      problem: { status: 403, fix: expect.stringContaining("kopiur-ui-viewer") as string },
    });
  });

  it("throws an ApiProblemError whose message is the what and the fix, never [object Object]", async () => {
    fetchMock.mockResponseOnce(JSON.stringify(forbidden), {
      status: 403,
      headers: { "content-type": "application/problem+json" },
    });
    const error = await apiFetch("/api/v1/repositories").catch((e: unknown) => e);
    expect(isApiProblemError(error)).toBe(true);
    expect(error).toBeInstanceOf(ApiProblemError);
    const message = (error as ApiProblemError).message;
    expect(message).toContain(forbidden.what);
    expect(message).toContain(forbidden.fix);
    expect(String(error)).not.toContain("[object Object]");
  });

  it("returns the parsed JSON body on success", async () => {
    fetchMock.mockResponseOnce(JSON.stringify({ items: [], total: 0, offset: 0, limit: 50 }), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
    await expect(apiFetch("/api/v1/snapshots")).resolves.toEqual({
      items: [],
      total: 0,
      offset: 0,
      limit: 50,
    });
  });

  it("never sends the CSRF marker on a read, and asks for JSON or a problem", async () => {
    fetchMock.mockResponseOnce("[]", { status: 200 });
    await apiFetch("/api/v1/gates");
    const init = sentInit();
    expect(init.method).toBe("GET");
    expect(init.headers[CSRF_HEADER]).toBeUndefined();
    expect(init.headers["Content-Type"]).toBeUndefined();
    expect(init.headers.Accept).toContain("application/problem+json");
  });

  it("throws a synthetic problem when the body is not problem+json", async () => {
    // A proxy in front of the UI can answer with HTML; the SPA must still
    // render what/why/fix, never a bare status or `[object Object]`.
    fetchMock.mockResponseOnce("<html>gateway</html>", {
      status: 502,
      headers: { "content-type": "text/html" },
    });
    await expect(apiFetch("/api/v1/status")).rejects.toMatchObject({
      problem: {
        status: 502,
        what: expect.any(String) as string,
        why: expect.any(String) as string,
        fix: expect.any(String) as string,
        type: expect.stringMatching(new RegExp(`^${CLIENT_PROBLEM_PREFIX}`)) as string,
      },
    });
  });

  it("names the method and path in a synthetic problem", async () => {
    fetchMock.mockResponseOnce("", { status: 504 });
    const error = (await apiFetch("/api/v1/status").catch((e: unknown) => e)) as ApiProblemError;
    expect(error.problem.what).toContain("GET /api/v1/status");
    expect(error.problem.what).toContain("504");
    expect(error.problem.instance).toBe("/api/v1/status");
  });

  it("falls back to a synthetic problem when a problem+json body is malformed", async () => {
    fetchMock.mockResponseOnce("{not json", {
      status: 500,
      headers: { "content-type": "application/problem+json" },
    });
    const error = (await apiFetch("/api/v1/status").catch((e: unknown) => e)) as ApiProblemError;
    expect(error.problem.status).toBe(500);
    expect(error.problem.type.startsWith(CLIENT_PROBLEM_PREFIX)).toBe(true);
    expect(error.problem.why).toMatch(/malformed|not valid JSON/i);
  });

  it("keeps a server problem's type verbatim so callers can match the kopiur prefix", async () => {
    fetchMock.mockResponseOnce(JSON.stringify(forbidden), {
      status: 403,
      headers: { "content-type": "application/problem+json; charset=utf-8" },
    });
    const error = (await apiFetch("/api/v1/repositories").catch(
      (e: unknown) => e,
    )) as ApiProblemError;
    expect(error.problem.type).toBe(`${KOPIUR_PROBLEM_PREFIX}forbidden`);
  });

  it("throws a synthetic problem when a 2xx body is not valid JSON", async () => {
    fetchMock.mockResponseOnce("<html>login</html>", {
      status: 200,
      headers: { "content-type": "text/html" },
    });
    const error = (await apiFetch("/api/v1/me").catch((e: unknown) => e)) as ApiProblemError;
    expect(error.problem.status).toBe(200);
    expect(error.problem.what).toContain("GET /api/v1/me");
    expect(error.problem.fix.length).toBeGreaterThan(0);
  });

  it("turns a network failure into a problem with status 0", async () => {
    fetchMock.mockRejectOnce(new TypeError("Failed to fetch"));
    const error = (await apiFetch("/api/v1/status").catch((e: unknown) => e)) as ApiProblemError;
    expect(isApiProblemError(error)).toBe(true);
    expect(error.problem.status).toBe(0);
    expect(error.problem.type).toBe(`${CLIENT_PROBLEM_PREFIX}network`);
    expect(error.problem.why).toContain("Failed to fetch");
  });

  it("passes the AbortSignal through and rethrows an abort untouched", async () => {
    const controller = new AbortController();
    const abort = new DOMException("The operation was aborted.", "AbortError");
    fetchMock.mockRejectOnce(abort);
    const pending = apiFetch("/api/v1/status", { signal: controller.signal });
    controller.abort();
    const error = await pending.catch((e: unknown) => e);
    expect(error).toBe(abort);
    expect(isApiProblemError(error)).toBe(false);
    expect(sentInit().signal).toBe(controller.signal);
  });
});

describe("apiPost", () => {
  it("sends the CSRF marker and JSON content type on a mutation", async () => {
    fetchMock.mockResponseOnce(JSON.stringify(receipt), {
      status: 202,
      headers: { "content-type": "application/json" },
    });
    const body: ScanCatalogBody = { kind: "Repository", namespace: "prod", name: "nas" };
    const result = await apiPost<ActionReceipt>("/api/v1/actions/scan-catalog", body);
    const init = sentInit();
    expect(init.method).toBe("POST");
    expect(init.headers[CSRF_HEADER]).toBe("1");
    expect(init.headers["Content-Type"]).toBe("application/json");
    expect(init.body).toBe(JSON.stringify(body));
    expect(result).toEqual(receipt);
  });

  it("returns the receipt on a 201 and on a 200 alike", async () => {
    // create-snapshot / create-restore answer 201, suspend answers 200,
    // the run actions 202 — the client does not care which (addenda item 18).
    for (const status of [200, 201, 202]) {
      fetchMock.mockResponseOnce(JSON.stringify(receipt), {
        status,
        headers: { "content-type": "application/json" },
      });
      await expect(apiPost("/api/v1/actions/suspend", { kind: "SnapshotPolicy" })).resolves.toEqual(
        receipt,
      );
    }
  });

  it("surfaces a mutation's problem the same way a read does", async () => {
    fetchMock.mockResponseOnce(JSON.stringify(forbidden), {
      status: 403,
      headers: { "content-type": "application/problem+json" },
    });
    await expect(apiPost("/api/v1/actions/snapshot-now", {})).rejects.toMatchObject({
      problem: { status: 403, kubeReason: "Forbidden" },
    });
  });
});

describe("apiDelete", () => {
  it("resolves to undefined on a 204 with an empty body instead of parsing it", async () => {
    // Both session deletes answer 204 with no body (addenda item 18); a
    // blanket JSON.parse here throws on the first "stop session" click.
    fetchMock.mockResponseOnce(new Response(null, { status: 204 }));
    await expect(apiDelete("/api/v1/snapshots/prod/nightly-1/session")).resolves.toBeUndefined();
  });

  it("resolves to undefined on any 2xx with an empty body", async () => {
    fetchMock.mockResponseOnce("", { status: 200 });
    await expect(apiDelete("/api/v1/repositories/repository/nas/session")).resolves.toBeUndefined();
  });

  it("sends the CSRF marker and no content type, because it sends no body", async () => {
    fetchMock.mockResponseOnce(new Response(null, { status: 204 }));
    await apiDelete("/api/v1/snapshots/prod/nightly-1/session");
    const init = sentInit();
    expect(init.method).toBe("DELETE");
    expect(init.headers[CSRF_HEADER]).toBe("1");
    expect(init.headers["Content-Type"]).toBeUndefined();
    expect(init.body).toBeUndefined();
  });

  it("returns the receipt a 202 snapshot delete carries, note included", async () => {
    // Delete is "requested", not "done" (addenda item 19): the receipt's
    // note may carry the mass-deletion-breaker hold and must survive.
    const held: ActionReceipt = {
      ...receipt,
      kind: "deleteSnapshot",
      note: "held by the repository's deletion breaker (threshold 10)",
    };
    fetchMock.mockResponseOnce(JSON.stringify(held), {
      status: 202,
      headers: { "content-type": "application/json" },
    });
    await expect(apiDelete<ActionReceipt>("/api/v1/snapshots/prod/nightly-1")).resolves.toEqual(
      held,
    );
  });
});

describe("withQuery", () => {
  it("appends only the parameters that have a value", () => {
    // Every backend query struct is `deny_unknown_fields`, and an empty
    // `namespace=` is not "no namespace" — absent keys must stay absent.
    expect(
      withQuery("/api/v1/snapshots", {
        namespace: "prod",
        policy: undefined,
        origin: null,
        offset: 0,
        limit: 50,
      }),
    ).toBe("/api/v1/snapshots?namespace=prod&offset=0&limit=50");
  });

  it("returns the bare path when nothing is set", () => {
    expect(withQuery("/api/v1/status", {})).toBe("/api/v1/status");
    expect(withQuery("/api/v1/status", { namespace: undefined })).toBe("/api/v1/status");
    expect(withQuery("/api/v1/status")).toBe("/api/v1/status");
  });

  it("encodes values", () => {
    expect(withQuery("/api/v1/snapshots/prod/n/tree", { path: "/etc/a b&c" })).toBe(
      "/api/v1/snapshots/prod/n/tree?path=%2Fetc%2Fa+b%26c",
    );
  });
});

describe("isProblemResponse", () => {
  it("classifies by media type, ignoring parameters and case", () => {
    const withType = (value: string | null) =>
      new Response("", { headers: value === null ? {} : { "content-type": value } });
    expect(isProblemResponse(withType("application/problem+json"))).toBe(true);
    expect(isProblemResponse(withType("Application/Problem+JSON; charset=utf-8"))).toBe(true);
    expect(isProblemResponse(withType("application/json"))).toBe(false);
    expect(isProblemResponse(withType("text/html"))).toBe(false);
    expect(isProblemResponse(withType(null))).toBe(false);
  });
});
