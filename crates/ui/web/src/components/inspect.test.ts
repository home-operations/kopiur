import { describe, expect, it } from "vitest";

import type { ObjectKind } from "../api/types";
import { Route as RootRoute } from "../routes/__root";
import { KIND_META } from "./kind";
import {
  inspectPushEntry,
  inspectStackParam,
  inspectStepsBack,
  inspectToken,
  parseInspect,
  parseInspectStack,
  pushInspect,
} from "./inspect";

const KINDS = Object.keys(KIND_META) as ObjectKind[];

describe("inspect token", () => {
  it("round-trips every kind", () => {
    for (const kind of KINDS) {
      const target =
        kind === "clusterRepository"
          ? { kind, name: "shared" }
          : { kind, name: "app-data", namespace: "kopiur-dev" };
      expect(parseInspect(inspectToken(target)), kind).toEqual(target);
    }
  });

  it("names the kind by its slug and drops the namespace for the cluster-scoped kind", () => {
    expect(
      inspectToken({ kind: "snapshotPolicy", namespace: "kopiur-dev", name: "app-data" }),
    ).toBe("snapshot-policy/kopiur-dev/app-data");
    expect(inspectToken({ kind: "clusterRepository", name: "shared" })).toBe(
      "cluster-repository/shared",
    );
  });

  it("refuses anything it cannot name exactly", () => {
    for (const raw of [
      "bogus/x/y",
      "snapshot/x",
      "snapshot//y",
      "snapshot/x/",
      "cluster-repository/a/b",
      "snapshot/a/b/c",
      "",
      42,
      null,
      undefined,
    ]) {
      expect(parseInspect(raw), String(raw)).toBeNull();
    }
  });

  it("is kept by the root route when it parses and dropped when it does not", () => {
    const validate = RootRoute.options.validateSearch as (s: Record<string, unknown>) => unknown;
    expect(validate({ inspect: "snapshot/kopiur-dev/a" })).toEqual({
      inspect: "snapshot/kopiur-dev/a",
    });
    expect(validate({ inspect: "nonsense", namespace: "media" })).toEqual({ namespace: "media" });
  });
});

describe("inspect stack", () => {
  const nas = { kind: "repository" as const, namespace: "media", name: "nas" };
  const photos = { kind: "snapshotPolicy" as const, namespace: "media", name: "photos" };
  const shared = { kind: "clusterRepository" as const, name: "shared" };

  it("reads the drawers bottom first, and writes them back the same", () => {
    const raw = "repository/media/nas,snapshot-policy/media/photos,cluster-repository/shared";
    expect(parseInspectStack(raw)).toEqual([nas, photos, shared]);
    expect(inspectStackParam([nas, photos, shared])).toBe(raw);
  });

  it("reads one token as a stack of one", () => {
    expect(parseInspectStack("repository/media/nas")).toEqual([nas]);
  });

  it("stops at the first part it cannot name, keeping the drawers under it", () => {
    expect(parseInspectStack("repository/media/nas,bogus/x,snapshot-policy/media/photos")).toEqual([
      nas,
    ]);
    expect(parseInspectStack("nonsense,repository/media/nas")).toEqual([]);
    for (const raw of ["", 42, null, undefined]) {
      expect(parseInspectStack(raw), String(raw)).toEqual([]);
    }
  });

  it("opens a new object on top", () => {
    expect(pushInspect([nas], photos)).toEqual({ stack: [nas, photos], truncated: false });
    expect(pushInspect([], nas)).toEqual({ stack: [nas], truncated: false });
  });

  it("backs up to an object already open lower down, rather than opening it twice", () => {
    expect(pushInspect([nas, photos, shared], { ...photos })).toEqual({
      stack: [nas, photos],
      truncated: true,
    });
  });

  it("is kept by the root route as the stack it parses to", () => {
    const validate = RootRoute.options.validateSearch as (s: Record<string, unknown>) => unknown;
    expect(validate({ inspect: "repository/media/nas,snapshot-policy/media/photos" })).toEqual({
      inspect: "repository/media/nas,snapshot-policy/media/photos",
    });
    expect(validate({ inspect: "repository/media/nas,bogus" })).toEqual({
      inspect: "repository/media/nas",
    });
  });
});

describe("inspect history entries", () => {
  it("starts a chain on the page or a deep link, and continues one it was pushed onto", () => {
    expect(inspectPushEntry({}, 0)).toEqual({ inspectDepth: 1, inspectBase: 0 });
    // Two drawers from a deep link: nothing behind it to step back to.
    expect(inspectPushEntry({}, 2)).toEqual({ inspectDepth: 3, inspectBase: 2 });
    expect(inspectPushEntry({ inspectDepth: 1, inspectBase: 0 }, 1)).toEqual({
      inspectDepth: 2,
      inspectBase: 0,
    });
  });

  it("steps back as far as the chain reaches, and no further", () => {
    const three = { inspectDepth: 3, inspectBase: 0 };
    expect(inspectStepsBack(three, 3, 1)).toBe(2);
    expect(inspectStepsBack(three, 3, 0)).toBe(3);
    expect(inspectStepsBack({ inspectDepth: 3, inspectBase: 2 }, 3, 1)).toBeNull();
    // An entry not pushed at this depth (a deep link, a replaced one).
    expect(inspectStepsBack({}, 3, 1)).toBeNull();
    expect(inspectStepsBack({ inspectDepth: 2, inspectBase: 0 }, 3, 1)).toBeNull();
  });
});
