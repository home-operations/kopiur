import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { DirEntryView, DirListing, Me, Problem, SessionInfo } from "../api/types";
import {
  ME,
  bodyRows,
  calledPaths,
  fetchMock,
  forbiddenProblem,
  jsonResponse,
  mountApp,
  nth,
  problemResponse,
} from "../test-utils";

const MIB = 1024 * 1024;
const ROUTE = "/snapshots/media/nightly-1/browse";
const SESSION_PATH = "/api/v1/snapshots/media/nightly-1/session";
const TREE_PATH = "/api/v1/snapshots/media/nightly-1/tree";
const FILE_PATH = "/api/v1/snapshots/media/nightly-1/file";

/**
 * A deadline `n` whole minutes out, landing mid-minute.
 *
 * The half-minute is what makes the assertion deterministic: the label floors
 * the remaining seconds into minutes, so a fixture built at module load and
 * read a few seconds later would otherwise slip from `in 27m` to `in 26m`
 * whenever the suite ran under load. Called from inside the test that reads
 * the label, so the slack is measured from the render and not from import.
 */
function expiringIn(minutes: number): string {
  return new Date(Date.now() + minutes * 60_000 + 30_000).toISOString();
}

const SESSION: SessionInfo = {
  namespace: "media",
  job: "kopiur-browse-nas",
  pod: "kopiur-browse-nas-abcde",
  reused: true,
  expiresAt: expiringIn(27),
  downloadMaxBytes: 100 * MIB,
  manifestMaxBytes: 32 * MIB,
};

const ENTRIES: DirEntryView[] = [
  { name: "log", kind: "dir", size: 40960, mtime: "2026-06-10T09:00:00Z", mode: "drwxr-xr-x" },
  { name: "app.log", kind: "file", size: 2048, mtime: "2026-06-11T08:15:00Z", mode: "-rw-r--r--" },
  { name: "core.img", kind: "file", size: 512 * MIB, mtime: null, mode: "-rw-------" },
];

function listing(over: Partial<DirListing> = {}): DirListing {
  return {
    path: "",
    entries: ENTRIES,
    total: ENTRIES.length,
    offset: 0,
    limit: 500,
    session: SESSION,
    ...over,
  };
}

/** A kopiur problem, as `crates/ui/src/api/problem.rs` writes one. */
function kopiurProblem(kind: string, status: number, what = `a ${kind} answer`): Problem {
  return {
    type: `urn:kopiur:problem:${kind}`,
    title: kind,
    status,
    detail: what,
    what,
    why: `why ${kind} happened`,
    fix: `how to fix ${kind}`,
    instance: null,
    kubeReason: null,
  };
}

/** The one answer both statuses carry — 404 from `…/session`, 409 from `…/tree`. */
function sessionRequired(status: number): Problem {
  return kopiurProblem(
    "session-required",
    status,
    "No browse session is running for media/nightly-1.",
  );
}

type Method = "GET" | "POST" | "DELETE";
type Reply = () => Response;

/**
 * A fetch mock that can tell the three `…/session` methods apart, which
 * `mockApi` deliberately cannot — this screen is the only one whose GET,
 * POST and DELETE all land on one path.
 *
 * An unrouted `…/session` GET defaults to the 404 `session-required` the
 * server actually sends when none is running, so the common case needs no
 * setup at all.
 */
function mockBrowse(routes: {
  session?: Partial<Record<Method, Reply>>;
  tree?: (url: URL) => Response;
  me?: Me;
}): void {
  fetchMock.resetMocks();
  fetchMock.mockResponse((request) => {
    const url = new URL(request.url, "http://localhost");
    const method = request.method.toUpperCase() as Method;
    if (url.pathname === "/api/v1/me") {
      return Promise.resolve(jsonResponse(routes.me ?? ME));
    }
    if (url.pathname === SESSION_PATH) {
      const reply = routes.session?.[method];
      return Promise.resolve(reply === undefined ? problemResponse(sessionRequired(404)) : reply());
    }
    if (url.pathname === TREE_PATH) {
      return Promise.resolve(
        routes.tree === undefined ? jsonResponse(listing()) : routes.tree(url),
      );
    }
    return Promise.resolve(
      new Response(`no mock for ${method} ${url.pathname}`, {
        status: 404,
        headers: { "content-type": "text/plain" },
      }),
    );
  });
}

/** A session that is running; `…/tree` then serves whatever the test set. */
function running(): Partial<Record<Method, Reply>> {
  return { GET: () => jsonResponse(SESSION) };
}

function table(name = "Entries in the snapshot") {
  return screen.findByRole("table", { name });
}

describe("Snapshot file browser — the session", () => {
  it("renders the start prompt when GET …/session answers 404 session-required", async () => {
    // Addenda item 14: matched on the problem's TYPE. A screen keyed on the
    // status would have to know that this one 404 is not a missing snapshot.
    mockBrowse({});
    mountApp(ROUTE);
    expect(await screen.findByText("No browse session is running")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Start a browse session/ })).toBeInTheDocument();
  });

  it("says plainly, above the button, that the pod mounts the repository's credentials", async () => {
    // The whole reason this screen has a start prompt at all rather than
    // opening a session the moment it loads.
    mockBrowse({});
    mountApp(ROUTE);
    const panel = await screen.findByRole("region", { name: "Browse session" });
    expect(panel).toHaveTextContent(/mounts this repository[’']s credentials/);
    expect(panel).toHaveTextContent(/runs a pod in\s*media/);
    expect(panel).toHaveTextContent(/It runs as you/);
    expect(panel).toHaveTextContent(/pods\/exec/);
  });

  it("never asks for a listing while there is no session", async () => {
    // `…/tree` without one is a guaranteed 409; asking anyway would spend an
    // apiserver round trip to be told what the page already knows.
    mockBrowse({});
    mountApp(ROUTE);
    await screen.findByText("No browse session is running");
    expect(calledPaths().some((path) => path.startsWith(TREE_PATH))).toBe(false);
  });

  it("shows a running session with its expiry, its Job and its pod", async () => {
    const live: SessionInfo = { ...SESSION, expiresAt: expiringIn(27) };
    mockBrowse({ session: { GET: () => jsonResponse(live) } });
    mountApp(ROUTE);
    expect(await screen.findByText("A browse session is running")).toBeInTheDocument();
    const panel = screen.getByRole("region", { name: "Browse session" });
    expect(panel).toHaveTextContent("kopiur-browse-nas");
    expect(panel).toHaveTextContent("kopiur-browse-nas-abcde");
    expect(within(panel).getByText("in 27m")).toHaveAttribute("datetime", live.expiresAt);
    expect(panel).toHaveTextContent(/holding this repository open with its credentials mounted/);
  });

  it("marks a session whose deadline has already passed rather than counting up quietly", async () => {
    mockBrowse({ session: { GET: () => jsonResponse({ ...SESSION, expiresAt: expiringIn(-6) }) } });
    mountApp(ROUTE);
    expect(await screen.findByText(/it may already be gone/)).toBeInTheDocument();
  });

  it("reports no deadline rather than inventing one when expiresAt is absent", async () => {
    mockBrowse({ session: { GET: () => jsonResponse({ ...SESSION, expiresAt: null }) } });
    mountApp(ROUTE);
    expect(await screen.findByText(/no deadline published/)).toBeInTheDocument();
  });

  it("starts a session on the button, and the panel flips to running", async () => {
    let started = false;
    mockBrowse({
      session: {
        GET: () => (started ? jsonResponse(SESSION) : problemResponse(sessionRequired(404))),
        POST: () => {
          started = true;
          return jsonResponse(SESSION, 201);
        },
      },
    });
    mountApp(ROUTE);
    await userEvent.click(await screen.findByRole("button", { name: /Start a browse session/ }));
    expect(await screen.findByText("A browse session is running")).toBeInTheDocument();
    expect(await table()).toBeInTheDocument();
  });

  it("stops a session on a 204 with an empty body without throwing", async () => {
    // Addenda item 18: a blanket JSON.parse would throw on the first click.
    let live = true;
    mockBrowse({
      session: {
        GET: () => (live ? jsonResponse(SESSION) : problemResponse(sessionRequired(404))),
        DELETE: () => {
          live = false;
          return new Response(null, { status: 204 });
        },
      },
    });
    mountApp(ROUTE);
    await userEvent.click(await screen.findByRole("button", { name: /Stop the session/ }));
    expect(await screen.findByText("No browse session is running")).toBeInTheDocument();
    expect(screen.queryByText(/not JSON/)).toBeNull();
  });

  it("renders the start prompt — not a failure — when a LISTING answers 409 session-required", async () => {
    // The other half of addenda item 14, and the harder half: the session
    // query's cached 200 is stale, and the read is what found out.
    mockBrowse({
      session: running(),
      tree: () => problemResponse(sessionRequired(409)),
    });
    mountApp(ROUTE);
    expect(await screen.findByText("The browse session has ended")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Start a browse session/ })).toBeInTheDocument();
    expect(screen.queryByText("A browse session is running")).toBeNull();
  });

  it("disables both controls with the reason when the identity holds neither grant", async () => {
    const viewer: Me = {
      ...ME,
      user: "bob",
      can: { ...ME.can, createSessionJobs: false, execSessions: false },
    };
    mockBrowse({ me: viewer });
    mountApp(ROUTE);
    // The reason starts as "your permissions have not loaded yet" — itself
    // correct, and never an RBAC verdict the console has not received — so
    // wait for `/me` to answer before reading the real one.
    await screen.findByText(/Start browse sessions is not permitted for bob/);
    const start = screen.getByRole("button", { name: /Start a browse session/ });
    expect(start).toHaveAttribute("aria-disabled", "true");
    expect(start).toHaveAccessibleDescription(/Start browse sessions is not permitted for bob/);
    expect(start).toHaveAccessibleDescription(/kopiur-ui-user or kopiur-ui-editor/);
  });

  it("renders a refused session read as not permitted, never as the start prompt", async () => {
    // A 403 is not "there is no session"; the question could not be put.
    mockBrowse({
      session: { GET: () => problemResponse(forbiddenProblem("listing Jobs", SESSION_PATH)) },
    });
    mountApp(ROUTE);
    expect(await screen.findByText(/Not permitted to view/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Start a browse session/ })).toBeNull();
  });
});

describe("Snapshot file browser — the listing", () => {
  it("lists a directory once a session is running, and asks for the root by default", async () => {
    mockBrowse({ session: running() });
    mountApp(ROUTE);
    expect(bodyRows(await table())).toHaveLength(3);
    expect(calledPaths()).toContain(`${TREE_PATH}?path=&offset=0`);
  });

  it("walks into a directory by its link and lists the child path", async () => {
    mockBrowse({
      session: running(),
      tree: (url) =>
        jsonResponse(
          url.searchParams.get("path") === "log"
            ? listing({ path: "log", entries: [nth(ENTRIES, 1)], total: 1 })
            : listing(),
        ),
    });
    mountApp(ROUTE);
    await userEvent.click(await screen.findByRole("link", { name: "log" }));
    expect(await table("Entries in log")).toBeInTheDocument();
    expect(calledPaths()).toContain(`${TREE_PATH}?path=log&offset=0`);
    // And the trail back is on the page.
    expect(screen.getByRole("link", { name: "nightly-1" })).toHaveAttribute("href", ROUTE);
  });

  it("pages with the window stated in entries, not in page numbers", async () => {
    mockBrowse({
      session: running(),
      tree: (url) => {
        const offset = Number(url.searchParams.get("offset") ?? "0");
        return jsonResponse(listing({ offset, total: 12043, limit: 500 }));
      },
    });
    mountApp(ROUTE);
    await table();
    const pager = screen.getByRole("navigation", { name: "Directory pages" });
    expect(pager).toHaveTextContent("1–3 of 12043");
    expect(within(pager).queryByRole("link", { name: /Previous/ })).toBeNull();
    expect(within(pager).getByRole("link", { name: /Next/ })).toHaveAttribute(
      "href",
      `${ROUTE}?offset=3`,
    );
  });

  it("steps forward and back, and the previous link returns to the first page", async () => {
    mockBrowse({
      session: running(),
      tree: (url) => {
        const offset = Number(url.searchParams.get("offset") ?? "0");
        return jsonResponse(listing({ offset, total: 12043, limit: 500 }));
      },
    });
    mountApp(`${ROUTE}?offset=500`);
    await table();
    const pager = screen.getByRole("navigation", { name: "Directory pages" });
    expect(pager).toHaveTextContent("501–503 of 12043");
    expect(within(pager).getByRole("link", { name: /Previous/ })).toHaveAttribute("href", ROUTE);
    expect(calledPaths()).toContain(`${TREE_PATH}?path=&offset=500`);
  });

  it("offers no next page once the window reaches the total", async () => {
    mockBrowse({ session: running() });
    mountApp(ROUTE);
    await table();
    const pager = screen.getByRole("navigation", { name: "Directory pages" });
    expect(within(pager).queryByRole("link", { name: /Next/ })).toBeNull();
  });

  it("renders an empty directory as a fact about the snapshot, not as a failure", async () => {
    mockBrowse({
      session: running(),
      tree: () => jsonResponse(listing({ entries: [], total: 0 })),
    });
    mountApp(ROUTE);
    expect(await screen.findByText(/The snapshot root is empty/)).toBeInTheDocument();
  });

  it("refuses a ?path= the server would refuse, naming the value, without asking for it", async () => {
    mockBrowse({ session: running() });
    mountApp(`${ROUTE}?path=..%2F..%2Fetc`);
    expect(await screen.findByText("That path cannot be browsed")).toBeInTheDocument();
    expect(screen.getByText("../../etc")).toBeInTheDocument();
    expect(calledPaths().some((path) => path.startsWith(TREE_PATH))).toBe(false);
  });
});

describe("Snapshot file browser — downloads", () => {
  it("renders a downloadable file as an anchor and never fetches …/file", async () => {
    mockBrowse({ session: running() });
    mountApp(ROUTE);
    await table();
    const link = screen.getByRole("link", { name: "Download app.log" });
    expect(link.tagName).toBe("A");
    expect(link).toHaveAttribute("href", `${FILE_PATH}?path=app.log`);
    // The point of the anchor: the file never passes through the SPA's heap.
    expect(calledPaths().some((path) => path.startsWith(FILE_PATH))).toBe(false);
  });

  it("disables an oversized entry with its reason BEFORE the link can be clicked", async () => {
    // Addenda item 20: the server's 413 arrives on a navigation, so it would
    // be rendered by the browser as a tab of JSON the SPA never sees.
    mockBrowse({ session: running() });
    mountApp(ROUTE);
    await table();
    const refused = await screen.findByRole("button", { name: /Download core\.img/ });
    expect(refused).toHaveAttribute("aria-disabled", "true");
    expect(refused).toHaveAccessibleDescription(/core\.img is 512\.0 MiB/);
    expect(refused).toHaveAccessibleDescription(/above this deployment's download limit/);
    expect(screen.queryByRole("link", { name: /Download core\.img/ })).toBeNull();
    // And the short form is on screen, not behind a hover a ledger would clip.
    expect(screen.getByText("over 100.0 MiB")).toBeInTheDocument();
  });

  it("takes the limit from the listing's own session, so a re-configured server is honoured", async () => {
    mockBrowse({
      session: running(),
      tree: () =>
        jsonResponse(listing({ session: { ...SESSION, downloadMaxBytes: 1024 * 1024 * 1024 } })),
    });
    mountApp(ROUTE);
    await table();
    expect(screen.getByRole("link", { name: "Download core.img" })).toHaveAttribute(
      "href",
      `${FILE_PATH}?path=core.img`,
    );
  });
});

describe("Snapshot file browser — refused reads", () => {
  it("renders a 422 directory-too-large with no retry and a way up a level", async () => {
    // Retrying re-buffers the same oversized manifest; the parent might not be.
    mockBrowse({
      session: running(),
      tree: () => problemResponse(kopiurProblem("directory-too-large", 422)),
    });
    mountApp(`${ROUTE}?path=var%2Fspool`);
    expect(await screen.findByText("a directory-too-large answer")).toBeInTheDocument();
    expect(screen.getByText("how to fix directory-too-large")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
    expect(screen.getByRole("link", { name: /Up one level/ })).toHaveAttribute(
      "href",
      `${ROUTE}?path=var`,
    );
  });

  it("renders a 422 catalog-too-large with no retry and no way up — the repository is the cause", async () => {
    mockBrowse({
      session: running(),
      tree: () => problemResponse(kopiurProblem("catalog-too-large", 422)),
    });
    mountApp(`${ROUTE}?path=var`);
    expect(await screen.findByText("a catalog-too-large answer")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
    expect(screen.queryByRole("link", { name: /Up one level/ })).toBeNull();
  });

  it("renders a 429 too-many-requests with a retry, because it clears on its own", async () => {
    mockBrowse({
      session: running(),
      tree: () => problemResponse(kopiurProblem("too-many-requests", 429)),
    });
    mountApp(ROUTE);
    expect(await screen.findByText("a too-many-requests answer")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("renders a 413 download-too-large reaching a fetch route as the problem it is", async () => {
    mockBrowse({
      session: running(),
      tree: () => problemResponse(kopiurProblem("download-too-large", 413)),
    });
    mountApp(ROUTE);
    expect(await screen.findByText("a download-too-large answer")).toBeInTheDocument();
    expect(screen.getByText("how to fix download-too-large")).toBeInTheDocument();
  });

  it("renders a 422 download-size-unknown reaching a fetch route as the problem it is", async () => {
    mockBrowse({
      session: running(),
      tree: () => problemResponse(kopiurProblem("download-size-unknown", 422)),
    });
    mountApp(ROUTE);
    expect(await screen.findByText("a download-size-unknown answer")).toBeInTheDocument();
  });

  it("renders a refused listing as not permitted", async () => {
    mockBrowse({
      session: running(),
      tree: () => problemResponse(forbiddenProblem("exec into the session pod", TREE_PATH)),
    });
    mountApp(ROUTE);
    expect(await screen.findByText(/Not permitted to view/)).toBeInTheDocument();
    // The session panel stays: the session is real, the read was refused.
    expect(screen.getByText("A browse session is running")).toBeInTheDocument();
  });

  it("does not read an unrelated 409 as session-required", async () => {
    mockBrowse({
      session: running(),
      tree: () => problemResponse(kopiurProblem("conflict", 409)),
    });
    mountApp(ROUTE);
    expect(await screen.findByText("a conflict answer")).toBeInTheDocument();
    expect(screen.queryByText("The browse session has ended")).toBeNull();
  });
});
