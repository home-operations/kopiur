import { Link, createFileRoute } from "@tanstack/react-router";
import { Camera, ChevronLeft, ChevronRight, FolderOpen, FolderX } from "lucide-react";

import { useBrowseSession, useSnapshotTree } from "../api/hooks";
import type { Problem } from "../api/types";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { Breadcrumbs } from "../components/browse/Breadcrumbs";
import { DirTable } from "../components/browse/DirTable";
import { SessionBar } from "../components/browse/SessionBar";
import {
  breadcrumbs,
  browseOffsetParam,
  browsePathParam,
  classifyBrowseFailure,
  pageWindow,
  validateBrowsePath,
} from "../components/browse/browse";
import { namespaceFromSearch } from "../util/namespace";

/**
 * The file browser: what is actually inside one snapshot.
 *
 * This is the screen where "the backup ran" becomes "the backup contains my
 * data", so everything it shows is read live out of the repository rather
 * than out of a status field. That has a price, and the price is the point of
 * [`SessionBar`]: listing a directory needs a mover pod holding the
 * repository open, with that repository's credentials mounted, execed into as
 * the signed-in user. The panel says so where the session is started.
 *
 * **No session is a state, not an error** — and it arrives two ways. `GET
 * …/session` answers `404` while `…/tree` answers `409`, both carrying
 * `urn:kopiur:problem:session-required`, so both are matched on the problem's
 * **type** and never on its status (addenda item 14). The first is the
 * ordinary case, folded to `null` by `useBrowseSession`; the second is the
 * session lapsing between the page render and a click, which renders the same
 * prompt with a line saying the session ended.
 *
 * **The route file is `snapshots_.$namespace.$name.browse.tsx`.** The escaped
 * form — the trailing `_` — is load-bearing: named `snapshots.$namespace…`
 * beside `snapshots.tsx`, the list would become this route's *layout*, and
 * this page would render into an `<Outlet/>` the list does not have. The
 * symptom is a silently blank screen, not an error. The URL is unchanged:
 * `/snapshots/{namespace}/{name}/browse`.
 */
export interface BrowseSearch {
  /** The directory being listed, relative to the snapshot root. */
  path?: string;
  /** Index of the first entry shown. */
  offset?: number;
}

export const Route = createFileRoute("/snapshots_/$namespace/$name_/browse")({
  validateSearch: (search: Record<string, unknown>): BrowseSearch => {
    const out: BrowseSearch = {};
    const path = browsePathParam(search.path);
    // A path the server would refuse is KEPT, not dropped. Dropping it would
    // silently list the snapshot root instead, so a shared link would answer
    // a question nobody asked; kept, the route can name the refused value.
    if (path.length > 0) {
      out.path = path;
    }
    const offset = browseOffsetParam(search.offset);
    if (offset > 0) {
      out.offset = offset;
    }
    return out;
  },
  component: BrowseRoute,
});

function BrowseRoute() {
  const { namespace, name } = Route.useParams();
  // Re-read raw: the router hands the component the parsed value even for a
  // key `validateSearch` dropped, so the declared types above are a claim
  // about the validator's output and not about what render is given.
  const search: Record<string, unknown> = Route.useSearch();
  const scope = namespaceFromSearch(search);
  const asked = browsePathParam(search.path);
  const path = validateBrowsePath(asked);
  const offset = browseOffsetParam(search.offset);

  const session = useBrowseSession(namespace, name);
  const treeParams = { path: path ?? "", offset };
  const tree = useSnapshotTree(namespace, name, treeParams, {
    // Never issue a read that cannot be served: without a session `…/tree` is
    // a guaranteed 409, and a path the server refuses is a guaranteed 400.
    enabled: session.data != null && path !== null,
  });

  // The session can lapse between the page render and this listing, and the
  // read is the first thing that finds out. Its 409 supersedes the session
  // query's cached answer: the prompt is the truth, not the running panel.
  const treeFailure = tree.error !== null ? classifyBrowseFailure(tree.error.problem) : null;
  const lapsed = treeFailure?.state === "sessionRequired";
  const live = lapsed ? null : (session.data ?? null);

  const title = (
    <header className="browse-head">
      <p className="page__prose">
        The files inside <span className="mono">{name}</span>, read live out of the repository —
        this is where a backup stops being a green row and starts being your data. Listing and
        downloading both go through a browse session; sizes and timestamps are the snapshot&rsquo;s
        own, not the source volume&rsquo;s as it stands today.
      </p>
      <Link
        className="button"
        to="/snapshots"
        search={scope !== undefined ? { namespace: scope } : {}}
      >
        <Camera size={14} strokeWidth={2} aria-hidden="true" />
        All snapshots
      </Link>
    </header>
  );

  if (session.isPending) {
    return (
      <div className="page">
        {title}
        <LoadingState what={`the browse session for ${name}`} rows={3} />
      </div>
    );
  }

  // A failure the hook did not fold to `null` — a 403 on the session read, a
  // gateway's 502. The prompt would be a lie here: nothing was asked and
  // refused, the question itself could not be put.
  if (session.isError) {
    return (
      <div className="page">
        {title}
        <ErrorState
          problem={session.error.problem}
          what={`the browse session for ${name}`}
          onRetry={
            classifyBrowseFailure(session.error.problem).state === "tooLarge"
              ? undefined
              : () => void session.refetch()
          }
        />
      </div>
    );
  }

  return (
    <div className="page">
      {title}
      <SessionBar namespace={namespace} name={name} session={live} lapsed={lapsed} />
      {live === null ? null : (
        <section className="page__section" aria-label="Snapshot contents">
          {path === null ? (
            <RefusedPath asked={asked} namespace={namespace} name={name} scope={scope} />
          ) : (
            <>
              <Breadcrumbs
                namespace={namespace}
                name={name}
                path={path}
                rootLabel={name}
                scope={scope}
              />
              <Listing
                namespace={namespace}
                name={name}
                path={path}
                scope={scope}
                pending={tree.isPending || tree.isFetching}
                problem={tree.error?.problem}
                listing={tree.data}
                onRetry={() => void tree.refetch()}
              />
            </>
          )}
        </section>
      )}
    </div>
  );
}

interface ListingProps {
  namespace: string;
  name: string;
  path: string;
  scope: string | undefined;
  pending: boolean;
  problem: Problem | undefined;
  listing: ReturnType<typeof useSnapshotTree>["data"];
  onRetry: () => void;
}

/** One directory: its page of entries, or the reason there is none. */
function Listing({
  namespace,
  name,
  path,
  scope,
  pending,
  problem,
  listing,
  onRetry,
}: ListingProps) {
  if (problem !== undefined) {
    const failure = classifyBrowseFailure(problem);
    // A buffer the server will not grow cannot be talked round by retrying:
    // the same manifest is read and refused again. Going up a level can
    // actually help, so that is what is offered instead.
    const tooLarge = failure.state === "tooLarge";
    const parent = breadcrumbs(path).at(-2);
    return (
      <ErrorState
        problem={problem}
        what={path.length > 0 ? `${path} in ${name}` : `the contents of ${name}`}
        onRetry={tooLarge ? undefined : onRetry}
        actions={
          tooLarge && failure.scope === "directory" && parent !== undefined ? (
            <Link
              className="button"
              to="/snapshots/$namespace/$name/browse"
              params={{ namespace, name }}
              search={{
                ...(scope !== undefined ? { namespace: scope } : {}),
                ...(parent.path.length > 0 ? { path: parent.path } : {}),
              }}
            >
              <FolderOpen size={14} strokeWidth={2} aria-hidden="true" />
              Up one level
            </Link>
          ) : undefined
        }
      />
    );
  }

  if (listing === undefined) {
    return <LoadingState what={path.length > 0 ? path : "the snapshot root"} rows={6} />;
  }

  const window = pageWindow(listing);
  if (listing.entries.length === 0) {
    return (
      <EmptyState
        title={
          window.total === 0
            ? `${path.length > 0 ? path : "The snapshot root"} is empty`
            : "Nothing on this page"
        }
        icon={FolderX}
      >
        {window.total === 0
          ? "The backup recorded this directory with no entries in it. That is a fact about the snapshot, not a failure to read it — an excluded path or an empty source directory both look like this."
          : `This directory holds ${window.total} entries, but the page starts past the last one. Go back to the first page.`}
      </EmptyState>
    );
  }

  return (
    <>
      <DirTable namespace={namespace} name={name} listing={listing} scope={scope} />
      <Pager
        namespace={namespace}
        name={name}
        path={path}
        scope={scope}
        window={window}
        pending={pending}
      />
    </>
  );
}

interface PagerProps {
  namespace: string;
  name: string;
  path: string;
  scope: string | undefined;
  window: ReturnType<typeof pageWindow>;
  pending: boolean;
}

/**
 * Which slice of the directory is on screen, and the two steps either side.
 *
 * The window is stated in words rather than as a page number: a directory
 * with twelve thousand entries has no meaningful page count to a reader who
 * arrived here looking for one file, but "1–500 of 12,043" says both how much
 * is in front of them and that there is more.
 */
function Pager({ namespace, name, path, scope, window, pending }: PagerProps) {
  const scoped = scope !== undefined ? { namespace: scope } : {};
  const pathed = path.length > 0 ? { path } : {};
  const step = (offset: number | null) =>
    offset === null ? undefined : { ...scoped, ...pathed, ...(offset > 0 ? { offset } : {}) };
  const previous = step(window.previousOffset);
  const next = step(window.nextOffset);
  return (
    <nav className="browse-pager" aria-label="Directory pages">
      <p className="browse-pager__window" aria-live="polite">
        {pending ? "Loading…" : `${window.first}–${window.last} of ${window.total}`}
      </p>
      <div className="browse-pager__steps">
        {previous !== undefined ? (
          <Link
            className="button"
            to="/snapshots/$namespace/$name/browse"
            params={{ namespace, name }}
            search={previous}
          >
            <ChevronLeft size={14} strokeWidth={2} aria-hidden="true" />
            Previous
          </Link>
        ) : null}
        {next !== undefined ? (
          <Link
            className="button"
            to="/snapshots/$namespace/$name/browse"
            params={{ namespace, name }}
            search={next}
          >
            Next
            <ChevronRight size={14} strokeWidth={2} aria-hidden="true" />
          </Link>
        ) : null}
      </div>
    </nav>
  );
}

interface RefusedPathProps {
  asked: string;
  namespace: string;
  name: string;
  scope: string | undefined;
}

/**
 * A `?path=` the server would refuse, refused here instead.
 *
 * `validate_rel_path` rejects an absolute path and any `..` component, so
 * sending one would earn a `400` that says the same thing less usefully — and
 * would cost a pod exec to say it. Naming the value the URL carried is the
 * part that matters: a link pasted from a shell is exactly how one gets here.
 */
function RefusedPath({ asked, namespace, name, scope }: RefusedPathProps) {
  return (
    <EmptyState title="That path cannot be browsed" icon={FolderX}>
      <p>
        The address asked for <span className="mono">{asked}</span>. A browse path is relative to
        the snapshot&rsquo;s own root: it may not begin with <span className="mono">/</span> and may
        not contain a <span className="mono">..</span> component, because a snapshot has no parent
        to climb into.
      </p>
      <p>
        <Link
          className="button"
          to="/snapshots/$namespace/$name/browse"
          params={{ namespace, name }}
          search={scope !== undefined ? { namespace: scope } : {}}
        >
          <FolderOpen size={14} strokeWidth={2} aria-hidden="true" />
          Start at the snapshot root
        </Link>
      </p>
    </EmptyState>
  );
}
