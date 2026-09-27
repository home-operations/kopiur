import { describe, expect, it } from "vitest";

import { parseGoDuration, renderGoDuration } from "./duration";

describe("parseGoDuration", () => {
  it("accepts the CLI's single-unit grammar and bare seconds", () => {
    // Mirrors `kopiur_api::parse_go_duration`: one suffix of s/m/h, or a bare
    // number of seconds — the same strings `kubectl kopiur doctor
    // --stuck-threshold` takes.
    expect(parseGoDuration("90s")).toBe(90);
    expect(parseGoDuration("30m")).toBe(1800);
    expect(parseGoDuration("1h")).toBe(3600);
    expect(parseGoDuration("24h")).toBe(86400);
    expect(parseGoDuration("3600")).toBe(3600);
    expect(parseGoDuration("  2h ")).toBe(7200);
  });

  it("rejects what the CLI rejects", () => {
    for (const bad of ["", "   ", "1d", "1.5h", "-1h", "h", "1h30m", "abc", "1 h"]) {
      expect(parseGoDuration(bad), bad).toBeNull();
    }
  });

  it("rejects an absurd value rather than overflowing", () => {
    expect(parseGoDuration("99999999999999999999h")).toBeNull();
  });
});

describe("renderGoDuration", () => {
  it("uses the largest unit that divides exactly, round-tripping through parse", () => {
    expect(renderGoDuration(21600)).toBe("6h");
    expect(renderGoDuration(1200)).toBe("20m");
    expect(renderGoDuration(90)).toBe("90s");
    expect(renderGoDuration(3600)).toBe("1h");
    for (const seconds of [1, 60, 61, 3600, 3661, 86400]) {
      expect(parseGoDuration(renderGoDuration(seconds))).toBe(seconds);
    }
  });
});
