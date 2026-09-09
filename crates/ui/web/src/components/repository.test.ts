import { describe, expect, it } from "vitest";

import type { RepositorySummary } from "../api/types";
import { EMPTY_CELL } from "../util/format";
import {
  accessLabel,
  actionNamespace,
  detailSearch,
  filterByHealth,
  isClusterScoped,
  isHealthKey,
  repositoryPatchCapability,
  repositoryPhaseLabel,
  repositoryVerdict,
  suspendKindToken,
} from "./repository";

function summary(over: Partial<RepositorySummary> = {}): RepositorySummary {
  return {
    kind: "Repository",
    kindPath: "repository",
    name: "nas",
    namespace: "media",
    phase: "ready",
    health: "healthy",
    backend: "S3",
    mode: "ReadWrite",
    serverBacked: false,
    suspended: false,
    snapshotCount: 412,
    totalSizeBytes: 987654321,
    indexBlobCount: 17,
    lastObservedAt: null,
    serverEndpoint: null,
    allowedNamespaceCount: null,
    ...over,
  };
}

describe("isHealthKey", () => {
  it("accepts every lamp the health strip links with", () => {
    for (const key of ["healthy", "degraded", "failed", "suspended", "pending", "unknown"]) {
      expect(isHealthKey(key)).toBe(true);
    }
  });

  it("rejects anything else, so a hand-edited ?health= cannot filter to nothing silently", () => {
    expect(isHealthKey("archived")).toBe(false);
    expect(isHealthKey("")).toBe(false);
    expect(isHealthKey(3)).toBe(false);
    expect(isHealthKey(undefined)).toBe(false);
  });
});

describe("repositoryPhaseLabel", () => {
  it("titles each unit variant", () => {
    expect(repositoryPhaseLabel("ready")).toBe("Ready");
    expect(repositoryPhaseLabel("initializing")).toBe("Initializing");
    expect(repositoryPhaseLabel("degraded")).toBe("Degraded");
    expect(repositoryPhaseLabel("failed")).toBe("Failed");
    expect(repositoryPhaseLabel("pending")).toBe("Pending");
  });

  it("renders a newer operator's phase as the raw word it wrote", () => {
    expect(repositoryPhaseLabel({ unknown: { raw: "Reconciling" } })).toBe("Reconciling");
  });

  it("is the empty cell when the operator has written no phase at all", () => {
    expect(repositoryPhaseLabel(null)).toBe(EMPTY_CELL);
    expect(repositoryPhaseLabel(undefined)).toBe(EMPTY_CELL);
  });
});

describe("accessLabel", () => {
  it("separates how clients reach the repository from what they may do", () => {
    expect(accessLabel(summary({ serverBacked: false }))).toBe("Direct to backend");
    expect(accessLabel(summary({ serverBacked: true }))).toBe("Repository server");
  });
});

describe("suspendKindToken", () => {
  it("is the row's own kindPath, never a mapping table", () => {
    expect(suspendKindToken(summary())).toBe("repository");
    expect(
      suspendKindToken(summary({ kind: "ClusterRepository", kindPath: "cluster-repository" })),
    ).toBe("cluster-repository");
  });
});

describe("the cluster-scoped split", () => {
  const cluster = summary({
    kind: "ClusterRepository",
    kindPath: "cluster-repository",
    namespace: null,
  });

  it("decides scope from the one field that also builds the URL and the body", () => {
    expect(isClusterScoped(summary())).toBe(false);
    expect(isClusterScoped(cluster)).toBe(true);
  });

  it("judges a ClusterRepository against the cluster-scoped review, never a namespaced one", () => {
    expect(repositoryPatchCapability(summary())).toBe("patchRepositories");
    expect(repositoryPatchCapability(cluster)).toBe("patchClusterRepositories");
  });

  it("sends no namespace for a ClusterRepository, which the handler would drop anyway", () => {
    expect(actionNamespace(summary())).toBe("media");
    expect(actionNamespace(cluster)).toBeUndefined();
  });

  it("never sends an empty namespace, which the handler answers 400 for", () => {
    expect(actionNamespace(summary({ namespace: "" }))).toBeUndefined();
  });
});

describe("detailSearch", () => {
  it("carries the repository's own namespace for a namespaced kind", () => {
    expect(detailSearch(summary({ namespace: "media" }))).toEqual({ namespace: "media" });
  });

  it("carries no namespace for a ClusterRepository, so /me is reviewed cluster-scoped", () => {
    expect(
      detailSearch(
        summary({ kind: "ClusterRepository", kindPath: "cluster-repository", namespace: null }),
      ),
    ).toEqual({});
  });

  it("carries none when a namespaced row arrived without one, so the server explains it", () => {
    expect(detailSearch(summary({ namespace: null }))).toEqual({});
  });
});

describe("filterByHealth", () => {
  const rows = [
    summary({ name: "a", health: "healthy" }),
    summary({ name: "b", health: "failed" }),
    summary({ name: "c", health: "failed" }),
  ];

  it("returns every row when no health is asked for", () => {
    expect(filterByHealth(rows, undefined)).toHaveLength(3);
  });

  it("keeps only the asked-for lamp", () => {
    expect(filterByHealth(rows, "failed").map((r) => r.name)).toEqual(["b", "c"]);
  });

  it("files a health this bundle has never seen under unknown, never under healthy", () => {
    const odd = [summary({ name: "d", health: "archived" as RepositorySummary["health"] })];
    expect(filterByHealth(odd, "healthy")).toHaveLength(0);
    expect(filterByHealth(odd, "unknown")).toHaveLength(1);
  });
});

describe("repositoryVerdict", () => {
  it("leads with the lamp's word and says the mode and access path", () => {
    const verdict = repositoryVerdict(summary());
    expect(verdict.lamp.key).toBe("healthy");
    expect(verdict.text).toContain("Ready");
    expect(verdict.text).toContain("read-write");
    expect(verdict.text).toContain("directly");
  });

  it("says a suspended repository is taking no new backups", () => {
    const verdict = repositoryVerdict(summary({ suspended: true, health: "suspended" }));
    expect(verdict.lamp.key).toBe("suspended");
    expect(verdict.text).toContain("no new backups");
  });

  it("never claims a phase it was not told", () => {
    const verdict = repositoryVerdict(summary({ phase: null, health: "unknown" }));
    expect(verdict.lamp.key).toBe("unknown");
    expect(verdict.text).toContain("has written no phase");
  });

  it("names read-only mode, which is a repository that cannot be written to", () => {
    expect(repositoryVerdict(summary({ mode: "ReadOnly" })).text).toContain("read-only");
  });

  it("renders a mode string this bundle does not know as the server's own word", () => {
    expect(repositoryVerdict(summary({ mode: "Append" })).text).toContain("Append");
  });
});
