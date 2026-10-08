import { describe, expect, it } from "vitest";

import type { ObjectKind } from "../api/types";
import { Route as RootRoute } from "../routes/__root";
import { KIND_META } from "./kind";
import { inspectToken, parseInspect } from "./inspect";

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
