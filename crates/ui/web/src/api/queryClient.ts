import { MutationCache, QueryClient } from "@tanstack/react-query";

import { toasts } from "../components/toast/toasts";
import { isApiProblemError } from "./client";
import { problemForNetworkFailure } from "./problem";
import type { ActionReceipt } from "./types";

declare module "@tanstack/react-query" {
  interface Register {
    mutationMeta: {
      /** The action, worded for its toast: "Suspend schedule media/nightly". */
      describe?: (variables: unknown) => string;
      /**
       * Which answers toast. `receipt` (the default): both. `errors`: only a
       * refusal — for a mutation whose success already shows where it was
       * asked, such as a browse session starting.
       */
      announce?: "receipt" | "errors";
    };
  }
}

/** What a mutation answered, if it answered with a receipt. */
function isReceipt(data: unknown): data is ActionReceipt {
  return (
    typeof data === "object" &&
    data !== null &&
    "kind" in data &&
    "created" in data &&
    Array.isArray((data as { created: unknown }).created)
  );
}

/**
 * The app's `QueryClient`.
 *
 * Every mutation is answered here, as a toast, rather than beside the button
 * that asked: a receipt when it was accepted, the problem when it was
 * refused. Doing it in the cache means the answer arrives even when the
 * drawer or row that asked has gone, and no action surface can forget to
 * show one. Queries do not toast; a route renders its own `ErrorState`.
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
      onSuccess: (data, variables, _context, mutation) => {
        const meta = mutation.meta;
        if (meta?.announce === "errors" || !isReceipt(data)) return;
        toasts.push({
          kind: "receipt",
          label: meta?.describe?.(variables) ?? "The action",
          receipt: data,
        });
      },
      onError: (error, variables, _context, mutation) => {
        const label = mutation.meta?.describe?.(variables) ?? "The action";
        const problem = isApiProblemError(error)
          ? error.problem
          : // The client only ever throws ApiProblemError; anything else is a
            // bug in a mutationFn, still worth saying rather than silence.
            problemForNetworkFailure({ method: "MUTATION", path: label, cause: error });
        toasts.push({ kind: "problem", label, problem });
      },
    }),
  });
}
