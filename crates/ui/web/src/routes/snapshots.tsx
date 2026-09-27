import { Link, createFileRoute, useNavigate } from "@tanstack/react-router";
import { Camera, Filter, Sigma } from "lucide-react";
import { type SubmitEvent, useState } from "react";

import { type SnapshotListParams, useSnapshots } from "../api/hooks";
import { problemKind } from "../api/problem";
import { SnapshotSizeChart } from "../charts/SnapshotSizeChart";
import { ActionButton } from "../components/ActionButton";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { Finding } from "../components/Finding";
import { LoadingState } from "../components/LoadingState";
import { SnapshotTable } from "../components/SnapshotTable";
import { healthLamp } from "../components/health";
import {
  ORIGIN_FILTERS,
  PHASE_FILTERS,
  isOriginFilter,
  isPhaseFilter,
} from "../components/snapshot";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Snapshots — every `Snapshot` resource the caller may see, filtered by the
 * URL.
 *
 * **The URL is the state.** All seven filters and the page window live in the
 * address bar, so a filtered view is a link an operator can paste into an
 * incident channel and a colleague sees the same rows. Nothing is held in
 * component state except the unsubmitted contents of the form.
 *
 * **A value the server would refuse is kept and named, never silently
 * dropped.** `?phase=Succeeded` (wrong case) or `?origin=cron` would be a 400,
 * so it is not sent — but it stays in the URL and the page says which value it
 * ignored and what the accepted ones are. Dropping it would run an unfiltered
 * list under a heading that claimed to be filtered, which is the same class of
 * mistake the doctor route's windows made. The vocabularies are the *server's*
 * (`components/snapshot.ts`), so an accepted value cannot be spelled wrong
 * here either.
 *
 * **An over-cap answer is not an error.** `/snapshots` refuses rather than
 * truncating — silently returning the first N rows of a backup list reads as
 * "these are all my backups" — so the 422 renders as "narrow the filter", with
 * the server's own count, cap and remedy, and the filter bar still on the page
 * to narrow with.
 */
export interface SnapshotsSearch {
  repository?: string;
  repositoryKind?: string;
  repositoryNamespace?: string;
  policy?: string;
  origin?: string;
  phase?: string;
  offset?: number;
  limit?: number;
}

/** The page size when the URL does not say — the server's own default. */
const DEFAULT_LIMIT = 50;

/**
 * One text filter as the reader wrote it — **total over `unknown`**.
 *
 * The router hands the component the raw parsed value for a key
 * `validateSearch` dropped, so the declared `string` is not something render
 * may rely on: `?policy=3` arrives as the number `3`, and calling `.trim()` on
 * it would throw inside render and take the route to its error boundary. A
 * number or boolean is rendered back as written, so a message can name the
 * reader's own value.
 */
function textParam(value: unknown): string | undefined {
  if (typeof value === "string") {
    const text = value.trim();
    return text.length > 0 ? text : undefined;
  }
  return typeof value === "number" || typeof value === "boolean" ? String(value) : undefined;
}

/** A non-negative whole number, or `undefined` for anything else. */
function countParam(value: unknown): number | undefined {
  const raw = typeof value === "string" ? Number(value) : value;
  if (typeof raw !== "number" || !Number.isFinite(raw) || raw < 0) {
    return undefined;
  }
  return Math.floor(raw);
}

/** The two `?repositoryKind=` tokens `RepositoryKindPath::parse` takes. */
const KIND_FILTERS = [
  { value: "repository", label: "Repository (namespaced)" },
  { value: "cluster-repository", label: "ClusterRepository (cluster-scoped)" },
] as const;

function isKindFilter(value: unknown): boolean {
  return typeof value === "string" && KIND_FILTERS.some((kind) => kind.value === value);
}

export const Route = createFileRoute("/snapshots")({
  validateSearch: (search: Record<string, unknown>): SnapshotsSearch => {
    const out: SnapshotsSearch = {};
    for (const key of [
      "repository",
      "repositoryKind",
      "repositoryNamespace",
      "policy",
      "origin",
      "phase",
    ] as const) {
      const value = textParam(search[key]);
      if (value !== undefined) {
        out[key] = value;
      }
    }
    const offset = countParam(search.offset);
    if (offset !== undefined) {
      out.offset = offset;
    }
    const limit = countParam(search.limit);
    if (limit !== undefined) {
      out.limit = limit;
    }
    return out;
  },
  component: Snapshots,
});

/** A filter the URL asked for that the request could not carry. */
interface IgnoredFilter {
  key: string;
  value: string;
  accepted: string;
}

function Snapshots() {
  const namespace = useCurrentNamespace();
  // Re-normalised here, not just in `validateSearch`: the router hands render
  // the raw parsed value for a key the validator dropped.
  const search: Record<string, unknown> = Route.useSearch();
  const asked = {
    repository: textParam(search.repository),
    repositoryKind: textParam(search.repositoryKind),
    repositoryNamespace: textParam(search.repositoryNamespace),
    policy: textParam(search.policy),
    origin: textParam(search.origin),
    phase: textParam(search.phase),
  };
  const offset = countParam(search.offset) ?? 0;
  const limit = countParam(search.limit) ?? DEFAULT_LIMIT;

  const ignored: IgnoredFilter[] = [];
  const usable = (key: "origin" | "phase" | "repositoryKind"): string | undefined => {
    const value = asked[key];
    if (value === undefined) {
      return undefined;
    }
    const accepts =
      key === "origin" ? isOriginFilter : key === "phase" ? isPhaseFilter : isKindFilter;
    if (accepts(value)) {
      return value;
    }
    const vocabulary =
      key === "origin"
        ? ORIGIN_FILTERS.map((f) => f.value)
        : key === "phase"
          ? PHASE_FILTERS.map((f) => f.value)
          : KIND_FILTERS.map((f) => f.value);
    ignored.push({ key, value, accepted: vocabulary.join(", ") });
    return undefined;
  };

  const params: SnapshotListParams = {
    namespace,
    repository: asked.repository,
    repositoryKind: usable("repositoryKind"),
    repositoryNamespace: asked.repositoryNamespace,
    policy: asked.policy,
    origin: usable("origin"),
    phase: usable("phase"),
    offset: offset > 0 ? offset : undefined,
    limit,
  };

  const snapshots = useSnapshots(params);
  const scope = namespace ?? "all namespaces";
  // Read defensively rather than trusting the body's shape. The declared
  // `Page<SnapshotRow>` is what a healthy kopiur-ui sends; a proxy, a sign-in
  // page or a version skew can put something else on the wire, and a route that
  // indexed into it would take the whole page to the error boundary — a blank
  // screen where an empty state belongs.
  const page = snapshots.data;
  const items = page?.items ?? [];
  const total = page?.total ?? items.length;
  const serverWindow = { offset: page?.offset ?? 0, limit: page?.limit ?? limit };
  const overCap =
    snapshots.isError && problemKind(snapshots.error.problem) === "list-too-large"
      ? snapshots.error.problem
      : undefined;

  return (
    <div className="page">
      <p className="page__prose">
        A <span className="mono">Snapshot</span> is one backup run: the invocation, separate from
        the <span className="mono">SnapshotPolicy</span> recipe that produced it and from the{" "}
        <span className="mono">SnapshotSchedule</span> that fired it. Every filter here is in the
        address bar, so a filtered view is a link. The list is capped rather than truncated — a
        backup table that quietly dropped rows would read as &ldquo;these are all my backups&rdquo;
        — so a filter that selects too many is refused with the number it matched.
      </p>

      <section className="page__section" aria-label="Filters">
        <Filters search={asked} namespace={namespace} limit={limit} key={JSON.stringify(search)} />
      </section>

      {ignored.map((filter) => (
        <p className="page__prose" role="status" key={filter.key}>
          This link asked for <span className="mono">{filter.key}</span> ={" "}
          <span className="mono">{filter.value}</span>, which the operator would reject, so it was
          not sent and nothing was filtered by it. The accepted values are{" "}
          <span className="mono">{filter.accepted}</span>.
        </p>
      ))}

      <section className="page__section" aria-label="Snapshots">
        {snapshots.isPending ? (
          <LoadingState what="snapshots" rows={8} />
        ) : overCap !== undefined ? (
          <div className="state">
            <h2 className="state__title">
              <Filter size={18} strokeWidth={1.75} aria-hidden="true" />
              <span>Too many snapshots to list — narrow the filter</span>
            </h2>
            <Finding
              what={overCap.what}
              why={overCap.why}
              fix={overCap.fix}
              lamp={healthLamp("degraded")}
            />
            <p className="state__body">
              The filter bar above is still live. Narrowing by repository or policy is what turns
              this into a list; nothing is wrong with the cluster.
            </p>
          </div>
        ) : snapshots.isError ? (
          <ErrorState
            problem={snapshots.error.problem}
            what="snapshots"
            onRetry={() => void snapshots.refetch()}
          />
        ) : items.length === 0 && total > 0 ? (
          <EmptyState title="This page is past the end of the list" icon={Camera}>
            The filter matches {total} {total === 1 ? "snapshot" : "snapshots"}, but the window this
            link asks for starts after the last of them — a bookmark taken before retention pruned
            rows looks exactly like this.
            <div className="state__actions">
              <Link className="button" to="/snapshots" search={firstPage(asked, namespace)}>
                Back to the first page
              </Link>
            </div>
          </EmptyState>
        ) : items.length === 0 ? (
          <EmptyState title={`No snapshots in ${scope}`} icon={Camera}>
            A snapshot appears here when a <span className="mono">SnapshotSchedule</span> fires,
            when someone asks for one from a policy, or when a catalog scan discovers a backup
            already in the repository. If you expected rows, check the filters above — they are in
            the URL and survive a reload.
          </EmptyState>
        ) : (
          <>
            <SnapshotTable rows={items} />
            <Pager
              total={total}
              offset={serverWindow.offset}
              limit={serverWindow.limit}
              count={items.length}
              search={asked}
              namespace={namespace}
            />
          </>
        )}
      </section>

      {items.length > 0 ? (
        <section className="page__section" aria-label="Snapshot size over time">
          <div className="page__section-head">
            <h2>
              <Sigma size={16} strokeWidth={2} aria-hidden="true" />
              Size over time
            </h2>
          </div>
          <p className="page__section-note">
            Drawn from the rows on this page only, so it moves with the filter and the page window
            rather than summarising the whole cluster.
          </p>
          <SnapshotSizeChart rows={items} />
        </section>
      ) : null}
    </div>
  );
}

/** The search a link back to page one carries: every filter, and no window. */
function firstPage(
  search: Record<string, string | undefined>,
  namespace: string | undefined,
): Record<string, string> {
  const base: Record<string, string> = {};
  for (const [key, value] of Object.entries(search)) {
    if (value !== undefined) {
      base[key] = value;
    }
  }
  if (namespace !== undefined) {
    base.namespace = namespace;
  }
  return base;
}

interface PagerProps {
  total: number;
  offset: number;
  limit: number;
  count: number;
  search: Record<string, string | undefined>;
  namespace: string | undefined;
}

/**
 * "Showing 21–40 of 137", with the two moves either side.
 *
 * The window is the server's own — `Page` carries back the `offset` and `limit`
 * that produced it — so the sentence cannot drift from the rows beneath it even
 * when the server clamped what was asked for.
 *
 * Back to the first page drops the key rather than spelling `offset=0`: the
 * default is the absence, and one view must not have two URLs.
 */
function Pager({ total, offset, limit, count, search, namespace }: PagerProps) {
  const first = offset + 1;
  const last = offset + count;
  const prev = Math.max(0, offset - limit);
  const next = offset + limit;
  const base: Record<string, string | number> = firstPage(search, namespace);
  if (limit !== DEFAULT_LIMIT) {
    base.limit = limit;
  }

  return (
    <div className="pager">
      <span className="pager__window" role="status">
        Showing {first}–{last} of {total}
      </span>
      {offset > 0 ? (
        <Link
          className="button"
          to="/snapshots"
          search={prev > 0 ? { ...base, offset: prev } : base}
        >
          Previous
        </Link>
      ) : null}
      {next < total ? (
        <Link className="button" to="/snapshots" search={{ ...base, offset: next }}>
          Next
        </Link>
      ) : null}
    </div>
  );
}

interface FiltersProps {
  search: Record<string, string | undefined>;
  namespace: string | undefined;
  limit: number;
}

/**
 * The seven filters, submitted into the URL.
 *
 * Submitting always resets `offset`: a page-3 window over the old filter is
 * meaningless under a new one, and leaving it would show an empty page that
 * looks like "no such snapshots".
 */
function Filters({ search, namespace, limit }: FiltersProps) {
  const navigate = useNavigate();
  const [ns, setNs] = useState(namespace ?? "");
  const [repository, setRepository] = useState(search.repository ?? "");
  const [repositoryKind, setRepositoryKind] = useState(search.repositoryKind ?? "");
  const [repositoryNamespace, setRepositoryNamespace] = useState(search.repositoryNamespace ?? "");
  const [policy, setPolicy] = useState(search.policy ?? "");
  const [origin, setOrigin] = useState(search.origin ?? "");
  const [phase, setPhase] = useState(search.phase ?? "");

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    const next: Record<string, string | number> = {};
    const put = (key: string, value: string) => {
      const text = value.trim();
      if (text.length > 0) {
        next[key] = text;
      }
    };
    put("namespace", ns);
    put("repository", repository);
    put("repositoryKind", repositoryKind);
    put("repositoryNamespace", repositoryNamespace);
    put("policy", policy);
    put("origin", origin);
    put("phase", phase);
    if (limit !== DEFAULT_LIMIT) {
      next.limit = limit;
    }
    void navigate({ to: "/snapshots", search: next });
  };

  return (
    <form className="controls snapshot-filters" onSubmit={submit} aria-label="Snapshot filters">
      <div className="controls__field">
        <label htmlFor="snapshots-namespace">Namespace</label>
        <input
          id="snapshots-namespace"
          className="controls__input"
          name="namespace"
          value={ns}
          onChange={(event) => {
            setNs(event.target.value);
          }}
          placeholder="all namespaces"
          autoComplete="off"
          spellCheck={false}
        />
      </div>
      <div className="controls__field">
        <label htmlFor="snapshots-policy">Policy</label>
        <input
          id="snapshots-policy"
          className="controls__input"
          name="policy"
          value={policy}
          onChange={(event) => {
            setPolicy(event.target.value);
          }}
          placeholder="any policy"
          autoComplete="off"
          spellCheck={false}
        />
      </div>
      <div className="controls__field">
        <label htmlFor="snapshots-repository">Repository</label>
        <input
          id="snapshots-repository"
          className="controls__input"
          name="repository"
          value={repository}
          onChange={(event) => {
            setRepository(event.target.value);
          }}
          placeholder="any repository"
          autoComplete="off"
          spellCheck={false}
        />
      </div>
      <div className="controls__field">
        <label htmlFor="snapshots-repository-kind">Repository kind</label>
        <select
          id="snapshots-repository-kind"
          className="controls__input"
          name="repositoryKind"
          value={repositoryKind}
          onChange={(event) => {
            setRepositoryKind(event.target.value);
          }}
        >
          <option value="">Repository (default)</option>
          {KIND_FILTERS.map((kind) => (
            <option value={kind.value} key={kind.value}>
              {kind.label}
            </option>
          ))}
        </select>
      </div>
      <div className="controls__field">
        <label htmlFor="snapshots-repository-namespace">Repository namespace</label>
        <input
          id="snapshots-repository-namespace"
          className="controls__input"
          name="repositoryNamespace"
          value={repositoryNamespace}
          onChange={(event) => {
            setRepositoryNamespace(event.target.value);
          }}
          placeholder="the listing's namespace"
          aria-describedby="snapshots-repository-namespace-hint"
          autoComplete="off"
          spellCheck={false}
        />
        <span className="controls__hint" id="snapshots-repository-namespace-hint">
          where a namespaced Repository lives; ignored for a ClusterRepository
        </span>
      </div>
      <div className="controls__field">
        <label htmlFor="snapshots-origin">Origin</label>
        <select
          id="snapshots-origin"
          className="controls__input"
          name="origin"
          value={origin}
          onChange={(event) => {
            setOrigin(event.target.value);
          }}
        >
          <option value="">any origin</option>
          {ORIGIN_FILTERS.map((option) => (
            <option value={option.value} key={option.value}>
              {option.label}
            </option>
          ))}
        </select>
      </div>
      <div className="controls__field">
        <label htmlFor="snapshots-phase">Phase</label>
        <select
          id="snapshots-phase"
          className="controls__input"
          name="phase"
          value={phase}
          onChange={(event) => {
            setPhase(event.target.value);
          }}
        >
          <option value="">any phase</option>
          {PHASE_FILTERS.map((option) => (
            <option value={option.value} key={option.value}>
              {option.label}
            </option>
          ))}
        </select>
      </div>
      <div className="controls__actions">
        <ActionButton variant="primary" type="submit">
          <Filter size={14} strokeWidth={2} aria-hidden="true" />
          Apply filters
        </ActionButton>
        <Link className="button button--quiet" to="/snapshots" search={{}}>
          Clear
        </Link>
      </div>
    </form>
  );
}
