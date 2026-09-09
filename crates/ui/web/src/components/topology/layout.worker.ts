/**
 * The layout worker: the only module in this bundle that imports `elkjs`.
 *
 * It exists for two reasons, and both matter. The engine is ~1.6 MB of
 * generated Java-to-JS, so importing it anywhere the entry chunk can reach
 * would be paid on every page load; and a layered layout of a real cluster's
 * topology is long enough to drop frames, so running it on the main thread
 * would freeze the console while it thinks.
 *
 * The protocol is one message in, one message out — `LayoutRequest` /
 * `LayoutResponse` in `layout.ts`. A failure is answered, never thrown away:
 * an engine that refuses a graph must reach the operator as a stated reason,
 * not as an empty panel.
 */

import ELK from "elkjs/lib/elk.bundled.js";

import type { LayoutRequest, LayoutResponse } from "./layout";

/**
 * The worker global, typed for the two calls this file makes.
 *
 * `tsconfig.json` gives every file the DOM lib (the app is a browser app),
 * and in the DOM lib `self` is a `Window`, whose `postMessage` demands a
 * target origin. Pulling in `lib.webworker` for one file would redeclare half
 * the DOM. One narrow, documented assertion is the smaller lie.
 */
const scope = globalThis as unknown as {
  addEventListener: (
    type: "message",
    listener: (event: MessageEvent<LayoutRequest>) => void,
  ) => void;
  postMessage: (message: LayoutResponse) => void;
};

const elk = new ELK();

scope.addEventListener("message", (event) => {
  elk.layout(event.data.graph).then(
    (graph) => {
      scope.postMessage({ ok: true, graph });
    },
    (cause: unknown) => {
      scope.postMessage({
        ok: false,
        message: cause instanceof Error ? cause.message : String(cause),
      });
    },
  );
});
