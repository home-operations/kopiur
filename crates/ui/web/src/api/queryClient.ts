import { MutationCache, QueryClient } from "@tanstack/react-query";

import { isApiProblemError } from "./client";
import { problemBanner, problemForNetworkFailure } from "./problem";

declare module "@tanstack/react-query" {
  interface Register {
    /** What a mutation was doing, for the global banner: "Suspend policy nightly". */
    mutationMeta: { source?: string };
  }
}

/**
 * The app's `QueryClient`.
 *
 * Every failed mutation is reported to the global problem banner, so a
 * refused action is seen wherever the user is — a dialog may render the same
 * problem inline as well, and that is fine: a failure that changed nothing
 * on the cluster still deserves to be noticed twice rather than never.
 * Queries do not report here; a route renders its own `ErrorState`.
 *
 * Retries are off for reads and writes alike: a problem already says what
 * to do, and retrying a 403 only delays the not-permitted state.
 */
export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: { retry: false, refetchOnWindowFocus: true },
      mutations: { retry: false },
    },
    mutationCache: new MutationCache({
      onError: (error, _variables, _context, mutation) => {
        const source = mutation.meta?.source;
        const problem = isApiProblemError(error)
          ? error.problem
          : // The client only ever throws ApiProblemError; anything else is a
            // bug in a mutationFn, still worth a banner rather than silence.
            problemForNetworkFailure({
              method: "MUTATION",
              path: source ?? "unknown",
              cause: error,
            });
        problemBanner.report(problem, source !== undefined ? { source } : {});
      },
    }),
  });
}
