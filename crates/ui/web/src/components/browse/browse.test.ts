import { afterEach, describe, expect, it, vi } from "vitest";

import type { DirEntryView, DirListing, Problem, SessionInfo } from "../../api/types";
import {
  breadcrumbs,
  browseOffsetParam,
  browsePathParam,
  childPath,
  classifyBrowseFailure,
  entryAction,
  entryKindLabel,
  isDirectory,
  pageWindow,
  sessionExpiry,
  validateBrowsePath,
} from "./browse";

const MIB = 1024 * 1024;

export const SESSION: SessionInfo = {
  namespace: "media",
  job: "kopiur-browse-nas",
  pod: "kopiur-browse-nas-abcde",
  reused: true,
  expiresAt: "2026-06-11T12:30:00Z",
  downloadMaxBytes: 100 * MIB,
  manifestMaxBytes: 32 * MIB,
};

function entry(over: Partial<DirEntryView> = {}): DirEntryView {
  return { name: "a.txt", kind: "file", size: 16, mtime: null, mode: null, ...over };
}

function problem(type: string, status: number): Problem {
  return {
    type,
    title: "t",
    status,
    detail: "d",
    what: "w",
    why: "y",
    fix: "f",
    instance: null,
    kubeReason: null,
  };
}

function listing(over: Partial<DirListing> = {}): DirListing {
  return {
    path: "",
    entries: [entry()],
    total: 1,
    offset: 0,
    limit: 500,
    session: SESSION,
    ...over,
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("entryKindLabel", () => {
  it("names each unit variant", () => {
    expect(entryKindLabel("file")).toBe("File");
    expect(entryKindLabel("dir")).toBe("Directory");
    expect(entryKindLabel("symlink")).toBe("Symlink");
  });

  it("carries kopia's own letter through the Other fallback, not an Unknown variant", () => {
    // Addenda item 11: EntryKind's fallback is `Other { raw }`, so a socket, a
    // device node or a type a future kopia introduces arrives as an object.
    expect(entryKindLabel({ other: { raw: "S" } })).toBe("Other (S)");
    expect(entryKindLabel({ other: { raw: "" } })).toBe("Other");
  });

  it("renders a unit variant this bundle has never seen rather than throwing", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    // A newer server sending a new unit variant must not take a table cell
    // down: `unknownVariant` warns once and renders the raw word.
    expect(entryKindLabel("fifo" as never)).toBe("fifo");
    expect(warn).toHaveBeenCalledOnce();
  });
});

describe("isDirectory", () => {
  it("is true only for a directory", () => {
    expect(isDirectory("dir")).toBe(true);
    expect(isDirectory("file")).toBe(false);
    expect(isDirectory({ other: { raw: "d" } })).toBe(false);
  });
});

describe("entryAction", () => {
  it("opens a directory and downloads a file within the limit", () => {
    expect(entryAction(entry({ kind: "dir", size: 4096 }), SESSION)).toEqual({ action: "open" });
    expect(entryAction(entry({ size: 12 }), SESSION)).toEqual({ action: "download" });
    expect(entryAction(entry({ size: SESSION.downloadMaxBytes }), SESSION)).toEqual({
      action: "download",
    });
  });

  it("refuses a file over downloadMaxBytes with the size, the limit and the way out", () => {
    // Addenda item 20: the server's 413 arrives on a navigation the SPA can
    // never observe, so the refusal has to be made here, before the click.
    const refusal = entryAction(entry({ name: "big.img", size: 512 * MIB }), SESSION);
    expect(refusal.action).toBe("refused");
    if (refusal.action !== "refused") {
      throw new Error("expected a refusal");
    }
    expect(refusal.reason).toContain("big.img is 512.0 MiB");
    expect(refusal.reason).toContain("100.0 MiB");
    expect(refusal.reason).toContain("kubectl kopiur download");
    // The short form the ledger cell shows, because a scroll container clips
    // the tooltip the sentence would otherwise ride in.
    expect(refusal.word).toBe("over 100.0 MiB");
  });

  it("refuses a file with no recorded size — the 422 download-size-unknown case", () => {
    // Three ways kopia reports no usable size: an explicit null, an absent
    // key, and the negative the server filters with `size >= 0`.
    const noSize: DirEntryView[] = [
      entry({ name: "odd", size: null }),
      { name: "odd", kind: "file", mtime: null, mode: null },
      entry({ name: "odd", size: -1 }),
    ];
    for (const candidate of noSize) {
      const refusal = entryAction(candidate, SESSION);
      expect(refusal.action).toBe("refused");
      if (refusal.action !== "refused") {
        throw new Error("expected a refusal");
      }
      expect(refusal.reason).toContain("records no size for odd");
      expect(refusal.reason).toContain("kubectl kopiur cat");
      expect(refusal.word).toBe("size unknown");
    }
  });

  it("refuses a symlink and an Other entry, each for its own reason", () => {
    const link = entryAction(entry({ name: "latest", kind: "symlink" }), SESSION);
    expect(link.action).toBe("refused");
    if (link.action !== "refused") {
      throw new Error("expected a refusal");
    }
    expect(link.reason).toContain("symbolic link");
    expect(link.word).toBe("a link");

    const socket = entryAction(entry({ name: "sock", kind: { other: { raw: "S" } } }), SESSION);
    expect(socket.action).toBe("refused");
    if (socket.action !== "refused") {
      throw new Error("expected a refusal");
    }
    expect(socket.reason).toContain("as type S");
    expect(socket.reason).toContain("neither a file nor a directory");
    expect(socket.word).toBe("not a file");
  });

  it("treats a zero-byte file as downloadable, not as an absent size", () => {
    expect(entryAction(entry({ size: 0 }), SESSION)).toEqual({ action: "download" });
  });
});

describe("validateBrowsePath", () => {
  it("keeps a relative path and drops empty and `.` components", () => {
    expect(validateBrowsePath("")).toBe("");
    expect(validateBrowsePath("var/log")).toBe("var/log");
    expect(validateBrowsePath("var//log/")).toBe("var/log");
    expect(validateBrowsePath("./var/./log")).toBe("var/log");
  });

  it("refuses exactly what the server refuses: absolute paths and `..`", () => {
    // Mirrors kopiur_ops::browse::validate_rel_path, so the refusal is made
    // here rather than rendered as the handler's 400.
    expect(validateBrowsePath("/etc/passwd")).toBeNull();
    expect(validateBrowsePath("..")).toBeNull();
    expect(validateBrowsePath("var/../../etc")).toBeNull();
  });

  it("keeps a name that merely contains dots", () => {
    expect(validateBrowsePath("..config")).toBe("..config");
    expect(validateBrowsePath("a..b/c")).toBe("a..b/c");
  });
});

describe("browsePathParam", () => {
  it("is total over what the router actually hands a component", () => {
    // The router parses `?path=5` into the number 5 whatever validateSearch
    // declared; a directory named `5` is real, so it survives as written.
    expect(browsePathParam("var/log")).toBe("var/log");
    expect(browsePathParam(5)).toBe("5");
    expect(browsePathParam(true)).toBe("true");
    expect(browsePathParam(undefined)).toBe("");
    expect(browsePathParam({ nested: 1 })).toBe("");
  });
});

describe("browseOffsetParam", () => {
  it("accepts a non-negative whole number and falls back to the start", () => {
    expect(browseOffsetParam(500)).toBe(500);
    expect(browseOffsetParam("500")).toBe(500);
    expect(browseOffsetParam(12.7)).toBe(12);
    expect(browseOffsetParam(-1)).toBe(0);
    expect(browseOffsetParam("oops")).toBe(0);
    expect(browseOffsetParam(undefined)).toBe(0);
  });
});

describe("childPath", () => {
  it("joins under the root without a leading slash", () => {
    expect(childPath("", "var")).toBe("var");
    expect(childPath("var", "log")).toBe("var/log");
  });
});

describe("breadcrumbs", () => {
  it("always starts at the root and names each component with its own path", () => {
    expect(breadcrumbs("", "nightly-1")).toEqual([{ label: "nightly-1", path: "" }]);
    expect(breadcrumbs("var/log/nginx")).toEqual([
      { label: "/", path: "" },
      { label: "var", path: "var" },
      { label: "log", path: "var/log" },
      { label: "nginx", path: "var/log/nginx" },
    ]);
  });

  it("never emits a blank crumb", () => {
    expect(breadcrumbs("var//log/")).toEqual([
      { label: "/", path: "" },
      { label: "var", path: "var" },
      { label: "log", path: "var/log" },
    ]);
  });
});

describe("pageWindow", () => {
  it("describes the first page of a directory with more to come", () => {
    expect(pageWindow(listing({ entries: [entry(), entry()], total: 1200, limit: 500 }))).toEqual({
      first: 1,
      last: 2,
      total: 1200,
      previousOffset: null,
      nextOffset: 2,
    });
  });

  it("steps by what arrived, not by the limit that was asked for", () => {
    // The server clamps `limit` to MAX_TREE_LIMIT, so stepping by the request
    // could skip entries that were never shown.
    const window = pageWindow(
      listing({ entries: [entry(), entry(), entry()], total: 10, offset: 4, limit: 9000 }),
    );
    expect(window.nextOffset).toBe(7);
    expect(window.first).toBe(5);
    expect(window.last).toBe(7);
  });

  it("offers no next page once the window reaches the total", () => {
    expect(
      pageWindow(listing({ entries: [entry(), entry()], total: 2, offset: 0 })).nextOffset,
    ).toBeNull();
  });

  it("reads an offset past the end as an empty page, not as a failure", () => {
    expect(pageWindow(listing({ entries: [], total: 3, offset: 900, limit: 500 }))).toEqual({
      first: 0,
      last: 0,
      total: 3,
      previousOffset: 400,
      nextOffset: null,
    });
  });

  it("clamps the previous offset to the start", () => {
    expect(pageWindow(listing({ offset: 10, limit: 500 })).previousOffset).toBe(0);
  });
});

describe("classifyBrowseFailure", () => {
  it("recognises session-required on BOTH statuses, by type", () => {
    // Addenda item 14, the single most likely way this route ships broken:
    // GET …/session answers 404 and …/tree answers 409, one type.
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:session-required", 404))).toEqual({
      state: "sessionRequired",
    });
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:session-required", 409))).toEqual({
      state: "sessionRequired",
    });
  });

  it("does not read an unrelated 409 or 404 as session-required", () => {
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:conflict", 409))).toEqual({
      state: "other",
    });
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:not-found", 404))).toEqual({
      state: "other",
    });
  });

  it("separates the two buffer refusals, which no retry can clear", () => {
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:directory-too-large", 422))).toEqual({
      state: "tooLarge",
      scope: "directory",
    });
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:catalog-too-large", 422))).toEqual({
      state: "tooLarge",
      scope: "catalog",
    });
  });

  it("marks the exec permit and the readiness wait retryable", () => {
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:too-many-requests", 429))).toEqual({
      state: "retryable",
    });
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:timeout", 504))).toEqual({
      state: "retryable",
    });
  });

  it("treats an unrecognised type as generic rather than guessing", () => {
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:brand-new", 418))).toEqual({
      state: "other",
    });
    expect(classifyBrowseFailure(problem("urn:kopiur-ui:problem:network", 0))).toEqual({
      state: "other",
    });
  });

  it("routes the download refusals to the generic state when they reach a fetch", () => {
    // They normally arrive on a navigation, where the SPA never sees them —
    // but `…/file` is reachable by hand, so they still render as themselves.
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:download-too-large", 413))).toEqual({
      state: "other",
    });
    expect(classifyBrowseFailure(problem("urn:kopiur:problem:download-size-unknown", 422))).toEqual(
      {
        state: "other",
      },
    );
  });
});

describe("sessionExpiry", () => {
  const now = new Date("2026-06-11T12:00:00Z");

  it("says how long a live session has left", () => {
    expect(sessionExpiry(SESSION, now)).toEqual({
      at: "2026-06-11T12:30:00Z",
      label: "in 30m",
      expired: false,
    });
  });

  it("marks a deadline that has already passed", () => {
    const stale = sessionExpiry({ ...SESSION, expiresAt: "2026-06-11T11:55:00Z" }, now);
    expect(stale?.expired).toBe(true);
    expect(stale?.label).toBe("5m ago");
  });

  it("reports no deadline rather than inventing one", () => {
    expect(sessionExpiry({ ...SESSION, expiresAt: null }, now)).toBeNull();
    expect(sessionExpiry({ ...SESSION, expiresAt: "" }, now)).toBeNull();
    expect(sessionExpiry({ ...SESSION, expiresAt: "not a time" }, now)).toBeNull();
  });
});
