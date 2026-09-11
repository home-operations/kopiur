import { describe, expect, it } from "vitest";

import {
  EMPTY_CELL,
  formatTimestamp,
  humanAge,
  humanBytes,
  humanDuration,
  relativeTime,
} from "./format";

describe("humanBytes", () => {
  it("mirrors crates/ops/src/format.rs::human_bytes exactly", () => {
    // Same fact, same rendering, in the CLI and in the browser.
    expect(humanBytes(0)).toBe("0 B");
    expect(humanBytes(512)).toBe("512 B");
    expect(humanBytes(1023)).toBe("1023 B");
    expect(humanBytes(1536)).toBe("1.5 KiB");
    expect(humanBytes(3 * 1024 * 1024)).toBe("3.0 MiB");
    expect(humanBytes(2 * 1024 * 1024 * 1024)).toBe("2.0 GiB");
    expect(humanBytes(5_368_709_120)).toBe("5.0 GiB");
    expect(humanBytes(1024 ** 5 * 3)).toBe("3.0 PiB");
  });

  it("renders an absent size as the empty cell, never as 0 B", () => {
    // `DirEntryView.size` is null when kopia reported no size — not zero.
    expect(humanBytes(null)).toBe(EMPTY_CELL);
    expect(humanBytes(undefined)).toBe(EMPTY_CELL);
  });
});

describe("humanAge", () => {
  it("matches kubectl's AGE column, like crates/cli/src/output.rs::human_age", () => {
    const now = new Date("2026-06-11T12:00:00Z");
    const at = (secs: number) => new Date(now.getTime() - secs * 1000).toISOString();
    expect(humanAge(at(42), now)).toBe("42s");
    expect(humanAge(at(119), now)).toBe("119s");
    expect(humanAge(at(120), now)).toBe("2m");
    expect(humanAge(at(3 * 3600), now)).toBe("3h");
    expect(humanAge(at(40 * 3600), now)).toBe("40h");
    expect(humanAge(at(3 * 86400), now)).toBe("3d");
    expect(humanAge(at(800 * 86400), now)).toBe("2y");
    // A clock-skewed future timestamp clamps to 0s rather than going negative.
    expect(humanAge(new Date(now.getTime() + 30_000).toISOString(), now)).toBe("0s");
  });

  it("renders an absent or unparseable timestamp as the empty cell", () => {
    const now = new Date("2026-06-11T12:00:00Z");
    expect(humanAge(null, now)).toBe(EMPTY_CELL);
    expect(humanAge(undefined, now)).toBe(EMPTY_CELL);
    expect(humanAge("not a date", now)).toBe(EMPTY_CELL);
  });
});

describe("relativeTime", () => {
  const now = new Date("2026-06-11T12:00:00Z");

  it("says ago for the past and in for the future, in kubectl units", () => {
    expect(relativeTime("2026-06-11T11:59:18Z", now)).toBe("42s ago");
    expect(relativeTime("2026-06-11T09:00:00Z", now)).toBe("3h ago");
    expect(relativeTime("2026-06-11T12:05:00Z", now)).toBe("in 5m");
    expect(relativeTime("2026-06-13T12:00:00Z", now)).toBe("in 2d");
  });

  it("calls the present moment now", () => {
    expect(relativeTime("2026-06-11T12:00:00Z", now)).toBe("now");
  });

  it("renders an absent timestamp as the empty cell", () => {
    expect(relativeTime(null, now)).toBe(EMPTY_CELL);
    expect(relativeTime("garbage", now)).toBe(EMPTY_CELL);
  });
});

describe("humanDuration", () => {
  it("uses at most two units, largest first", () => {
    expect(humanDuration(0)).toBe("0s");
    expect(humanDuration(45)).toBe("45s");
    expect(humanDuration(90)).toBe("1m 30s");
    expect(humanDuration(1800)).toBe("30m");
    expect(humanDuration(4332)).toBe("1h 12m");
    expect(humanDuration(7200)).toBe("2h");
    expect(humanDuration(3 * 86400 + 4 * 3600)).toBe("3d 4h");
    expect(humanDuration(3 * 86400)).toBe("3d");
  });

  it("rounds sub-second and fractional values down to whole seconds", () => {
    expect(humanDuration(0.4)).toBe("0s");
    expect(humanDuration(61.9)).toBe("1m 1s");
  });

  it("renders an absent or negative duration as the empty cell", () => {
    expect(humanDuration(null)).toBe(EMPTY_CELL);
    expect(humanDuration(undefined)).toBe(EMPTY_CELL);
    expect(humanDuration(-5)).toBe(EMPTY_CELL);
  });
});

describe("formatTimestamp", () => {
  it("renders a sortable local timestamp to the second", () => {
    const iso = "2026-06-11T12:34:56Z";
    const d = new Date(iso);
    const pad = (n: number) => String(n).padStart(2, "0");
    const expected = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    expect(formatTimestamp(iso)).toBe(expected);
    expect(formatTimestamp(iso)).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
  });

  it("renders an absent or unparseable timestamp as the empty cell", () => {
    expect(formatTimestamp(null)).toBe(EMPTY_CELL);
    expect(formatTimestamp(undefined)).toBe(EMPTY_CELL);
    expect(formatTimestamp("yesterday")).toBe(EMPTY_CELL);
  });
});

describe("EMPTY_CELL", () => {
  it("is the CLI's single placeholder", () => {
    expect(EMPTY_CELL).toBe("-");
  });
});
