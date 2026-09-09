/**
 * One TanStack Query hook per `/api/v1` endpoint, and one mutation per action.
 *
 * Every hook is built on `useApiQuery` / `useApiMutation`, which pin the two
 * things a route must never get wrong: the error type is always
 * `ApiProblemError` (the client throws nothing else, and TanStack swallows
 * aborts), and the query key names the endpoint and *every* parameter that
 * changes its answer.
 *
 * Two contract facts shape this file:
 *
 * - `/me` is **namespace-scoped** (addenda item 17): its capability flags are
 *   the answer for the `?namespace=` it was asked about, so the key carries
 *   the namespace and a namespace switch is a different query — never one
 *   cached answer across all of them.
 * - `/events` is a **per-object feed** (addenda item 22) that requires
 *   `namespace`, `kind` and `name`; it has no global form and the hook's
 *   signature says so.
 *
 * Stale times: topology 30s, lists and details 15s, doctor 60s, the static
 * gate registry 5 minutes.
 */

import {
  type QueryKey,
  useMutation,
  type UseMutationOptions,
  type UseMutationResult,
  useQuery,
  useQueryClient,
  type UseQueryOptions,
  type UseQueryResult,
} from "@tanstack/react-query";

import {
  type ApiInit,
  ApiProblemError,
  apiDelete,
  apiFetch,
  apiPost,
  isApiProblemError,
  withQuery,
} from "./client";
import { isSessionRequired } from "./problem";
import type {
  ActionReceipt,
  DirListing,
  DoctorReportView,
  EventRow,
  GateDescriptor,
  MaintenanceRow,
  MaintenanceRunBody,
  Me,
  Page,
  PolicyDetail,
  PolicyRow,
  ReplicationRunBody,
  ReplicationsView,
  RepositoryDetail,
  RepositoryGraph,
  RepositorySummary,
  RestoreBody,
  RestoreDetail,
  RestoreRow,
  RetentionPlan,
  ScanCatalogBody,
  ScheduleRow,
  SessionCreateBody,
  SessionInfo,
  SnapshotDetail,
  SnapshotNowBody,
  SnapshotRow,
  StatusOverview,
  SuspendBody,
} from "./types";

declare module "@tanstack/react-query" {
  interface Register {
    defaultError: ApiProblemError;
  }
}

/** Stale times per resource class, in milliseconds. */
export const staleTime = {
  topology: 30_000,
  list: 15_000,
  detail: 15_000,
  doctor: 60_000,
  me: 30_000,
  gates: 300_000,
  session: 10_000,
} as const;

const API = "/api/v1";

/** Every path the SPA requests, in one place. */
export const paths = {
  me: `${API}/me`,
  status: `${API}/status`,
  graph: `${API}/graph`,
  repositories: `${API}/repositories`,
  repository: (kindPath: string, name: string) =>
    `${API}/repositories/${encodeURIComponent(kindPath)}/${encodeURIComponent(name)}`,
  repositorySession: (kindPath: string, name: string) =>
    `${paths.repository(kindPath, name)}/session`,
  snapshots: `${API}/snapshots`,
  snapshot: (namespace: string, name: string) =>
    `${API}/snapshots/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
  snapshotRetention: (namespace: string, name: string) =>
    `${paths.snapshot(namespace, name)}/retention`,
  snapshotSession: (namespace: string, name: string) =>
    `${paths.snapshot(namespace, name)}/session`,
  snapshotTree: (namespace: string, name: string) => `${paths.snapshot(namespace, name)}/tree`,
  /**
   * A file download is a top-level navigation (an `<a href>`), never a
   * fetch — the browser streams it. Size limits come from `SessionInfo`, so
   * the table refuses an oversized entry before this URL is ever clicked.
   */
  snapshotFile: (namespace: string, name: string, path: string) =>
    withQuery(`${paths.snapshot(namespace, name)}/file`, { path }),
  policies: `${API}/policies`,
  policy: (namespace: string, name: string) =>
    `${API}/policies/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
  schedules: `${API}/schedules`,
  restores: `${API}/restores`,
  restore: (namespace: string, name: string) =>
    `${API}/restores/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
  maintenance: `${API}/maintenance`,
  replications: `${API}/replications`,
  doctor: `${API}/doctor`,
  gates: `${API}/gates`,
  events: `${API}/events`,
  actions: {
    snapshotNow: `${API}/actions/snapshot-now`,
    restore: `${API}/actions/restore`,
    suspend: `${API}/actions/suspend`,
    maintenanceRun: `${API}/actions/maintenance-run`,
    replicationRun: `${API}/actions/replication-run`,
    scanCatalog: `${API}/actions/scan-catalog`,
  },
} as const;

/** A namespace scope; `undefined` means cluster-wide. */
export type Namespace = string | undefined;

/** `GET /api/v1/snapshots` filters and window — the URL is the state. */
export interface SnapshotListParams {
  namespace?: string | undefined;
  repository?: string | undefined;
  repositoryKind?: string | undefined;
  repositoryNamespace?: string | undefined;
  policy?: string | undefined;
  origin?: string | undefined;
  phase?: string | undefined;
  offset?: number | undefined;
  limit?: number | undefined;
}

/** `GET /api/v1/doctor` controls (addenda item 12: these live here, not on `/status`). */
export interface DoctorParams {
  namespace?: string | undefined;
  stuckThreshold?: number | undefined;
  failureLookback?: number | undefined;
  /**
   * Run only these checks, by `check_id`. Absent runs all ten.
   *
   * A **cost** control, not a display filter: the server skips the work an
   * unasked-for check would have done. A caller that names a subset gets a
   * shorter report, not one with the rest passing — so a screen that asks for
   * less must not present its row count as the whole verdict.
   */
  checks?: readonly string[] | undefined;
}

/** `GET /api/v1/events` — a per-object feed; all three are required. */
export interface EventParams {
  namespace: string;
  kind: string;
  name: string;
}

/** `GET …/tree` window. */
export interface TreeParams {
  path: string;
  offset?: number | undefined;
  limit?: number | undefined;
}

/**
 * Query keys, so a mutation can invalidate exactly the reads it changed.
 * Each key names the endpoint, then the parameters that change the answer.
 */
export const queryKeys = {
  me: (namespace: Namespace) => ["me", { namespace: namespace ?? null }] as const,
  status: (namespace: Namespace) => ["status", { namespace: namespace ?? null }] as const,
  graph: (namespace: Namespace) => ["graph", { namespace: namespace ?? null }] as const,
  repositories: (namespace: Namespace) =>
    ["repositories", { namespace: namespace ?? null }] as const,
  repository: (kindPath: string, name: string, namespace: Namespace) =>
    ["repositories", kindPath, name, { namespace: namespace ?? null }] as const,
  snapshots: (params: SnapshotListParams) => ["snapshots", params] as const,
  snapshot: (namespace: string, name: string) => ["snapshots", namespace, name] as const,
  snapshotRetention: (namespace: string, name: string) =>
    ["snapshots", namespace, name, "retention"] as const,
  snapshotSession: (namespace: string, name: string) =>
    ["snapshots", namespace, name, "session"] as const,
  snapshotTree: (namespace: string, name: string, params: TreeParams) =>
    ["snapshots", namespace, name, "tree", params] as const,
  policies: (namespace: Namespace) => ["policies", { namespace: namespace ?? null }] as const,
  policy: (namespace: string, name: string) => ["policies", namespace, name] as const,
  schedules: (namespace: Namespace) => ["schedules", { namespace: namespace ?? null }] as const,
  restores: (namespace: Namespace) => ["restores", { namespace: namespace ?? null }] as const,
  restore: (namespace: string, name: string) => ["restores", namespace, name] as const,
  maintenance: (namespace: Namespace) => ["maintenance", { namespace: namespace ?? null }] as const,
  replications: (namespace: Namespace) =>
    ["replications", { namespace: namespace ?? null }] as const,
  doctor: (params: DoctorParams) => ["doctor", params] as const,
  gates: () => ["gates"] as const,
  events: (params: EventParams) => ["events", params] as const,
} as const;

/** Options a route may pass to any read hook. */
export interface ReadOptions {
  enabled?: boolean | undefined;
  refetchInterval?: number | false | undefined;
}

/**
 * The read primitive every endpoint hook is built on: a query keyed by
 * `key`, fetched from `path`, cancelled through the client's `AbortSignal`
 * pass-through, typed to fail only with `ApiProblemError`. Retries are off —
 * a problem already says what to do, and retrying a 403 three times only
 * delays the not-permitted state.
 */
export function useApiQuery<T>(
  key: QueryKey,
  path: string,
  options: { staleTime: number } & ReadOptions,
): UseQueryResult<T> {
  const queryOptions: UseQueryOptions<T> = {
    queryKey: key,
    queryFn: ({ signal }) => apiFetch<T>(path, { signal }),
    staleTime: options.staleTime,
    retry: false,
  };
  if (options.enabled !== undefined) {
    queryOptions.enabled = options.enabled;
  }
  if (options.refetchInterval !== undefined) {
    queryOptions.refetchInterval = options.refetchInterval;
  }
  return useQuery(queryOptions);
}

/** What a mutation needs to say about itself. */
export interface ApiMutationOptions<TData, TVariables> {
  mutationFn: (variables: TVariables, init: ApiInit) => Promise<TData>;
  /**
   * The query keys this mutation makes stale. Invalidated on success, *and*
   * on failure — a refused action may still have changed what the user should
   * be looking at (a breaker hold, a suspended parent).
   */
  invalidates: (variables: TVariables) => readonly QueryKey[];
  onSuccess?: ((data: TData, variables: TVariables) => void) | undefined;
}

/**
 * The mutation primitive: runs `mutationFn`, then invalidates the keys it
 * names. The caller receives the result (an `ActionReceipt` for every action;
 * a `SessionInfo` for a session start; nothing for a session stop) through
 * TanStack's `data`, so every dialog can render the receipt — `note`
 * included, which is where a "requested, not done" delete explains itself
 * (addenda item 19).
 */
export function useApiMutation<TData, TVariables>(
  options: ApiMutationOptions<TData, TVariables>,
): UseMutationResult<TData, ApiProblemError, TVariables> {
  const client = useQueryClient();
  const invalidate = (variables: TVariables) =>
    Promise.all(
      options.invalidates(variables).map((queryKey) => client.invalidateQueries({ queryKey })),
    );
  const mutationOptions: UseMutationOptions<TData, ApiProblemError, TVariables> = {
    mutationFn: (variables) => options.mutationFn(variables, {}),
    onSettled: (_data, _error, variables) => invalidate(variables),
  };
  if (options.onSuccess !== undefined) {
    mutationOptions.onSuccess = options.onSuccess;
  }
  return useMutation(mutationOptions);
}

// ---- reads ---------------------------------------------------------------

/**
 * Who am I, and what may I do *in this namespace*. Re-asked per namespace:
 * with no namespace the review is cluster-scoped, and a user holding only a
 * namespaced RoleBinding would see everything disabled.
 */
export function useMe(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<Me>(queryKeys.me(namespace), withQuery(paths.me, { namespace }), {
    staleTime: staleTime.me,
    ...options,
  });
}

export function useStatus(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<StatusOverview>(
    queryKeys.status(namespace),
    withQuery(paths.status, { namespace }),
    { staleTime: staleTime.list, ...options },
  );
}

export function useGraph(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<RepositoryGraph>(
    queryKeys.graph(namespace),
    withQuery(paths.graph, { namespace }),
    { staleTime: staleTime.topology, ...options },
  );
}

export function useRepositories(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<RepositorySummary[]>(
    queryKeys.repositories(namespace),
    withQuery(paths.repositories, { namespace }),
    { staleTime: staleTime.list, ...options },
  );
}

/**
 * `kindPath` is the summary's `kindPath` field (`repository` /
 * `cluster-repository`), never derived from `kind` (addenda item 16). A
 * namespaced repository needs `namespace`; a `ClusterRepository` must not
 * send one.
 */
export function useRepository(
  kindPath: string,
  name: string,
  namespace: Namespace,
  options: ReadOptions = {},
) {
  return useApiQuery<RepositoryDetail>(
    queryKeys.repository(kindPath, name, namespace),
    withQuery(paths.repository(kindPath, name), { namespace }),
    { staleTime: staleTime.detail, ...options },
  );
}

export function useSnapshots(params: SnapshotListParams, options: ReadOptions = {}) {
  return useApiQuery<Page<SnapshotRow>>(
    queryKeys.snapshots(params),
    withQuery(paths.snapshots, { ...params }),
    { staleTime: staleTime.list, ...options },
  );
}

export function useSnapshot(namespace: string, name: string, options: ReadOptions = {}) {
  return useApiQuery<SnapshotDetail>(
    queryKeys.snapshot(namespace, name),
    paths.snapshot(namespace, name),
    { staleTime: staleTime.detail, ...options },
  );
}

/** The bucketed GFS plan (addenda item 21) — not `SnapshotDetail.retentionPreview`. */
export function useSnapshotRetention(namespace: string, name: string, options: ReadOptions = {}) {
  return useApiQuery<RetentionPlan>(
    queryKeys.snapshotRetention(namespace, name),
    paths.snapshotRetention(namespace, name),
    { staleTime: staleTime.detail, ...options },
  );
}

export function usePolicies(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<PolicyRow[]>(
    queryKeys.policies(namespace),
    withQuery(paths.policies, { namespace }),
    { staleTime: staleTime.list, ...options },
  );
}

export function usePolicy(namespace: string, name: string, options: ReadOptions = {}) {
  return useApiQuery<PolicyDetail>(
    queryKeys.policy(namespace, name),
    paths.policy(namespace, name),
    {
      staleTime: staleTime.detail,
      ...options,
    },
  );
}

export function useSchedules(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<ScheduleRow[]>(
    queryKeys.schedules(namespace),
    withQuery(paths.schedules, { namespace }),
    { staleTime: staleTime.list, ...options },
  );
}

export function useRestores(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<RestoreRow[]>(
    queryKeys.restores(namespace),
    withQuery(paths.restores, { namespace }),
    { staleTime: staleTime.list, ...options },
  );
}

export function useRestore(namespace: string, name: string, options: ReadOptions = {}) {
  return useApiQuery<RestoreDetail>(
    queryKeys.restore(namespace, name),
    paths.restore(namespace, name),
    { staleTime: staleTime.detail, ...options },
  );
}

export function useMaintenance(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<MaintenanceRow[]>(
    queryKeys.maintenance(namespace),
    withQuery(paths.maintenance, { namespace }),
    { staleTime: staleTime.list, ...options },
  );
}

export function useReplications(namespace: Namespace, options: ReadOptions = {}) {
  return useApiQuery<ReplicationsView>(
    queryKeys.replications(namespace),
    withQuery(paths.replications, { namespace }),
    { staleTime: staleTime.list, ...options },
  );
}

/**
 * `checks` is comma-separated on the wire, not a repeated key: the handler
 * deserializes with `serde_urlencoded`, which has no sequence support, and
 * the query is `deny_unknown_fields`. The key carries the subset because it
 * changes the answer — a cached full report must never be served to a caller
 * that asked for less, or the other way round.
 */
export function useDoctor(params: DoctorParams, options: ReadOptions = {}) {
  const { checks, ...rest } = params;
  return useApiQuery<DoctorReportView>(
    queryKeys.doctor(params),
    withQuery(paths.doctor, {
      ...rest,
      checks: checks !== undefined ? checks.join(",") : undefined,
    }),
    {
      staleTime: staleTime.doctor,
      ...options,
    },
  );
}

/** The static gate registry; it changes only with a release. */
export function useGates(options: ReadOptions = {}) {
  return useApiQuery<GateDescriptor[]>(queryKeys.gates(), paths.gates, {
    staleTime: staleTime.gates,
    ...options,
  });
}

/** One object's events, for a detail drawer where all three are known. */
export function useEvents(params: EventParams, options: ReadOptions = {}) {
  return useApiQuery<EventRow[]>(queryKeys.events(params), withQuery(paths.events, { ...params }), {
    staleTime: staleTime.detail,
    ...options,
  });
}

/**
 * The browse session for a snapshot, or `null` when none is running.
 *
 * `GET …/session` answers a `session-required` problem (as a 404) when no
 * session exists — that is a state, not an error, so it resolves to `null`
 * and the route renders "start a browse session". The check is on the
 * problem's type, never its status (addenda item 14).
 */
export function useBrowseSession(namespace: string, name: string, options: ReadOptions = {}) {
  const queryOptions: UseQueryOptions<SessionInfo | null> = {
    queryKey: queryKeys.snapshotSession(namespace, name),
    queryFn: async ({ signal }) => {
      try {
        return await apiFetch<SessionInfo>(paths.snapshotSession(namespace, name), { signal });
      } catch (error: unknown) {
        if (isApiProblemError(error) && isSessionRequired(error.problem)) {
          return null;
        }
        throw error;
      }
    },
    staleTime: staleTime.session,
    retry: false,
  };
  if (options.enabled !== undefined) {
    queryOptions.enabled = options.enabled;
  }
  if (options.refetchInterval !== undefined) {
    queryOptions.refetchInterval = options.refetchInterval;
  }
  return useQuery(queryOptions);
}

/** A directory listing through a running session; enable it once one exists. */
export function useSnapshotTree(
  namespace: string,
  name: string,
  params: TreeParams,
  options: ReadOptions = {},
) {
  return useApiQuery<DirListing>(
    queryKeys.snapshotTree(namespace, name, params),
    withQuery(paths.snapshotTree(namespace, name), { ...params }),
    { staleTime: staleTime.session, ...options },
  );
}

// ---- mutations -----------------------------------------------------------

/** `POST /actions/snapshot-now` → 201. `pin` is required and permanent; ask for it. */
export function useSnapshotNow() {
  return useApiMutation<ActionReceipt, SnapshotNowBody>({
    mutationFn: (body, init) => apiPost<ActionReceipt>(paths.actions.snapshotNow, body, init),
    invalidates: (body) => [
      ["snapshots"],
      queryKeys.policy(body.namespace, body.policy),
      queryKeys.status(body.namespace),
      queryKeys.status(undefined),
    ],
  });
}

/** `POST /actions/restore` → 201. The destructive lever is `overwrite`; name it. */
export function useCreateRestore() {
  return useApiMutation<ActionReceipt, RestoreBody>({
    mutationFn: (body, init) => apiPost<ActionReceipt>(paths.actions.restore, body, init),
    invalidates: (body) => [
      ["restores"],
      queryKeys.status(body.namespace),
      queryKeys.status(undefined),
    ],
  });
}

/** `POST /actions/suspend` → 200. Explicit `suspend: true|false`, never a toggle. */
export function useSuspend() {
  return useApiMutation<ActionReceipt, SuspendBody>({
    mutationFn: (body, init) => apiPost<ActionReceipt>(paths.actions.suspend, body, init),
    // Which list a suspend touches depends on `kind`; a kind this bundle does
    // not know still invalidates every candidate rather than none.
    invalidates: () => [
      ["policies"],
      ["schedules"],
      ["repositories"],
      ["replications"],
      ["graph"],
      ["status"],
    ],
  });
}

/** `POST /actions/maintenance-run` → 202. */
export function useMaintenanceRun() {
  return useApiMutation<ActionReceipt, MaintenanceRunBody>({
    mutationFn: (body, init) => apiPost<ActionReceipt>(paths.actions.maintenanceRun, body, init),
    invalidates: () => [["maintenance"], ["status"]],
  });
}

/** `POST /actions/replication-run` → 202. */
export function useReplicationRun() {
  return useApiMutation<ActionReceipt, ReplicationRunBody>({
    mutationFn: (body, init) => apiPost<ActionReceipt>(paths.actions.replicationRun, body, init),
    invalidates: () => [["replications"], ["graph"], ["status"]],
  });
}

/** `POST /actions/scan-catalog` → 202. A `Repository` needs `namespace`. */
export function useScanCatalog() {
  return useApiMutation<ActionReceipt, ScanCatalogBody>({
    mutationFn: (body, init) => apiPost<ActionReceipt>(paths.actions.scanCatalog, body, init),
    invalidates: () => [["repositories"], ["snapshots"], ["status"]],
  });
}

/**
 * `DELETE /snapshots/{ns}/{name}` → 202 with a receipt whose `note` may say
 * the delete is *held* by the repository's deletion breaker. Requested, not
 * done: word the confirmation that way and render the note.
 */
export function useDeleteSnapshot() {
  return useApiMutation<ActionReceipt, { namespace: string; name: string }>({
    mutationFn: ({ namespace, name }, init) =>
      apiDelete<ActionReceipt>(paths.snapshot(namespace, name), init),
    invalidates: ({ namespace, name }) => [
      ["snapshots"],
      queryKeys.snapshot(namespace, name),
      ["status"],
    ],
  });
}

/** `POST /snapshots/{ns}/{name}/session` → 201 with the session. Needs two grants. */
export function useStartSession() {
  return useApiMutation<SessionInfo, { namespace: string; name: string; body: SessionCreateBody }>({
    mutationFn: ({ namespace, name, body }, init) =>
      apiPost<SessionInfo>(paths.snapshotSession(namespace, name), body, init),
    invalidates: ({ namespace, name }) => [queryKeys.snapshotSession(namespace, name)],
  });
}

/** `DELETE /snapshots/{ns}/{name}/session` → 204, no body. */
export function useEndSession() {
  return useApiMutation<undefined, { namespace: string; name: string }>({
    mutationFn: ({ namespace, name }, init) =>
      apiDelete(paths.snapshotSession(namespace, name), init),
    invalidates: ({ namespace, name }) => [
      queryKeys.snapshotSession(namespace, name),
      ["repositories"],
    ],
  });
}

/** `DELETE /repositories/{kind}/{name}/session` → 204, no body. */
export function useEndRepositorySession() {
  return useApiMutation<
    undefined,
    {
      kindPath: string;
      name: string;
      namespace?: string | undefined;
      sessionNamespace?: string | undefined;
    }
  >({
    mutationFn: ({ kindPath, name, namespace, sessionNamespace }, init) =>
      apiDelete(
        withQuery(paths.repositorySession(kindPath, name), { namespace, sessionNamespace }),
        init,
      ),
    invalidates: ({ kindPath, name, namespace }) => [
      queryKeys.repository(kindPath, name, namespace),
      ["snapshots"],
    ],
  });
}
