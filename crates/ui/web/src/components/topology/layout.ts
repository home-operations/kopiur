/**
 * Running the ELK layout off the main thread.
 *
 * A topology of a few hundred nodes takes ELK tens to hundreds of
 * milliseconds; on the main thread that is a frozen console, and this one is
 * read at 3am. So the engine runs in a dedicated module worker
 * (`layout.worker.ts`) and this module is the small client that talks to it.
 *
 * It is also what keeps the engine out of the bundle every page load pays
 * for. What lands here is `elk-api` — about 10 kB of client — and a `new
 * Worker(new URL(…))`, the form Vite compiles into a separate emitted asset.
 * The 1.4 MB of generated layout code is only ever in that asset, and this
 * module is reached only from the lazy topology route.
 *
 * Everything about the *shape* of the graph — what goes to the engine and
 * what comes back — is `elk.ts`, pure and unit-tested. This module owns only
 * the transport: one engine per layout, its worker terminated either way, and
 * a failure rendered as a client `Problem` so the board fails the way every
 * other read on this console fails.
 */

import type { ELK, ElkNode } from "elkjs/lib/elk-api";
import ElkConstructor from "elkjs/lib/elk-api.js";
import { useEffect, useState } from "react";

import { CLIENT_PROBLEM_PREFIX } from "../../api/problem";
import type { Problem } from "../../api/types";
import { type LaidOut, fromElkLayout, toElkGraph } from "./elk";
import type { TopologyModel } from "./model";

/** The engine's surface this module uses; the rest of `ELK` is never called. */
export type LayoutEngine = Pick<ELK, "layout" | "terminateWorker">;

/** How an engine is made — swapped in tests, which have no `Worker`. */
export type LayoutEngineFactory = () => LayoutEngine;

/**
 * The real engine, driving `layout.worker.ts`.
 *
 * `workerFactory` rather than `workerUrl` because the URL elk would build for
 * itself is not the hashed asset path Vite emits; handing it the constructed
 * `Worker` is the only form that survives a production build. Constructing an
 * `ELK` starts the worker, so this is called per layout, not at import.
 */
export const createLayoutEngine: LayoutEngineFactory = () =>
  new ElkConstructor({
    workerFactory: () =>
      new Worker(new URL("./layout.worker.ts", import.meta.url), { type: "module" }),
  });

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
 * Lay one model out on an engine of its own.
 *
 * One engine per run rather than a pool: a layout is rare (a route visit, a
 * 30s refetch), and a fresh worker cannot carry a previous run's state or
 * leave a stale answer to be matched up. It is terminated on success, on
 * failure and on `cancel`.
 */
export function layoutTopology(
  model: TopologyModel,
  factory: LayoutEngineFactory = createLayoutEngine,
): RunningLayout {
  const graph = toElkGraph(model);
  let started: LayoutEngine;
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
  const engine = started;
  const result = engine.layout(graph).then(
    (laid: ElkNode) => {
      engine.terminateWorker();
      return fromElkLayout(laid);
    },
    (cause: unknown) => {
      engine.terminateWorker();
      throw new Error(describeCause(cause));
    },
  );
  return {
    result,
    cancel: () => {
      engine.terminateWorker();
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
