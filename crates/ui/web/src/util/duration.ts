/**
 * The CLI's duration grammar, so the doctor controls take the same strings
 * `kubectl kopiur doctor --stuck-threshold 1h` takes.
 *
 * Mirrors `kopiur_api::parse_go_duration` / `render_go_duration`
 * (`crates/api/src/duration.rs`): one unit suffix of `s`, `m` or `h`, or a
 * bare number of seconds. Nothing else — not `1d`, not `1h30m`, not a
 * fraction — because the server would not accept it either and a control
 * that accepts more than the server is a control that lies.
 */

const UNITS: Readonly<Record<string, number>> = { h: 3600, m: 60, s: 1 };

/**
 * Seconds for a duration string, or `null` when it is not one. A value the
 * server would refuse as absurd (past `Number.MAX_SAFE_INTEGER`) is `null`
 * too, rather than a garbage number.
 */
export function parseGoDuration(input: string): number | null {
  const text = input.trim();
  if (text.length === 0) {
    return null;
  }
  const suffix = text.slice(-1);
  const multiplier = UNITS[suffix];
  const digits = multiplier === undefined ? text : text.slice(0, -1);
  if (!/^\d+$/.test(digits)) {
    return null;
  }
  const seconds = Number(digits) * (multiplier ?? 1);
  return Number.isSafeInteger(seconds) ? seconds : null;
}

/**
 * The largest unit that divides `seconds` exactly: `21600` → `6h`, `1200` →
 * `20m`, `90` → `90s`. Round-trips through `parseGoDuration` by construction.
 */
export function renderGoDuration(seconds: number): string {
  if (seconds > 0 && seconds % 3600 === 0) {
    return `${seconds / 3600}h`;
  }
  if (seconds > 0 && seconds % 60 === 0) {
    return `${seconds / 60}m`;
  }
  return `${seconds}s`;
}
