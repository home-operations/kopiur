/**
 * Running the ELK layout off the main thread.
 *
 * A topology of a few hundred nodes takes ELK tens to hundreds of
 * milliseconds; on the main thread that is a frozen console, and this one is
 * read at 3am. So the engine runs in a dedicated module worker
 * (`layout.worker.ts`) and this module is the only thing that talks to it.
 *
 * It is also what keeps `elkjs` out of the bundle every page load pays for.
 * The engine is imported by the worker and by nothing else, so it is emitted
 * as the worker's own asset; this module is reached only from the lazy
 * topology route, and the entry chunk never learns either exists.
 *
 * Everything about the *shape* of the graph — what goes to the engine and
 * what comes back — is `elk.ts`, pure and unit-tested. This module owns only
 * the transport: one worker per layout, terminated either way, and a failure
 * rendered as a client `Problem` so the board fails the way every other read
 * on this console fails.
 */

import type { ElkNode } from "elkjs/lib/elk-api";
import { useEffect, useState } from "react";

import { CLIENT_PROBLEM_PREFIX } from "../../api/problem";
import type { Problem } from "../../api/types";
import { type LaidOut, fromElkLayout, toElkGraph } from "./elk";
import type { TopologyModel } from "./model";

/** What the main thread sends: one ELK graph to lay out. */
export interface LayoutRequest {
  graph: ElkNode;
}

/** What the worker answers: the laid-out graph, or why it could not. */
export type LayoutResponse = { ok: true; graph: ElkNode } | { ok: false; message: string };

/** How a layout worker is made — swapped in tests, which have no `Worker`. */
export type LayoutWorkerFactory = () => Worker;

/**
 * The real worker. `new URL(…, import.meta.url)` is the form Vite compiles
 * into a separate emitted asset; anything else would inline the engine here.
 */
export const createLayoutWorker: LayoutWorkerFactory = () =>
  new Worker(new URL("./layout.worker.ts", import.meta.url), { type: "module" });

/** A layout in flight: its answer, and the handle that stops it. */
export interface RunningLayout {
  result: Promise<LaidOut>;
  /**
   * Terminate the worker. The promise is then never settled — the caller has
   * already stopped listening, and rejecting it would only raise an unhandled
   * rejection for an answer nobody wants.
   */
  cancel: () => void;
}

function describeCause(cause: unknown): string {
  if (cause instanceof Error) {
    return cause.message;
  }
  if (typeof cause === "string") {
    return cause;
  }
  return "unknown cause";
}

/**
 * Lay one model out on a worker of its own.
 *
 * One worker per run rather than a pool: a layout is rare (a route visit, a
 * 30s refetch), and a fresh worker cannot carry a previous run's state or
 * leave a stale answer to be matched up. It is terminated on success, on
 * failure and on `cancel`.
 */
export function layoutTopology(
  model: TopologyModel,
  factory: LayoutWorkerFactory = createLayoutWorker,
): RunningLayout {
  const graph = toElkGraph(model);
  let started: Worker;
  try {
    started = factory();
  } catch (cause: unknown) {
    return {
      result: Promise.reject(
        new Error(`the layout worker could not start (${describeCause(cause)})`),
      ),
      cancel: () => undefined,
    };
  }
  const worker = started;
  const result = new Promise<LaidOut>((resolve, reject) => {
    worker.onmessage = (event: MessageEvent<LayoutResponse>) => {
      worker.terminate();
      const response = event.data;
      if (!response.ok) {
        reject(new Error(response.message));
        return;
      }
      try {
        resolve(fromElkLayout(response.graph));
      } catch (cause: unknown) {
        reject(new Error(describeCause(cause)));
      }
    };
    worker.onerror = (event: ErrorEvent) => {
      worker.terminate();
      reject(new Error(event.message.length > 0 ? event.message : "the layout worker failed"));
    };
    worker.postMessage({ graph } satisfies LayoutRequest);
  });
  return {
    result,
    cancel: () => {
      worker.terminate();
    },
  };
}

/**
 * A layout failure as a `Problem`, so the board's failure reads like every
 * other failure on this console: what, why, and the next step. The URN is the
 * client's own namespace — this problem never came from the API
 * (`api/problem.ts`).
 */
export function layoutProblem(message: string): Problem {
  const what = "The topology could not be laid out.";
  const why = `The layout worker did not return a usable graph (${message}).`;
  return {
    type: `${CLIENT_PROBLEM_PREFIX}layout`,
    title: "Layout failed",
    status: 0,
    detail: `${what} ${why}`,
    what,
    why,
    fix: "reload the page; if it persists, scope the view to one namespace so there is less to lay out",
    instance: "/topology",
  };
}

/** What the board needs to know about the layout it is waiting for. */
export interface TopologyLayoutState {
  layout: LaidOut | null;
  problem: Problem | null;
  pending: boolean;
}

/** A finished layout, tagged with the model it was computed for. */
interface SettledLayout {
  model: TopologyModel;
  layout: LaidOut | null;
  problem: Problem | null;
}

/**
 * Lay `model` out, re-running whenever it changes and terminating the worker
 * on unmount. Pass a memoized model: a new object every render would start a
 * new worker every render.
 *
 * The answer is stored tagged with the model it belongs to, and "pending" is
 * derived from that tag rather than written by the effect. That is not a
 * style preference: clearing the state on the way in would be a second render
 * per model change, and an untagged answer from the previous model would be
 * drawn for one frame against the new one — plates at positions computed for
 * nodes that are no longer there.
 */
export function useTopologyLayout(model: TopologyModel | null): TopologyLayoutState {
  const [settled, setSettled] = useState<SettledLayout | null>(null);
  useEffect(() => {
    if (model === null) {
      return;
    }
    let listening = true;
    const running = layoutTopology(model);
    running.result.then(
      (layout) => {
        if (listening) {
          setSettled({ model, layout, problem: null });
        }
      },
      (error: unknown) => {
        if (listening) {
          setSettled({ model, layout: null, problem: layoutProblem(describeCause(error)) });
        }
      },
    );
    return () => {
      listening = false;
      running.cancel();
    };
  }, [model]);
  const current = settled !== null && settled.model === model ? settled : null;
  return {
    layout: current?.layout ?? null,
    problem: current?.problem ?? null,
    pending: model !== null && current === null,
  };
}
