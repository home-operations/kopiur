import { describe, expect, it } from "vitest";

import type { ObjectKind } from "../api/types";
import { KIND_META, parseRef } from "./kind";

const KINDS: readonly ObjectKind[] = [
  "repository",
  "clusterRepository",
  "maintenance",
  "snapshotPolicy",
  "snapshotSchedule",
  "snapshot",
  "restore",
  "repositoryReplication",
  "snapshotReplication",
];

describe("KIND_META", () => {
  it("gives every kind its own glyph and its own token slug", () => {
    expect(new Set(KINDS.map((k) => KIND_META[k].icon)).size).toBe(KINDS.length);
    expect(new Set(KINDS.map((k) => KIND_META[k].slug)).size).toBe(KINDS.length);
  });

  it("names each kind exactly as the CRD kind, the word kubectl takes", () => {
    expect(KIND_META.snapshotPolicy.label).toBe("SnapshotPolicy");
    expect(KIND_META.clusterRepository.slug).toBe("cluster-repository");
  });
});

describe("parseRef", () => {
  it("reads the wire's Kind/ns/name and Kind/name reference strings", () => {
    expect(parseRef("Repository/media/nas")).toEqual({
      kind: "repository",
      namespace: "media",
      name: "nas",
    });
    expect(parseRef("ClusterRepository/shared")).toEqual({
      kind: "clusterRepository",
      name: "shared",
    });
    expect(parseRef("SnapshotPolicy/kopiur-dev/app-data")?.kind).toBe("snapshotPolicy");
  });

  it("does not invent a kind for something that is not one", () => {
    expect(parseRef("Backend/x")).toBeNull();
    expect(parseRef("nas")).toBeNull();
    expect(parseRef("")).toBeNull();
  });
});
