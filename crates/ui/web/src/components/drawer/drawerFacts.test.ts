import { describe, expect, it } from "vitest";

import type { CardRow } from "../objectCard";
import { drawerFacts } from "./drawerFacts";

function fact(card: CardRow, label: string): unknown {
  return drawerFacts(card).facts.find((f) => f.label === label)?.value;
}

describe("drawerFacts", () => {
  it("names a repository's backend and a cluster repository's admission", () => {
    const row = {
      kind: "ClusterRepository",
      kindPath: "cluster-repository",
      name: "shared",
      phase: "ready" as const,
      health: "healthy" as const,
      backend: "S3",
      mode: "ReadWrite",
      serverBacked: false,
      suspended: false,
      admits: "all" as const,
    };
    const card: CardRow = { kind: "clusterRepository", row };
    expect(fact(card, "Backend")).toBe("S3");
    expect(fact(card, "Admits")).toBe("admits all namespaces");
    expect(fact({ kind: "repository", row: { ...row, admits: null } }, "Admits")).toBeUndefined();
    expect(drawerFacts(card).related).toEqual([]);
  });

  it("relates a policy to the repositories it writes into", () => {
    const card: CardRow = {
      kind: "snapshotPolicy",
      row: {
        namespace: "kopiur-dev",
        name: "app-data",
        repositories: ["Repository/kopiur-dev/dev-repo", "ClusterRepository/shared"],
        multiRepo: true,
        suspended: false,
      },
    };
    expect(drawerFacts(card).related.map((r) => r.target)).toEqual([
      { kind: "repository", namespace: "kopiur-dev", name: "dev-repo" },
      { kind: "clusterRepository", name: "shared" },
    ]);
  });

  it("relates a schedule to the policy it fires, or says it fires by selector", () => {
    const base = {
      namespace: "kopiur-dev",
      name: "nightly",
      cron: "H 2 * * *",
      timezone: "Europe/Paris",
      suspended: false,
      consecutiveFailures: 0,
    };
    const named: CardRow = { kind: "snapshotSchedule", row: { ...base, policy: "app-data" } };
    expect(fact(named, "Cron")).toBe("H 2 * * * · Europe/Paris");
    expect(drawerFacts(named).related.map((r) => r.target)).toEqual([
      { kind: "snapshotPolicy", namespace: "kopiur-dev", name: "app-data" },
    ]);
    const selected: CardRow = {
      kind: "snapshotSchedule",
      row: { ...base, policySelector: "tier=gold" },
    };
    expect(fact(selected, "Fires policies matching")).toBe("tier=gold");
    expect(drawerFacts(selected).related).toEqual([]);
  });

  it("relates a snapshot to its policy, its repository and where it was copied from", () => {
    const card: CardRow = {
      kind: "snapshot",
      row: {
        namespace: "kopiur-dev",
        name: "app-data-1",
        policy: "app-data",
        repository: "Repository/kopiur-dev/dev-repo",
        copiedFrom: "ClusterRepository/shared",
        identity: "app@kopiur-dev:/data",
        pinned: false,
      },
    };
    expect(fact(card, "Identity")).toBe("app@kopiur-dev:/data");
    expect(fact(card, "Kopia snapshot")).toEqual({ absent: "na" });
    expect(drawerFacts(card).related.map((r) => r.label)).toEqual([
      "Policy",
      "Repository",
      "Copied from",
    ]);
  });

  it("relates a restore to its repository and lists the claims it writes", () => {
    const card: CardRow = {
      kind: "restore",
      row: {
        namespace: "kopiur-dev",
        name: "r1",
        targetKind: "Pvc",
        repository: "Repository/kopiur-dev/dev-repo",
        claims: [
          { pvc: "a", phase: "Populated" },
          { pvc: "b", phase: "Pending" },
        ],
      },
    };
    expect(fact(card, "Claims")).toBe("a (Populated), b (Pending)");
    expect(drawerFacts(card).related.map((r) => r.target.kind)).toEqual(["repository"]);
  });

  it("says who owns a maintenance object and relates it to its repository", () => {
    const card: CardRow = {
      kind: "maintenance",
      row: {
        namespace: "kopiur-dev",
        name: "dev-repo",
        repository: "Repository/kopiur-dev/dev-repo",
        managedByRepository: false,
        quick: { consecutiveFailures: 0 },
        full: { consecutiveFailures: 0 },
      },
    };
    expect(fact(card, "Managed by")).toBe("authored by hand");
    expect(drawerFacts(card).related.map((r) => r.label)).toEqual(["Repository"]);
  });

  it("names the owner only when it is not the repository already named", () => {
    const row = {
      namespace: "kopiur-dev",
      name: "dev-repo",
      repository: "Repository/kopiur-dev/dev-repo",
      owner: "Repository/kopiur-dev/dev-repo",
      managedByRepository: true,
      quick: { consecutiveFailures: 0 },
      full: { consecutiveFailures: 0 },
    };
    expect(drawerFacts({ kind: "maintenance", row }).related.map((r) => r.label)).toEqual([
      "Repository",
    ]);
    const other = { ...row, owner: "ClusterRepository/shared" };
    expect(drawerFacts({ kind: "maintenance", row: other }).related.map((r) => r.label)).toEqual([
      "Repository",
      "Owner",
    ]);
  });

  it("relates replications to their ends, and leaves a bare backend as text", () => {
    const blob: CardRow = {
      kind: "repositoryReplication",
      row: {
        namespace: "kopiur-dev",
        name: "offsite",
        source: "Repository/kopiur-dev/dev-repo",
        destinationBackend: "S3",
        cron: "0 3 * * *",
        suspended: false,
      },
    };
    expect(fact(blob, "Destination")).toBe("S3");
    expect(drawerFacts(blob).related.map((r) => r.label)).toEqual(["Source"]);
    const copy: CardRow = {
      kind: "snapshotReplication",
      row: {
        namespace: "kopiur-dev",
        name: "copy",
        source: "Repository/kopiur-dev/dev-repo",
        destination: "ClusterRepository/shared",
        cron: "0 4 * * *",
        suspended: false,
      },
    };
    expect(drawerFacts(copy).related.map((r) => r.label)).toEqual(["Source", "Destination"]);
  });

  it("keeps a reference it cannot parse as text, never as a guessed link", () => {
    const card: CardRow = {
      kind: "maintenance",
      row: {
        namespace: "kopiur-dev",
        name: "odd",
        repository: "Backend/elsewhere",
        managedByRepository: true,
        quick: { consecutiveFailures: 0 },
        full: { consecutiveFailures: 0 },
      },
    };
    expect(drawerFacts(card).related).toEqual([]);
    expect(fact(card, "Repository")).toBe("Backend/elsewhere");
  });
});
