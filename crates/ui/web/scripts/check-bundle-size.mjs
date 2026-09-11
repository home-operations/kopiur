// Bundle-size budget, run by `pnpm build` after `vite build`.
//
// The budget was set when the shell landed and the numbers were a measured
// framework floor rather than an application: entry chunk 332,763 B
// (React 19 + react-dom, TanStack Router + Query, the client, the tokens and
// the shell), ten placeholder route chunks of well under 1 kB. Setting it then
// was the only time it was cheap. Raise a number here deliberately, in a
// commit that says why — never because the build went red.
//
// Two ceilings:
//   ENTRY_MAX  — the eager chunk every page load pays for. Route code must be
//                lazy (autoCodeSplitting), so this should grow only with shared
//                components and the client.
//   CHUNK_MAX  — any single lazy chunk. elkjs (topology's layout engine) is
//                the one known heavy dependency and has its own manualChunks
//                entry; ~1.4 MB raw is what it weighs.
import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const ENTRY_MAX = 480_000; // bytes, raw (about 150 kB gzipped)
const CHUNK_MAX = 1_600_000; // bytes, raw

const assets = join(process.cwd(), "dist", "assets");
let files;
try {
  files = readdirSync(assets).filter((name) => name.endsWith(".js"));
} catch (error) {
  console.error(`check-bundle-size: cannot read ${assets}: ${error.message}`);
  process.exit(2);
}
if (files.length === 0) {
  console.error("check-bundle-size: no JS chunks in dist/assets — did vite build run?");
  process.exit(2);
}

const sizes = files.map((name) => ({ name, bytes: statSync(join(assets, name)).size }));
sizes.sort((a, b) => b.bytes - a.bytes);

// The entry is the chunk index.html loads eagerly: the largest `index-*.js`.
const entry = sizes.find((f) => f.name.startsWith("index-"));
const format = (n) => `${n.toLocaleString("en-US")} B`;
let failed = false;

if (entry === undefined) {
  console.error("check-bundle-size: no index-*.js entry chunk found");
  failed = true;
} else if (entry.bytes > ENTRY_MAX) {
  console.error(
    `check-bundle-size: entry chunk ${entry.name} is ${format(entry.bytes)}, over the ${format(ENTRY_MAX)} budget. ` +
      "Something eager should be lazy, or the budget needs a deliberate raise (see scripts/check-bundle-size.mjs).",
  );
  failed = true;
}
for (const chunk of sizes) {
  if (chunk.bytes > CHUNK_MAX) {
    console.error(
      `check-bundle-size: chunk ${chunk.name} is ${format(chunk.bytes)}, over the ${format(CHUNK_MAX)} per-chunk budget.`,
    );
    failed = true;
  }
}

const total = sizes.reduce((sum, f) => sum + f.bytes, 0);
console.log(
  `check-bundle-size: entry ${entry ? format(entry.bytes) : "?"} (budget ${format(ENTRY_MAX)}), ` +
    `${sizes.length} chunks, ${format(total)} JS total — ${failed ? "OVER BUDGET" : "ok"}`,
);
process.exit(failed ? 1 : 0);
