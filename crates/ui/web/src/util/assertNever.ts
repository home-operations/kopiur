/**
 * Exhaustiveness helpers for the generated enum unions.
 *
 * ts-rs renders an externally-tagged Rust enum as a union of string literals
 * (unit variants) and single-key objects (struct variants), mixed in one type.
 * The shape `crates/ui-model/src/lib.rs` prescribes:
 *
 * ```ts
 * if (typeof phase === "string") {
 *   switch (phase) { … default: return unknownVariant(phase, "SnapshotPhaseView"); }
 * } else if ("unknown" in phase) {
 *   render(phase.unknown.raw);
 * }
 * ```
 *
 * Two helpers, because compile-time and run-time exhaustiveness are different
 * promises. Both take `never`, so a switch that misses a literal fails to
 * compile — that is the whole point of generating the types. At run time they
 * differ: a newer server can send a string this bundle has never seen, and
 * five enums (`IdentitySource`, `NodeKind`, `EdgeKind`, `OriginView`,
 * `GateSeverityView` — see the module doc) have no fallback variant at all.
 * Rendering paths use `unknownVariant` and keep drawing; only control flow
 * that genuinely cannot continue uses `assertNever`.
 */

/** Compile-time exhaustiveness that also fails loudly at run time. */
export function assertNever(value: never, what = "variant"): never {
  throw new Error(`Unhandled ${what}: ${describe(value)}`);
}

/**
 * Compile-time exhaustiveness that degrades to the raw value at run time — for
 * a `default` arm in a rendering path. Warns once per call so an unexpected
 * variant is visible in the console without taking the page down.
 */
export function unknownVariant(value: never, what = "variant"): string {
  const shown = describe(value);
  console.warn(`kopiur-ui: unrecognised ${what} ${shown}; rendering it as-is`);
  return shown;
}

/**
 * A `{ unknown: { raw } }` / `{ other: { raw } }` fallback renders as its raw
 * string; a plain string as itself; anything else as JSON.
 */
function describe(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "object" && value !== null) {
    const [key, ...rest] = Object.keys(value);
    if (key !== undefined && rest.length === 0) {
      const inner: unknown = (value as Record<string, unknown>)[key];
      if (typeof inner === "object" && inner !== null && "raw" in inner) {
        const { raw } = inner;
        if (typeof raw === "string") {
          return raw;
        }
      }
    }
  }
  try {
    // `JSON.stringify` yields `undefined` for an undefined value or a function.
    const json = JSON.stringify(value) as string | undefined;
    return json ?? String(value);
  } catch {
    return String(value);
  }
}
