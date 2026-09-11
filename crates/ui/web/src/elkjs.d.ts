/**
 * `elkjs` ships typings for `elk-api`, `elk.bundled` and `elk-worker`, but
 * none for `elk-worker.min` — and the minified build is the one worth
 * shipping (the readable one is 4.8 MB of generated code).
 *
 * It is imported for its side effect only: loaded inside a Web Worker it
 * installs elk's own `self.onmessage`, which is what makes
 * `topology/layout.worker.ts` an elk worker. Nothing reads a value from it.
 */
declare module "elkjs/lib/elk-worker.min.js" {
  const elkWorker: unknown;
  export default elkWorker;
}
