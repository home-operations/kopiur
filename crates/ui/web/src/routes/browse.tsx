import { Link, createFileRoute, redirect, useNavigate } from "@tanstack/react-router";
import { Camera, ChevronLeft, ChevronRight, FolderOpen, FolderX, Info } from "lucide-react";
import { type ReactNode, useEffect } from "react";

import { useBrowseSession, useSnapshot, useSnapshotTree } from "../api/hooks";
import type { Problem } from "../api/types";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { Facts, type Fact } from "../components/Facts";
import { InspectLink } from "../components/InspectLink";
import { LoadingState } from "../components/LoadingState";
import { SnapshotField } from "../components/ResourceFields";
import { Breadcrumbs } from "../components/browse/Breadcrumbs";
import { DirTable } from "../components/browse/DirTable";
import { SessionBar } from "../components/browse/SessionBar";
import { browsed } from "../components/browse/browsed";
import {
  breadcrumbs,
  browseOffsetParam,
  browsePathParam,
  browseSearch,
  classifyBrowseFailure,
  pageWindow,
  parseSnapshotParam,
  validateBrowsePath,
} from "../components/browse/browse";
import { formatTimestamp, humanBytes, relativeTime } from "../util/format";
import { namespaceFromSearch } from "../util/namespace";

/**
 * Browse: what is actually inside a snapshot.
 *
 * This is the screen where "the backup ran" becomes "the backup contains my
 * data", so everything it shows is read live out of the repository rather
 * than out of a status field. That has a price, and the price is the point of
 * [`SessionBar`]: listing a directory needs a mover pod holding the
 * repository open, with that repository's credentials mounted, execed into as
 * the signed-in user. The panel says so where the session is started.
 *
 * **The snapshot is part of the address** (`?snapshot=namespace/name`), and
 * the head of the page says which one it is and picks another. A snapshot's
 * drawer opens this page with its own already chosen. The last one browsed is
 * kept for the tab, so coming back to Browse from the sidebar lands on it
 * again rather than on an empty picker; the address still wins whenever it
 * names one.
 *
 * **No session is a state, not an error** — and it arrives two ways. `GET
 * …/session` answers `404` while `…/tree` answers `409`, both carrying
 * `urn:kopiur:problem:session-required`, so both are matched on the problem's
 * **type** and never on its status (addenda item 14). The first is the
 * ordinary case, folded to `null` by `useBrowseSession`; the second is the
 * session lapsing between the page render and a click, which renders the same
 * prompt with a line saying the session ended.
 */
export interface BrowseSearch {
  /** The snapshot being browsed, as `namespace/name`. */
  snapshot?: string;
  /** The directory being listed, relative to the snapshot root. */
  path?: string;
  /** Index of the first entry shown. */
  offset?: number;
}

export const Route = createFileRoute("/browse")({
  validateSearch: (search: Record<string, unknown>): BrowseSearch => {
    const out: BrowseSearch = {};
    // A snapshot or path the page cannot use is KEPT, not dropped: dropped, a
    // shared link would silently show something nobody asked for; kept, the
    // page can name the value it refused.
    const snapshot = browsePathParam(search.snapshot);
    if (snapshot.length > 0) {
      out.snapshot = snapshot;
    }
    const path = browsePathParam(search.path);
    if (path.length > 0) {
      out.path = path;
    }
    const offset = browseOffsetParam(search.offset);
    if (offset > 0) {
      out.offset = offset;
    }
    return out;
  },
  beforeLoad: ({ search }) => {
    const raw: Record<string, unknown> = search;
    if (raw.snapshot !== undefined) {
      return;
    }
    const last = browsed.stored();
    if (last === null) {
      return;
    }
    const scope = namespaceFromSearch(raw);
    redirect({
      throw: true,
      to: "/browse",
      search: { ...(scope !== undefined ? { namespace: scope } : {}), snapshot: last },
      replace: true,
    });
  },
  component: BrowseRoute,
});

function BrowseRoute() {
  // Re-read raw: the router hands the component the parsed value even for a
  // key `validateSearch` dropped, so the declared types above are a claim
  // about the validator's output and not about what render is given.
  const search: Record<string, unknown> = Route.useSearch();
  const scope = namespaceFromSearch(search);
  const asked = browsePathParam(search.snapshot);
  const target = parseSnapshotParam(asked);
  const key = target === null ? null : `${target.namespace}/${target.name}`;

  useEffect(() => {
    if (key !== null) {
      browsed.browse(key);
    }
  }, [key]);

  return (
    <div className="page">
      <p className="page__prose">
        The files inside a snapshot, read from the repository through a browse session. Sizes and
        times are as they were when the snapshot was taken.
      </p>
      {target === null ? (
        <ActiveSnapshot target={null} value="" scope={scope}>
          {asked.length > 0 ? (
            <BrowseNote title="That is not a snapshot address">
              The address asked for <span className="mono">{asked}</span>. A snapshot is named as{" "}
              <span className="mono">namespace/name</span>. Pick one above.
            </BrowseNote>
          ) : (
            <BrowseNote
              title="Pick a snapshot to browse"
              action={
                <Link
                  className="button"
                  to="/snapshots"
                  search={scope !== undefined ? { namespace: scope } : {}}
                >
                  <Camera size={14} strokeWidth={2} aria-hidden="true" />
                  Snapshots
                </Link>
              }
            >
              Choose one above, or open a snapshot from the list and use its Browse files button.
            </BrowseNote>
          )}
        </ActiveSnapshot>
      ) : (
        <Browser
          // A new snapshot is a new browser: nothing of the last one's
          // listing state may carry over.
          key={key}
          namespace={target.namespace}
          name={target.name}
          scope={scope}
          search={search}
        />
      )}
    </div>
  );
}

interface ActiveSnapshotProps {
  target: { namespace: string; name: string } | null;
  value: string;
  scope: string | undefined;
  /** The session, or what stands in for it, under the snapshot's facts. */
  children: ReactNode;
}

/**
 * The page's one card: which snapshot this is, a way to pick another, its
 * facts, and under them the browse session that reads it — the two halves of
 * one question, "what am I looking at, and can I look yet".
 */
function ActiveSnapshot({ target, value, scope, children }: ActiveSnapshotProps) {
  const navigate = useNavigate();
  const detail = useSnapshot(target?.namespace ?? "", target?.name ?? "", {
    enabled: target !== null,
  });
  const row = target !== null && detail.isSuccess ? detail.data.row : null;
  const facts: Fact[] = [];
  if (row !== null) {
    if (row.policy != null && row.policy.length > 0) {
      facts.push({ term: "Policy", value: <span className="mono">{row.policy}</span> });
    }
    if (row.repository != null && row.repository.length > 0) {
      facts.push({ term: "Repository", value: <span className="mono">{row.repository}</span> });
    }
    const at = row.endTime ?? row.startTime;
    if (at != null) {
      facts.push({
        term: "Taken",
        value: (
          <>
            {formatTimestamp(at)} <span className="facts__age">({relativeTime(at)})</span>
          </>
        ),
      });
    }
    if (row.sizeBytes != null) {
      facts.push({ term: "Size", value: humanBytes(row.sizeBytes) });
    }
  }
  return (
    <section className="browse-head" aria-label="Snapshot being browsed">
      <div className="browse-head__pick">
        <SnapshotField
          id="browse-snapshot"
          label="Snapshot"
          placeholder="Choose a snapshot"
          value={value}
          onChange={(next) => {
            const picked = parseSnapshotParam(next);
            if (picked === null) return;
            void navigate({
              to: "/browse",
              search: browseSearch(picked.namespace, picked.name, { scope }),
            });
          }}
        />
        {target !== null ? (
          <InspectLink
            target={{ kind: "snapshot", namespace: target.namespace, name: target.name }}
            className="button"
          >
            <Info size={14} strokeWidth={2} aria-hidden="true" />
            Details
          </InspectLink>
        ) : null}
      </div>
      {facts.length > 0 ? <Facts label="About this snapshot" facts={facts} /> : null}
      <div className="browse-head__body">{children}</div>
    </section>
  );
}

/** A state of the card's lower half that is not a session: a title, a line, maybe a way on. */
function BrowseNote({
  title,
  action,
  children,
}: {
  title: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="browse-session">
      <h2 className="browse-session__title">{title}</h2>
      <p className="browse-session__prose">{children}</p>
      {action !== undefined ? <div className="action-bar">{action}</div> : null}
    </div>
  );
}

interface BrowserProps {
  namespace: string;
  name: string;
  scope: string | undefined;
  search: Record<string, unknown>;
}

/** One snapshot's session and listing — or why it cannot be browsed. */
function Browser({ namespace, name, scope, search }: BrowserProps) {
  const asked = browsePathParam(search.path);
  const path = validateBrowsePath(asked);
  const offset = browseOffsetParam(search.offset);

  // The operator says up front whether there are files to read. Asked first,
  // so a failed or still-running backup never costs a session request.
  const detail = useSnapshot(namespace, name);
  const blocker = detail.isSuccess && !detail.data.browsable ? detail.data.browseBlocker : null;
  const session = useBrowseSession(namespace, name, {
    enabled: detail.isError || (detail.isSuccess && detail.data.browsable),
  });
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

  const card = (body: ReactNode) => (
    <ActiveSnapshot target={{ namespace, name }} value={`${namespace}/${name}`} scope={scope}>
      {body}
    </ActiveSnapshot>
  );

  if (detail.isSuccess && !detail.data.browsable) {
    return card(
      <BrowseNote title={`${name} cannot be browsed`}>
        {blocker ?? "The operator did not say why."}
      </BrowseNote>,
    );
  }

  if (session.isPending) {
    return card(<LoadingState what={`the browse session for ${name}`} rows={3} />);
  }

  // A failure the hook did not fold to `null` — a 403 on the session read, a
  // gateway's 502. The prompt would be a lie here: nothing was asked and
  // refused, the question itself could not be put.
  if (session.isError) {
    return card(
      <ErrorState
        problem={session.error.problem}
        what={`the browse session for ${name}`}
        onRetry={
          classifyBrowseFailure(session.error.problem).state === "tooLarge"
            ? undefined
            : () => void session.refetch()
        }
      />,
    );
  }

  return (
    <>
      {card(<SessionBar namespace={namespace} name={name} session={live} lapsed={lapsed} />)}
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
    </>
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
              to="/browse"
              search={browseSearch(namespace, name, { scope, path: parent.path })}
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
          ? "This directory was empty (or excluded) when the backup ran."
          : `This directory holds ${window.total} entries, but this page starts past the last one. Go back to the first page.`}
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
  const step = (offset: number | null) =>
    offset === null ? undefined : browseSearch(namespace, name, { scope, path, offset });
  const previous = step(window.previousOffset);
  const next = step(window.nextOffset);
  return (
    <nav className="browse-pager" aria-label="Directory pages">
      <p className="browse-pager__window" aria-live="polite">
        {pending ? "Loading…" : `${window.first}–${window.last} of ${window.total}`}
      </p>
      <div className="browse-pager__steps">
        {previous !== undefined ? (
          <Link className="button" to="/browse" search={previous}>
            <ChevronLeft size={14} strokeWidth={2} aria-hidden="true" />
            Previous
          </Link>
        ) : null}
        {next !== undefined ? (
          <Link className="button" to="/browse" search={next}>
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
        the snapshot root: it may not start with <span className="mono">/</span> or contain{" "}
        <span className="mono">..</span>.
      </p>
      <p>
        <Link className="button" to="/browse" search={browseSearch(namespace, name, { scope })}>
          <FolderOpen size={14} strokeWidth={2} aria-hidden="true" />
          Start at the snapshot root
        </Link>
      </p>
    </EmptyState>
  );
}
