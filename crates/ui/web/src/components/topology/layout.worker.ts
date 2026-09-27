/**
 * The layout worker: this module *is* elkjs's worker.
 *
 * It exists for two reasons, and both matter. The engine is ~1.6 MB of
 * generated Java-to-JS, so importing it anywhere the entry chunk can reach
 * would be paid on every page load; and a layered layout of a real cluster's
 * topology is long enough to drop frames, so running it on the main thread
 * would freeze the console while it thinks.
 *
 * There is no hand-written protocol here, and that is deliberate — the first
 * attempt had one and it could not work. `elkjs/lib/elk.bundled.js` is the
 * main-thread build, and its "fake worker" shim (`elk-worker.min.js`)
 * branches on its own context:
 *
 * ```js
 * if (typeof document === "undefined" && typeof self !== "undefined") {
 *   self.onmessage = dispatcher.saveDispatch;   // we are the elk worker
 * } else if (typeof module !== "undefined" && module.exports) {
 *   module.exports = { default: FakeWorker, Worker: FakeWorker };
 * }
 * ```
 *
 * Imported *inside* a Web Worker, the first branch wins: the module installs
 * elk's own `onmessage` and exports nothing, so `new ELK()` there dies on
 * `new undefined(url)`. The engine cannot be driven from inside a worker; it
 * can only *be* one. So this file is the worker elk expects, `layout.ts`
 * holds the small `elk-api` client that talks to it, and the message format
 * is elk's own rather than one of ours to keep in sync.
 */

import "elkjs/lib/elk-worker.min.js";
