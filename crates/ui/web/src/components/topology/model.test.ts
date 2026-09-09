import { afterEach, describe, expect, it, vi } from "vitest";

import type { EdgeKind, GraphNode, NodeKind, RepositoryGraph } from "../../api/types";
import {
  BLOBSYNC_BACKEND,
  FIXTURE_EDGE_IDS,
  FIXTURE_GRAPH,
  FIXTURE_NODE_IDS,
  GONE,
  MIRROR,
  NAS,
  NIGHTLY,
  SHARED,
} from "./fixture";
import {
  EDGE_LEGEND,
  edgeStyle,
  isRepositoryKind,
  nodeKindWord,
  nodeLamp,
  relationships,
  topologyModel,
  topologyVerdict,
} from "./model";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("topologyModel", () => {
  it("carries exactly the server's node and edge sets, by id", () => {
    const model = topologyModel(FIXTURE_GRAPH);
    expect(model.nodes.map((n) => n.id).sort()).toEqual([...FIXTURE_NODE_IDS]);
    expect(model.edges.map((e) => e.id).sort()).toEqual([...FIXTURE_EDGE_IDS]);
    expect(model.byId.size).toBe(FIXTURE_NODE_IDS.length);
  });

  it("gives the dangling reference the missing treatment, never a plain lamp", () => {
    const model = topologyModel(FIXTURE_GRAPH);
    const ghost = model.byId.get(GONE.id);
    expect(ghost?.missing).toBe(true);
    expect(ghost?.lamp.key).toBe("failed");
    expect(ghost?.lamp.word).toBe("referenced but not found");
    // The edge into it is failed however healthy its source is.
    const into = model.edges.find((e) => e.to === GONE.id);
    expect(into?.lamp.key).toBe("failed");
  });

  it("renders an unknown phase as the unknown lamp, never healthy", () => {
    const model = topologyModel(FIXTURE_GRAPH);
    const mirror = model.byId.get(MIRROR.id);
    expect(mirror?.lamp.key).toBe("unknown");
    expect(mirror?.lamp.word).toBe("Unknown");
  });

  it("marks a repository with no replication out of it, and only those", () => {
    const model = topologyModel(FIXTURE_GRAPH);
    // `nas` replicates to `shared` and to the backend; `shared` and `mirror` go nowhere.
    expect(model.byId.get(NAS.id)?.replicatesNowhere).toBe(false);
    expect(model.byId.get(SHARED.id)?.replicatesNowhere).toBe(true);
    expect(model.byId.get(MIRROR.id)?.replicatesNowhere).toBe(true);
    // A ghost is not a repository that could replicate; a policy or backend never is.
    expect(model.byId.get(GONE.id)?.replicatesNowhere).toBe(false);
    expect(model.byId.get(NIGHTLY.id)?.replicatesNowhere).toBe(false);
    expect(model.byId.get(BLOBSYNC_BACKEND.id)?.replicatesNowhere).toBe(false);
  });

  it("keeps an edge whose endpoint the server did not send, rather than dropping it", () => {
    const graph: RepositoryGraph = {
      nodes: [NAS],
      edges: [
        {
          id: "SnapshotReplication/media/x",
          from: NAS.id,
          to: "Repository/media/elsewhere",
          kind: "snapshotReplication",
          health: "healthy",
        },
      ],
      generatedAt: "2026-09-08T12:00:00Z",
    };
    const model = topologyModel(graph);
    expect(model.edges).toHaveLength(1);
    expect(model.edges[0]?.dangling).toBe(true);
  });
});

describe("relationships", () => {
  it("splits a node's edges into inbound and outbound", () => {
    const model = topologyModel(FIXTURE_GRAPH);
    const nas = relationships(model, NAS.id);
    expect(nas.inbound.map((e) => e.id)).toEqual([
      "PolicyMembership/Policy/media/nightly/Repository/media/nas",
    ]);
    expect(nas.outbound.map((e) => e.id).sort()).toEqual([
      "RepositoryReplication/media/blobsync",
      "Seed/Repository/media/nas/Repository/media/mirror",
      "SnapshotReplication/media/offsite",
    ]);
  });
});

describe("nodeLamp / nodeKindWord / edgeStyle", () => {
  it("is exhaustive over the node kinds and renders an unseen kind as its raw word", () => {
    expect(nodeKindWord("repository")).toBe("Repository");
    expect(nodeKindWord("clusterRepository")).toBe("Cluster repository");
    expect(nodeKindWord("backend")).toBe("Backend");
    expect(nodeKindWord("policy")).toBe("Policy");
    expect(nodeKindWord("namespace")).toBe("Namespace");
    expect(nodeKindWord("namespaceSelector")).toBe("Namespace selector");
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    expect(nodeKindWord("volume" as NodeKind)).toBe("volume");
    expect(warn).toHaveBeenCalledOnce();
  });

  it("styles every edge kind distinctly and never throws on an unseen one", () => {
    const styles = EDGE_LEGEND.map(edgeStyle);
    expect(EDGE_LEGEND).toEqual([
      "snapshotReplication",
      "repositoryReplication",
      "seed",
      "policyMembership",
      "allowedNamespace",
    ]);
    const signatures = new Set(styles.map((s) => `${s.dash ?? "solid"}/${s.width}/${s.marker}`));
    expect(signatures.size).toBe(EDGE_LEGEND.length);
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const unseen = edgeStyle("mirror" as EdgeKind);
    expect(unseen.key).toBe("unknown");
    expect(unseen.word).toBe("mirror");
    expect(warn).toHaveBeenCalledOnce();
  });

  it("lamps a health this bundle has never seen as unknown, not healthy", () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const odd: GraphNode = { ...NAS, health: "archived" as GraphNode["health"] };
    expect(nodeLamp(odd).key).toBe("unknown");
    expect(nodeLamp(odd).word).toBe("archived");
  });

  it("knows which kinds are repositories", () => {
    expect(isRepositoryKind("repository")).toBe(true);
    expect(isRepositoryKind("clusterRepository")).toBe(true);
    expect(isRepositoryKind("backend")).toBe(false);
    expect(isRepositoryKind("policy")).toBe(false);
  });
});

describe("topologyVerdict", () => {
  const without = (ids: string[]): RepositoryGraph => ({
    nodes: FIXTURE_GRAPH.nodes.filter((n) => !ids.includes(n.id)),
    edges: FIXTURE_GRAPH.edges.filter((e) => !ids.includes(e.from) && !ids.includes(e.to)),
    generatedAt: FIXTURE_GRAPH.generatedAt,
  });

  it("names the dangling reference first, above every other fault", () => {
    const verdict = topologyVerdict(topologyModel(FIXTURE_GRAPH));
    expect(verdict.health).toBe("failed");
    expect(verdict.text).toBe("1 reference points at a repository that does not exist");
  });

  it("then a failing replication", () => {
    const verdict = topologyVerdict(topologyModel(without([GONE.id])));
    expect(verdict.health).toBe("failed");
    expect(verdict.text).toBe("1 replication is failing");
  });

  it("then a repository whose state this console cannot read — never healthy", () => {
    const graph = without([GONE.id, BLOBSYNC_BACKEND.id]);
    const verdict = topologyVerdict(topologyModel(graph));
    expect(verdict.health).toBe("unknown");
    expect(verdict.text).toBe("1 object reports a state this console cannot read");
  });

  it("does not count a bare backend's unobserved health as unknown", () => {
    const graph: RepositoryGraph = {
      ...without([GONE.id, MIRROR.id]),
      edges: without([GONE.id, MIRROR.id]).edges.map((e) =>
        e.id === "RepositoryReplication/media/blobsync" ? { ...e, health: "healthy" } : e,
      ),
    };
    const verdict = topologyVerdict(topologyModel(graph));
    expect(verdict.health).toBe("healthy");
    expect(verdict.text).toBe("Every repository, policy and replication reports healthy");
  });

  it("counts the repositories that replicate nowhere as a fact beside the verdict", () => {
    const verdict = topologyVerdict(topologyModel(FIXTURE_GRAPH));
    expect(verdict.facts).toEqual({
      repositories: 3,
      policies: 2,
      replications: 2,
      replicateNowhere: 2,
      missing: 1,
    });
  });

  it("is the unknown lamp over an empty graph, not the healthy one", () => {
    const verdict = topologyVerdict(
      topologyModel({ nodes: [], edges: [], generatedAt: FIXTURE_GRAPH.generatedAt }),
    );
    expect(verdict.health).toBe("unknown");
  });
});
