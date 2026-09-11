import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  CLIENT_PROBLEM_PREFIX,
  KOPIUR_PROBLEM_PREFIX,
  isNotPermitted,
  isSessionRequired,
  problemBanner,
  problemForHttpStatus,
  problemForNetworkFailure,
  problemKind,
} from "./problem";
import type { Problem } from "./types";

function problem(overrides: Partial<Problem>): Problem {
  return {
    type: "urn:kopiur:problem:not-found",
    title: "Not found",
    status: 404,
    detail: "Snapshot prod/nightly-1 was not found.",
    what: "Snapshot prod/nightly-1 was not found.",
    why: "It does not exist in the scope the request named, or it was deleted after the page was loaded.",
    fix: "reload the page; if the link came from elsewhere, check the namespace and name it points at",
    ...overrides,
  };
}

describe("problemKind", () => {
  it("returns the kind after the kopiur URN prefix", () => {
    expect(problemKind(problem({ type: `${KOPIUR_PROBLEM_PREFIX}forbidden` }))).toBe("forbidden");
    expect(problemKind(problem({ type: `${KOPIUR_PROBLEM_PREFIX}session-required` }))).toBe(
      "session-required",
    );
  });

  it("treats a type outside the prefix as generic, whatever it says", () => {
    // Addenda item 24: match by prefix, never hard-code the set, and an
    // unrecognised type is generic — including the client's own synthetic ones.
    expect(problemKind(problem({ type: "https://example.test/problems/forbidden" }))).toBeNull();
    expect(problemKind(problem({ type: `${CLIENT_PROBLEM_PREFIX}network` }))).toBeNull();
    expect(problemKind(problem({ type: "about:blank" }))).toBeNull();
    expect(problemKind(problem({ type: "" }))).toBeNull();
  });

  it("does not need to know a kind to return it", () => {
    expect(problemKind(problem({ type: `${KOPIUR_PROBLEM_PREFIX}some-future-kind` }))).toBe(
      "some-future-kind",
    );
  });
});

describe("isNotPermitted", () => {
  it("is derived from the 403 status, not from the type", () => {
    expect(
      isNotPermitted(problem({ status: 403, type: `${KOPIUR_PROBLEM_PREFIX}forbidden` })),
    ).toBe(true);
    expect(
      isNotPermitted(problem({ status: 403, type: `${KOPIUR_PROBLEM_PREFIX}forbidden-principal` })),
    ).toBe(true);
    expect(
      isNotPermitted(problem({ status: 403, type: `${CLIENT_PROBLEM_PREFIX}http-status` })),
    ).toBe(true);
    expect(isNotPermitted(problem({ status: 404 }))).toBe(false);
    expect(isNotPermitted(problem({ status: 401 }))).toBe(false);
  });
});

describe("isSessionRequired", () => {
  it("switches on the type, never on the status", () => {
    // `GET …/session` answers 404 while `/tree` and `/file` answer 409, all
    // with the same type (addenda item 14).
    const kind = `${KOPIUR_PROBLEM_PREFIX}session-required`;
    expect(isSessionRequired(problem({ type: kind, status: 404 }))).toBe(true);
    expect(isSessionRequired(problem({ type: kind, status: 409 }))).toBe(true);
    expect(
      isSessionRequired(problem({ type: `${KOPIUR_PROBLEM_PREFIX}not-found`, status: 404 })),
    ).toBe(false);
  });
});

describe("synthetic problems", () => {
  it("describe an HTTP status with what/why/fix and the request that produced it", () => {
    const p = problemForHttpStatus({
      status: 502,
      statusText: "Bad Gateway",
      method: "GET",
      path: "/api/v1/status",
    });
    expect(p.status).toBe(502);
    expect(p.type).toBe(`${CLIENT_PROBLEM_PREFIX}http-status`);
    expect(p.what).toContain("GET /api/v1/status");
    expect(p.what).toContain("502");
    expect(p.why.length).toBeGreaterThan(0);
    expect(p.fix.length).toBeGreaterThan(0);
    expect(p.instance).toBe("/api/v1/status");
    expect(p.title).toBe("Bad Gateway");
  });

  it("explain a 401 as the proxy's business, not the operator's RBAC", () => {
    const p = problemForHttpStatus({
      status: 401,
      statusText: "Unauthorized",
      method: "GET",
      path: "/api/v1/me",
    });
    expect(p.why).toMatch(/proxy|sign/i);
  });

  it("describe a network failure with status 0 and the cause", () => {
    const p = problemForNetworkFailure({
      method: "POST",
      path: "/api/v1/actions/suspend",
      cause: new TypeError("Failed to fetch"),
    });
    expect(p.status).toBe(0);
    expect(p.type).toBe(`${CLIENT_PROBLEM_PREFIX}network`);
    expect(p.what).toContain("POST /api/v1/actions/suspend");
    expect(p.why).toContain("Failed to fetch");
  });
});

describe("problemBanner", () => {
  beforeEach(() => {
    problemBanner.dismiss();
  });

  it("starts empty and notifies subscribers on report and dismiss", () => {
    expect(problemBanner.get()).toBeNull();
    const seen = vi.fn();
    const unsubscribe = problemBanner.subscribe(seen);
    const p = problem({ status: 403 });
    problemBanner.report(p, { source: "Suspend policy nightly" });
    expect(problemBanner.get()).toEqual({ problem: p, source: "Suspend policy nightly" });
    problemBanner.dismiss();
    expect(problemBanner.get()).toBeNull();
    expect(seen).toHaveBeenCalledTimes(2);
    unsubscribe();
    problemBanner.report(p);
    expect(seen).toHaveBeenCalledTimes(2);
  });

  it("keeps the latest problem when several are reported", () => {
    problemBanner.report(problem({ status: 404 }));
    problemBanner.report(problem({ status: 503 }));
    expect(problemBanner.get()?.problem.status).toBe(503);
  });
});
