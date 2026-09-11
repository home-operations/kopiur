/**
 * The file browser's pure core: what an entry is, whether it can be
 * downloaded, where a path sits, and what a refused read means.
 *
 * Everything here is a plain function over plain data, so the rules that
 * decide whether a user may click a download — the rules that keep the tab
 * from being handed a file it cannot hold — are testable without a session
 * pod, a cluster, or a browser.
 *
 * Two of them mirror the server deliberately.
 *
 * **`validateBrowsePath` mirrors `kopiur_ops::browse::validate_rel_path`.** A
 * `?path=` the server would refuse never leaves this bundle; the route says
 * which value was refused instead of rendering the handler's 400.
 *
 * **`entryAction` mirrors `crates/ui/src/browse/mod.rs::download_size`.** A
 * download is a top-level navigation, so the `413` and the `422` that
 * function raises are rendered by the *browser*, as a tab full of JSON the
 * SPA never sees (addenda item 20). The limits ride on `SessionInfo`
 * precisely so the same two refusals can be made here, before the anchor is
 * clickable, in the same words.
 */

import { isSessionRequired, problemKind } from "../../api/problem";
import type { DirEntryView, DirListing, EntryKind, Problem, SessionInfo } from "../../api/types";
import { unknownVariant } from "../../util/assertNever";
import { humanBytes, relativeTime } from "../../util/format";

/**
 * What an entry is, in a word.
 *
 * `EntryKind` is a heterogeneous union: three unit variants as plain strings
 * and the fallback as `{ other: { raw } }` — **not** `Unknown` (addenda item
 * 11). The string arm is switched exhaustively so a new unit variant fails to
 * compile, and its `default` still degrades to the raw word at run time
 * rather than throwing inside a table cell.
 */
export function entryKindLabel(kind: EntryKind): string {
  if (typeof kind !== "string") {
    const { raw } = kind.other;
    return raw.length > 0 ? `Other (${raw})` : "Other";
  }
  switch (kind) {
    case "file":
      return "File";
    case "dir":
      return "Directory";
    case "symlink":
      return "Symlink";
    default:
      return unknownVariant(kind, "EntryKind");
  }
}

/** Whether an entry is a directory the browser can descend into. */
export function isDirectory(kind: EntryKind): boolean {
  return kind === "dir";
}

/**
 * What the file table offers for one entry: descend into it, download it, or
 * neither — with the reason, in both lengths.
 *
 * A refusal is never a hidden row. The entry stays in the listing with its
 * size and its kind, and the control beside it is disabled *and explained*,
 * because "this file is not listed" and "this file is too big for a browser
 * download" are very different facts about a backup.
 *
 * **Two lengths, deliberately.** A ledger scrolls inside `.ledger-scroll`,
 * which clips the floating tooltip a disabled control would otherwise carry
 * (`styles.css`: `.ledger-scroll .button[data-reason]::after { content: none }`),
 * so the sentence alone would be invisible to a sighted mouse user on exactly
 * the screen that most needs it. `word` goes in the cell, always visible and
 * behind no hover; `reason` goes to `aria-describedby` and to the native
 * `title`. The same split `ReplicationTable`'s run control uses.
 */
export type EntryAction =
  | { action: "open" }
  | { action: "download" }
  | { action: "refused"; word: string; reason: string };

export function entryAction(entry: DirEntryView, session: SessionInfo): EntryAction {
  if (typeof entry.kind !== "string") {
    return otherKindRefusal(entry.name, entry.kind.other.raw);
  }
  switch (entry.kind) {
    case "dir":
      return { action: "open" };
    case "symlink":
      return {
        action: "refused",
        word: "a link",
        reason: `${entry.name} is a symbolic link. The snapshot records where it points, not contents of its own, so there is nothing to download.`,
      };
    case "file":
      return fileAction(entry, session);
    default:
      return otherKindRefusal(entry.name, unknownVariant(entry.kind, "EntryKind"));
  }
}

function otherKindRefusal(name: string, raw: string): EntryAction {
  const what = raw.length > 0 ? `as type ${raw}` : "with no type";
  return {
    action: "refused",
    word: "not a file",
    reason: `kopia recorded ${name} ${what}, which is neither a file nor a directory — there is nothing to list or download.`,
  };
}

/**
 * A file's own verdict: the two refusals `download_size` raises, decided
 * here so they land on the control instead of on a navigated-away tab.
 *
 * A negative size counts as no size, exactly as the server's
 * `entry.size.filter(|s| *s >= 0)` does.
 */
function fileAction(entry: DirEntryView, session: SessionInfo): EntryAction {
  const size = entry.size;
  if (size === null || size === undefined || Number.isNaN(size) || size < 0) {
    return {
      action: "refused",
      word: "size unknown",
      reason: `The snapshot records no size for ${entry.name}. kopiur-ui commits a Content-Length before it streams, so that a truncated transfer is visible to the browser rather than silently saved, and an entry with no recorded size cannot be served that way. Read it with \`kubectl kopiur cat\`, which streams without a declared length.`,
    };
  }
  if (size > session.downloadMaxBytes) {
    return {
      action: "refused",
      word: `over ${humanBytes(session.downloadMaxBytes)}`,
      reason: `${entry.name} is ${humanBytes(size)}, above this deployment's download limit of ${humanBytes(session.downloadMaxBytes)}. A browser download is streamed through kopiur-ui's own process, so the limit bounds what one click costs the UI pod and the session pod. Restore it instead — \`kubectl kopiur download\` streams straight to disk — or raise KOPIUR_UI_MAX_DOWNLOAD_BYTES.`,
    };
  }
  return { action: "download" };
}

// ---- paths ---------------------------------------------------------------

/**
 * The `?path=` a reader wrote, as a string.
 *
 * Total over `unknown` on purpose: the router parses `?path=5` into the
 * **number** `5` and hands the component that number whatever the declared
 * search type says — the trap that took the doctor route to its error
 * boundary. A directory really can be named `5`, so it is rendered back as
 * written rather than dropped.
 */
export function browsePathParam(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  return typeof value === "number" || typeof value === "boolean" ? String(value) : "";
}

/**
 * The normalized path, or `null` when the server would refuse it.
 *
 * The rule is `kopiur_ops::browse::validate_rel_path`'s, repeated here so a
 * refused value never leaves the SPA: an absolute path and a `..` component
 * are refused, empty and `.` components are dropped, and everything else is
 * rejoined. A refusal is `null` rather than a thrown error, so the route can
 * name the value the URL carried.
 */
export function validateBrowsePath(path: string): string | null {
  if (path.startsWith("/")) {
    return null;
  }
  const parts: string[] = [];
  for (const part of path.split("/")) {
    if (part === "" || part === ".") {
      continue;
    }
    if (part === "..") {
      return null;
    }
    parts.push(part);
  }
  return parts.join("/");
}

/** A child of `dir`; the snapshot root is the empty path. */
export function childPath(dir: string, name: string): string {
  return dir.length === 0 ? name : `${dir}/${name}`;
}

/** One breadcrumb: the label to show and the path it navigates to. */
export interface Crumb {
  label: string;
  path: string;
}

/**
 * The trail from the snapshot root down to `path`, root first and always
 * present. `path` is expected to be normalized already
 * ([`validateBrowsePath`]); empty and `.` components are dropped again here
 * so a crumb is never blank.
 */
export function breadcrumbs(path: string, rootLabel = "/"): Crumb[] {
  const crumbs: Crumb[] = [{ label: rootLabel, path: "" }];
  let walked = "";
  for (const part of path.split("/")) {
    if (part === "" || part === ".") {
      continue;
    }
    walked = childPath(walked, part);
    crumbs.push({ label: part, path: walked });
  }
  return crumbs;
}

// ---- pagination ----------------------------------------------------------

/** A `?offset=` a reader wrote: a non-negative whole number, else the start. */
export function browseOffsetParam(value: unknown): number {
  const parsed = typeof value === "string" ? Number(value) : value;
  if (typeof parsed !== "number" || !Number.isFinite(parsed) || parsed < 0) {
    return 0;
  }
  return Math.floor(parsed);
}

/** Which slice of a directory is on screen, and where the neighbours start. */
export interface PageWindow {
  /** 1-based index of the first entry shown; `0` when the page is empty. */
  first: number;
  /** 1-based index of the last entry shown; `0` when the page is empty. */
  last: number;
  total: number;
  previousOffset: number | null;
  nextOffset: number | null;
}

/**
 * The window a listing describes.
 *
 * The next offset steps by the entries actually returned, not by the
 * requested `limit`: the server clamps `limit` to `MAX_TREE_LIMIT`, so
 * stepping by what was asked for could skip entries that were never shown.
 * An offset past the end is an empty page, which is a state and not an error
 * (the server says the same), so both neighbours are still computed from what
 * arrived.
 */
export function pageWindow(listing: DirListing): PageWindow {
  const shown = listing.entries.length;
  const offset = Math.max(0, listing.offset);
  const step = listing.limit > 0 ? listing.limit : shown;
  return {
    first: shown === 0 ? 0 : offset + 1,
    last: shown === 0 ? 0 : offset + shown,
    total: listing.total,
    previousOffset: offset > 0 ? Math.max(0, offset - step) : null,
    nextOffset: shown > 0 && offset + shown < listing.total ? offset + shown : null,
  };
}

// ---- failures ------------------------------------------------------------

/**
 * What a refused browse read means for the screen.
 *
 * `sessionRequired` is **matched on the problem's type, never on its status**
 * (addenda item 14): `GET …/session` sends it as a `404` and `…/tree` sends
 * it as a `409`, both as `urn:kopiur:problem:session-required`, and a screen
 * that keyed on the status would render one of them as a failure and the
 * other as a prompt.
 *
 * `tooLarge` exists to suppress a Retry button: the same request buffers the
 * same oversized manifest and fails identically, so offering a retry would be
 * a lie. `retryable` is the opposite — a `429` exec permit and a `504`
 * readiness wait both clear on their own, and the server's own `fix` says to
 * retry.
 */
export type BrowseFailure =
  | { state: "sessionRequired" }
  | { state: "tooLarge"; scope: "directory" | "catalog" }
  | { state: "retryable" }
  | { state: "other" };

export function classifyBrowseFailure(problem: Problem): BrowseFailure {
  if (isSessionRequired(problem)) {
    return { state: "sessionRequired" };
  }
  switch (problemKind(problem)) {
    case "directory-too-large":
      return { state: "tooLarge", scope: "directory" };
    case "catalog-too-large":
      return { state: "tooLarge", scope: "catalog" };
    case "too-many-requests":
    case "timeout":
      return { state: "retryable" };
    // A type that is not a kopiur URN at all — a synthetic client problem, a
    // proxy's own body — is `null` and is generic, like any other stranger.
    case null:
    default:
      // An unrecognised type is generic, by rule (addenda item 24): the
      // problem's own what / why / fix is rendered and nothing is assumed.
      return { state: "other" };
  }
}

// ---- the session ---------------------------------------------------------

/** When a session will be reaped, said in words. */
export interface SessionExpiry {
  /** The RFC3339 instant, for a `<time datetime>`. */
  at: string;
  /** `in 27m` / `4m ago`, from the shared relative formatter. */
  label: string;
  /** Whether the deadline has already passed. */
  expired: boolean;
}

/**
 * The session's deadline, or `null` when the server published none.
 *
 * `expiresAt` is optional on the wire — a Job whose deadline is not yet
 * stamped has none — and an absent deadline must not render as "expired" or
 * as "never": both are claims the console never received.
 */
export function sessionExpiry(session: SessionInfo, now: Date = new Date()): SessionExpiry | null {
  const at = session.expiresAt;
  if (at === null || at === undefined || at.length === 0) {
    return null;
  }
  const date = new Date(at);
  if (Number.isNaN(date.getTime())) {
    return null;
  }
  return { at, label: relativeTime(date, now), expired: date.getTime() <= now.getTime() };
}
