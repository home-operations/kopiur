/**
 * Human-readable formatting for table cells and summaries.
 *
 * These mirror the CLI's helpers (`crates/ops/src/format.rs`,
 * `crates/cli/src/output.rs`) so a byte count or an age renders the same in
 * `kubectl kopiur` and in the browser — the same fact told the same way.
 * Everything here is pure and takes `now` explicitly where time is involved.
 */

/**
 * Placeholder for an absent value in a table cell, matching the CLI's
 * `EMPTY_CELL`. Never render an absent size as `0 B` or an absent time as
 * `now`: an absence is a fact of its own.
 */
export const EMPTY_CELL = "-";

const BYTE_UNITS = ["KiB", "MiB", "GiB", "TiB", "PiB"] as const;

/**
 * `512 B`, `1.5 KiB`, `2.0 GiB` — binary units, one decimal, exactly as
 * `ops::format::human_bytes`. `null` (kopia reported no size) is the empty
 * cell, not zero.
 */
export function humanBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || Number.isNaN(bytes)) {
    return EMPTY_CELL;
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  let value = bytes;
  let unit: string = BYTE_UNITS[0];
  for (const candidate of BYTE_UNITS) {
    value /= 1024;
    unit = candidate;
    if (value < 1024) {
      break;
    }
  }
  return `${value.toFixed(1)} ${unit}`;
}

function parseInstant(value: string | Date | null | undefined): Date | null {
  if (value === null || value === undefined) {
    return null;
  }
  const date = value instanceof Date ? value : new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

/**
 * The largest single unit of a non-negative number of seconds, the way
 * `kubectl get`'s AGE column and `cli::output::human_age` do it:
 * `42s`, `119s`, `2m`, `40h`, `3d`, `2y`.
 */
function kubectlUnit(secs: number): string {
  if (secs < 120) {
    return `${secs}s`;
  }
  if (secs < 60 * 60) {
    return `${Math.floor(secs / 60)}m`;
  }
  if (secs < 48 * 60 * 60) {
    return `${Math.floor(secs / 3600)}h`;
  }
  if (secs < 365 * 24 * 60 * 60) {
    return `${Math.floor(secs / 86400)}d`;
  }
  return `${Math.floor(secs / (365 * 24 * 60 * 60))}y`;
}

/**
 * How long ago `then` was, kubectl-style. A future timestamp (clock skew)
 * clamps to `0s` rather than going negative; an absent or unparseable one is
 * the empty cell.
 */
export function humanAge(then: string | Date | null | undefined, now: Date = new Date()): string {
  const date = parseInstant(then);
  if (date === null) {
    return EMPTY_CELL;
  }
  const secs = Math.max(0, Math.floor((now.getTime() - date.getTime()) / 1000));
  return kubectlUnit(secs);
}

/**
 * `42s ago`, `in 5m`, or `now` — an age with a direction, for "last run" and
 * "next run" cells alike.
 */
export function relativeTime(at: string | Date | null | undefined, now: Date = new Date()): string {
  const date = parseInstant(at);
  if (date === null) {
    return EMPTY_CELL;
  }
  const delta = Math.round((date.getTime() - now.getTime()) / 1000);
  if (delta === 0) {
    return "now";
  }
  return delta < 0 ? `${kubectlUnit(-delta)} ago` : `in ${kubectlUnit(delta)}`;
}

/**
 * A duration in at most two units, largest first: `45s`, `1m 30s`, `1h 12m`,
 * `3d 4h`. Fractions of a second are dropped; a negative or absent value is
 * the empty cell.
 */
export function humanDuration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || Number.isNaN(seconds) || seconds < 0) {
    return EMPTY_CELL;
  }
  const total = Math.floor(seconds);
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  const parts: string[] = [];
  if (days > 0) {
    parts.push(`${days}d`);
    if (hours > 0) {
      parts.push(`${hours}h`);
    }
  } else if (hours > 0) {
    parts.push(`${hours}h`);
    if (minutes > 0) {
      parts.push(`${minutes}m`);
    }
  } else if (minutes > 0) {
    parts.push(`${minutes}m`);
    if (secs > 0) {
      parts.push(`${secs}s`);
    }
  } else {
    parts.push(`${secs}s`);
  }
  return parts.join(" ");
}

const pad2 = (n: number): string => String(n).padStart(2, "0");

/**
 * A sortable local timestamp to the second: `2026-06-11 14:34:56`. Local time
 * because the operator reads it against their own clock; the ISO layout
 * because it aligns in a column and never needs a locale to parse.
 */
export function formatTimestamp(at: string | Date | null | undefined): string {
  const date = parseInstant(at);
  if (date === null) {
    return EMPTY_CELL;
  }
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}`;
}
