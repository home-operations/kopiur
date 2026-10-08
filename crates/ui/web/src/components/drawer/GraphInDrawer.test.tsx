import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import type { PolicyDetail, RepositoryDetail, RepositorySummary } from "../../api/types";
import {
  forbiddenProblem,
  jsonResponse,
  mockApi,
  mountApp,
  problemResponse,
} from "../../test-utils";
import { FIXTURE_GRAPH } from "../topology/fixture";

/**
 * What the topology board knows about a resource — every relationship with its
 * own state, what it admits, what it was seeded from, whether anything copies
 * it, and references to things that do not exist — read in the resource
 * drawer, now that the board has no drawer of its own.
 */

function summary(over: Partial<RepositorySummary>): RepositorySummary {
  return {
    kind: "Repository",
    kindPath: "repository",
    name: "nas",
    namespace: "media",
    phase: "ready",
    health: "healthy",
    backend: "Filesystem",
    mode: "ReadWrite",
    serverBacked: false,
    suspended: false,
    ...over,
  };
}

function repo(over: Partial<RepositorySummary>): RepositoryDetail {
  return {
    summary: summary(over),
    gates: [],
    conditions: [],
    policies: [],
    schedules: [],
    replicationsOut: [],
    replicationsIn: [],
    sessions: [],
  };
}

const orphaned: PolicyDetail = {
  row: {
    namespace: "media",
    name: "orphaned",
    repositories: ["Repository/media/gone"],
    multiRepo: false,
    suspended: false,
  },
  sources: [],
  verification: [],
  schedules: [],
  recentSnapshots: [],
  gates: [],
  conditions: [],
};

const GRAPH = { "/api/v1/graph": jsonResponse(FIXTURE_GRAPH) };
const PAGE = "/doctor";

async function drawer() {
  return screen.findByRole("dialog");
}

async function openTab(name: string) {
  const panel = await drawer();
  await userEvent.click(await within(panel).findByRole("tab", { name: new RegExp(`^${name}`) }));
  return within(panel).getByRole("tabpanel");
}

describe("the resource drawer, with what the board knows", () => {
  it("lists every relationship with its own state and label, ends named in their kind", async () => {
    mockApi({ ...GRAPH, "/api/v1/repositories/repository/nas": jsonResponse(repo({})) });
    mountApp(`${PAGE}?inspect=repository/media/nas`);
    const tab = await openTab("Relationships");
    const out = within(tab).getByRole("region", { name: "Points at" });
    expect(within(out).getByText("Seed")).toBeInTheDocument();
    expect(within(out).getByRole("link", { name: /mirror/ })).toBeInTheDocument();
    // The failing replication's own lamp, its cron, and a backend that is not a resource.
    const blob = within(out).getByText("Repository replication").closest("li");
    expect(blob).not.toBeNull();
    expect(blob?.querySelector(".health[data-health='failed']")).not.toBeNull();
    expect(blob).toHaveTextContent("0 5 * * *");
    expect(blob).toHaveTextContent("s3 dr-bucket/nas/");
    expect(blob?.querySelector("a")).toBeNull();
    const into = within(tab).getByRole("region", { name: "Pointed at by" });
    expect(within(into).getByText("Policy membership")).toBeInTheDocument();
    expect(within(into).getByRole("link", { name: /nightly/ })).toBeInTheDocument();
  });

  it("says in one line what the kind is", async () => {
    mockApi({ ...GRAPH, "/api/v1/repositories/repository/nas": jsonResponse(repo({})) });
    mountApp(`${PAGE}?inspect=repository/media/nas`);
    expect(await within(await drawer()).findByText(/a namespaced Repository/)).toBeInTheDocument();
  });

  it("says loudly when nothing copies a repository anywhere", async () => {
    mockApi({
      ...GRAPH,
      "/api/v1/repositories/repository/mirror": jsonResponse(repo({ name: "mirror" })),
    });
    mountApp(`${PAGE}?inspect=repository/media/mirror`);
    expect(
      await within(await drawer()).findByText("Nothing copies mirror anywhere."),
    ).toBeInTheDocument();
  });

  it("names the namespaces a ClusterRepository admits", async () => {
    mockApi({
      ...GRAPH,
      "/api/v1/repositories/cluster-repository/shared": jsonResponse(
        repo({
          kind: "ClusterRepository",
          kindPath: "cluster-repository",
          name: "shared",
          namespace: null,
        }),
      ),
    });
    mountApp(`${PAGE}?inspect=cluster-repository/shared`);
    const out = within(await openTab("Relationships")).getByRole("region", { name: "Points at" });
    expect(within(out).getAllByText("Allowed namespace")).toHaveLength(2);
    expect(out).toHaveTextContent("prod");
    expect(out).toHaveTextContent("staging");
  });

  it("names a selector once, not again as the link's label", async () => {
    const graph = {
      ...FIXTURE_GRAPH,
      nodes: [
        ...FIXTURE_GRAPH.nodes,
        {
          id: "NamespaceSelector/shared",
          kind: "namespaceSelector" as const,
          name: "shared",
          label: "team=media",
          health: "healthy" as const,
          missing: false,
          allowsAllNamespaces: false,
          gates: [],
        },
      ],
      edges: [
        ...FIXTURE_GRAPH.edges,
        {
          id: "AllowedNamespace/ClusterRepository/shared/NamespaceSelector/shared",
          from: "ClusterRepository/shared",
          to: "NamespaceSelector/shared",
          kind: "allowedNamespace" as const,
          label: "team=media",
          health: "healthy" as const,
        },
      ],
    };
    mockApi({
      "/api/v1/graph": jsonResponse(graph),
      "/api/v1/repositories/cluster-repository/shared": jsonResponse(
        repo({
          kind: "ClusterRepository",
          kindPath: "cluster-repository",
          name: "shared",
          namespace: null,
        }),
      ),
    });
    mountApp(`${PAGE}?inspect=cluster-repository/shared`);
    const out = within(await openTab("Relationships")).getByRole("region", { name: "Points at" });
    expect(within(out).getAllByText("team=media")).toHaveLength(1);
  });

  it("calls out a reference to a repository that does not exist", async () => {
    mockApi({ ...GRAPH, "/api/v1/policies/media/orphaned": jsonResponse(orphaned) });
    mountApp(`${PAGE}?inspect=snapshot-policy/media/orphaned`);
    expect(
      await within(await drawer()).findByText("gone is referenced here but does not exist."),
    ).toBeInTheDocument();
  });

  it("explains a ghost: what refers to it, and that it is not there", async () => {
    mockApi({
      ...GRAPH,
      "/api/v1/repositories/repository/gone": problemResponse({
        type: "urn:kopiur:problem:not-found",
        title: "Not Found",
        status: 404,
        detail: "gone",
        what: "There is no Repository called gone in namespace media.",
        why: "It was deleted or never existed.",
        fix: "create it",
        instance: "/api/v1/repositories/repository/gone",
      }),
    });
    mountApp(`${PAGE}?inspect=repository/media/gone`);
    const panel = await drawer();
    expect(
      await within(panel).findByText("gone is referenced here but does not exist."),
    ).toBeInTheDocument();
    const into = within(panel).getByRole("region", { name: "Pointed at by" });
    expect(within(into).getByRole("link", { name: /orphaned/ })).toBeInTheDocument();
  });

  it("still shows the resource when the graph is refused, just without what it adds", async () => {
    mockApi({
      "/api/v1/graph": problemResponse(forbiddenProblem("read the graph", "/api/v1/graph")),
      "/api/v1/repositories/repository/nas": jsonResponse(repo({})),
    });
    mountApp(`${PAGE}?inspect=repository/media/nas`);
    const panel = await drawer();
    await within(panel).findByRole("status", { name: "Repository verdict" });
    expect(within(panel).queryByRole("tab", { name: /Relationships/ })).toBeNull();
  });
});
